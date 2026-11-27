// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The life of a container as the docker CLI drives it: start, stop, wait, attach, logs, exec and remove.
//!
//! `docker run` is create, attach, wait and only then start, so attach and wait must answer before the container runs
//! and follow the run that comes. `docker run`, `attach` and `exec` also need the connection upgraded to a raw stream
//! (`101 Switching Protocols`).

use super::*;
use axum::extract::Request;
use bytes::Bytes;
use fluxvm_oci_init::config::{EXIT_MARKER, split_exit_marker};
use fluxvm_scheduler::oci_sandbox::{KILLED_EXIT_CODE, ProcessState, process_state};
use futures::stream::{BoxStream, StreamExt as _};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Labels that keep what `docker create` asked for with the container.
pub(super) const TTY_LABEL: &str = "fluxvm.docker.tty";
pub(super) const AUTO_REMOVE_LABEL: &str = "fluxvm.docker.auto-remove";

const POLL: Duration = Duration::from_millis(100);
/// How long `--rm` waits for the streams following a container to deliver its last output.
const DRAIN_LIMIT: Duration = Duration::from_secs(10);
/// Most lines one `docker logs` returns from history.
const MAX_LOG_LINES: usize = 10_000;
/// Exit code of a container whose start failed.
const START_FAILED_EXIT_CODE: i32 = 128;

pub(super) fn is_up(state: ProcessState) -> bool {
    matches!(state, ProcessState::Running | ProcessState::Paused)
}

/// What the engine remembers about containers between requests.
#[derive(Clone, Default)]
pub(super) struct Tracker(Arc<Mutex<BTreeMap<Uuid, Track>>>);

#[derive(Default)]
struct Track {
    /// Held by start, stop, kill, restart and remove, so they do not interleave on one container.
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Starts attempted through this engine: tells a waiter that the run it waits for has come, however short.
    starts: u64,
    /// Attach and log streams following the container.
    streams: usize,
    /// The exit code at removal, for a `wait` that outlives the container.
    exit_code: Option<i32>,
    reaping: bool,
}

impl Tracker {
    fn with<T>(&self, id: Uuid, f: impl FnOnce(&mut Track) -> T) -> T {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(map.entry(id).or_default())
    }

    pub(super) async fn lock(&self, id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.with(id, |t| t.lock.clone());
        lock.lock_owned().await
    }

    fn starts(&self, id: Uuid) -> u64 {
        self.with(id, |t| t.starts)
    }

    fn started(&self, id: Uuid) {
        self.with(id, |t| t.starts += 1);
    }

    fn follow(&self, id: Uuid) -> Following {
        self.with(id, |t| t.streams += 1);
        Following(self.clone(), id)
    }
}

struct Following(Tracker, Uuid);

impl Drop for Following {
    fn drop(&mut self) {
        self.0
            .with(self.1, |t| t.streams = t.streams.saturating_sub(1));
    }
}

async fn observe(m: &VmManager, id: Uuid) -> Option<(VmRecord, ProcessState)> {
    let vm = m.get(id).await.ok()?;
    let state = process_state(&vm).await;
    Some((vm, state))
}

/// One run of a container, followed from a request that may arrive before the start.
struct Run {
    began: bool,
    starts_before: u64,
}

impl Run {
    fn new(tracker: &Tracker, id: Uuid, state: ProcessState) -> Self {
        Self {
            began: is_up(state),
            starts_before: tracker.starts(id),
        }
    }

    /// `starts` must be read before `state`, so a state from before the start is never taken for the run's end.
    fn update(&mut self, starts: u64, state: ProcessState) {
        self.began |= starts != self.starts_before || is_up(state);
    }
}

// ---- start, stop, remove ----

/// Start a container; the caller holds its lock.
pub(super) async fn start_locked(st: &DockerState, vm: &VmRecord) -> anyhow::Result<()> {
    let auto_remove = vm.labels.contains_key(AUTO_REMOVE_LABEL);
    let result = st.manager.start_sandbox(vm.id).await;
    // Also after a failure: whoever waits for this run must learn that it is over.
    st.containers.started(vm.id);
    match result {
        Ok(_) => {
            if auto_remove {
                reap_when_exited(st.clone(), vm.id);
            }
            Ok(())
        }
        Err(e) => {
            // `docker run --rm` leaves nothing behind when the container cannot start.
            if auto_remove {
                st.containers
                    .with(vm.id, |t| t.exit_code = Some(START_FAILED_EXIT_CODE));
                if let Err(e) = remove_locked(st, vm.id).await {
                    tracing::warn!(container = %vm.id, error = %e, "removing a --rm container that did not start");
                }
            }
            Err(e)
        }
    }
}

