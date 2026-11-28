// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The requests the docker CLI (v29) sends for `run`, `ps`, `logs`, `stop`, `rm` and `exec`, replayed over a real unix
//! socket. No VM boots here: "starting" a container is done by editing its record, and the test writes the guest's
//! console.

use super::*;
use fluxvm_core::config::Config;
use fluxvm_scheduler::oci_sandbox::{CREATED_LABEL, OCI_IMAGE_LABEL};
use std::io::Write as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

const LIMIT: Duration = Duration::from_secs(10);

struct Engine {
    dir: tempfile::TempDir,
    st: DockerState,
    socket: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn engine() -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        state_dir: dir.path().join("state"),
        run_dir: dir.path().join("run"),
        ..Config::default()
    };
    let manager = VmManager::new(cfg).unwrap();
    let build = Arc::new(crate::docker_build::BuildEngine::new(
        Arc::new(crate::docker_build::FluxctlBuilder::new(None, None)),
        Arc::new(crate::docker_build::History::new(None)),
    ));
    let st = DockerState::new(manager, build);
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let app = routes(st.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Engine {
        dir,
        st,
        socket,
        server,
    }
}

/// A container VM's record as `docker create` leaves it.
async fn created(e: &Engine, name: &str, labels: &[&str]) -> VmRecord {
    let mut vm = crate::docker_engine::vm_fixture(name, VmStatus::Stopped, true);
    vm.workspace = e.dir.path().join(vm.id.to_string());
    std::fs::create_dir_all(&vm.workspace).unwrap();
    vm.log_path = vm.workspace.join("console.log");
    vm.disk = vm.workspace.join("root.raw");
    vm.request.apple = Some(serde_json::from_value(json!({ "init_config": {} })).unwrap());
    vm.labels.insert(OCI_IMAGE_LABEL.into(), "alpine".into());
    vm.labels.insert(CREATED_LABEL.into(), "true".into());
    for l in labels {
        vm.labels.insert((*l).into(), "true".into());
    }
    e.st.manager.store.insert(vm.clone()).await.unwrap();
    vm
}

/// What a successful start leaves behind, without booting anything.
async fn start(e: &Engine, id: Uuid) {
    let mut vm = e.st.manager.get(id).await.unwrap();
    vm.status = VmStatus::Running;
    vm.labels.remove(CREATED_LABEL);
    e.st.manager.store.update(vm).await.unwrap();
    e.st.containers.started(id);
}

fn guest_writes(vm: &VmRecord, bytes: &str) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&vm.log_path)
        .unwrap()
        .write_all(bytes.as_bytes())
        .unwrap();
}

struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    stream: BufReader<UnixStream>,
}

impl Reply {
    /// The whole body: chunked, of a known length, or (an upgraded connection) up to the close.
    async fn body(mut self) -> Vec<u8> {
        tokio::time::timeout(LIMIT, async {
            let mut out = Vec::new();
            let chunked = self.headers.get("transfer-encoding").map(String::as_str);
            let length = self.headers.get("content-length");
            if chunked == Some("chunked") {
                loop {
                    let mut size = String::new();
                    self.stream.read_line(&mut size).await.unwrap();
                    let n = usize::from_str_radix(size.trim(), 16).unwrap();
                    let mut chunk = vec![0; n + 2];
                    self.stream.read_exact(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
            } else if let Some(n) = length
                && self.status != 101
            {
                out = vec![0; n.parse().unwrap()];
                self.stream.read_exact(&mut out).await.unwrap();
            } else if self.status != 204 && self.status != 304 {
                self.stream.read_to_end(&mut out).await.unwrap();
            }
            out
        })
        .await
        .expect("the body did not end")
    }

    async fn json(self) -> Value {
        let body = self.body().await;
        serde_json::from_slice(&body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&body)))
    }
}

