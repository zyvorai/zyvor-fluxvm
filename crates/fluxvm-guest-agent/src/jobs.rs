// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The guest job supervisor.
//!
//! A job is a command the agent keeps running between requests. Each job leads its own process
//! group, writes stdout and stderr to separate spool files under [`jobs_dir`], and keeps draining
//! its pipes after the retention cap so the process never sees SIGPIPE and its real exit status is
//! preserved. The agent answers one JSON line per connection, so callers poll with `JobRead`, a
//! long poll bounded by `wait_ms`.
//!
//! The job table is in agent memory: it survives a memory snapshot but not an agent restart. The
//! spool files are on disk and are covered by disk checkpoints.

use crate::exec_policy;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use fluxvm_guest_protocol::{
    AgentResponse, DEFAULT_JOB_OUTPUT_BYTES, ExecEnforcement, ExecPolicy, ExecProcess, JobPhase,
    JobStatus, JobStream, MAX_JOB_READ_BYTES, MAX_JOB_WAIT_MS, valid_job_id,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where spools live. `FLUXVM_JOBS_DIR` overrides it (tests, read-only roots).
pub fn jobs_dir() -> PathBuf {
    std::env::var_os("FLUXVM_JOBS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/fluxvm-jobs"))
}

/// How long a stopped job's pipes may stay open (a background child holding them) before the
/// spools are closed anyway.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Time between SIGTERM and SIGKILL when a job times out.
const KILL_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_READ_BYTES: u32 = 256 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn err(message: impl Into<String>) -> AgentResponse {
    AgentResponse::Error {
        message: message.into(),
    }
}

#[derive(Default)]
struct SpoolState {
    kept: u64,
    dropped: u64,
    closed: bool,
}

/// One output stream: bytes up to `cap` are appended to a file, the rest are counted and dropped.
struct Spool {
    path: PathBuf,
    cap: u64,
    state: Mutex<SpoolState>,
    changed: Condvar,
}

impl Spool {
    fn new(path: PathBuf, cap: u64) -> std::io::Result<Arc<Spool>> {
        File::create(&path)?;
        Ok(Arc::new(Spool {
            path,
            cap,
            state: Mutex::new(SpoolState::default()),
            changed: Condvar::new(),
        }))
    }

    /// Copies `r` into the spool until EOF. Keeps reading past the cap so the writer never blocks.
    fn pump<R: Read>(&self, mut r: R) {
        let mut file = match std::fs::OpenOptions::new().append(true).open(&self.path) {
            Ok(f) => Some(f),
            Err(_) => None,
        };
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let room = {
                let st = lock(&self.state);
                if st.closed {
                    // Closed by the supervisor: keep draining, keep nothing.
                    0
                } else {
                    self.cap.saturating_sub(st.kept) as usize
                }
            };
            let take = n.min(room);
            let mut wrote = 0;
            if take > 0 {
                if let Some(f) = file.as_mut() {
                    if f.write_all(&buf[..take]).is_ok() {
                        wrote = take;
                    } else {
                        file = None;
                    }
                }
            }
            let mut st = lock(&self.state);
            st.kept += wrote as u64;
            st.dropped += (n - wrote) as u64;
            self.changed.notify_all();
        }
        self.close();
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.changed.notify_all();
    }

    fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    fn counters(&self) -> (u64, u64) {
        let st = lock(&self.state);
        (st.kept, st.dropped)
    }

    /// Bytes from `offset`, waiting up to `wait` for any to exist or the stream to end.
    fn read(&self, offset: u64, max: usize, wait: Duration) -> std::io::Result<(Vec<u8>, bool)> {
        let deadline = Instant::now() + wait;
        let (kept, closed) = {
            let mut st = lock(&self.state);
            loop {
                if st.kept > offset || st.closed {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = self
                    .changed
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
            }
            (st.kept, st.closed)
        };
        if offset >= kept {
            return Ok((Vec::new(), closed));
        }
        let len = ((kept - offset) as usize).min(max);
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; len];
        f.read_exact(&mut data)?;
        let eof = closed && offset + len as u64 >= kept;
        Ok((data, eof))
    }
}

struct Meta {
    phase: JobPhase,
    exit_code: Option<i32>,
    signal: Option<i32>,
    ended_ms: Option<u64>,
}

struct Job {
    id: String,
    pid: u32,
    started_ms: u64,
    dir: PathBuf,
    stdin: Mutex<Option<ChildStdin>>,
    out: Arc<Spool>,
    err: Arc<Spool>,
    meta: Mutex<Meta>,
    /// The signal a caller asked for, 0 if none; a death on it reads as `Canceled`.
    requested_signal: AtomicI32,
    enforcement: Option<ExecEnforcement>,
}