pub(super) async fn containers_start(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    let _guard = st.containers.lock(vm.id).await;
    let vm = st.manager.get(vm.id).await?;
    if is_up(process_state(&vm).await) {
        return Ok(StatusCode::NOT_MODIFIED);
    }
    if vm.status == VmStatus::Creating {
        return Ok(StatusCode::NO_CONTENT);
    }
    start_locked(&st, &vm).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn containers_stop(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<StopQuery>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    let _guard = st.containers.lock(vm.id).await;
    let vm = st.manager.get(vm.id).await?;
    let state = process_state(&vm).await;
    if state == ProcessState::Created || matches!(vm.status, VmStatus::Stopped | VmStatus::Failed) {
        return Ok(StatusCode::NOT_MODIFIED);
    }
    if q.t == Some(0) {
        st.manager.kill(vm.id).await?;
    } else {
        st.manager.stop(vm.id).await?;
    }
    // A process that had already exited was only cleaned up.
    Ok(if is_up(state) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_MODIFIED
    })
}

/// Kill what still runs and delete the container; the caller holds its lock.
async fn remove_locked(st: &DockerState, id: Uuid) -> anyhow::Result<()> {
    let Some((vm, state)) = observe(&st.manager, id).await else {
        return Ok(());
    };
    // Before anything is killed: a protected container keeps running.
    fluxvm_core::protect::guard_delete(&vm.name, &vm.labels)?;
    st.containers.with(id, |t| match state {
        ProcessState::Exited(code) => t.exit_code = Some(code),
        s if is_up(s) => t.exit_code = Some(KILLED_EXIT_CODE),
        _ => {}
    });
    if matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
        st.manager.kill(id).await?;
    }
    st.manager.delete(id).await?;
    st.execs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, exec| exec.container != id);
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct DeleteQuery {
    #[serde(default, deserialize_with = "docker_bool")]
    force: bool,
    /// `docker rm -l`: remove a link between containers, not the container.
    #[serde(default, deserialize_with = "docker_bool")]
    link: bool,
}