/// Send a request as the docker CLI does and return once the response head has arrived.
async fn request(e: &Engine, line: &str, upgrade: bool, body: &str) -> Reply {
    let mut stream = UnixStream::connect(&e.socket).await.unwrap();
    let mut head = format!(
        "{line} HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: Docker-Client/29.0.0 (linux)\r\nContent-Length: {}\r\n",
        body.len()
    );
    if upgrade {
        head.push_str("Connection: Upgrade\r\nUpgrade: tcp\r\n");
    }
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    head.push_str(body);
    stream.write_all(head.as_bytes()).await.unwrap();
    tokio::time::timeout(LIMIT, async {
        let mut stream = BufReader::new(stream);
        let mut status = String::new();
        stream.read_line(&mut status).await.unwrap();
        let status: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut headers = BTreeMap::new();
        loop {
            let mut l = String::new();
            stream.read_line(&mut l).await.unwrap();
            let Some((k, v)) = l.trim().split_once(':') else {
                break;
            };
            headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
        }
        Reply {
            status,
            headers,
            stream,
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no response head for {line}"))
}

/// Docker's multiplexed stream as (stream type, text).
fn demux(mut raw: &[u8]) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    while !raw.is_empty() {
        let n = u32::from_be_bytes(raw[4..8].try_into().unwrap()) as usize;
        out.push((raw[0], String::from_utf8_lossy(&raw[8..8 + n]).into_owned()));
        raw = &raw[8 + n..];
    }
    out
}

fn text(raw: &[u8]) -> String {
    demux(raw).into_iter().map(|(_, s)| s).collect()
}

async fn inspected_state(e: &Engine, name: &str) -> Value {
    let line = format!("GET /v1.44/containers/{name}/json");
    request(e, &line, false, "").await.json().await["State"].clone()
}

/// `docker ps` as "name status" rows.
async fn ps(e: &Engine, query: &str) -> Vec<String> {
    let line = format!("GET /v1.44/containers/json{query}");
    let reply = request(e, &line, false, "").await;
    assert_eq!(reply.status, 200);
    reply
        .json()
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["Names"][0].as_str().unwrap(),
                c["Status"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn foreground_run_attaches_waits_then_follows_the_run() {
    let e = engine().await;
    let vm = created(&e, "fg", &[]).await;
    let id = docker_id(vm.id);

    let line = format!("POST /v1.44/containers/{id}/attach?stderr=1&stdout=1&stream=1");
    let attach = request(&e, &line, true, "").await;
    assert_eq!(attach.status, 101);
    assert_eq!(attach.headers["upgrade"], "tcp");
    assert_eq!(attach.headers["connection"].to_ascii_lowercase(), "upgrade");
    assert_eq!(
        attach.headers["content-type"],
        "application/vnd.docker.multiplexed-stream"
    );
    // The CLI starts the container only after this head arrives.
    let line = format!("POST /v1.44/containers/{id}/wait?condition=next-exit");
    let wait = request(&e, &line, false, "").await;
    assert_eq!(wait.status, 200);
    assert_eq!(inspected_state(&e, "fg").await["Status"], "created");
    assert!(ps(&e, "").await.is_empty());

    start(&e, vm.id).await;
    guest_writes(
        &vm,
        "VELORA-IP 192.168.64.2\r\nfluxvm-guest-agent listening\r\nhello\r\n",
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    guest_writes(
        &vm,
        "tailFLUXVM-EXIT 3\r\nfluxvm-oci-init: powering off\r\n[    1.234567] reboot: Power down\r\n",
    );

    assert_eq!(text(&attach.body().await), "hello\ntail");
    assert_eq!(wait.json().await["StatusCode"], 3);
    let state = inspected_state(&e, "fg").await;
    assert_eq!(
        (state["Status"].as_str(), state["ExitCode"].as_i64()),
        (Some("exited"), Some(3))
    );
}

#[tokio::test]
async fn a_run_shorter_than_a_poll_is_still_seen() {
    let e = engine().await;
    let vm = created(&e, "quick", &[]).await;
    let line = "POST /v1.44/containers/quick/attach?stderr=1&stdout=1&stream=1";
    let attach = request(&e, line, true, "").await;
    let line = "POST /v1.44/containers/quick/wait?condition=next-exit";
    let wait = request(&e, line, false, "").await;
    guest_writes(&vm, "done\r\nFLUXVM-EXIT 0\r\n");
    start(&e, vm.id).await;
    assert_eq!(text(&attach.body().await), "done\n");
    assert_eq!(wait.json().await["StatusCode"], 0);
}

#[tokio::test]
async fn detached_run_gets_the_wait_head_at_once() {
    let e = engine().await;
    let vm = created(&e, "bg", &[]).await;
    // `docker run -d` calls wait before start too; holding the head back until the exit hung it.
    let line = "POST /v1.44/containers/bg/wait?condition=next-exit";
    let wait = request(&e, line, false, "").await;
    assert_eq!(wait.status, 200);
    start(&e, vm.id).await;
    let again = request(&e, "POST /v1.44/containers/bg/start", false, "").await;
    assert_eq!(again.status, 304, "already running");
    assert_eq!(ps(&e, "").await, ["/bg Up"]);
}

#[tokio::test]
async fn ps_logs_stop_and_rm_take_the_cli_flags() {
    let e = engine().await;
    let vm = created(&e, "web", &[]).await;
    start(&e, vm.id).await;
    guest_writes(&vm, "one\r\ntwo\r\nthree\r\n");

    assert_eq!(ps(&e, "?all=1").await, ["/web Up"]);
    for (query, want) in [
        ("stderr=1&stdout=1&tail=all", "one\ntwo\nthree\n"),
        ("stderr=1&stdout=1&tail=1", "three\n"),
        ("stderr=1&stdout=1&tail=0", ""),
        ("stderr=1&stdout=1&timestamps=1", "one\ntwo\nthree\n"),
    ] {
        let line = format!("GET /v1.44/containers/web/logs?{query}");
        let reply = request(&e, &line, false, "").await;
        assert_eq!(reply.status, 200, "{query}");
        assert_eq!(text(&reply.body().await), want, "{query}");
    }
    let line = "GET /v1.44/containers/web/stats?stream=0&one-shot=1";
    let stats = request(&e, line, false, "").await;
    assert_ne!(stats.status, 400, "stream=0 parses");

    // `logs -f` keeps going until the process exits.
    let line = "GET /v1.44/containers/web/logs?follow=1&stderr=1&stdout=1&tail=1";
    let follow = request(&e, line, false, "").await;
    assert_eq!(follow.status, 200);
    guest_writes(&vm, "four\r\n");
    tokio::time::sleep(Duration::from_millis(250)).await;
    guest_writes(&vm, "FLUXVM-EXIT 0\r\n");
    assert_eq!(text(&follow.body().await), "three\nfour\n");

    // Exited while its VM record still says running: not listed by `ps`, and `stop` has nothing to stop.
    assert!(ps(&e, "").await.is_empty());
    assert_eq!(ps(&e, "?all=1").await, ["/web Exited (0)"]);
    let stop = request(&e, "POST /v1.44/containers/web/stop", false, "").await;
    assert_eq!(stop.status, 304);
    let rm = request(&e, "DELETE /v1.44/containers/web?force=1", false, "").await;
    assert_eq!(rm.status, 204);
    assert!(ps(&e, "?all=1").await.is_empty());
}

#[tokio::test]
async fn rm_needs_force_while_running_and_stop_reports_137() {
    let e = engine().await;
    let vm = created(&e, "live", &[]).await;
    let stop = request(&e, "POST /v1.44/containers/live/stop", false, "").await;
    assert_eq!(stop.status, 304, "a created container is not running");
    start(&e, vm.id).await;
    let line = "DELETE /v1.44/containers/live?force=0";
    assert_eq!(request(&e, line, false, "").await.status, 409);
    let line = "POST /v1.44/containers/live/stop?t=1";
    assert_eq!(request(&e, line, false, "").await.status, 204);
    assert_eq!(inspected_state(&e, "live").await["ExitCode"], 137);

    let other = created(&e, "gone", &[]).await;
    start(&e, other.id).await;
    let line = "POST /v1.44/containers/gone/wait?condition=removed";
    let wait = request(&e, line, false, "").await;
    let line = "DELETE /v1.44/containers/gone?v=1&force=1";
    assert_eq!(request(&e, line, false, "").await.status, 204);
    assert_eq!(wait.json().await["StatusCode"], 137);
}

#[tokio::test]
async fn run_rm_removes_the_container_after_its_output_is_read() {
    let e = engine().await;
    let vm = created(&e, "once", &[AUTO_REMOVE_LABEL]).await;
    let line = "POST /v1.44/containers/once/attach?stderr=1&stdout=1&stream=1";
    let attach = request(&e, line, true, "").await;
    let line = "POST /v1.44/containers/once/wait?condition=removed";
    let wait = request(&e, line, false, "").await;
    start(&e, vm.id).await;
    reap_when_exited(e.st.clone(), vm.id);
    guest_writes(&vm, "bye\r\nFLUXVM-EXIT 5\r\n");
    assert_eq!(text(&attach.body().await), "bye\n");
    assert_eq!(wait.json().await["StatusCode"], 5);
    assert!(ps(&e, "?all=1").await.is_empty());
}

#[tokio::test]
async fn exec_takes_nulls_and_upgrades_the_connection() {
    let e = engine().await;
    let vm = created(&e, "box", &[]).await;
    // What `docker exec box id` sends.
    let create = r#"{"User":"","Privileged":false,"Tty":false,"AttachStdin":false,"AttachStderr":true,"AttachStdout":true,"DetachKeys":"","Env":null,"WorkingDir":"","Cmd":["id"]}"#;
    let line = "POST /v1.44/containers/box/exec";
    assert_eq!(request(&e, line, false, create).await.status, 409);
    start(&e, vm.id).await;
    let reply = request(&e, line, false, create).await;
    assert_eq!(reply.status, 201);
    let exec = reply.json().await["Id"].as_str().unwrap().to_string();
    let run = request(
        &e,
        &format!("POST /v1.44/exec/{exec}/start"),
        true,
        r#"{"Detach":false,"Tty":false,"ConsoleSize":null}"#,
    )
    .await;
    assert_eq!(run.status, 101);
    assert_eq!(run.headers["upgrade"], "tcp");
    // There is no guest here, so the failure arrives on the stream and in the exit code.
    let out = demux(&run.body().await);
    assert_eq!(out[0].0, 2, "{out:?}");
    assert!(out[0].1.contains("guest agent"), "{out:?}");
    let line = format!("GET /v1.44/exec/{exec}/json");
    let seen = request(&e, &line, false, "").await.json().await;
    assert_eq!(
        (seen["Running"].as_bool(), seen["ExitCode"].as_i64()),
        (Some(false), Some(126))
    );
    let line = format!("POST /v1.44/exec/{exec}/resize?h=24&w=80");
    assert_eq!(request(&e, &line, false, "").await.status, 200);
}

#[tokio::test]
async fn create_refuses_a_name_in_use() {
    let e = engine().await;
    created(&e, "taken", &[]).await;
    let line = "POST /v1.44/containers/create?name=taken";
    let body = r#"{"Image":"alpine","Cmd":null,"Env":null,"HostConfig":null}"#;
    let reply = request(&e, line, false, body).await;
    assert_eq!(reply.status, 409);
    let message = reply.json().await["message"].as_str().unwrap().to_string();
    assert!(message.contains("already in use"), "{message}");
}

#[test]
fn flags_are_read_as_docker_reads_them() {
    for v in ["1", "true", "True", "yes", "2"] {
        assert!(docker_flag(Some(v)), "{v}");
    }
    for v in ["", "0", "false", "False", "no", "none"] {
        assert!(!docker_flag(Some(v)), "{v}");
    }
    assert!(!docker_flag(None));
}

#[test]
fn console_lines_of_the_guest_system_are_dropped() {
    assert_eq!(container_output(b"hello"), Some((&b"hello"[..], true)));
    assert_eq!(
        container_output(b"[1] done"),
        Some((&b"[1] done"[..], true))
    );
    assert_eq!(
        container_output(b"hiFLUXVM-EXIT 0"),
        Some((&b"hi"[..], false))
    );
    for l in [
        &b"fluxvm-oci-init: powering off"[..],
        b"FLUXVM-RESTART 1 3",
        b"FLUXVM-EXIT 0",
        b"VELORA-IP 10.0.0.2",
        b"[    0.000000] Booting Linux",
        b"[12345.1] reboot: Power down",
    ] {
        assert_eq!(container_output(l), None, "{}", String::from_utf8_lossy(l));
    }
}

#[test]
fn tail_keeps_newlines() {
    let all = || b"a\nb\nc\n".to_vec();
    assert_eq!(last_lines(all(), 2), b"b\nc\n");
    assert_eq!(last_lines(all(), 9), all());
    assert_eq!(last_lines(all(), 0), b"");
    assert_eq!(last_lines(b"a\nb".to_vec(), 1), b"b");
    assert_eq!(parse_tail(Some("all")), MAX_LOG_LINES);
    assert_eq!(parse_tail(Some("7")), 7);
    assert_eq!(parse_tail(None), MAX_LOG_LINES);
}

#[tokio::test]
async fn the_console_survives_rotation_and_a_new_boot() {
    let e = engine().await;
    let vm = created(&e, "rot", &[]).await;
    let mut console = Console::new(&vm);
    guest_writes(&vm, "a\r\nb");
    assert_eq!(console.read_new().await, b"a\n");
    // Rotated: the rest of the old file, then the new one.
    guest_writes(&vm, "\r\nc\r\n");
    std::fs::rename(&vm.log_path, console.rotated()).unwrap();
    guest_writes(&vm, "d\r\n");
    assert_eq!(console.read_new().await, b"b\nc\nd\n");
    // A new boot replaces the file.
    std::fs::remove_file(console.rotated()).unwrap();
    let fresh = vm.log_path.with_extension("new");
    std::fs::write(&fresh, "again\r\n").unwrap();
    std::fs::rename(&fresh, &vm.log_path).unwrap();
    assert_eq!(console.read_new().await, b"again\n");
}

#[test]
fn exec_user_is_numeric_or_root() {
    assert_eq!(exec_user("root").unwrap(), ExecRunAs { uid: 0, gid: 0 });
    assert_eq!(
        exec_user("1000:50").unwrap(),
        ExecRunAs { uid: 1000, gid: 50 }
    );
    assert_eq!(exec_user("7").unwrap(), ExecRunAs { uid: 7, gid: 7 });
    assert!(exec_user("nobody").is_err());
}