impl Job {
    fn status(&self) -> JobStatus {
        let m = lock(&self.meta);
        let (stdout_bytes, stdout_dropped) = self.out.counters();
        let (stderr_bytes, stderr_dropped) = self.err.counters();
        JobStatus {
            job_id: self.id.clone(),
            pid: self.pid,
            phase: m.phase,
            exit_code: m.exit_code,
            signal: m.signal,
            stdout_bytes,
            stderr_bytes,
            stdout_dropped,
            stderr_dropped,
            started_unix_ms: self.started_ms,
            ended_unix_ms: m.ended_ms,
            enforcement: self.enforcement.clone(),
        }
    }

    fn phase(&self) -> JobPhase {
        lock(&self.meta).phase
    }
}

fn table() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    static T: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn find(id: &str) -> Result<Arc<Job>, AgentResponse> {
    lock(table())
        .get(id)
        .cloned()
        .ok_or_else(|| err(format!("no such job {id:?}")))
}

pub struct StartArgs {
    pub job_id: String,
    pub command: String,
    pub process: Option<ExecProcess>,
    pub policy: Option<ExecPolicy>,
    pub timeout_seconds: Option<u64>,
    pub stdin: bool,
    pub max_output_bytes: Option<u64>,
}

pub fn start(a: StartArgs) -> AgentResponse {
    if !valid_job_id(&a.job_id) {
        return err("job_id must be 1-64 characters of [A-Za-z0-9_-]");
    }
    if a.policy.is_some() && !cfg!(target_os = "linux") {
        return exec_policy::unsupported_policy_response();
    }
    if let Some(p) = &a.process {
        if let Err(message) = p.validate() {
            return err(message);
        }
    } else if a.command.trim().is_empty() {
        return err("give a command or a process");
    }
    {
        let t = lock(table());
        if t.contains_key(&a.job_id) {
            return err(format!("job {:?} already exists", a.job_id));
        }
    }
    let dir = jobs_dir().join(&a.job_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(format!("creating {}: {e}", dir.display()));
    }
    let cap = a.max_output_bytes.unwrap_or(DEFAULT_JOB_OUTPUT_BYTES);
    let (out, errs) = match (
        Spool::new(dir.join("stdout"), cap),
        Spool::new(dir.join("stderr"), cap),
    ) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(e), _) | (_, Err(e)) => {
            let _ = std::fs::remove_dir_all(&dir);
            return err(format!("creating spool files: {e}"));
        }
    };

    let (mut child, enforcement) = match spawn_child(&a) {
        Ok(c) => c,
        Err(resp) => {
            let _ = std::fs::remove_dir_all(&dir);
            return resp;
        }
    };

    let timeout = match (
        a.timeout_seconds,
        a.policy.as_ref().and_then(|p| p.timeout_secs),
    ) {
        (Some(t), Some(p)) => Some(t.min(p)),
        (t, p) => t.or(p),
    };
    // Always take the pipe: dropping it when stdin is not wanted gives the job an immediate EOF.
    let pipe = child.stdin.take();
    let stdin = if a.stdin { pipe } else { None };
    let job = Arc::new(Job {
        id: a.job_id.clone(),
        pid: child.id(),
        started_ms: now_ms(),
        dir,
        stdin: Mutex::new(stdin),
        out: out.clone(),
        err: errs.clone(),
        meta: Mutex::new(Meta {
            phase: JobPhase::Running,
            exit_code: None,
            signal: None,
            ended_ms: None,
        }),
        requested_signal: AtomicI32::new(0),
        enforcement,
    });
    lock(table()).insert(a.job_id.clone(), job.clone());

    if let Some(o) = child.stdout.take() {
        let s = out.clone();
        std::thread::spawn(move || s.pump(o));
    } else {
        out.close();
    }
    if let Some(e) = child.stderr.take() {
        let s = errs.clone();
        std::thread::spawn(move || s.pump(e));
    } else {
        errs.close();
    }
    let j = job.clone();
    std::thread::spawn(move || supervise(j, child, timeout));

    AgentResponse::JobStarted {
        status: job.status(),
    }
}