pub(super) async fn containers_delete(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> DockerResult<StatusCode> {
    // Ignoring it would delete the container the link points from.
    if q.link {
        return Err(DockerError::new(
            StatusCode::NOT_IMPLEMENTED,
            "the built-in engine has no container links; nothing was removed",
        ));
    }
    let vm = resolve_container(&st.manager, &id).await?;
    let _guard = st.containers.lock(vm.id).await;
    // `--rm` may have removed it while this request waited for the lock.
    let Some((vm, state)) = observe(&st.manager, vm.id).await else {
        return Ok(StatusCode::NO_CONTENT);
    };
    if is_up(state) && !q.force {
        return Err(DockerError::new(
            StatusCode::CONFLICT,
            "cannot remove a running container; use force=true or stop first",
        ));
    }
    remove_locked(&st, vm.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `--rm`: delete the container once its run is over and the streams following it have its output.
pub(super) fn reap_when_exited(st: DockerState, id: Uuid) {
    if st
        .containers
        .with(id, |t| std::mem::replace(&mut t.reaping, true))
    {
        return;
    }
    tokio::spawn(async move {
        while let Some((_, state)) = observe(&st.manager, id).await {
            if is_up(state) || state == ProcessState::Created {
                tokio::time::sleep(POLL).await;
                continue;
            }
            let began = std::time::Instant::now();
            while st.containers.with(id, |t| t.streams) > 0 && began.elapsed() < DRAIN_LIMIT {
                tokio::time::sleep(POLL).await;
            }
            let _guard = st.containers.lock(id).await;
            // A `docker restart` that held the lock has it running again.
            if observe(&st.manager, id)
                .await
                .is_some_and(|(_, s)| is_up(s))
            {
                continue;
            }
            if let Err(e) = remove_locked(&st, id).await {
                tracing::warn!(container = %id, error = %e, "removing an exited --rm container");
                break;
            }
        }
        st.containers.with(id, |t| t.reaping = false);
    });
}

/// After a daemon restart, take up `--rm` for the containers that were started before it.
pub(super) async fn resume_auto_remove(st: DockerState) {
    for vm in oci_containers(&st.manager).await {
        if vm.labels.contains_key(AUTO_REMOVE_LABEL)
            && process_state(&vm).await != ProcessState::Created
        {
            reap_when_exited(st.clone(), vm.id);
        }
    }
}

// ---- wait ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Condition {
    NotRunning,
    NextExit,
    Removed,
}

#[derive(Deserialize)]
pub(super) struct WaitQuery {
    condition: Option<String>,
}

async fn wait_for(st: &DockerState, id: Uuid, condition: Condition, mut run: Run) -> i32 {
    let mut last = None;
    loop {
        let starts = st.containers.starts(id);
        let Some((_, state)) = observe(&st.manager, id).await else {
            return st
                .containers
                .with(id, |t| t.exit_code)
                .or(last)
                .unwrap_or(0);
        };
        if let ProcessState::Exited(code) = state {
            last = Some(code);
        }
        run.update(starts, state);
        let over = match condition {
            Condition::NotRunning => !is_up(state),
            Condition::NextExit => run.began && !is_up(state),
            Condition::Removed => false,
        };
        if over {
            return match state {
                ProcessState::Exited(code) => code,
                // Started, yet still never ran: the start failed.
                _ if condition == Condition::NextExit => START_FAILED_EXIT_CODE,
                _ => 0,
            };
        }
        tokio::time::sleep(POLL).await;
    }
}

pub(super) async fn containers_wait(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> DockerResult<Response> {
    let condition = match q.condition.as_deref().unwrap_or("") {
        "" | "not-running" => Condition::NotRunning,
        "next-exit" => Condition::NextExit,
        "removed" => Condition::Removed,
        other => {
            return Err(DockerError::bad(format!(
                "invalid condition: {other:?}; use not-running, next-exit or removed"
            )));
        }
    };
    let vm = resolve_container(&st.manager, &id).await?;
    // Taken before the response head goes out: the CLI starts the container as soon as it has the head.
    let run = Run::new(&st.containers, vm.id, process_state(&vm).await);
    let body = futures::stream::once(async move {
        let code = wait_for(&st, vm.id, condition, run).await;
        Ok::<_, std::io::Error>(Bytes::from(format!("{}\n", json!({ "StatusCode": code }))))
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(body))
        .unwrap())
}

// ---- console ----

/// The part of one console line (without its newline) of a container VM that the container wrote: `None` for a line
/// of init, the guest agent or the kernel. The flag says whether the newline belongs to the container too.
fn container_output(line: &[u8]) -> Option<(&[u8], bool)> {
    const SYSTEM: [&[u8]; 4] = [
        b"fluxvm-oci-init",
        b"fluxvm-guest-agent",
        b"FLUXVM-",
        b"VELORA-IP ",
    ];
    if SYSTEM.iter().any(|p| line.starts_with(p)) || is_kernel_line(line) {
        return None;
    }
    // Output without a final newline shares its line with init's exit marker.
    if split_exit_marker(&String::from_utf8_lossy(line)).is_some()
        && let Some(at) = line
            .windows(EXIT_MARKER.len())
            .rposition(|w| w == EXIT_MARKER.as_bytes())
    {
        return Some((&line[..at], false));
    }
    Some((line, true))
}

/// A `printk` line with its time stamp: `[    1.234567] reboot: Power down`.
fn is_kernel_line(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"[") else {
        return false;
    };
    let Some(end) = rest.iter().position(|b| *b == b']') else {
        return false;
    };
    let stamp = rest[..end].trim_ascii_start();
    let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    stamp
        .iter()
        .position(|b| *b == b'.')
        .is_some_and(|dot| digits(&stamp[..dot]) && digits(&stamp[dot + 1..]))
        && rest.get(end + 1) == Some(&b' ')
}

/// Reads a container's console as it grows, across the runner's rotation and the replacement a new boot makes.
struct Console {
    path: PathBuf,
    offset: u64,
    file: Option<u64>,
    /// A VM console, which also carries lines that are not the container's.
    filter: bool,
    tty: bool,
    partial: Vec<u8>,
}

fn file_id(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

impl Console {
    fn new(vm: &VmRecord) -> Self {
        Self {
            path: vm.log_path.clone(),
            offset: 0,
            file: None,
            // A native container writes its own console; a VM's is shared with the guest system.
            filter: vm
                .request
                .apple
                .as_ref()
                .is_some_and(|a| a.init_config.is_some()),
            tty: vm.labels.contains_key(TTY_LABEL),
            partial: Vec::new(),
        }
    }

    fn rotated(&self) -> PathBuf {
        let mut p = self.path.as_os_str().to_owned();
        p.push(".1");
        PathBuf::from(p)
    }

    /// Forget what has been written so far.
    async fn skip_to_end(&mut self) {
        if let Ok(meta) = tokio::fs::metadata(&self.path).await {
            self.offset = meta.len();
            self.file = Some(file_id(&meta));
        }
    }

    /// Everything the container has written, including the rotated file. An unfinished last line stays buffered.
    async fn history(&mut self) -> Vec<u8> {
        let older = tokio::fs::read(self.rotated()).await.unwrap_or_default();
        let mut out = self.clean(older);
        out.extend(self.read_new().await);
        out
    }

    async fn read_from(path: &FsPath, offset: u64) -> Option<(Vec<u8>, std::fs::Metadata)> {
        let mut f = tokio::fs::File::open(path).await.ok()?;
        let meta = f.metadata().await.ok()?;
        f.seek(std::io::SeekFrom::Start(offset)).await.ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).await.ok()?;
        Some((buf, meta))
    }

    /// What was written since the last read.
    async fn read_new(&mut self) -> Vec<u8> {
        let Ok(meta) = tokio::fs::metadata(&self.path).await else {
            return Vec::new();
        };
        let mut data = Vec::new();
        if self.file.is_some_and(|f| f != file_id(&meta)) {
            // Rotated: finish the old file first. Replaced by a new boot: start over.
            if let Some((rest, old)) = Self::read_from(&self.rotated(), self.offset).await
                && Some(file_id(&old)) == self.file
            {
                data = rest;
            }
            self.offset = 0;
        } else if meta.len() < self.offset {
            self.offset = 0;
        }
        if let Some((new, meta)) = Self::read_from(&self.path, self.offset).await {
            self.file = Some(file_id(&meta));
            self.offset += new.len() as u64;
            data.extend(new);
        }
        self.clean(data)
    }

    /// Drop the lines that are not the container's, keeping an unfinished one for later.
    fn clean(&mut self, data: Vec<u8>) -> Vec<u8> {
        if !self.filter {
            return data;
        }
        self.partial.extend(data);
        let Some(last) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let rest = self.partial.split_off(last + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        let mut out = Vec::new();
        for line in complete[..complete.len() - 1].split(|b| *b == b'\n') {
            // The guest's terminal ends lines with CR LF.
            let bare = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some((text, newline)) = container_output(bare) {
                out.extend_from_slice(if self.tty && newline { line } else { text });
                if newline {
                    out.push(b'\n');
                }
            }
        }
        out
    }

    /// The unfinished last line, when nothing more will come.
    fn finish(&mut self) -> Vec<u8> {
        let line = std::mem::take(&mut self.partial);
        match container_output(&line) {
            Some((text, _)) => text.to_vec(),
            None => Vec::new(),
        }
    }
}

/// The last `lines` lines of `data`, newlines kept.
fn last_lines(mut data: Vec<u8>, lines: usize) -> Vec<u8> {
    if lines == 0 {
        return Vec::new();
    }
    let body = data.len() - usize::from(data.ends_with(b"\n"));
    let cut = data[..body]
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'\n')
        .nth(lines - 1)
        .map(|(i, _)| i + 1);
    if let Some(cut) = cut {
        data.drain(..cut);
    }
    data
}

/// Console bytes as the CLI reads them: as they are for a terminal, in stdout frames otherwise.
fn framed(tty: bool, data: Vec<u8>) -> Bytes {
    if tty {
        Bytes::from(data)
    } else {
        Bytes::from(mux_stdout(&data))
    }
}

/// `history`, then what the console gains until the run is over. With `await_start` the stream stays open for a run
/// that has not begun.
fn follow_console(
    st: DockerState,
    id: Uuid,
    mut console: Console,
    history: Vec<u8>,
    mut run: Run,
    await_start: bool,
) -> BoxStream<'static, Bytes> {
    let following = st.containers.follow(id);
    let tty = console.tty;
    async_stream::stream! {
        let _following = following;
        if !history.is_empty() {
            yield framed(tty, history);
        }
        loop {
            let starts = st.containers.starts(id);
            // The state is read before the console, so the last read has everything written before the exit.
            let state = observe(&st.manager, id).await.map(|(_, s)| s);
            if let Some(state) = state {
                run.update(starts, state);
            }
            let new = console.read_new().await;
            if !new.is_empty() {
                yield framed(tty, new);
            }
            match state {
                None => break,
                Some(s) if !is_up(s) && (run.began || !await_start) => break,
                Some(_) => tokio::time::sleep(POLL).await,
            }
        }
        let rest = console.finish();
        if !rest.is_empty() {
            yield framed(tty, rest);
        }
    }
    .boxed()
}

// ---- raw streams ----

/// The pending connection upgrade, when the client asks for one (`Connection: Upgrade`, `Upgrade: tcp`).
fn upgrade_of(req: &mut Request) -> Option<OnUpgrade> {
    req.headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(req))
}

