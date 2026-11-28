// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The rest of the day-to-day Docker CLI for the built-in engine: restart, kill, pause, unpause, top, stats, rename, events,
//! the honest `network` and `volume` answers, and Docker-shaped "not supported" errors that say what to do next.

use super::*;
use chrono::{DateTime, Utc};
use fluxvm_core::model::{VmMetrics, VmPatch};
use fluxvm_scheduler::events::{self, EventFilter, EventLog, VmEvent};

/// The one sentence every "not supported here" answer ends with.
pub(super) const FULL_ENGINE: &str = "For a full Docker engine (build, Compose, user-defined networks, volumes, every signal) run `fluxctl docker up`, then `eval \"$(fluxctl docker env)\"`.";

/// The only network the built-in engine has: every container is a microVM behind user-mode NAT.
const NAT_NAME: &str = "fluxvm-nat";
const NAT_ID: &str = "5f1a0c0d5f1a0c0d5f1a0c0d5f1a0c0d5f1a0c0d5f1a0c0d5f1a0c0d5f1a0c0d";

fn unsupported(message: impl Into<String>) -> DockerError {
    DockerError::new(StatusCode::NOT_IMPLEMENTED, message)
}

fn conflict(message: impl Into<String>) -> DockerError {
    DockerError::new(StatusCode::CONFLICT, message)
}

/// Anything the router has no route for: a Docker `{"message": ...}` body the CLI prints as one sentence.
pub(super) async fn fallback(method: axum::http::Method, uri: Uri) -> DockerError {
    unsupported(format!(
        "The built-in FluxVM Docker engine does not support {method} {}. {FULL_ENGINE}",
        uri.path()
    ))
}

fn not_running(vm: &VmRecord) -> DockerError {
    conflict(format!(
        "Container {} is not running. Start it with `docker start {}`.",
        vm.name, vm.name
    ))
}

