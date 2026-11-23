// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mcp serve` tools for the macOS workflow (`fluxctl mac`) and for Apple-native k3s (`fluxctl kube apple`).
//!
//! The conventions of `mcp.rs` hold: reads are always listed, writes only with `--allow-write`, one tool result is capped,
//! errors are text the model can read. Specific to these tools:
//!
//! * Nothing prints to stdout (stdout is the protocol). The CLI commands that stream a guest's output (`mac run`, `mac exec`)
//!   are not reused; `mac_exec` goes through the daemon's `POST /v1/vms/{id}/agent` like `vm_exec`, and `mac_run` only
//!   starts an instance (to run a command, call `mac_exec` next).
//! * No secrets: the k3s token, kubeconfig and host keys have no tool; `kube_apple_logs` withholds lines that look like they
//!   carry a credential; an instance's environment is never listed, only its labels that are not `fluxvm.mac.*`.
//! * `mac_build_dry_run` takes the Macfile text and reads no file, so it cannot be pointed at one.

use crate::kube_apple::host::RealHost;
use crate::kube_apple::lifecycle;
use crate::kube_apple::model::{self, State};
use crate::mac_cmd::INSTANCE_SELECTOR;
use crate::mac_config;
use crate::mac_events::MAX_SLOTS;
use crate::mac_inspect::{self as view, PROJECT_LABEL, SERVICE_LABEL};
use crate::mac_instance::{LaunchSpec, Restart, parse_label};
use crate::mac_store::{self, Entry, Store};
use crate::mcp::{CALL_TIMEOUT, Tool, object, pretty, str_arg, timed, tool};
use crate::remote::Remote;
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// The longest Macfile `mac_build_dry_run` accepts.
pub const MAX_MACFILE: usize = 256 * 1024;
const MAX_LOG_LINES: u64 = 500;
const SLOW: Duration = Duration::from_secs(120);

/// `mac_images`: one object per name.
pub fn images_json(rows: &[(String, Entry)], size_of: impl Fn(&Entry) -> u64) -> Value {
    Value::Array(
        rows.iter()
            .map(|(name, e)| {
                json!({
                    "reference": name,
                    "digest": (!e.digest.is_empty()).then(|| e.digest.clone()),
                    "size_bytes": size_of(e),
                    "source": e.source,
                })
            })
            .collect(),
    )
}

/// `mac_ps`: the instances with the slot count. `all` includes stopped and failed ones.
pub fn ps_json(
    items: &[Value],
    all: bool,
    status: Option<&str>,
    name: Option<&str>,
) -> Result<Value> {
    let mut filters = vec![];
    if let Some(s) = status {
        filters.push(view::parse_filter(&format!("status={s}"))?);
    }
    if let Some(n) = name {
        filters.push(view::parse_filter(&format!("name={n}"))?);
    }
    let all_rows: Vec<&Value> = items
        .iter()
        .filter(|v| v["labels"][view::INSTANCE_LABEL].as_str() == Some("instance"))
        .collect();
    let used = all_rows
        .iter()
        .filter(|v| matches!(view::status(v), "running" | "paused"))
        .count();
    let rows = view::select(&all_rows, all, &filters);
    Ok(json!({
        "instances": rows.iter().map(|v| view::summary(v)).collect::<Vec<_>>(),
        "slots": {"used": used, "max": MAX_SLOTS},
    }))
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{key} must be a list of strings"))
            })
            .collect(),
        Some(_) => bail!("{key} must be a list of strings"),
    }
}

/// `mac_run` arguments as a launch spec. Never a command, a volume or an environment: those stay with the CLI.
pub fn launch_spec(args: &Value) -> Result<LaunchSpec> {
    let image = str_arg(args, "image").context("image is required")?;
    let mut labels = vec![];
    if let Some(obj) = args.get("labels").filter(|v| !v.is_null()) {
        let obj = obj
            .as_object()
            .context("labels must be an object of strings")?;
        for (k, v) in obj {
            let v = v.as_str().context("labels must be an object of strings")?;
            labels.push(parse_label(&format!("{k}={v}"))?);
        }
    }
    let cpus = match args.get("cpus").filter(|v| !v.is_null()) {
        Some(v) => Some(
            u8::try_from(
                v.as_u64()
                    .context("cpus must be a small positive integer")?,
            )
            .context("cpus must be a small positive integer")?,
        ),
        None => None,
    };
    ensure!(cpus != Some(0), "cpus must be at least 1");
    Ok(LaunchSpec {
        name: str_arg(args, "name")
            .map(str::to_owned)
            .unwrap_or_else(crate::mac_cmd::random_name),
        image: image.to_owned(),
        publish: string_list(args, "publish")?,
        mounts: vec![],
        cpus,
        memory_mib: str_arg(args, "memory")
            .map(mac_config::parse_memory_mib)
            .transpose()?,
        labels,
        restart: str_arg(args, "restart").map(Restart::parse).transpose()?,
        project: None,
        healthcheck: None,
    })
}