/// Send `out` to the client: over the upgraded connection if it asked for one, otherwise as a response body.
fn raw_stream(upgrade: Option<OnUpgrade>, tty: bool, out: BoxStream<'static, Bytes>) -> Response {
    let content_type = if tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };
    let Some(upgrade) = upgrade else {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from_stream(out.map(Ok::<_, std::io::Error>)))
            .unwrap();
    };
    tokio::spawn(async move {
        let mut out = out;
        let connection = match upgrade.await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "the connection was not upgraded");
                return;
            }
        };
        let (mut input, mut output) = tokio::io::split(TokioIo::new(connection));
        // Standard input is not delivered to the container, but must be read so the client is not blocked.
        let drain = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut input, &mut tokio::io::sink()).await;
        });
        while let Some(chunk) = out.next().await {
            if output.write_all(&chunk).await.is_err() || output.flush().await.is_err() {
                break;
            }
        }
        let _ = output.shutdown().await;
        drain.abort();
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "tcp")
        .body(Body::empty())
        .unwrap()
}

#[derive(Deserialize)]
pub(super) struct AttachQuery {
    #[serde(default, deserialize_with = "docker_bool")]
    stream: bool,
    #[serde(default, deserialize_with = "docker_bool")]
    logs: bool,
}

pub(super) async fn containers_attach(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
    mut req: Request,
) -> DockerResult<Response> {
    let upgrade = upgrade_of(&mut req);
    let vm = resolve_container(&st.manager, &id).await?;
    let run = Run::new(&st.containers, vm.id, process_state(&vm).await);
    let mut console = Console::new(&vm);
    let tty = console.tty;
    let history = if q.logs {
        console.history().await
    } else {
        console.skip_to_end().await;
        Vec::new()
    };
    let out = if q.stream {
        follow_console(st, vm.id, console, history, run, true)
    } else {
        let mut all = history;
        if q.logs {
            all.extend(console.finish());
        }
        let chunk = (!all.is_empty()).then(|| framed(tty, all));
        futures::stream::iter(chunk).boxed()
    };
    Ok(raw_stream(upgrade, tty, out))
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    #[serde(default, deserialize_with = "docker_bool")]
    follow: bool,
    tail: Option<String>,
}