/// Starts the process with piped stdio, in its own process group, confined when a policy is given.
fn spawn_child(a: &StartArgs) -> Result<(Child, Option<ExecEnforcement>), AgentResponse> {
    if let Some(policy) = &a.policy {
        if !cfg!(target_os = "linux") {
            return Err(exec_policy::unsupported_policy_response());
        }
        let argv = match &a.process {
            Some(p) => p.argv.clone(),
            None => vec!["/bin/sh".into(), "-c".into(), a.command.clone()],
        };
        // The supervisor enforces the timeout and output cap; procbox is only the confinement.
        let pb = exec_policy::to_procbox(policy, Duration::from_secs(u32::MAX as u64));
        return match fluxvm_procbox::spawn(&pb, &argv, fluxvm_procbox::StdioMode::Piped) {
            Ok(s) => Ok((
                s.child,
                Some(exec_policy::enforcement_to_wire(&s.enforcement)),
            )),
            Err(e) => Err(err(format!(
                "policy could not be enforced, job not started: {e:#}"
            ))),
        };
    }
    let mut cmd = match &a.process {
        Some(p) => crate::process_command(p),
        None => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(&a.command);
            c
        }
    };
    cmd.stdin(if a.stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);
    cmd.spawn()
        .map(|c| (c, None))
        .map_err(|e| err(format!("spawning job: {e}")))
}

fn killpg(pid: u32, sig: i32) {
    // The job leads its own group, so the group id is its pid.
    unsafe { libc::kill(-(pid as i32), sig) };
}

/// Waits for the process, enforces the timeout, and records how it ended.
fn supervise(job: Arc<Job>, mut child: Child, timeout: Option<u64>) {
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let mut timed_out = false;
    let mut kill_at: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => break None,
        }
        let now = Instant::now();
        if let (Some(d), false) = (deadline, timed_out) {
            if now >= d {
                timed_out = true;
                killpg(job.pid, libc::SIGTERM);
                kill_at = Some(now + KILL_GRACE);
            }
        }
        if kill_at.is_some_and(|k| now >= k) {
            killpg(job.pid, libc::SIGKILL);
            kill_at = None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // The shell is gone; stop anything it left in the group still attached to the job.
    // Background children that closed their pipes are left running, like they are for exec.
    drop(lock(&job.stdin).take());
    let end = Instant::now() + DRAIN_GRACE;
    while Instant::now() < end && !(job.out.closed() && job.err.closed()) {
        std::thread::sleep(Duration::from_millis(10));
    }
    if timed_out || !(job.out.closed() && job.err.closed()) {
        killpg(job.pid, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(50));
    }
    job.out.close();
    job.err.close();

    let requested = job.requested_signal.load(Ordering::SeqCst);
    let mut m = lock(&job.meta);
    m.ended_ms = Some(now_ms());
    match status {
        Some(st) => {
            m.exit_code = st.code();
            m.signal = st.signal();
            m.phase = if timed_out {
                JobPhase::TimedOut
            } else if st.signal().is_some() && st.signal() == Some(requested) {
                JobPhase::Canceled
            } else if st.signal().is_some() {
                JobPhase::Signaled
            } else {
                JobPhase::Exited
            };
        }
        None => {
            m.phase = if timed_out {
                JobPhase::TimedOut
            } else {
                JobPhase::Signaled
            }
        }
    }
}

pub fn read(
    job_id: &str,
    stream: JobStream,
    offset: u64,
    max_bytes: Option<u32>,
    wait_ms: Option<u64>,
) -> AgentResponse {
    let job = match find(job_id) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let spool = match stream {
        JobStream::Stdout => &job.out,
        JobStream::Stderr => &job.err,
    };
    let max = max_bytes
        .unwrap_or(DEFAULT_READ_BYTES)
        .min(MAX_JOB_READ_BYTES) as usize;
    let wait = Duration::from_millis(wait_ms.unwrap_or(0).min(MAX_JOB_WAIT_MS));
    match spool.read(offset, max, wait) {
        Ok((data, eof)) => AgentResponse::JobOutput {
            next_offset: offset + data.len() as u64,
            data_base64: B64.encode(&data),
            eof,
            status: job.status(),
        },
        Err(e) => err(format!("reading job output: {e}")),
    }
}

pub fn write(job_id: &str, data_base64: &str, close: bool) -> AgentResponse {
    let job = match find(job_id) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let data = match B64.decode(data_base64) {
        Ok(d) => d,
        Err(e) => return err(format!("data is not valid base64: {e}")),
    };
    if data.len() > MAX_JOB_READ_BYTES as usize {
        return err(format!(
            "write is limited to {MAX_JOB_READ_BYTES} bytes per call"
        ));
    }
    let mut guard = lock(&job.stdin);
    if !data.is_empty() {
        let Some(w) = guard.as_mut() else {
            return err("the job's stdin is closed (start it with stdin enabled)");
        };
        if let Err(e) = w.write_all(&data).and_then(|_| w.flush()) {
            // A job that exited or closed stdin: drop the pipe so later writes say so.
            *guard = None;
            return err(format!("writing to the job's stdin: {e}"));
        }
    }
    if close {
        *guard = None;
    }
    AgentResponse::JobAck
}