/// The Macfile's plan for `tag`, as `mac build --dry-run` prints it.
pub fn build_dry_run(src: &str, tag: &str, args: &Value, platform: Option<&str>) -> Result<String> {
    if let Some(p) = platform {
        crate::mac_build::check_platform(p)
            .map_err(|m| crate::mac_errors::coded(m, "MAC_PLATFORM_UNSUPPORTED"))?;
    }
    ensure!(!src.trim().is_empty(), "macfile is empty");
    ensure!(
        src.len() <= MAX_MACFILE,
        "macfile is {} bytes; the limit is {MAX_MACFILE}",
        src.len()
    );
    let mut build_args = std::collections::BTreeMap::new();
    if let Some(o) = args.as_object() {
        for (k, v) in o {
            build_args.insert(
                k.clone(),
                v.as_str()
                    .context("build_args must be an object of strings")?
                    .to_owned(),
            );
        }
    }
    let key = mac_store::normalize(tag)?;
    let file = crate::mac_build::parse(src)?;
    let steps = crate::mac_build::plan_with(&file, &build_args).0;
    let mut out = format!("build {key}\n{}", crate::mac_build::render_dry_run(&steps));
    for w in crate::mac_build::check_build_args(&file, &build_args) {
        out.push_str(&format!("warning: {w}\n"));
    }
    Ok(out)
}

/// The selector for a compose project's instances. The project name is checked because it goes into a label selector.
pub fn compose_selector(project: &str) -> Result<String> {
    ensure!(
        !project.is_empty()
            && project.len() <= 63
            && project
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'),
        "project {project:?} is not a valid name (lowercase letters, digits, - and _)"
    );
    Ok(format!("{INSTANCE_SELECTOR},{PROJECT_LABEL}={project}"))
}

/// `mac_compose_ps`: a project's instances in service order of the daemon's list.
pub fn compose_ps_json(items: &[Value], all: bool) -> Value {
    let rows: Vec<&Value> = items.iter().collect();
    let rows = view::select(&rows, all, &[]);
    Value::Array(
        rows.iter()
            .map(|v| {
                let mut s = view::summary(v);
                s["service"] = v["labels"][SERVICE_LABEL].clone();
                s
            })
            .collect(),
    )
}

/// The `mac_exec` request for the daemon.
pub fn exec_body(command: &str, timeout: u64) -> Value {
    json!({"command": command, "timeout_seconds": timeout})
}