fn parse_tail(tail: Option<&str>) -> usize {
    match tail {
        None => MAX_LOG_LINES,
        Some(t) if t.is_empty() || t.eq_ignore_ascii_case("all") => MAX_LOG_LINES,
        Some(t) => t.parse().unwrap_or(MAX_LOG_LINES).min(MAX_LOG_LINES),
    }
}

/// `docker logs`. The console is one combined stream, delivered as stdout.
pub(super) async fn containers_logs(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> DockerResult<Response> {
    let vm = resolve_container(&st.manager, &id).await?;
    let state = process_state(&vm).await;
    let run = Run::new(&st.containers, vm.id, state);
    let mut console = Console::new(&vm);
    let tty = console.tty;
    let mut history = console.history().await;
    let follow = q.follow && is_up(state);
    if !follow {
        history.extend(console.finish());
    }
    let history = last_lines(history, parse_tail(q.tail.as_deref()));
    let out = if follow {
        follow_console(st, vm.id, console, history, run, false)
    } else {
        let chunk = (!history.is_empty()).then(|| framed(tty, history));
        futures::stream::iter(chunk).boxed()
    };
    Ok(raw_stream(None, tty, out))
}

// ---- exec ----

/// Run an exec instance to its end and record how it went.
async fn finish_exec(
    st: &DockerState,
    id: &str,
    session: &ExecSession,
) -> DockerResult<(String, String, i32)> {
    let result = run_exec(&st.manager, session).await;
    let code = match &result {
        Ok((_, _, code)) => *code,
        Err(_) => 126,
    };
    if let Some(s) = st
        .execs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(id)
    {
        s.running = false;
        s.exit_code = Some(code);
    }
    result
}

fn exec_output(tty: bool, stdout: &str, stderr: &str) -> Vec<u8> {
    if tty {
        return [stdout.as_bytes(), stderr.as_bytes()].concat();
    }
    let mut bytes = mux_stdout(stdout.as_bytes());
    if !stderr.is_empty() {
        bytes.extend(mux_frame(2, stderr.as_bytes()));
    }
    bytes
}

pub(super) async fn exec_start(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    mut req: Request,
) -> DockerResult<Response> {
    let upgrade = upgrade_of(&mut req);
    let raw = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| DockerError::bad(format!("reading the request: {e}")))?;
    let body: ExecStartBody = if raw.is_empty() {
        ExecStartBody::default()
    } else {
        serde_json::from_slice(&raw)
            .map_err(|e| DockerError::bad(format!("invalid exec start request: {e}")))?
    };
    let session = st
        .execs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
        .ok_or_else(|| DockerError::not_found(format!("No such exec instance: {id}")))?;
    running_container(&st.manager, session.container).await?;
    if let Some(s) = st
        .execs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&id)
    {
        s.running = true;
        s.exit_code = None;
    }
    let tty = body.tty;
    // Its own task: the command runs to its end and is recorded even if the client goes away.
    let job = tokio::spawn(async move { finish_exec(&st, &id, &session).await });
    if body.detach {
        return Ok(StatusCode::OK.into_response());
    }
    let ended = |e: tokio::task::JoinError| {
        DockerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the exec task failed: {e}"),
        )
    };
    if upgrade.is_none() {
        let (stdout, stderr, _) = job.await.map_err(ended)??;
        let out = Bytes::from(exec_output(tty, &stdout, &stderr));
        return Ok(raw_stream(None, tty, futures::stream::iter([out]).boxed()));
    }
    // The connection is upgraded before the command ends, so a failure has to arrive on the stream.
    let out = futures::stream::once(async move {
        let result = job.await.map_err(ended).and_then(|r| r);
        Bytes::from(match result {
            Ok((stdout, stderr, _)) => exec_output(tty, &stdout, &stderr),
            Err(e) => exec_output(tty, "", &format!("{}\n", e.message)),
        })
    })
    .boxed();
    Ok(raw_stream(upgrade, tty, out))
}

/// The container, if `docker exec` can run in it now.
pub(super) async fn running_container(m: &VmManager, id: Uuid) -> DockerResult<VmRecord> {
    let (vm, state) = observe(m, id)
        .await
        .ok_or_else(|| DockerError::not_found(format!("No such container: {}", docker_id(id))))?;
    let why = match state {
        ProcessState::Running => return Ok(vm),
        ProcessState::Paused => "is paused",
        _ => "is not running",
    };
    Err(DockerError::new(
        StatusCode::CONFLICT,
        format!("Container {} {why}", vm.name),
    ))
}

#[cfg(test)]
mod tests;