pub(super) async fn containers_restart(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(_q): Query<StopQuery>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    let _guard = st.containers.lock(vm.id).await;
    let restarted = async {
        let vm = st.manager.get(vm.id).await?;
        if matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
            st.manager.stop(vm.id).await?;
        }
        let vm = st.manager.get(vm.id).await?;
        lifecycle::start_locked(&st, &vm).await
    };
    restarted.await.map_err(|e| {
        DockerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Could not restart {}: {e:#}. Run `docker logs {}` to see why, then try `docker start {}`.",
                vm.name, vm.name, vm.name
            ),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(super) struct KillQuery {
    signal: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Sig {
    Kill,
    Term,
}

/// `KILL`/`9` (the default) end the VM at once, `TERM`/`15` shut the guest down cleanly. Other real signals have no way into a
/// microVM, so they are refused with the reason; anything that is not a signal is a bad request.
fn parse_signal(raw: Option<&str>) -> Result<Sig, DockerError> {
    let s = raw.unwrap_or("").trim().to_ascii_uppercase();
    let s = s.strip_prefix("SIG").unwrap_or(&s);
    match s {
        "" | "KILL" | "9" => Ok(Sig::Kill),
        "TERM" | "15" => Ok(Sig::Term),
        "HUP" | "INT" | "QUIT" | "USR1" | "USR2" | "STOP" | "CONT" | "ALRM" | "1" | "2" | "3"
        | "10" | "12" | "14" | "17" | "18" | "19" => Err(unsupported(format!(
            "The built-in engine can only deliver KILL (9) and TERM (15) to a container, not signal {s}. Use `docker kill` or `docker stop`. {FULL_ENGINE}"
        ))),
        other => Err(DockerError::bad(format!(
            "Invalid signal: {other}. Use a name such as KILL or TERM, or a number such as 9 or 15."
        ))),
    }
}

pub(super) async fn containers_kill(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<KillQuery>,
) -> DockerResult<StatusCode> {
    let sig = parse_signal(q.signal.as_deref())?;
    let vm = resolve_container(&st.manager, &id).await?;
    let _guard = st.containers.lock(vm.id).await;
    let vm = st.manager.get(vm.id).await?;
    if !lifecycle::is_up(process_state(&vm).await) {
        return Err(not_running(&vm));
    }
    match sig {
        Sig::Kill => st.manager.kill(vm.id).await,
        Sig::Term => st.manager.stop(vm.id).await,
    }?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn containers_pause(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    match vm.status {
        VmStatus::Running => {}
        VmStatus::Paused => {
            return Err(conflict(format!(
                "Container {} is already paused. Resume it with `docker unpause {}`.",
                vm.name, vm.name
            )));
        }
        _ => return Err(not_running(&vm)),
    }
    st.manager.pause(vm.id).await.map_err(|e| {
        unsupported(format!(
            "This host cannot pause {}: {e:#}. {FULL_ENGINE}",
            vm.name
        ))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn containers_unpause(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    if vm.status != VmStatus::Paused {
        return Err(conflict(format!(
            "Container {} is not paused. Pause it with `docker pause {}`.",
            vm.name, vm.name
        )));
    }
    st.manager.resume(vm.id).await.map_err(|e| {
        unsupported(format!(
            "This host cannot resume {}: {e:#}. {FULL_ENGINE}",
            vm.name
        ))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// `ps` output as Docker's `{"Titles", "Processes"}`: the first line names the columns, the last column keeps the rest of the line.
fn parse_ps(out: &str) -> Option<Value> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let titles: Vec<&str> = lines.next()?.split_whitespace().collect();
    if titles.is_empty() {
        return None;
    }
    let rows: Vec<Vec<String>> = lines
        .map(|l| {
            let mut rest = l.trim_start();
            let mut cols = Vec::new();
            for _ in 1..titles.len() {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                cols.push(rest[..end].to_string());
                rest = rest[end..].trim_start();
            }
            cols.push(rest.trim_end().to_string());
            cols
        })
        .collect();
    Some(json!({ "Titles": titles, "Processes": rows }))
}

#[derive(Deserialize, Default)]
pub(super) struct TopQuery {
    #[allow(dead_code)]
    ps_args: Option<String>,
}

pub(super) async fn containers_top(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(_q): Query<TopQuery>,
) -> DockerResult<Json<Value>> {
    let vm = resolve_container(&st.manager, &id).await?;
    if vm.status != VmStatus::Running {
        return Err(not_running(&vm));
    }
    let process = ExecProcess {
        argv: vec!["ps".into()],
        env: vec![],
        clean_env: false,
        cwd: None,
        user: None,
    };
    let resp = st
        .manager
        .exec_process(vm.id, process, Some(20))
        .await
        .map_err(|e| {
            unsupported(format!(
                "Could not list the processes of {}: {e:#}. Try `docker exec {} ps` instead.",
                vm.name, vm.name
            ))
        })?;
    let no_ps = || {
        unsupported(format!(
            "The image of {} has no `ps` command, so the built-in engine cannot list its processes. {FULL_ENGINE}",
            vm.name
        ))
    };
    match resp {
        AgentResponse::Exec {
            exit_code: 0,
            stdout,
            ..
        } => parse_ps(&stdout).map(Json).ok_or_else(no_ps),
        _ => Err(no_ps()),
    }
}

#[derive(Deserialize)]
pub(super) struct StatsQuery {
    /// Absent means streaming; the CLI sends `0` or `1`.
    stream: Option<String>,
}

/// One Docker stats sample from the VM's lifetime-average CPU and its memory and disk counters. The CPU figure is a
/// lifetime average (the host gives no instantaneous rate), expressed so the CLI's own formula prints it.
fn stats_json(vm: &VmRecord, m: &VmMetrics, now: DateTime<Utc>) -> Value {
    let cpus = u64::from(vm.request.vcpus.max(1));
    let system = 1_000_000_000u64;
    let total = (m.cpu_usage_percent.max(0.0) / 100.0 * system as f64 / cpus as f64) as u64;
    let limit = vm.request.memory_mib * 1024 * 1024;
    json!({
        "read": now.to_rfc3339(),
        "preread": "0001-01-01T00:00:00Z",
        "id": docker_id(vm.id),
        "name": format!("/{}", vm.name),
        "pids_stats": {},
        "num_procs": 0,
        "cpu_stats": {
            "cpu_usage": { "total_usage": total },
            "system_cpu_usage": system,
            "online_cpus": cpus,
        },
        "precpu_stats": { "cpu_usage": { "total_usage": 0 }, "system_cpu_usage": 0, "online_cpus": cpus },
        "memory_stats": { "usage": m.memory_usage_bytes, "limit": limit },
        "blkio_stats": { "io_service_bytes_recursive": [
            { "major": 0, "minor": 0, "op": "read", "value": m.disk_read_bytes },
            { "major": 0, "minor": 0, "op": "write", "value": m.disk_write_bytes },
        ] },
        "networks": {},
    })
}

fn stats_unavailable(vm: &VmRecord, e: &anyhow::Error) -> DockerError {
    unsupported(format!(
        "This host cannot read resource use for {}: {e:#}. {FULL_ENGINE}",
        vm.name
    ))
}

pub(super) async fn containers_stats(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<StatsQuery>,
) -> DockerResult<Response> {
    let vm = resolve_container(&st.manager, &id).await?;
    if vm.status != VmStatus::Running {
        return Err(not_running(&vm));
    }
    let first = st
        .manager
        .metrics(vm.id)
        .await
        .map_err(|e| stats_unavailable(&vm, &e))?;
    let sample = stats_json(&vm, &first, Utc::now());
    if q.stream.is_some() && !docker_flag(q.stream.as_deref()) {
        return Ok(Json(sample).into_response());
    }
    let manager = st.manager.clone();
    let stream = async_stream::stream! {
        let mut next = Some(sample);
        while let Some(s) = next.take() {
            let mut line = serde_json::to_vec(&s).unwrap_or_default();
            line.push(b'\n');
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(line));
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Ok(vm) = manager.get(vm.id).await else { break };
            if vm.status != VmStatus::Running {
                break;
            }
            if let Ok(m) = manager.metrics(vm.id).await {
                next = Some(stats_json(&vm, &m, Utc::now()));
            }
        }
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap())
}

#[derive(Deserialize)]
pub(super) struct RenameQuery {
    name: Option<String>,
}

pub(super) async fn containers_rename(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<RenameQuery>,
) -> DockerResult<StatusCode> {
    let new = q.name.unwrap_or_default();
    let new = new.trim_start_matches('/').to_string();
    if new.is_empty() {
        return Err(DockerError::bad(
            "A new name is required: `docker rename OLD NEW`.",
        ));
    }
    let vm = resolve_container(&st.manager, &id).await?;
    if st
        .manager
        .list()
        .await
        .iter()
        .any(|v| v.name == new && v.id != vm.id)
    {
        return Err(conflict(format!(
            "The name \"/{new}\" is already in use. Pick another name or remove that container with `docker rm`."
        )));
    }
    st.manager
        .patch(
            vm.id,
            VmPatch {
                name: Some(new),
                ..Default::default()
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A FluxVM lifecycle event as the Docker events it stands for (`vm.stop` is `die` then `stop`).
fn docker_events(ev: &VmEvent, name: &str, image: &str) -> Vec<Value> {
    let Some(id) = ev.vm_id else { return vec![] };
    let actions: &[&str] = match ev.event.as_str() {
        "vm.create" => &["create"],
        "vm.start" => &["start"],
        "vm.stop" => &["die", "stop"],
        "vm.pause" => &["pause"],
        "vm.resume" => &["unpause"],
        "vm.restart" => &["restart"],
        "vm.delete" => &["destroy"],
        _ => &[],
    };
    let nanos = ev.ts.timestamp_nanos_opt().unwrap_or(0);
    actions
        .iter()
        .map(|a| {
            json!({
                "status": a,
                "id": docker_id(id),
                "from": image,
                "Type": "container",
                "Action": a,
                "Actor": { "ID": docker_id(id), "Attributes": { "image": image, "name": name } },
                "scope": "local",
                "time": ev.ts.timestamp(),
                "timeNano": nanos,
            })
        })
        .collect()
}

/// `since`/`until` as Docker sends them: seconds since the epoch, possibly with a fraction.
fn parse_when(raw: &Option<String>, what: &str) -> DockerResult<Option<DateTime<Utc>>> {
    let Some(s) = raw.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let secs: f64 = s.parse().map_err(|_| {
        DockerError::bad(format!(
            "`{what}` must be a Unix time in seconds, for example 1700000000, not {s:?}."
        ))
    })?;
    Ok(DateTime::from_timestamp_millis((secs * 1000.0) as i64))
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    filters: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

#[derive(Default)]
struct DockerEventFilters(BTreeMap<String, Vec<String>>);

impl DockerEventFilters {
    fn parse(raw: Option<&str>) -> DockerResult<Self> {
        let filters = filters::Filters::parse(
            raw,
            "event",
            &["container", "event", "scope", "type"],
            &[
                "config", "daemon", "image", "label", "network", "node", "plugin", "secret",
                "service", "volume",
            ],
        )?;
        Ok(Self(filters.0))
    }

    fn matches(&self, event: &Value) -> bool {
        self.0.iter().all(|(key, values)| {
            values.iter().any(|v| match key.as_str() {
                "container" => {
                    event["Actor"]["Attributes"]["name"].as_str() == Some(v.as_str())
                        || (!v.is_empty()
                            && event["id"]
                                .as_str()
                                .is_some_and(|id| id.starts_with(v.as_str())))
                }
                "type" => event["Type"].as_str() == Some(v.as_str()),
                "scope" => event["scope"].as_str() == Some(v.as_str()),
                "event" => event["Action"].as_str() == Some(v.as_str()),
                _ => false,
            })
        })
    }
}

pub(super) async fn events_route(
    State(st): State<DockerState>,
    Query(q): Query<EventsQuery>,
) -> DockerResult<Response> {
    let filters = DockerEventFilters::parse(q.filters.as_deref())?;
    let since = parse_when(&q.since, "since")?;
    let until = parse_when(&q.until, "until")?;
    let log = events::global().ok_or_else(|| {
        DockerError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This daemon is not recording events, so there is nothing to stream. Restart the FluxVM daemon and try again.",
        )
    })?;
    Ok(events_response(
        st.manager.clone(),
        log,
        since,
        until,
        filters,
    ))
}

fn events_response(
    manager: Arc<VmManager>,
    log: &'static EventLog,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    filters: DockerEventFilters,
) -> Response {
    let stream = async_stream::stream! {
        let filter = EventFilter {
            since: since.map(|t| t - chrono::Duration::milliseconds(1)),
            ..Default::default()
        };
        let mut offset = if since.is_some() { 0 } else { log.end_offset() };
        let mut known: BTreeMap<Uuid, (String, String)> = BTreeMap::new();
        loop {
            for vm in oci_containers(&manager).await {
                let image = vm.labels.get(OCI_IMAGE_LABEL).cloned().unwrap_or_default();
                known.insert(vm.id, (vm.name.clone(), image));
            }
            if let Ok((evs, pos)) = log.read_from(offset, &filter) {
                offset = pos;
                for ev in evs {
                    let Some((name, image)) = ev.vm_id.and_then(|id| known.get(&id)) else { continue };
                    if until.is_some_and(|u| ev.ts > u) {
                        continue;
                    }
                    for d in docker_events(&ev, name, image) {
                        if !filters.matches(&d) { continue; }
                        let mut line = serde_json::to_vec(&d).unwrap_or_default();
                        line.push(b'\n');
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from(line));
                    }
                }
            }
            if until.is_some_and(|u| Utc::now() >= u) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn nat_network(m: &VmManager) -> Value {
    let mut containers = serde_json::Map::new();
    for vm in oci_containers(m).await {
        containers.insert(
            docker_id(vm.id),
            json!({ "Name": vm.name, "EndpointID": "", "MacAddress": "",
                    "IPv4Address": vm.guest_ip.clone().unwrap_or_default(), "IPv6Address": "" }),
        );
    }
    json!({
        "Name": NAT_NAME,
        "Id": NAT_ID,
        "Created": "0001-01-01T00:00:00Z",
        "Scope": "local",
        "Driver": "user-mode-nat",
        "EnableIPv6": false,
        "IPAM": { "Driver": "default", "Options": null, "Config": [] },
        "Internal": false,
        "Attachable": false,
        "Ingress": false,
        "ConfigFrom": { "Network": "" },
        "ConfigOnly": false,
        "Containers": containers,
        "Options": {},
        "Labels": { "fluxvm.note": "each container is a microVM behind user-mode NAT; publish ports with -p" },
    })
}

#[derive(Deserialize)]
pub(super) struct FilterQuery {
    filters: Option<String>,
}

pub(super) async fn networks_list(
    State(st): State<DockerState>,
    Query(q): Query<FilterQuery>,
) -> DockerResult<Json<Value>> {
    let all = vec![nat_network(&st.manager).await];
    Ok(Json(Value::Array(filters::filter_networks(
        all,
        q.filters.as_deref(),
    )?)))
}

pub(super) async fn networks_inspect(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<Json<Value>> {
    let id_hit = id.len() >= 4 && NAT_ID.starts_with(&id);
    if id == NAT_NAME || id_hit {
        return Ok(Json(nat_network(&st.manager).await));
    }
    Err(DockerError::not_found(format!(
        "network {id} not found. The built-in engine has one network, \"{NAT_NAME}\" (user-mode NAT). {FULL_ENGINE}"
    )))
}

pub(super) async fn networks_create() -> DockerError {
    unsupported(format!(
        "The built-in engine cannot create Docker networks: every container sits behind user-mode NAT; publish ports with `docker run -p`. {FULL_ENGINE}"
    ))
}

pub(super) async fn volumes_list(Query(q): Query<FilterQuery>) -> DockerResult<Json<Value>> {
    filters::check_volume_filters(q.filters.as_deref())?;
    Ok(Json(json!({ "Volumes": [], "Warnings": null })))
}

pub(super) async fn volume_missing(Path(name): Path<String>) -> DockerError {
    DockerError::not_found(format!(
        "get {name}: no such volume. The built-in engine has no named volumes: a container's disk lives and dies with it. {FULL_ENGINE}"
    ))
}

pub(super) async fn volumes_create() -> DockerError {
    unsupported(format!(
        "The built-in engine has no named volumes: a container's disk lives and dies with the container. {FULL_ENGINE}"
    ))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use axum::http::Request;
    use fluxvm_core::config::Config;
    use fluxvm_core::model::{BackendKind, CreateVmRequest, NetworkSpec};
    use tower::ServiceExt;

    pub(crate) fn fixture(name: &str, status: VmStatus, oci: bool) -> VmRecord {
        let id = Uuid::new_v4();
        VmRecord {
            id,
            name: name.to_string(),
            backend: BackendKind::Qemu,
            status,
            pid: None,
            created_at: Utc::now(),
            expires_at: None,
            workspace: PathBuf::from("/tmp/does-not-matter"),
            disk: PathBuf::from("/tmp/does-not-matter/root.qcow2"),
            seed_disk: None,
            tap_name: None,
            control_socket: None,
            log_path: PathBuf::from("/tmp/does-not-matter/console.log"),
            error: None,
            request: CreateVmRequest {
                apple: None,
                name: name.to_string(),
                tenant: None,
                created_by_token: None,
                backend: BackendKind::Qemu,
                image: PathBuf::from("/tmp/base.qcow2"),
                vcpus: 2,
                memory_mib: 512,
                max_vcpus: None,
                max_memory_mib: None,
                loadvm_tag: None,
                disk_size_gib: None,
                kernel: None,
                initrd: None,
                firmware: None,
                kernel_args: None,
                network: NetworkSpec::None,
                cloud_init: None,
                ttl_seconds: None,
                extra_args: vec![],
                shared_memory: false,
                root_disk_io: None,
                disk_hotplug_ports: None,
                agent: None,
                qga: None,
                hyperv: false,
                storage: Default::default(),
                shared_folders: vec![],
                data_disks: vec![],
                cdroms: vec![],
                numa_node: None,
                cpuset: None,
                hugepages: None,
                vfio_devices: vec![],
                numa: vec![],
                balloon: None,
                run_strategy: None,
                on_crash: None,
                power_schedule: None,
                pod_uid: None,
                secure_boot: None,
                tpm: None,
                net_mbit_limit: None,
                net_pps_limit: None,
                blk_mbit_limit: None,
                blk_ops_limit: None,
                egress: None,
                cpu_template: None,
                security_profile: Default::default(),
                measurement_policy: None,
            },
            guest_cid: None,
            jail_path: None,
            vsock_socket: None,
            qga_socket: None,
            cgroup_path: None,
            netns: None,
            lvm_lv: None,
            nbd_pid: None,
            virtiofsd_pids: vec![],
            swtpm_pid: None,
            dhcp_leasefile: None,
            guest_ip: None,
            requested_security_profile: Default::default(),
            achieved_security_profile: Default::default(),
            security_evidence: None,
            labels: if oci {
                BTreeMap::from([(OCI_IMAGE_LABEL.to_string(), "alpine:3.22".to_string())])
            } else {
                Default::default()
            },
            hibernation: None,
        }
    }

    /// A router over a manager with the given containers in its (temporary) store.
    async fn app(vms: &[(&str, VmStatus)]) -> (Router, Arc<VmManager>) {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let cfg = Config {
            state_dir: dir.path().join("state"),
            run_dir: dir.path().join("run"),
            ..Config::default()
        };
        let m = VmManager::new(cfg).unwrap();
        for (name, status) in vms {
            m.store.insert(fixture(name, *status, true)).await.unwrap();
        }
        (router(m.clone()), m)
    }

    async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn msg(v: &Value) -> &str {
        v["message"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn version_prefixes_reach_the_routes() {
        let (app, _) = app(&[]).await;
        for uri in ["/v1.43/networks", "/v1.24/networks", "/networks"] {
            let (s, v) = call(&app, "GET", uri).await;
            assert_eq!(s, StatusCode::OK, "{uri}");
            assert_eq!(v[0]["Name"], NAT_NAME);
        }
    }

    #[tokio::test]
    async fn unknown_routes_say_what_to_do_next() {
        let (app, _) = app(&[]).await;
        let (s, v) = call(&app, "POST", "/v1.43/swarm/init").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("POST /swarm/init"));
        assert!(msg(&v).contains("fluxctl docker up"));
        // A known path with the wrong method gets the same kind of body.
        let (s, v) = call(&app, "PUT", "/containers/json").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("fluxctl docker up"));
    }

    #[test]
    fn signals_map_to_what_is_real() {
        assert_eq!(parse_signal(None).unwrap(), Sig::Kill);
        assert_eq!(parse_signal(Some("SIGKILL")).unwrap(), Sig::Kill);
        assert_eq!(parse_signal(Some("9")).unwrap(), Sig::Kill);
        assert_eq!(parse_signal(Some("term")).unwrap(), Sig::Term);
        assert_eq!(parse_signal(Some("15")).unwrap(), Sig::Term);
        let e = parse_signal(Some("HUP")).unwrap_err();
        assert_eq!(e.status, StatusCode::NOT_IMPLEMENTED);
        assert!(e.message.contains("only deliver KILL"));
        assert_eq!(
            parse_signal(Some("banana")).unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn kill_checks_state_and_signal() {
        let (app, m) = app(&[("web", VmStatus::Stopped), ("live", VmStatus::Running)]).await;
        let (s, v) = call(&app, "POST", "/v1.43/containers/web/kill").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(msg(&v).contains("is not running"));
        let (s, _) = call(&app, "POST", "/containers/web/kill?signal=HUP").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        let (s, v) = call(&app, "POST", "/containers/nope/kill").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(msg(&v).contains("No such container"));
        // A running record without a process: kill succeeds and marks it stopped.
        let (s, _) = call(&app, "POST", "/v1.43/containers/live/kill?signal=9").await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let vm = m
            .list()
            .await
            .into_iter()
            .find(|v| v.name == "live")
            .unwrap();
        assert_eq!(vm.status, VmStatus::Stopped);
    }

    #[tokio::test]
    async fn pause_and_unpause_check_state() {
        let (app, _) = app(&[
            ("off", VmStatus::Stopped),
            ("held", VmStatus::Paused),
            ("live", VmStatus::Running),
        ])
        .await;
        let (s, v) = call(&app, "POST", "/v1.43/containers/off/pause").await;
        assert_eq!(
            (s, msg(&v).contains("not running")),
            (StatusCode::CONFLICT, true)
        );
        let (s, v) = call(&app, "POST", "/containers/held/pause").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(msg(&v).contains("docker unpause"));
        let (s, v) = call(&app, "POST", "/containers/live/unpause").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(msg(&v).contains("not paused"));
        // The backend cannot pause this record: a Docker error that names the way out, not a bare failure.
        let (s, v) = call(&app, "POST", "/v1.43/containers/live/pause").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("fluxctl docker up"));
        let (s, _) = call(&app, "POST", "/containers/held/unpause").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn restart_reports_a_failure_in_plain_words() {
        let (app, _) = app(&[("web", VmStatus::Stopped)]).await;
        let (s, v) = call(&app, "POST", "/v1.43/containers/web/restart?t=1").await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(msg(&v).starts_with("Could not restart web"));
        assert!(msg(&v).contains("docker logs web"));
        let (s, _) = call(&app, "POST", "/containers/nope/restart").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn top_and_stats_need_a_running_container() {
        let (app, _) = app(&[("off", VmStatus::Stopped), ("live", VmStatus::Running)]).await;
        for uri in [
            "/v1.43/containers/off/top",
            "/v1.43/containers/off/stats?stream=false",
        ] {
            let (s, v) = call(&app, "GET", uri).await;
            assert_eq!(s, StatusCode::CONFLICT, "{uri}");
            assert!(msg(&v).contains("docker start off"));
        }
        let (s, v) = call(&app, "GET", "/containers/live/top").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("docker exec live ps"));
        let (s, v) = call(&app, "GET", "/containers/live/stats?stream=false").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("fluxctl docker up"));
    }

    #[test]
    fn ps_output_becomes_titles_and_rows() {
        let out = "PID   USER     TIME  COMMAND\n    1 root      0:00 sleep 300\n   12 root      0:01 sh -c echo hi\n";
        let v = parse_ps(out).unwrap();
        assert_eq!(v["Titles"], json!(["PID", "USER", "TIME", "COMMAND"]));
        assert_eq!(v["Processes"][0], json!(["1", "root", "0:00", "sleep 300"]));
        assert_eq!(v["Processes"][1][3], "sh -c echo hi");
        assert!(parse_ps("").is_none());
    }

    #[test]
    fn stats_sample_has_the_fields_the_cli_reads() {
        let vm = fixture("web", VmStatus::Running, true);
        let m = VmMetrics {
            cpu_usage_percent: 50.0,
            memory_usage_bytes: 1024,
            disk_read_bytes: 7,
            disk_write_bytes: 9,
        };
        let v = stats_json(&vm, &m, Utc::now());
        assert_eq!(v["name"], "/web");
        assert_eq!(v["memory_stats"]["usage"], 1024);
        assert_eq!(v["memory_stats"]["limit"], 512u64 * 1024 * 1024);
        // The CLI's formula: delta / system delta * online cpus * 100 = 50.
        let total = v["cpu_stats"]["cpu_usage"]["total_usage"].as_f64().unwrap();
        let sys = v["cpu_stats"]["system_cpu_usage"].as_f64().unwrap();
        let cpus = v["cpu_stats"]["online_cpus"].as_f64().unwrap();
        assert!((total / sys * cpus * 100.0 - 50.0).abs() < 0.01);
        assert_eq!(
            v["blkio_stats"]["io_service_bytes_recursive"][1]["value"],
            9
        );
    }

    #[tokio::test]
    async fn rename_changes_the_name_and_refuses_clashes() {
        let (app, m) = app(&[("a", VmStatus::Stopped), ("b", VmStatus::Stopped)]).await;
        let (s, v) = call(&app, "POST", "/v1.43/containers/a/rename").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(msg(&v).contains("docker rename OLD NEW"));
        let (s, v) = call(&app, "POST", "/containers/a/rename?name=b").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(msg(&v).contains("already in use"));
        let (s, _) = call(&app, "POST", "/v1.43/containers/a/rename?name=%2Fc").await;
        assert_eq!(s, StatusCode::NO_CONTENT, "name %2Fc is /c");
        assert!(m.list().await.iter().any(|v| v.name == "c"));
        let (s, _) = call(&app, "GET", "/v1.43/containers/c/json").await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(&app, "POST", "/containers/zzz/rename?name=q").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_exact_name_wins_over_another_containers_id_prefix() {
        // The ids are fixed so this does not depend on luck: `b`'s id starts with `c`, which is also the name of the
        // container `c`. Docker resolves the name first, so `c` must not be reported as an ambiguous prefix.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let cfg = Config {
            state_dir: dir.path().join("state"),
            run_dir: dir.path().join("run"),
            ..Config::default()
        };
        let m = VmManager::new(cfg).unwrap();
        for (name, id) in [
            ("b", "c0000000-0000-4000-8000-000000000001"),
            ("c", "10000000-0000-4000-8000-000000000002"),
        ] {
            let mut r = fixture(name, VmStatus::Stopped, true);
            r.id = Uuid::parse_str(id).unwrap();
            m.store.insert(r).await.unwrap();
        }
        let app = router(m.clone());
        let (s, v) = call(&app, "GET", "/v1.43/containers/c/json").await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["Name"], "/c");
        // A real prefix still works when no name matches it.
        let (s, v) = call(&app, "GET", "/v1.43/containers/c0000/json").await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["Name"], "/b");
    }

    #[test]
    fn lifecycle_events_become_docker_events() {
        let id = Uuid::new_v4();
        let ev = |name: &str| VmEvent {
            ts: Utc::now(),
            event: name.into(),
            vm_id: Some(id),
            fields: Default::default(),
        };
        let stop = docker_events(&ev("vm.stop"), "web", "alpine");
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["Action"], "die");
        assert_eq!(stop[1]["status"], "stop");
        assert_eq!(stop[1]["Actor"]["Attributes"]["name"], "web");
        assert_eq!(stop[1]["Type"], "container");
        assert_eq!(
            docker_events(&ev("vm.resume"), "w", "i")[0]["Action"],
            "unpause"
        );
        assert!(docker_events(&ev("vm.backup.delete"), "w", "i").is_empty());
        let none = VmEvent {
            vm_id: None,
            ..ev("vm.start")
        };
        assert!(docker_events(&none, "w", "i").is_empty());
    }

    #[test]
    fn since_and_until_are_unix_seconds() {
        assert!(parse_when(&None, "since").unwrap().is_none());
        let t = parse_when(&Some("1700000000.5".into()), "since")
            .unwrap()
            .unwrap();
        assert_eq!(t.timestamp(), 1_700_000_000);
        let e = parse_when(&Some("yesterday".into()), "until").unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert!(e.message.contains("`until`"));
    }

    #[tokio::test]
    async fn events_stream_replays_history_then_ends_at_until() {
        use http_body_util::BodyExt;
        let (_, m) = app(&[("web", VmStatus::Running)]).await;
        let dir = tempfile::tempdir().unwrap();
        let log: &'static EventLog = Box::leak(Box::new(EventLog::new(dir.path())));
        let vm = m.list().await.remove(0);
        let ts = Utc::now();
        for name in ["vm.start", "vm.pause"] {
            log.append(&VmEvent {
                ts,
                event: name.into(),
                vm_id: Some(vm.id),
                fields: Default::default(),
            })
            .unwrap();
        }
        let since = Some(ts - chrono::Duration::seconds(5));
        let until = Some(Utc::now() + chrono::Duration::milliseconds(300));
        let filters = DockerEventFilters::parse(Some(
            r#"{"event":{"start":true,"die":true},"container":["web"],"type":["container"]}"#,
        ))
        .unwrap();
        let resp = events_response(m, log, since, until, filters);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let lines: Vec<Value> = String::from_utf8_lossy(&body)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let actions: Vec<&str> = lines
            .iter()
            .map(|l| l["Action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, ["start"]);
        assert_eq!(lines[0]["Actor"]["Attributes"]["image"], "alpine:3.22");
    }

    #[tokio::test]
    async fn docker_ps_latest_includes_stopped_containers() {
        let (app, _) = app(&[("first", VmStatus::Running), ("last", VmStatus::Stopped)]).await;
        for (query, names) in [
            ("", vec!["/first"]),
            ("?all=1", vec!["/last", "/first"]),
            ("?limit=1", vec!["/last"]),
        ] {
            let (status, json) = call(&app, "GET", &format!("/containers/json{query}")).await;
            assert_eq!(status, StatusCode::OK);
            let names_returned: Vec<_> = json
                .as_array()
                .unwrap()
                .iter()
                .map(|vm| vm["Names"][0].as_str().unwrap())
                .collect();
            assert_eq!(names_returned, names);
        }
    }

    #[tokio::test]
    async fn networks_tell_the_truth() {
        let (app, _) = app(&[("web", VmStatus::Running)]).await;
        let (s, v) = call(&app, "GET", "/v1.43/networks").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["Driver"], "user-mode-nat");
        assert_eq!(v[0]["Containers"].as_object().unwrap().len(), 1);
        for id in [NAT_NAME, &NAT_ID[..12]] {
            let (s, v) = call(&app, "GET", &format!("/v1.43/networks/{id}")).await;
            assert_eq!(s, StatusCode::OK, "{id}");
            assert_eq!(v["Name"], NAT_NAME);
        }
        let (s, v) = call(&app, "GET", "/networks/mynet").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(msg(&v).contains("fluxctl docker up"));
        let (s, v) = call(&app, "POST", "/v1.43/networks/create").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("user-mode NAT"));
    }

    #[tokio::test]
    async fn volumes_are_empty_and_cannot_be_created() {
        let (app, _) = app(&[]).await;
        let (s, v) = call(&app, "GET", "/v1.43/volumes").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["Volumes"], json!([]));
        let (s, v) = call(&app, "GET", "/volumes/data").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(msg(&v).contains("no such volume"));
        let (s, _) = call(&app, "DELETE", "/v1.43/volumes/data").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, v) = call(&app, "POST", "/v1.43/volumes/create").await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        assert!(msg(&v).contains("no named volumes"));
    }
}