/// What `kube_apple_list` returns: shape, never secrets.
pub fn kube_list_json(states: &[State]) -> Value {
    Value::Array(
        states
            .iter()
            .map(|s| {
                json!({
                    "cluster": s.name,
                    "phase": s.phase,
                    "datastore": s.datastore,
                    "api_port": s.api_port,
                    "k3s_version": s.k3s_version,
                    "external": s.external_url.is_some(),
                    "nodes": s.nodes.iter().map(|n| json!({
                        "name": n.name, "role": n.role, "vm": n.vm_name,
                        "joined": n.joined, "version": n.version,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// The cluster to act on: the one named, or the only one there is.
pub fn choose_cluster(arg: Option<&str>, existing: &[String]) -> Result<String> {
    match arg {
        Some(c) => {
            ensure!(
                model::name_ok(c),
                "cluster {c:?} is not a valid name (lowercase letters, digits and dashes)"
            );
            Ok(c.to_owned())
        }
        None => match existing {
            [one] => Ok(one.clone()),
            [] => bail!("no kube apple cluster on this Mac (see kube_apple_list)"),
            many => bail!("several clusters ({}); pass cluster", many.join(", ")),
        },
    }
}

const SECRETISH: [&str; 8] = [
    "token",
    "password",
    "passwd",
    "secret",
    "bearer ",
    "authorization",
    "-----begin",
    "private key",
];

/// Journal text with every line that may carry a credential replaced by a notice. Conservative on purpose: k3s logs a
/// server's startup arguments, and the agent token is a join secret.
pub fn scrub_log(text: &str) -> String {
    let mut withheld = 0;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            let low = l.to_ascii_lowercase();
            if SECRETISH.iter().any(|s| low.contains(s)) {
                withheld += 1;
                "[line withheld: may contain a credential]".to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if withheld > 0 {
        out.push(format!("({withheld} line(s) withheld)"));
    }
    out.join("\n")
}

fn local_clusters() -> Result<Vec<State>> {
    model::Store::new(model::default_root()?).list()
}

async fn pick(args: &Value) -> Result<String> {
    let names: Vec<String> = local_clusters()?.into_iter().map(|s| s.name).collect();
    choose_cluster(str_arg(args, "cluster"), &names)
}

fn cluster_prop() -> Value {
    json!({"type": "string", "description": "k3s cluster name; optional when there is only one"})
}

pub fn tools(remote: Arc<Remote>) -> Vec<Tool> {
    let name_prop = json!({"type": "string", "description": "instance name (see mac_ps)"});
    vec![
        tool(
            "mac_images",
            "List the macOS images (templates) in this Mac's local store: reference, digest, size on disk, source. Read from disk, not from the daemon.",
            object(json!({}), &[]),
            false,
            &remote,
            |_, _| async move {
                timed(CALL_TIMEOUT, async {
                    let store = Store::open(mac_store::default_root()?)?;
                    let rows = store.list()?;
                    pretty(&images_json(&rows, |e| {
                        crate::mac_cmd::dir_size(&store.root().join(&e.dir))
                    }))
                })
                .await
            },
        ),
        tool(
            "mac_ps",
            "List macOS instances (VMs started by `fluxctl mac run`) with status, image, address and ports, and how many of the 2 macOS slots are held (running or paused). Stopped and failed ones only with all.",
            object(
                json!({
                    "all": {"type": "boolean", "description": "include stopped and failed instances"},
                    "status": {"type": "string", "description": "creating, running, paused, stopped or failed"},
                    "name": {"type": "string", "description": "substring of the name"}
                }),
                &[],
            ),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let items = r.list_vms(Some(INSTANCE_SELECTOR)).await?;
                    pretty(&ps_json(
                        &items,
                        args["all"].as_bool().unwrap_or(false),
                        str_arg(&args, "status"),
                        str_arg(&args, "name"),
                    )?)
                })
                .await
            },
        ),
        tool(
            "mac_build_dry_run",
            "Parse a Macfile (FROM, USER, RUN, COPY, ...) and print the build plan without building anything or touching the daemon. Pass the Macfile text, not a path.",
            object(
                json!({
                    "macfile": {"type": "string", "description": "the Macfile's text"},
                    "tag": {"type": "string", "description": "name:tag the image would get"},
                    "build_args": {"type": "object", "description": "values for ARG lines, NAME to value", "additionalProperties": {"type": "string"}},
                    "platform": {"type": "string", "description": "only darwin/arm64 is valid for a macOS image"}
                }),
                &["macfile", "tag"],
            ),
            false,
            &remote,
            |_, args| async move {
                timed(CALL_TIMEOUT, async {
                    build_dry_run(
                        str_arg(&args, "macfile").unwrap_or_default(),
                        str_arg(&args, "tag").unwrap_or_default(),
                        &args["build_args"],
                        str_arg(&args, "platform"),
                    )
                })
                .await
            },
        ),
        tool(
            "mac_compose_ps",
            "List the instances of a `fluxctl mac compose` project, with each one's service name.",
            object(
                json!({
                    "project": {"type": "string", "description": "compose project name"},
                    "all": {"type": "boolean", "description": "include stopped and failed instances"}
                }),
                &["project"],
            ),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let sel = compose_selector(str_arg(&args, "project").unwrap_or_default())?;
                    let items = r.list_vms(Some(&sel)).await?;
                    pretty(&compose_ps_json(
                        &items,
                        args["all"].as_bool().unwrap_or(false),
                    ))
                })
                .await
            },
        ),
        tool(
            "mac_run",
            "Start a macOS instance from a local image (an APFS clone) and return its name and id. It does not wait for the guest or run a command: use mac_exec once mac_ps shows an address. A Mac runs at most 2 macOS VMs; a 409 means both slots are taken.",
            object(
                json!({
                    "image": {"type": "string", "description": "a local image, see mac_images"},
                    "name": {"type": "string", "description": "instance name (default mac-<random>)"},
                    "publish": {"type": "array", "items": {"type": "string"}, "description": "[ADDR:]HOST:GUEST port forwards"},
                    "cpus": {"type": "integer", "minimum": 1},
                    "memory": {"type": "string", "description": "8g, 512m or MiB"},
                    "restart": {"type": "string", "description": "no, always, unless-stopped or on-failure"},
                    "labels": {"type": "object", "additionalProperties": {"type": "string"}}
                }),
                &["image"],
            ),
            true,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let spec = launch_spec(&args)?;
                    let store = Store::open(mac_store::default_root()?)?;
                    let id = crate::mac_instance::launch(&r, &store, &spec).await?;
                    pretty(&json!({"name": spec.name, "id": id, "image": spec.image}))
                })
                .await
            },
        ),
        tool(
            "mac_exec",
            "Run a shell command in a running macOS instance (through the daemon's SSH transport) and return its output. The instance's own environment is not exposed.",
            object(
                json!({
                    "name": name_prop,
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600}
                }),
                &["name", "command"],
            ),
            true,
            &remote,
            |r, args| async move {
                let timeout = args["timeout_seconds"].as_u64().unwrap_or(60).clamp(1, 600);
                timed(Duration::from_secs(timeout + 30), async {
                    let name = str_arg(&args, "name").unwrap_or_default();
                    let vm = crate::mac_cmd::find_instance(&r, name).await?;
                    ensure!(
                        view::status(&vm) == "running",
                        "{name} is {} (not running)",
                        view::status(&vm)
                    );
                    let id = vm["id"].as_str().context("no id")?;
                    let body = exec_body(str_arg(&args, "command").unwrap_or_default(), timeout);
                    pretty(
                        &r.call(Method::POST, &format!("/v1/vms/{id}/agent"), Some(body))
                            .await?,
                    )
                })
                .await
            },
        ),
        tool(
            "kube_apple_list",
            "List the Apple-native k3s clusters on this Mac (`fluxctl kube apple`): phase, datastore, API port, version and each node's role. Read from the local journals; no token, kubeconfig or key is included.",
            object(json!({}), &[]),
            false,
            &remote,
            |_, _| async move {
                timed(CALL_TIMEOUT, async {
                    pretty(&kube_list_json(&local_clusters()?))
                })
                .await
            },
        ),
        tool(
            "kube_apple_status",
            "Health of an Apple-native k3s cluster: each node's VM state, ssh, service state, Kubernetes Ready and version, /readyz, and the problems found. Connects to the nodes over ssh, read-only.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let name = pick(&args).await?;
                    let store = model::Store::new(model::default_root()?);
                    let h = RealHost::new(&*r, &store, None)?;
                    pretty(&serde_json::to_value(
                        lifecycle::status(&h, &store, &name).await?,
                    )?)
                })
                .await
            },
        ),
        tool(
            "kube_apple_snapshot_ls",
            "List the etcd snapshots on an Apple-native k3s server (embedded-etcd clusters only) with how many there are and the age of the newest.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let name = pick(&args).await?;
                    let store = model::Store::new(model::default_root()?);
                    let h = RealHost::new(&*r, &store, None)?;
                    let ls = lifecycle::snapshot_ls(&h, &store, &name).await?;
                    let (n, age) = crate::mac_metrics::snapshot_summary(&ls, chrono::Utc::now());
                    Ok(format!(
                        "{ls}\n\n{n} snapshot(s); newest {}",
                        age.map_or("age unknown".to_owned(), |a| format!("{a}s old"))
                    ))
                })
                .await
            },
        ),
        tool(
            "kube_apple_logs",
            "The last lines of a k3s node's journal (default the server, 100 lines, at most 500). Lines that look like they carry a token, password or key are withheld.",
            object(
                json!({
                    "cluster": cluster_prop(),
                    "node": {"type": "string", "description": "node name (default the server)"},
                    "lines": {"type": "integer", "minimum": 1, "maximum": 500}
                }),
                &[],
            ),
            false,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let name = pick(&args).await?;
                    let node = str_arg(&args, "node");
                    if let Some(n) = node {
                        ensure!(model::name_ok(n), "node {n:?} is not a valid name");
                    }
                    let lines = args["lines"]
                        .as_u64()
                        .unwrap_or(100)
                        .clamp(1, MAX_LOG_LINES) as u32;
                    let store = model::Store::new(model::default_root()?);
                    let h = RealHost::new(&*r, &store, None)?;
                    Ok(scrub_log(
                        &lifecycle::logs(&h, &store, &name, node, lines).await?,
                    ))
                })
                .await
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Tool> {
        tools(Arc::new(Remote::new("127.0.0.1:1", None)))
    }

    fn vm(id: &str, name: &str, st: &str, extra: Value) -> Value {
        let mut v = json!({"id": id, "name": name, "status": st, "guest_ip": "192.168.64.5",
            "labels": {"fluxvm.mac": "instance", "fluxvm.mac.image": "xcode:15", "fluxvm.mac.env": "TOKEN=hunter2"}});
        for (k, x) in extra.as_object().into_iter().flatten() {
            v["labels"][k] = x.clone();
        }
        v
    }

    #[test]
    fn reads_are_listed_and_changes_need_write_access() {
        let t = all();
        let write: Vec<&str> = t.iter().filter(|t| t.write).map(|t| t.name).collect();
        let read: Vec<&str> = t.iter().filter(|t| !t.write).map(|t| t.name).collect();
        assert_eq!(write, ["mac_run", "mac_exec"]);
        assert_eq!(
            read,
            [
                "mac_images",
                "mac_ps",
                "mac_build_dry_run",
                "mac_compose_ps",
                "kube_apple_list",
                "kube_apple_status",
                "kube_apple_snapshot_ls",
                "kube_apple_logs"
            ]
        );
        assert_eq!(t.len(), 10);
    }

    #[test]
    fn names_are_unique_and_schemas_are_objects_that_forbid_extra_keys() {
        let t = all();
        let mut names: Vec<&str> = t.iter().map(|t| t.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), t.len());
        for tool in &t {
            assert!(tool.description.len() > 30, "{}", tool.name);
            assert_eq!(tool.schema["type"], "object", "{}", tool.name);
            assert_eq!(tool.schema["additionalProperties"], false, "{}", tool.name);
        }
        let exec = t.iter().find(|t| t.name == "mac_exec").unwrap();
        assert_eq!(exec.schema["required"], json!(["name", "command"]));
    }

    #[tokio::test]
    async fn a_read_only_server_does_not_list_the_write_tools() {
        let mut s = crate::mcp::Server::new("t", "0", false);
        s.add(all());
        let (mut client, server_end) = tokio::io::duplex(1 << 20);
        s.serve(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.as_bytes(),
            server_end,
        )
        .await
        .unwrap();
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut client, &mut buf)
            .await
            .unwrap();
        assert!(
            buf.contains("mac_ps") && buf.contains("kube_apple_status"),
            "{buf}"
        );
        assert!(
            !buf.contains("mac_run") && !buf.contains("mac_exec"),
            "{buf}"
        );
    }

    #[tokio::test]
    async fn a_write_tool_cannot_be_called_on_a_read_only_server() {
        let mut s = crate::mcp::Server::new("t", "0", false);
        s.add(all());
        let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mac_exec","arguments":{"name":"a","command":"id"}}}"#;
        let (mut client, server_end) = tokio::io::duplex(1 << 20);
        s.serve(call.as_bytes(), server_end).await.unwrap();
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut client, &mut buf)
            .await
            .unwrap();
        assert!(buf.contains("error") || buf.contains("isError"), "{buf}");
        assert!(
            !buf.contains("not running"),
            "the tool must not have run: {buf}"
        );
    }

    #[test]
    fn ps_hides_stopped_instances_counts_slots_and_never_shows_the_environment() {
        let items = vec![
            vm("aaaaaaaaaaaa", "a", "running", json!({})),
            vm("bbbbbbbbbbbb", "b", "paused", json!({})),
            vm("cccccccccccc", "c", "stopped", json!({})),
            json!({"id": "dddd", "name": "not-ours", "status": "running", "labels": {}}),
        ];
        let v = ps_json(&items, false, None, None).unwrap();
        assert_eq!(v["instances"].as_array().unwrap().len(), 2);
        assert_eq!(v["slots"], json!({"used": 2, "max": 2}));
        assert_eq!(
            ps_json(&items, true, None, None).unwrap()["instances"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let stopped = ps_json(&items, false, Some("stopped"), None).unwrap();
        assert_eq!(stopped["instances"][0]["name"], "c");
        assert!(ps_json(&items, false, Some("bogus"), None).is_err());
        let text = v.to_string();
        assert!(
            !text.contains("hunter2") && !text.contains("TOKEN"),
            "{text}"
        );
    }

    #[test]
    fn run_arguments_become_a_launch_spec_without_commands_or_volumes() {
        let s = launch_spec(&json!({
            "image": "xcode:15", "name": "ci", "publish": ["8080:80"], "cpus": 4,
            "memory": "8g", "restart": "always", "labels": {"team": "ios"}
        }))
        .unwrap();
        assert_eq!(
            (s.name.as_str(), s.cpus, s.memory_mib),
            ("ci", Some(4), Some(8192))
        );
        assert_eq!(s.publish, ["8080:80"]);
        assert!(s.mounts.is_empty());
        assert_eq!(s.restart, Some(Restart::Always));
        assert_eq!(s.labels, [("team".to_owned(), "ios".to_owned())]);
        assert!(launch_spec(&json!({})).is_err());
        assert!(launch_spec(&json!({"image": "x", "cpus": 0})).is_err());
        assert!(launch_spec(&json!({"image": "x", "cpus": 999})).is_err());
        assert!(launch_spec(&json!({"image": "x", "restart": "sometimes"})).is_err());
        assert!(launch_spec(&json!({"image": "x", "publish": "8080:80"})).is_err());
        assert!(
            launch_spec(&json!({"image": "x"}))
                .unwrap()
                .name
                .starts_with("mac-")
        );
    }

    #[test]
    fn a_reserved_label_cannot_be_set_through_run() {
        // `labels_patch` is what the daemon call uses, and it refuses the fluxvm.mac prefix.
        let s =
            launch_spec(&json!({"image": "x", "labels": {"fluxvm.mac.image": "evil"}})).unwrap();
        assert!(crate::mac_instance::labels_patch(&s).is_err());
    }

    #[test]
    fn build_dry_run_prints_the_plan_and_reads_no_file() {
        let plan = build_dry_run(
            "FROM macos-base:14\nRUN echo hi\n",
            "out:1",
            &json!(null),
            None,
        )
        .unwrap();
        assert!(plan.starts_with("build out:1\n"), "{plan}");
        assert!(plan.contains("echo hi"), "{plan}");
        assert!(build_dry_run("", "out:1", &json!(null), None).is_err());
        assert!(build_dry_run("NOT A MACFILE", "out:1", &json!(null), None).is_err());
        assert!(build_dry_run("FROM a:1\n", "bad tag", &json!(null), None).is_err());
        assert!(build_dry_run("FROM a:1\n", "t:1", &json!({"X": 1}), None).is_err());
        let big = "# x\n".repeat(MAX_MACFILE);
        assert!(
            build_dry_run(&big, "t:1", &json!(null), None)
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        // COPY from outside the context is refused by the parser, as on the command line.
        assert!(build_dry_run("FROM a:1\nCOPY ../x /y\n", "t:1", &json!(null), None).is_err());
        let e = build_dry_run("FROM a:1\n", "t:1", &json!(null), Some("linux/amd64"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("darwin/arm64") && e.contains("MAC_PLATFORM_UNSUPPORTED"),
            "{e}"
        );
        let w = build_dry_run(
            "FROM a:1\n",
            "t:1",
            &json!({"X": "1"}),
            Some("darwin/arm64"),
        )
        .unwrap();
        assert!(w.contains("warning: --build-arg X is not used"), "{w}");
    }

    #[test]
    fn compose_project_names_are_checked_before_they_reach_a_selector() {
        assert_eq!(
            compose_selector("web-1").unwrap(),
            "fluxvm.mac=instance,fluxvm.mac.project=web-1"
        );
        for bad in ["", "A", "a,b", "a=b", "a b", &"x".repeat(64)] {
            assert!(compose_selector(bad).is_err(), "{bad:?}");
        }
        let items = vec![
            vm(
                "aaaaaaaaaaaa",
                "web-db",
                "running",
                json!({"fluxvm.mac.service": "db"}),
            ),
            vm(
                "bbbbbbbbbbbb",
                "web-app",
                "stopped",
                json!({"fluxvm.mac.service": "app"}),
            ),
        ];
        let v = compose_ps_json(&items, false);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["service"], "db");
        assert_eq!(compose_ps_json(&items, true).as_array().unwrap().len(), 2);
    }

    #[test]
    fn images_are_listed_with_size_and_without_credentials() {
        let rows = vec![
            (
                "xcode:15".to_owned(),
                Entry {
                    dir: "ab".into(),
                    digest: "sha256:ab".into(),
                    source: "ghcr.io/acme/x:15".into(),
                    added_unix: 1,
                },
            ),
            (
                "local:1".to_owned(),
                Entry {
                    dir: "cd".into(),
                    digest: String::new(),
                    source: "local".into(),
                    added_unix: 2,
                },
            ),
        ];
        let v = images_json(&rows, |e| if e.dir == "ab" { 10 } else { 20 });
        assert_eq!(v[0]["size_bytes"], 10);
        assert_eq!(v[0]["digest"], "sha256:ab");
        assert!(v[1]["digest"].is_null());
    }

    #[test]
    fn exec_body_carries_the_command_and_timeout_only() {
        assert_eq!(
            exec_body("sw_vers", 30),
            json!({"command": "sw_vers", "timeout_seconds": 30})
        );
    }

    #[test]
    fn the_cluster_is_named_or_the_only_one() {
        let one = vec!["dev".to_owned()];
        assert_eq!(choose_cluster(None, &one).unwrap(), "dev");
        assert_eq!(choose_cluster(Some("prod"), &one).unwrap(), "prod");
        assert!(choose_cluster(Some("Prod; rm"), &one).is_err());
        assert!(choose_cluster(None, &[]).is_err());
        let two = vec!["a".to_owned(), "b".to_owned()];
        assert!(
            choose_cluster(None, &two)
                .unwrap_err()
                .to_string()
                .contains("a, b")
        );
    }

    #[test]
    fn log_lines_that_may_hold_a_credential_are_withheld() {
        let log = "Starting k3s\nK3S_TOKEN=K10abc::server:s3cr3t\nlisten on :6443\nAuthorization: Bearer xyz\n-----BEGIN PRIVATE KEY-----";
        let out = scrub_log(log);
        assert!(out.contains("Starting k3s") && out.contains("listen on :6443"));
        assert!(
            !out.contains("s3cr3t") && !out.contains("xyz") && !out.contains("BEGIN"),
            "{out}"
        );
        assert!(out.ends_with("(3 line(s) withheld)"), "{out}");
        assert_eq!(scrub_log("all fine"), "all fine");
    }

    #[test]
    fn the_cluster_listing_has_no_key_token_or_kubeconfig() {
        use crate::kube_apple::model::*;
        let st = State {
            schema: 1,
            name: "dev".into(),
            owner: "ownerhex".into(),
            phase: Phase::Ready,
            user: "admin".into(),
            api_port: 16443,
            datastore: Datastore::Etcd,
            external_url: None,
            tls_sans: vec![],
            k3s_sha256: Some("deadbeef".into()),
            k3s_version: Some("v1.31.4+k3s1".into()),
            params: Params {
                cpus: 2,
                memory_mib: 4096,
                disk_gib: 20,
                image: "img".into(),
            },
            nodes: vec![Node {
                name: "cp1".into(),
                role: Role::Server,
                vm_name: "k3s-cp1".into(),
                id: Some("11111111-1111-1111-1111-111111111111".into()),
                host_key: "ssh-ed25519 AAAAhostkey".into(),
                joined: true,
                version: Some("v1.31.4+k3s1".into()),
            }],
            canary: Some(Canary {
                nonce: "n0nce".into(),
                boot_id: "b".into(),
                written_at: "t".into(),
            }),
            servers_wanted: 1,
            flags: Default::default(),
            endpoint: None,
            snapshots: None,
            agent_secret_rotated: None,
            server_secret_rotated: None,
        };
        let v = kube_list_json(&[st]);
        assert_eq!(v[0]["phase"], "ready");
        assert_eq!(v[0]["datastore"], "etcd");
        assert_eq!(v[0]["nodes"][0]["role"], "server");
        let text = v.to_string();
        for leak in ["AAAAhostkey", "n0nce", "ownerhex", "deadbeef"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
    }
}