pub fn signal(job_id: &str, sig: i32, tree: bool) -> AgentResponse {
    let job = match find(job_id) {
        Ok(j) => j,
        Err(e) => return e,
    };
    if !(1..=64).contains(&sig) {
        return err("signal must be between 1 and 64");
    }
    if job.phase().is_final() {
        return err("the job has already ended");
    }
    job.requested_signal.store(sig, Ordering::SeqCst);
    let rc = unsafe {
        if tree {
            libc::kill(-(job.pid as i32), sig)
        } else {
            libc::kill(job.pid as i32, sig)
        }
    };
    if rc != 0 {
        return err(format!(
            "signalling the job: {}",
            std::io::Error::last_os_error()
        ));
    }
    AgentResponse::JobAck
}

pub fn info(job_id: &str) -> AgentResponse {
    match find(job_id) {
        Ok(j) => AgentResponse::Job { status: j.status() },
        Err(e) => e,
    }
}

pub fn list() -> AgentResponse {
    let mut jobs: Vec<JobStatus> = lock(table()).values().map(|j| j.status()).collect();
    jobs.sort_by(|a, b| (a.started_unix_ms, &a.job_id).cmp(&(b.started_unix_ms, &b.job_id)));
    AgentResponse::Jobs { jobs }
}

pub fn reap(job_id: &str) -> AgentResponse {
    let job = match find(job_id) {
        Ok(j) => j,
        Err(e) => return e,
    };
    if !job.phase().is_final() {
        return err("the job is still running; signal it first");
    }
    lock(table()).remove(job_id);
    let _ = std::fs::remove_dir_all(&job.dir);
    AgentResponse::JobAck
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    fn setup() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        // One process-wide directory is enough: ids are unique per test.
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        let p = DIR.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("fluxvm-jobs-test-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            p
        });
        // SAFETY: set once to the same value before any job starts.
        unsafe { std::env::set_var("FLUXVM_JOBS_DIR", p) };
        d
    }

    fn id() -> String {
        format!("t{}", N.fetch_add(1, Ordering::SeqCst))
    }

    fn run(command: &str, stdin: bool, cap: Option<u64>, timeout: Option<u64>) -> String {
        let _d = setup();
        let job_id = id();
        let r = start(StartArgs {
            job_id: job_id.clone(),
            command: command.into(),
            process: None,
            policy: None,
            timeout_seconds: timeout,
            stdin,
            max_output_bytes: cap,
        });
        assert!(matches!(r, AgentResponse::JobStarted { .. }), "{r:?}");
        job_id
    }

    fn wait_final(job_id: &str) -> JobStatus {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let AgentResponse::Job { status } = info(job_id) {
                if status.phase.is_final() {
                    return status;
                }
            }
            assert!(Instant::now() < end, "job did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn read_all(job_id: &str, stream: JobStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0;
        loop {
            match read(job_id, stream, off, Some(64 * 1024), Some(2000)) {
                AgentResponse::JobOutput {
                    data_base64,
                    next_offset,
                    eof,
                    ..
                } => {
                    out.extend(B64.decode(data_base64).unwrap());
                    off = next_offset;
                    if eof {
                        return out;
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn separates_stdout_from_stderr_and_keeps_the_exit_code() {
        let j = run("echo out; echo err 1>&2; exit 7", false, None, None);
        let st = wait_final(&j);
        assert_eq!(st.phase, JobPhase::Exited);
        assert_eq!(st.exit_code, Some(7));
        assert_eq!(read_all(&j, JobStream::Stdout), b"out\n");
        assert_eq!(read_all(&j, JobStream::Stderr), b"err\n");
    }

    #[test]
    fn output_past_the_cap_is_dropped_and_the_exit_code_is_not_sigpipe() {
        // 3 MB of output against a 1 KiB cap; the producer must run to completion.
        let j = run(
            "i=0; while [ $i -lt 3000 ]; do head -c 1000 /dev/zero | tr '\\0' x; i=$((i+1)); done; exit 3",
            false,
            Some(1024),
            None,
        );
        let st = wait_final(&j);
        assert_eq!(st.phase, JobPhase::Exited, "{st:?}");
        assert_eq!(st.exit_code, Some(3));
        assert_eq!(st.stdout_bytes, 1024);
        assert_eq!(st.stdout_dropped, 3_000_000 - 1024);
        assert_eq!(read_all(&j, JobStream::Stdout).len(), 1024);
    }

    #[test]
    fn binary_output_round_trips() {
        let j = run("printf '\\000\\377\\200abc'", false, None, None);
        wait_final(&j);
        assert_eq!(
            read_all(&j, JobStream::Stdout),
            vec![0u8, 0xff, 0x80, b'a', b'b', b'c']
        );
    }

    #[test]
    fn stdin_is_written_and_closed_for_eof() {
        let j = run("cat", true, None, None);
        assert!(matches!(
            write(&j, &B64.encode(b"hello\n"), false),
            AgentResponse::JobAck
        ));
        write(&j, &B64.encode(b"world\n"), true);
        let st = wait_final(&j);
        assert_eq!(st.exit_code, Some(0));
        assert_eq!(read_all(&j, JobStream::Stdout), b"hello\nworld\n");
        // After close, further writes are refused.
        assert!(matches!(
            write(&j, &B64.encode(b"x"), false),
            AgentResponse::Error { .. }
        ));
    }

    #[test]
    fn a_job_without_stdin_sees_eof() {
        let j = run("cat; echo done", false, None, None);
        let st = wait_final(&j);
        assert_eq!(st.exit_code, Some(0));
        assert_eq!(read_all(&j, JobStream::Stdout), b"done\n");
    }

    #[test]
    fn reads_resume_from_an_offset() {
        let j = run("printf abcdef", false, None, None);
        wait_final(&j);
        match read(&j, JobStream::Stdout, 3, None, None) {
            AgentResponse::JobOutput {
                data_base64,
                next_offset,
                eof,
                ..
            } => {
                assert_eq!(B64.decode(data_base64).unwrap(), b"def");
                assert_eq!(next_offset, 6);
                assert!(eof);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_long_poll_wakes_when_output_arrives() {
        let j = run("sleep 0.3; echo late", false, None, None);
        let t = Instant::now();
        match read(&j, JobStream::Stdout, 0, None, Some(5000)) {
            AgentResponse::JobOutput { data_base64, .. } => {
                assert_eq!(B64.decode(data_base64).unwrap(), b"late\n");
            }
            other => panic!("{other:?}"),
        }
        assert!(t.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_timeout_kills_a_descendant_that_holds_the_pipes() {
        // The shell backgrounds a sleeper that inherits stdout; killing only the shell would hang.
        let j = run("sleep 60 & wait", false, None, Some(1));
        let t = Instant::now();
        let st = wait_final(&j);
        assert_eq!(st.phase, JobPhase::TimedOut, "{st:?}");
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        // The spool is closed, so a reader is not left waiting.
        assert!(read_all(&j, JobStream::Stdout).is_empty());
    }

    #[test]
    fn cancel_signals_the_whole_tree() {
        let j = run("sleep 60 & sleep 60; echo no", false, None, None);
        std::thread::sleep(Duration::from_millis(200));
        assert!(matches!(
            signal(&j, libc::SIGTERM, true),
            AgentResponse::JobAck
        ));
        let st = wait_final(&j);
        assert!(
            matches!(st.phase, JobPhase::Canceled | JobPhase::Signaled),
            "{st:?}"
        );
        assert_ne!(st.exit_code, Some(0));
    }

    #[test]
    fn reap_removes_a_finished_job_only() {
        let j = run("sleep 30", false, None, None);
        assert!(matches!(reap(&j), AgentResponse::Error { .. }));
        signal(&j, libc::SIGKILL, true);
        wait_final(&j);
        assert!(matches!(reap(&j), AgentResponse::JobAck));
        assert!(matches!(info(&j), AgentResponse::Error { .. }));
    }

    #[test]
    fn ids_are_validated_and_unique() {
        let _d = setup();
        for bad in ["", "../x", "a/b", "a b"] {
            let r = start(StartArgs {
                job_id: bad.into(),
                command: "true".into(),
                process: None,
                policy: None,
                timeout_seconds: None,
                stdin: false,
                max_output_bytes: None,
            });
            assert!(matches!(r, AgentResponse::Error { .. }), "{bad}");
        }
        let j = run("true", false, None, None);
        let r = start(StartArgs {
            job_id: j,
            command: "true".into(),
            process: None,
            policy: None,
            timeout_seconds: None,
            stdin: false,
            max_output_bytes: None,
        });
        assert!(matches!(r, AgentResponse::Error { .. }));
    }
}
