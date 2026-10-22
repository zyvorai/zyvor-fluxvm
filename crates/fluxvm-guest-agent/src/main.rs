// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Runs inside the guest: accepts one AF_VSOCK connection at a time (spawning
//! a thread per connection), reads one newline-delimited JSON request, acts
//! on it, and writes one newline-delimited JSON response.

mod exec_policy;
#[cfg(unix)]
mod jobs;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
#[cfg(unix)]
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use clap::Parser;
use fluxvm_guest_protocol::DEFAULT_PORT;
#[cfg(unix)]
use fluxvm_guest_protocol::{
    AgentRequest, AgentResponse, DEFAULT_EXEC_TIMEOUT_SECS, Envelope, MAX_FILE_TRANSFER_BYTES,
    TOKEN_FILE_PATH, constant_time_eq, decode_line, encode_line,
};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

/// Set by fluxvm-oci-init (`fluxvm_oci_init::config::POWEROFF_VIA_INIT_ENV`).
#[cfg(target_os = "linux")]
const POWEROFF_VIA_INIT_ENV: &str = "FLUXVM_POWEROFF_VIA_INIT";

#[derive(Parser)]
#[command(
    name = "fluxvm-guest-agent",
    version,
    about = "Zyvor FluxVM in-guest agent"
)]
struct Cli {
    /// AF_VSOCK port to listen on.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u32,
    /// Shared-secret token file (an OCI sandbox keeps it outside the image's read-only root).
    #[arg(long, default_value = fluxvm_guest_protocol::TOKEN_FILE_PATH)]
    token_file: String,
    /// A file the host provides the token in (macOS guests: the read-only `firstboot` share). Read only while the agent
    /// has no token of its own, then written to `--token-file`; a token already there always wins.
    #[arg(long)]
    token_seed: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run_server(cli.port, &cli.token_file, cli.token_seed.as_deref())
}

#[cfg(unix)]
fn run_server(port: u32, token_file: &str, token_seed: Option<&str>) -> Result<()> {
    use std::os::fd::FromRawFd;

    let expected_token: Arc<std::sync::Mutex<TokenState>> =
        Arc::new(std::sync::Mutex::new(TokenState {
            current: std::fs::read_to_string(token_file)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            previous: None,
            file: token_file.to_string(),
            seed: token_seed.map(str::to_owned),
        }));
    if expected_token.lock().is_ok_and(|t| t.current.is_some()) {
        eprintln!("fluxvm-guest-agent: token found at {token_file}, requests must authenticate");
    } else if exec_policy::insecure_opt_in(std::env::var(exec_policy::INSECURE_ENV).ok().as_deref())
    {
        eprintln!(
            "fluxvm-guest-agent: WARNING no token at {token_file} and {} is set — running unauthenticated, any vsock caller can run commands as root",
            exec_policy::INSECURE_ENV
        );
    } else {
        eprintln!(
            "fluxvm-guest-agent: ERROR no token at {token_file} — refusing every request (fail closed); provision the token, or set {}=1 to run unauthenticated",
            exec_policy::INSECURE_ENV
        );
    }

    let listener_fd = vsock::listen(port)?;

    eprintln!("fluxvm-guest-agent listening on vsock port {port}");

    loop {
        let conn_fd =
            unsafe { libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn_fd < 0 {
            eprintln!("accept failed: {}", std::io::Error::last_os_error());
            continue;
        }
        let expected_token = expected_token.clone();
        std::thread::spawn(move || {
            let file = unsafe { std::fs::File::from_raw_fd(conn_fd) };
            if let Err(e) = handle_connection(file, &expected_token, listener_fd) {
                eprintln!("connection error: {e:#}");
            }
        });
    }
}

/// The AF_VSOCK listener. Linux and Darwin differ only in the `sockaddr_vm` layout (Darwin's carries a length byte and
/// has no `VMADDR_CID_ANY` in the `libc` crate). The Darwin path is verified on an Apple M4: a macOS 27.0.1 guest with a
/// virtio-socket device (`apple.macos_vsock`) binds this address and the host reaches it.
#[cfg(unix)]
mod vsock {
    use anyhow::Result;

    /// `VMADDR_CID_ANY`, `(u32)-1`, on both platforms.
    pub const CID_ANY: u32 = u32::MAX;

    /// The `sockaddr_vm` for `port` on any local CID, and the length to pass to `bind`.
    pub fn sockaddr(port: u32) -> (libc::sockaddr_vm, libc::socklen_t) {
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        #[cfg(not(target_os = "linux"))]
        {
            addr.svm_len = std::mem::size_of::<libc::sockaddr_vm>() as u8;
        }
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = CID_ANY;
        addr.svm_port = port;
        (
            addr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    }

    pub fn listen(port: u32) -> Result<i32> {
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                anyhow::bail!("socket(AF_VSOCK): {}", std::io::Error::last_os_error());
            }
            let (addr, len) = sockaddr(port);
            if libc::bind(
                fd,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                len,
            ) != 0
            {
                anyhow::bail!(
                    "bind(vsock port={port}): {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::listen(fd, 16) != 0 {
                anyhow::bail!("listen: {}", std::io::Error::last_os_error());
            }
            super::set_cloexec(fd);
            Ok(fd)
        }
    }
}

#[cfg(not(unix))]
fn run_server(_port: u32, _token_file: &str, _token_seed: Option<&str>) -> Result<()> {
    anyhow::bail!("fluxvm-guest-agent supports Linux and macOS guests only (AF_VSOCK)")
}

#[cfg(unix)]
/// The agent's shared secret. After a rotation (`ResetIdentity.new_agent_token`) the token it replaced stays valid for
/// requests until one authenticates with the new one, so a reply the host never saw cannot lock it out.
struct TokenState {
    current: Option<String>,
    previous: Option<String>,
    file: String,
    /// Where the host leaves a token for a guest that was cloned from a template without one.
    seed: Option<String>,
}

/// How a request's token compares with the agent's.
#[derive(Debug, PartialEq, Eq)]
enum Auth {
    /// No token is provisioned.
    NoToken,
    Authorized,
    Denied,
}

impl TokenState {
    /// With no token, takes the host's from the seed file (a share that may mount after this process started, so this
    /// is tried on every request until it works) and keeps it in the token file. Never replaces a token that exists.
    fn adopt_seed(&mut self) {
        if self.current.is_some() {
            return;
        }
        let Some(seed) = self.seed.clone() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&seed) else {
            return;
        };
        let t = text.trim();
        if t.len() < 16 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return;
        }
        if self.rotate(t).is_ok() {
            self.previous = None;
            eprintln!(
                "fluxvm-guest-agent: took the token from {seed} and wrote it to {}",
                self.file
            );
        }
    }

    fn authorize(&mut self, got: Option<&str>) -> Auth {
        self.adopt_seed();
        let Some(cur) = self.current.as_deref() else {
            return Auth::NoToken;
        };
        if got.is_some_and(|g| constant_time_eq(cur, g)) {
            // The host has the new token: the old one is no longer needed.
            self.previous = None;
            return Auth::Authorized;
        }
        if let (Some(prev), Some(g)) = (self.previous.as_deref(), got)
            && constant_time_eq(prev, g)
        {
            return Auth::Authorized;
        }
        Auth::Denied
    }

    /// Writes `new` to the token file (0600, atomically) and starts accepting it.
    fn rotate(&mut self, new: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        if new.len() < 16 || new.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "new token must be at least 16 characters with no whitespace",
            ));
        }
        let tmp = format!("{}.new", self.file);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{new}\n").as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.file)?;
        self.previous = self.current.take();
        self.current = Some(new.to_string());
        Ok(())
    }
}

fn handle_connection(
    file: std::fs::File,
    token_state: &std::sync::Mutex<TokenState>,
    listener_fd: i32,
) -> Result<()> {
    let mut writer = file.try_clone().context("cloning connection fd")?;
    let mut reader = BufReader::new(file);

    let mut line = String::new();
    if reader.read_line(&mut line).context("reading request")? == 0 {
        return Ok(()); // peer closed without sending anything
    }
    let envelope: Envelope = match decode_line(&line) {
        Ok(envelope) => envelope,
        Err(_) => {
            // Answer instead of dropping the connection, so a newer host can tell "this agent
            // does not know that request" from a transient fault. This runs before authentication,
            // so the answer is generic: no echo of the request, no version, no list of operations.
            writer.write_all(
                encode_line(&AgentResponse::Error {
                    message: "unrecognised request: this agent cannot parse it and may be older than the host"
                        .into(),
                })?
                .as_bytes(),
            )?;
            writer.flush()?;
            return Ok(());
        }
    };

    let auth = token_state
        .lock()
        .map(|mut t| t.authorize(envelope.token.as_deref()))
        .unwrap_or(Auth::Denied);
    if auth == Auth::NoToken
        && !exec_policy::insecure_opt_in(std::env::var(exec_policy::INSECURE_ENV).ok().as_deref())
    {
        writer.write_all(
            encode_line(&AgentResponse::Error {
                message: format!(
                    "unauthorized: guest agent has no token provisioned at {TOKEN_FILE_PATH} (set {}=1 to allow unauthenticated use)",
                    exec_policy::INSECURE_ENV
                ),
            })?
            .as_bytes(),
        )?;
        writer.flush()?;
        return Ok(());
    }

    if auth != Auth::NoToken && auth != Auth::Authorized {
        writer.write_all(
            encode_line(&AgentResponse::Error {
                message: "unauthorized: missing or incorrect token".into(),
            })?
            .as_bytes(),
        )?;
        writer.flush()?;
        return Ok(());
    }

    let response = match envelope.request {
        AgentRequest::Ping => AgentResponse::Pong,
        AgentRequest::Exec {
            command,
            timeout_seconds,
            policy,
            process,
        } => {
            let timeout = Duration::from_secs(timeout_seconds.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS));
            match (policy, process) {
                (Some(_), Some(_)) => AgentResponse::Error {
                    message: "exec process cannot be combined with policy".into(),
                },
                (Some(policy), None) => exec_policy::exec_confined(&command, timeout, &policy),
                (None, Some(process)) => exec_process(&process, timeout),
                (None, None) => exec_with_timeout(&command, timeout),
            }
        }
        AgentRequest::PutFile {
            path,
            content_base64,
            mode,
        } => put_file(&path, &content_base64, mode),
        AgentRequest::GetFile { path } => get_file(&path),
        AgentRequest::OpenShell { cols, rows } => {
            // Leftover bytes the BufReader already pulled off the wire past
            // the request line (the client shouldn't send any before seeing
            // ShellOpened, but don't drop them silently if it does).
            let pending = reader.buffer().to_vec();
            let conn = reader.into_inner();
            return spawn_open_shell_session(listener_fd, conn, writer, cols, rows, pending);
        }
        AgentRequest::Capabilities => AgentResponse::Capabilities {
            features: vec![fluxvm_guest_protocol::FEATURE_JOBS_V2.to_string()],
        },
        AgentRequest::JobStart {
            job_id,
            command,
            process,
            policy,
            timeout_seconds,
            stdin,
            max_output_bytes,
        } => jobs::start(jobs::StartArgs {
            job_id,
            command,
            process,
            policy,
            timeout_seconds,
            stdin,
            max_output_bytes,
        }),
        AgentRequest::JobRead {
            job_id,
            stream,
            offset,
            max_bytes,
            wait_ms,
        } => jobs::read(&job_id, stream, offset, max_bytes, wait_ms),
        AgentRequest::JobWrite {
            job_id,
            data_base64,
            close,
        } => jobs::write(&job_id, &data_base64, close),
        AgentRequest::JobSignal {
            job_id,
            signal,
            tree,
        } => jobs::signal(&job_id, signal, tree),
        AgentRequest::JobInfo { job_id } => jobs::info(&job_id),
        AgentRequest::JobList => jobs::list(),
        AgentRequest::JobReap { job_id } => jobs::reap(&job_id),
        AgentRequest::Shutdown => {
            writer.write_all(encode_line(&AgentResponse::ShuttingDown)?.as_bytes())?;
            writer.flush()?;
            shutdown_guest();
            return Ok(());
        }
        AgentRequest::ResetIdentity {
            hostname,
            regenerate_machine_id,
            reseed_entropy,
            entropy_base64,
            regenerate_ssh_host_keys,
            reset_cloud_init,
            reset_dhcp_identity,
            flush_network_state,
            new_agent_token,
        } => {
            let mut resp = reset_identity(
                hostname.as_deref(),
                regenerate_machine_id,
                reseed_entropy,
                entropy_base64.as_deref(),
            );
            if let AgentResponse::IdentityReset { applied, failures } = &mut resp {
                reset_identity_extras(
                    ExtraResets {
                        ssh_host_keys: regenerate_ssh_host_keys,
                        cloud_init: reset_cloud_init,
                        dhcp: reset_dhcp_identity,
                        network_state: flush_network_state,
                    },
                    applied,
                    failures,
                );
                // Last, so every other step has run under the old token.
                if let Some(new) = new_agent_token.as_deref() {
                    match token_state.lock() {
                        Ok(mut t) => match t.rotate(new) {
                            Ok(()) => applied.push("agent-token".to_string()),
                            Err(e) => failures.push(format!("agent token: {e}")),
                        },
                        Err(_) => failures.push("agent token: state poisoned".to_string()),
                    }
                }
            }
            resp
        }
    };

    writer.write_all(encode_line(&response)?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Powers the guest off. Linux: `shutdown -h now`, or under fluxvm-oci-init (which has no shutdown(8)) SIGUSR2 to PID 1.
#[cfg(target_os = "linux")]
fn shutdown_guest() {
    if std::env::var_os(POWEROFF_VIA_INIT_ENV).is_some()
        || Command::new("shutdown")
            .args(["-h", "now"])
            .spawn()
            .is_err()
    {
        unsafe { libc::kill(1, libc::SIGUSR2) };
    }
}

/// macOS: `shutdown -h now` (the agent runs as root from a LaunchDaemon). PID 1 is launchd, so there is no signal fallback.
#[cfg(all(unix, not(target_os = "linux")))]
fn shutdown_guest() {
    let _ = Command::new("/sbin/shutdown").args(["-h", "now"]).spawn();
}

/// Marks `fd` close-on-exec so a later `fork`+`exec` (spawning the shell,
/// or any other child) doesn't inherit it. `fork()` copies *every* open fd
/// in the process by default, not just ones the child logically needs —
/// without this, the shell child would hold its own open copy of the vsock
/// connection fd, and the connection would never actually reach EOF even
/// after every fd this function itself holds is closed, since the kernel
/// still sees a live reference in the child.
#[cfg(unix)]
fn set_cloexec(fd: std::os::fd::RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

/// Runs an `OpenShell` session in a *double-forked, fully detached process*
/// rather than in this connection's thread — matching how OpenSSH's `sshd`
/// and systemd isolate `setsid()`/PTY/session-leader work away from their
/// own long-lived listener (see the README's "Interactive console"
/// section for the history: a version of this that ran `open_shell()`
/// in-process, even on its own thread, left the guest agent's *listener*
/// permanently unable to accept new vsock connections roughly 1-in-3
/// sessions — never reproduced in isolation despite extensive attempts,
/// but eliminated by not sharing a process with the listener at all,
/// which is the same reasoning sshd/systemd apply and is sufficient on
/// its own without needing the exact kernel mechanism).
///
/// First fork: the immediate child closes its inherited copy of the
/// listener fd right away, forks again, and exits — this process never
/// touches the PTY and is designed to be near-instant, so the parent's
/// `waitpid()` on it can't stall `accept()` for long. Second fork: the
/// grandchild runs the real `open_shell()` body, fully orphaned from the
/// agent's own process tree (reparented to the guest's init once its
/// immediate parent exits, so nothing in the agent ever calls
/// `wait()`/`kill()` on it either).
#[cfg(unix)]
fn spawn_open_shell_session(
    listener_fd: i32,
    conn: std::fs::File,
    writer: std::fs::File,
    cols: u16,
    rows: u16,
    pending: Vec<u8>,
) -> Result<()> {
    unsafe {
        let pid1 = libc::fork();
        if pid1 < 0 {
            anyhow::bail!(
                "fork (session isolation) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        if pid1 == 0 {
            libc::close(listener_fd);
            let pid2 = libc::fork();
            if pid2 == 0 {
                let _ = open_shell(conn, writer, cols, rows, &pending);
                libc::_exit(0);
            }
            libc::_exit(0);
        }
        // Parent: drop our own copies of the connection (the grandchild
        // owns it exclusively from here), then reap only the fast-exiting
        // first-level child — an ordinary, non-PTY, non-session-leader
        // child, the one shape already proven safe by every plain `exec`
        // call this agent has ever handled.
        drop(conn);
        drop(writer);
        let mut status = 0;
        libc::waitpid(pid1, &mut status, 0);
    }
    Ok(())
}

/// Allocates a PTY, spawns `/bin/sh` attached to it as its own session
/// leader with the slave as controlling terminal (so job control — Ctrl-C,
/// Ctrl-Z — works normally), acks `ShellOpened`, then relays between
/// `conn`/`writer` (the vsock connection, already split into its two halves
/// by the caller) and the PTY master until either side closes: `conn` is
/// read as a stream of [`fluxvm_guest_protocol::PtyFrame`]s (keystrokes and
/// live resizes), `writer` gets the PTY's raw output completely unframed —
/// see `PtyFrame`'s own docs for why the two directions differ.
/// `pending` is forwarded to the shell's stdin as raw bytes, unframed —
/// bytes the client already sent past the request line before this
/// function took over reading, which by definition predate the client
/// having seen `ShellOpened` and switching to `PtyFrame`s itself.
/// Always runs inside its own detached, double-forked process (see
/// `spawn_open_shell_session`) except in the unit test below, which calls
/// this directly to exercise the PTY logic in-process.
#[cfg(unix)]
fn open_shell(
    mut conn: std::fs::File,
    mut writer: std::fs::File,
    cols: u16,
    rows: u16,
    pending: &[u8],
) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};

    set_cloexec(conn.as_raw_fd());
    set_cloexec(writer.as_raw_fd());

    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        let err = std::io::Error::last_os_error();
        writer.write_all(
            encode_line(&AgentResponse::Error {
                message: format!("posix_openpt: {err}"),
            })?
            .as_bytes(),
        )?;
        writer.flush()?;
        return Ok(());
    }
    set_cloexec(master_fd);
    let setup: Result<std::path::PathBuf> = (|| unsafe {
        if libc::grantpt(master_fd) != 0 {
            anyhow::bail!("grantpt: {}", std::io::Error::last_os_error());
        }
        if libc::unlockpt(master_fd) != 0 {
            anyhow::bail!("unlockpt: {}", std::io::Error::last_os_error());
        }
        let mut buf = vec![0u8; 256];
        #[cfg(target_os = "linux")]
        {
            if libc::ptsname_r(master_fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) != 0 {
                anyhow::bail!("ptsname_r: {}", std::io::Error::last_os_error());
            }
        }
        // Darwin has no ptsname_r; ptsname's static buffer is fine here (open_shell runs in its own detached process).
        #[cfg(not(target_os = "linux"))]
        {
            let p = libc::ptsname(master_fd);
            if p.is_null() {
                anyhow::bail!("ptsname: {}", std::io::Error::last_os_error());
            }
            buf.clear();
            buf.extend_from_slice(std::ffi::CStr::from_ptr(p).to_bytes());
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        Ok(std::path::PathBuf::from(
            String::from_utf8_lossy(&buf).into_owned(),
        ))
    })();
    let slave_path = match setup {
        Ok(p) => p,
        Err(e) => {
            unsafe { libc::close(master_fd) };
            writer.write_all(
                encode_line(&AgentResponse::Error {
                    message: e.to_string(),
                })?
                .as_bytes(),
            )?;
            writer.flush()?;
            return Ok(());
        }
    };

    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &winsize) };

    let spawn_result = (|| -> Result<std::process::Child> {
        let slave0 = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)?;
        let slave1 = slave0.try_clone()?;
        let slave2 = slave0.try_clone()?;
        // `TIOCSCTTY` on an *inherited* PTY fd can fail EPERM in some
        // guest-kernel/PTY-allocation states even from a fresh session
        // leader — found live: `setsid()` succeeds, the ioctl still
        // returns EPERM. POSIX guarantees a *fresh open* of a terminal by
        // a session leader with no controlling terminal yet acquires one
        // automatically, no ioctl needed — re-opening the slave from
        // *inside* the child (after setsid, before exec) side-steps
        // whatever inherited-fd state trips up the ioctl path.
        let slave_path_c = std::ffi::CString::new(slave_path.as_os_str().as_encoded_bytes())
            .context("slave PTY path contains a NUL byte")?;
        let mut cmd = Command::new("/bin/sh");
        cmd.stdin(Stdio::from(slave0))
            .stdout(Stdio::from(slave1))
            .stderr(Stdio::from(slave2));
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let ctty_fd = libc::open(slave_path_c.as_ptr(), libc::O_RDWR);
                if ctty_fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ctty_fd);
                Ok(())
            });
        }
        Ok(cmd.spawn()?)
    })();

    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(master_fd) };
            writer.write_all(
                encode_line(&AgentResponse::Error {
                    message: format!("spawning shell: {e}"),
                })?
                .as_bytes(),
            )?;
            writer.flush()?;
            return Ok(());
        }
    };

    writer.write_all(encode_line(&AgentResponse::ShellOpened)?.as_bytes())?;
    writer.flush()?;

    let mut master_in = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let mut master_out = master_in.try_clone().context("cloning PTY master fd")?;
    if !pending.is_empty() {
        let _ = std::io::Write::write_all(&mut master_in, pending);
    }

    // Either relay direction finishing signals the session is over (client
    // hung up, or the shell exited and its PTY slave closed) — a channel
    // both threads share the sender end of lets the main thread block on
    // the first one to finish. (Not `JoinHandle::is_finished()` polling:
    // found live, under real vsock/PTY load, that a poll loop checking
    // `is_finished()` right after both threads had provably already
    // returned never observed it and spun forever — root cause not
    // identified; recv() on a channel each thread explicitly signals into
    // sidesteps whatever that was and is more idiomatic anyway.)
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

    // conn -> PTY master: client keystrokes (`PtyFrame::Data`) and live
    // resizes (`PtyFrame::Resize`) into the shell. `master_fd` is a plain
    // `Copy` int, still valid here even though `master_in`/`master_out`
    // above already wrap the same underlying fd in a `File` -- the ioctl
    // below doesn't touch Rust-side ownership of it at all, exactly like
    // the initial `TIOCSWINSZ` call earlier in this function.
    let to_shell_tx = done_tx.clone();
    let to_shell = std::thread::spawn(move || {
        while let Ok(Some(frame)) = fluxvm_guest_protocol::PtyFrame::read_from(&mut conn) {
            match frame {
                fluxvm_guest_protocol::PtyFrame::Data(bytes) => {
                    if std::io::Write::write_all(&mut master_in, &bytes).is_err() {
                        break;
                    }
                }
                fluxvm_guest_protocol::PtyFrame::Resize { cols, rows } => {
                    let winsize = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &winsize) };
                }
            }
        }
        let _ = to_shell_tx.send(());
    });
    // PTY master -> conn (shell output to client)
    let to_client_tx = done_tx.clone();
    let to_client = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = match std::io::Read::read(&mut master_out, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if writer.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to_client_tx.send(());
    });
    drop(done_tx);
    let _ = done_rx.recv();

    // Safe to kill()/wait() normally again: this function now always runs
    // inside its own double-forked, fully detached process (see
    // `spawn_open_shell_session`), never sharing a process with the
    // agent's vsock listener — which is what made explicit reaping here
    // risky before. See the README's "Interactive console" section.
    let _ = child.kill();
    let _ = child.wait();
    // Don't join to_shell/to_client either: whichever side didn't finish
    // is still blocked on its own read (the peer hasn't closed that half
    // yet) — it'll unblock and exit on its own once `conn`/the PTY master
    // actually close.
    drop(to_shell);
    drop(to_client);
    Ok(())
}

/// Runs `command` under `/bin/sh -c`, killing it if it runs longer than
/// `timeout` (the draft this was ported from accepted a timeout field but
/// never enforced it — this does). stdout/stderr are drained on their own
/// threads concurrently with the wait loop below: reading them only after
/// the child exits would deadlock on any command whose output exceeds the
/// pipe buffer before it's done (child blocks writing to a full pipe that
/// nothing is draining, so it never exits, so `try_wait` never returns).
#[cfg(unix)]
fn exec_with_timeout(command: &str, timeout: Duration) -> AgentResponse {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    run_with_timeout(cmd, timeout)
}

/// Runs `process.argv` with execve (no shell), with its environment, directory and identity.
#[cfg(unix)]
fn exec_process(process: &fluxvm_guest_protocol::ExecProcess, timeout: Duration) -> AgentResponse {
    if let Err(message) = process.validate() {
        return AgentResponse::Error { message };
    }
    run_with_timeout(process_command(process), timeout)
}

/// The command for a validated `process`: argv, environment, directory and identity.
#[cfg(unix)]
pub(crate) fn process_command(process: &fluxvm_guest_protocol::ExecProcess) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&process.argv[0]);
    cmd.args(&process.argv[1..]);
    if process.clean_env {
        cmd.env_clear();
    }
    for e in &process.env {
        if let Some((k, v)) = e.split_once('=') {
            cmd.env(k, v);
        }
    }
    if let Some(cwd) = &process.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(user) = process.user {
        // std drops root's supplementary groups (setgroups(0)) before setuid; pre_exec would run too late.
        cmd.gid(user.gid).uid(user.uid);
    }
    cmd
}

/// Most output a synchronous exec returns per stream. The rest is read and discarded so the
/// command keeps running and finishes with its real exit code.
#[cfg(unix)]
const EXEC_OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// How long to wait for the pipes to close after the command has exited or been killed. A
/// background child that inherited them can keep them open indefinitely.
#[cfg(unix)]
const EXEC_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A pipe drained on its own thread into a capped buffer.
#[cfg(unix)]
struct Drain {
    buf: Arc<std::sync::Mutex<(Vec<u8>, bool)>>,
    done: std::sync::mpsc::Receiver<()>,
}

#[cfg(unix)]
impl Drain {
    fn spawn<R: std::io::Read + Send + 'static>(mut pipe: R) -> Drain {
        let buf = Arc::new(std::sync::Mutex::new((Vec::new(), false)));
        let (tx, done) = std::sync::mpsc::channel();
        let b = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = b.lock().unwrap_or_else(|p| p.into_inner());
                        let room = EXEC_OUTPUT_CAP.saturating_sub(g.0.len());
                        if n > room {
                            g.1 = true;
                        }
                        g.0.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            let _ = tx.send(());
        });
        Drain { buf, done }
    }

    /// What was read, waiting until `deadline` for the pipe to reach EOF.
    fn collect(&self, deadline: Instant) -> String {
        let wait = deadline.saturating_duration_since(Instant::now());
        let _ = self.done.recv_timeout(wait);
        let g = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        let mut text = String::from_utf8_lossy(&g.0).into_owned();
        if g.1 {
            text.push_str(&format!(
                "\n[fluxvm: output truncated at {EXEC_OUTPUT_CAP} bytes]\n"
            ));
        }
        text
    }
}

#[cfg(unix)]
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> AgentResponse {
    use std::os::unix::process::CommandExt;
    // Its own process group, so a timeout reaches the command's descendants too.
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("spawning command: {e}"),
            };
        }
    };

    let stdout = Drain::spawn(child.stdout.take().expect("stdout was piped"));
    let stderr = Drain::spawn(child.stderr.take().expect("stderr was piped"));

    let pgid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return AgentResponse::Error {
                    message: format!("waiting on command: {e}"),
                };
            }
        }
    };

    let drain_by = Instant::now() + EXEC_DRAIN_GRACE;
    let stdout = stdout.collect(drain_by);
    let stderr = stderr.collect(drain_by);

    match status {
        Some(status) => AgentResponse::Exec {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            enforcement: None,
        },
        None => AgentResponse::Error {
            message: format!(
                "command exceeded {}s timeout and was killed",
                timeout.as_secs()
            ),
        },
    }
}

#[cfg(unix)]
fn put_file(path: &str, content_base64: &str, mode: Option<u32>) -> AgentResponse {
    let bytes = match B64.decode(content_base64) {
        Ok(b) => b,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("content is not valid base64: {e}"),
            };
        }
    };
    if bytes.len() > MAX_FILE_TRANSFER_BYTES {
        return AgentResponse::Error {
            message: format!(
                "content is {} bytes, exceeds the {}-byte transfer limit",
                bytes.len(),
                MAX_FILE_TRANSFER_BYTES
            ),
        };
    }
    if let Some(parent) = std::path::Path::new(path).parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return AgentResponse::Error {
            message: format!("creating {}: {e}", parent.display()),
        };
    }
    if let Err(e) = std::fs::write(path, &bytes) {
        return AgentResponse::Error {
            message: format!("writing {path}: {e}"),
        };
    }
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode.unwrap_or(0o644));
    if let Err(e) = std::fs::set_permissions(path, perms) {
        return AgentResponse::Error {
            message: format!("setting permissions on {path}: {e}"),
        };
    }
    AgentResponse::FileWritten
}

#[cfg(unix)]
fn get_file(path: &str) -> AgentResponse {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("reading {path}: {e}"),
            };
        }
    };
    if metadata.len() as usize > MAX_FILE_TRANSFER_BYTES {
        return AgentResponse::Error {
            message: format!(
                "{path} is {} bytes, exceeds the {}-byte transfer limit",
                metadata.len(),
                MAX_FILE_TRANSFER_BYTES
            ),
        };
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return AgentResponse::Error {
                message: format!("reading {path}: {e}"),
            };
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    AgentResponse::FileContent {
        content_base64: B64.encode(&bytes),
        mode,
    }
}

/// Valid hostname: 1..=63 chars of `[A-Za-z0-9-]`, no leading/trailing `-`.
#[cfg(unix)]
fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 16 random bytes as 32 lowercase hex chars, the `/etc/machine-id` format.
#[cfg(target_os = "linux")]
fn fresh_machine_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    // Mark as a version-4 UUID like systemd-id128 does.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Credit `data` to the kernel entropy pool (RNDADDENTROPY, needs root), then reseed the CRNG at once (RNDRESEEDCRNG).
///
/// The second step matters for a forked guest: every child resumes the same CRNG state, and crediting the pool does
/// not by itself make the kernel reseed (it does so on a schedule, at most once a minute), so without it two children
/// draw identical "random" machine-ids and SSH host keys right after the restore. Found on hardware.
#[cfg(target_os = "linux")]
fn add_entropy(data: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // _IOW('R', 0x03, int[2])
    const RNDADDENTROPY: u32 = 0x4008_5203;
    // _IO('R', 0x07)
    const RNDRESEEDCRNG: u32 = 0x5207;
    // struct rand_pool_info { int entropy_count; int buf_size; u32 buf[]; }
    let mut buf = Vec::with_capacity(8 + data.len());
    buf.extend_from_slice(&((data.len() * 8) as i32).to_ne_bytes());
    buf.extend_from_slice(&(data.len() as i32).to_ne_bytes());
    buf.extend_from_slice(data);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/urandom")?;
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), RNDADDENTROPY as _, buf.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), RNDRESEEDCRNG as _) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!("RNDRESEEDCRNG (the pool was credited but the CRNG was not reseeded): {e}"),
        ));
    }
    Ok(())
}

/// Reset the per-VM identity a forked/restored guest inherited from its
/// parent snapshot. Every step is independent and best-effort; the response
/// lists what was applied and what failed.
#[cfg(target_os = "linux")]
fn reset_identity(
    hostname: Option<&str>,
    regenerate_machine_id: bool,
    reseed_entropy: bool,
    entropy_base64: Option<&str>,
) -> AgentResponse {
    let mut applied = Vec::new();
    let mut failures = Vec::new();

    // Entropy first, so the machine-id below is drawn from the fresh pool.
    if reseed_entropy {
        let host_bytes = entropy_base64
            .filter(|s| !s.is_empty())
            .map(|s| B64.decode(s));
        match host_bytes {
            Some(Ok(bytes)) if !bytes.is_empty() => {
                match add_entropy(&bytes[..bytes.len().min(512)]) {
                    Ok(()) => applied.push("entropy".to_string()),
                    Err(e) => failures.push(format!("RNDADDENTROPY: {e}")),
                }
            }
            Some(Err(e)) => failures.push(format!("entropy_base64: {e}")),
            _ => failures.push("no host entropy supplied; pool not reseeded".to_string()),
        }
    }

    if let Some(name) = hostname {
        if !valid_hostname(name) {
            failures.push(format!("invalid hostname {name:?}"));
        } else {
            let rc = unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
            if rc != 0 {
                failures.push(format!("sethostname: {}", std::io::Error::last_os_error()));
            } else {
                applied.push("hostname".to_string());
            }
            if let Err(e) = std::fs::write("/etc/hostname", format!("{name}\n")) {
                failures.push(format!("/etc/hostname: {e}"));
            }
        }
    }

    if regenerate_machine_id {
        match fresh_machine_id() {
            Ok(id) => {
                match std::fs::write("/etc/machine-id", format!("{id}\n")) {
                    Ok(()) => applied.push("machine-id".to_string()),
                    Err(e) => failures.push(format!("/etc/machine-id: {e}")),
                }
                // Debian-style copy; leave a symlink (-> /etc/machine-id) alone.
                let dbus = "/var/lib/dbus/machine-id";
                if std::fs::symlink_metadata(dbus).is_ok_and(|m| m.file_type().is_file())
                    && let Err(e) = std::fs::write(dbus, format!("{id}\n"))
                {
                    failures.push(format!("{dbus}: {e}"));
                }
            }
            Err(e) => failures.push(format!("generating machine-id: {e}")),
        }
    }

    // Stale L2/L3 state from the parent: neighbour cache entries.
    if Command::new("ip")
        .args(["neigh", "flush", "all"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        applied.push("neigh-flush".to_string());
    }

    AgentResponse::IdentityReset { applied, failures }
}

/// Which of the extra, strict-mode identity steps a request asked for.
#[derive(Clone, Copy)]
struct ExtraResets {
    ssh_host_keys: bool,
    cloud_init: bool,
    dhcp: bool,
    network_state: bool,
}

/// Runs `cmd`; `Ok(false)` when the tool is not installed (nothing to reset), `Err` when it ran and failed.
#[cfg(target_os = "linux")]
fn run_tool(cmd: &str, args: &[&str]) -> Result<bool, String> {
    match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => Ok(true),
        Ok(s) => Err(format!("{cmd} {}: {s}", args.join(" "))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{cmd}: {e}")),
    }
}

#[cfg(target_os = "linux")]
fn extras_ssh_host_keys(applied: &mut Vec<String>, failures: &mut Vec<String>) {
    let dir = std::path::Path::new("/etc/ssh");
    let mut ok = true;
    if dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("ssh_host_")
                    && (n.ends_with("_key") || n.ends_with("_key.pub"))
                    && let Err(err) = std::fs::remove_file(e.path())
                {
                    failures.push(format!("removing {n}: {err}"));
                    ok = false;
                }
            }
        }
        match run_tool("ssh-keygen", &["-A"]) {
            Ok(_) => {}
            Err(e) => {
                failures.push(e);
                ok = false;
            }
        }
        // Best effort: a running sshd still holds the old keys in memory.
        let _ = run_tool(
            "systemctl",
            &["try-reload-or-restart", "ssh.service", "sshd.service"],
        );
    }
    if ok {
        applied.push("ssh-host-keys".to_string());
    }
}

#[cfg(target_os = "linux")]
fn extras_cloud_init(applied: &mut Vec<String>, failures: &mut Vec<String>) {
    match run_tool("cloud-init", &["clean", "--logs"]) {
        Ok(_) => applied.push("cloud-init".to_string()),
        Err(e) => failures.push(e),
    }
}

#[cfg(target_os = "linux")]
fn extras_dhcp(applied: &mut Vec<String>, failures: &mut Vec<String>) {
    let networkd_active =
        run_tool("systemctl", &["is-active", "--quiet", "systemd-networkd"]).unwrap_or(false);
    if !networkd_active {
        applied.push("dhcp".to_string());
    } else {
        let ifaces: Vec<String> = std::fs::read_dir("/sys/class/net")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n != "lo")
                    .collect()
            })
            .unwrap_or_default();
        let _ = run_tool("networkctl", &["reload"]);
        let mut args = vec!["reconfigure"];
        args.extend(ifaces.iter().map(String::as_str));
        match run_tool("networkctl", &args) {
            Ok(_) => applied.push("dhcp".to_string()),
            Err(e) => failures.push(e),
        }
    }
}

#[cfg(target_os = "linux")]
fn extras_network_state(applied: &mut Vec<String>, failures: &mut Vec<String>) {
    let mut ok = true;
    for (cmd, args) in [
        ("ip", &["neigh", "flush", "all"][..]),
        ("ip", &["route", "flush", "cache"][..]),
    ] {
        if let Err(e) = run_tool(cmd, args) {
            failures.push(e);
            ok = false;
        }
    }
    let _ = run_tool("conntrack", &["-F"]);
    if ok {
        applied.push("network-state".to_string());
    }
}

/// The identity a clone shares with its parent beyond hostname, machine-id and entropy. A step whose tool or
/// subject is absent (no sshd, no cloud-init, no networkd) is reported applied: there is nothing to reset, and a strict
/// caller must not fail a guest for lacking software.
#[cfg(target_os = "linux")]
fn reset_identity_extras(x: ExtraResets, applied: &mut Vec<String>, failures: &mut Vec<String>) {
    // Host keys and sshd, cloud-init's state and the network steps do not depend on one another, and the slowest,
    // RSA host key generation, dominates a reset, so those three run side by side. The network steps stay in order:
    // `ip neigh flush` exits non-zero ("dump interrupted") when `networkctl reconfigure` changes the neighbour table
    // under it, and a strict fork treats that as a failed reset (seen on a real guest). Results are merged in a fixed
    // order, so the response is the same as before.
    type Step = Box<dyn FnOnce(&mut Vec<String>, &mut Vec<String>) + Send>;
    let (dhcp, network_state) = (x.dhcp, x.network_state);
    let steps: [(bool, Step); 3] = [
        (x.ssh_host_keys, Box::new(extras_ssh_host_keys)),
        (x.cloud_init, Box::new(extras_cloud_init)),
        (
            dhcp || network_state,
            Box::new(move |a, f| {
                if dhcp {
                    extras_dhcp(a, f);
                }
                if network_state {
                    extras_network_state(a, f);
                }
            }),
        ),
    ];
    let results: Vec<(Vec<String>, Vec<String>)> = std::thread::scope(|sc| {
        let handles: Vec<_> = steps
            .into_iter()
            .filter(|(asked, _)| *asked)
            .map(|(_, step)| {
                sc.spawn(move || {
                    let (mut a, mut f) = (Vec::new(), Vec::new());
                    step(&mut a, &mut f);
                    (a, f)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    (
                        Vec::new(),
                        vec!["an identity reset step panicked".to_string()],
                    )
                })
            })
            .collect()
    });
    for (a, f) in results {
        applied.extend(a);
        failures.extend(f);
    }
}

/// macOS guests have none of these steps; asking for them is reported, never silently skipped.
#[cfg(all(unix, not(target_os = "linux")))]
fn reset_identity_extras(x: ExtraResets, _applied: &mut Vec<String>, failures: &mut Vec<String>) {
    for (asked, what) in [
        (x.ssh_host_keys, "ssh-host-keys"),
        (x.cloud_init, "cloud-init"),
        (x.dhcp, "dhcp"),
        (x.network_state, "network-state"),
    ] {
        if asked {
            failures.push(format!("{what} reset is not supported on macOS guests"));
        }
    }
}

/// macOS guests: only the hostname is applied (sethostname + scutil). The Linux machine-id, RNDADDENTROPY and `ip neigh`
/// steps have no macOS equivalent, so a request for them is reported as a failure rather than silently skipped.
/// UNVERIFIED on hardware.
#[cfg(all(unix, not(target_os = "linux")))]
fn reset_identity(
    hostname: Option<&str>,
    regenerate_machine_id: bool,
    reseed_entropy: bool,
    _entropy_base64: Option<&str>,
) -> AgentResponse {
    let mut applied = Vec::new();
    let mut failures = Vec::new();
    if reseed_entropy {
        failures.push("entropy reseed is not supported on macOS guests".to_string());
    }
    if regenerate_machine_id {
        failures.push("machine-id does not exist on macOS guests".to_string());
    }
    if let Some(name) = hostname {
        if !valid_hostname(name) {
            failures.push(format!("invalid hostname {name:?}"));
        } else {
            let rc = unsafe {
                libc::sethostname(
                    name.as_ptr() as *const libc::c_char,
                    name.len() as libc::c_int,
                )
            };
            if rc != 0 {
                failures.push(format!("sethostname: {}", std::io::Error::last_os_error()));
            } else {
                applied.push("hostname".to_string());
            }
            for key in ["HostName", "LocalHostName", "ComputerName"] {
                let ok = Command::new("/usr/sbin/scutil")
                    .args(["--set", key, name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if !ok {
                    failures.push(format!("scutil --set {key}"));
                }
            }
        }
    }
    AgentResponse::IdentityReset { applied, failures }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn state(dir: &std::path::Path, token: &str) -> TokenState {
        let file = dir.join("token").to_string_lossy().into_owned();
        std::fs::write(&file, format!("{token}\n")).unwrap();
        TokenState {
            current: Some(token.to_string()),
            previous: None,
            file,
            seed: None,
        }
    }

    #[test]
    fn rotation_replaces_the_token_and_keeps_the_old_one_for_one_request() {
        let d = tempfile::tempdir().unwrap();
        let mut t = state(d.path(), "old-token-0123456789");
        assert_eq!(t.authorize(Some("old-token-0123456789")), Auth::Authorized);
        assert_eq!(t.authorize(Some("nope")), Auth::Denied);
        assert_eq!(t.authorize(None), Auth::Denied);

        t.rotate("new-token-abcdefghij").unwrap();
        // The file holds the new token, 0600.
        let on_disk = std::fs::read_to_string(&t.file).unwrap();
        assert_eq!(on_disk.trim(), "new-token-abcdefghij");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&t.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A host that never saw the reply still gets in with the old token...
        assert_eq!(t.authorize(Some("old-token-0123456789")), Auth::Authorized);
        // ...until a request authenticates with the new one, which retires the old.
        assert_eq!(t.authorize(Some("new-token-abcdefghij")), Auth::Authorized);
        assert_eq!(t.authorize(Some("old-token-0123456789")), Auth::Denied);
    }

    #[test]
    fn rotation_refuses_weak_or_malformed_tokens_and_leaves_the_old_one() {
        let d = tempfile::tempdir().unwrap();
        let mut t = state(d.path(), "old-token-0123456789");
        for bad in [
            "short",
            "has space in it 0123456789",
            "line\nbreak-0123456789",
        ] {
            assert!(t.rotate(bad).is_err(), "{bad:?}");
        }
        assert_eq!(t.authorize(Some("old-token-0123456789")), Auth::Authorized);
        assert!(t.previous.is_none());
    }

    #[test]
    fn no_token_is_reported_as_such() {
        let mut t = TokenState {
            current: None,
            previous: None,
            file: "/nonexistent".into(),
            seed: None,
        };
        assert_eq!(t.authorize(Some("anything")), Auth::NoToken);
    }

    #[test]
    fn a_seed_provides_the_token_only_when_there_is_none() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let seed = d.path().join("agent-token");
        let file = d.path().join("token");
        let mk = |current: Option<&str>| TokenState {
            current: current.map(str::to_owned),
            previous: None,
            file: file.to_string_lossy().into_owned(),
            seed: Some(seed.to_string_lossy().into_owned()),
        };

        // The share is not mounted yet: still no token, and nothing is written.
        let mut t = mk(None);
        assert_eq!(t.authorize(Some("host-token-0123456789")), Auth::NoToken);
        assert!(!file.exists());

        // Mounted later: the next request adopts it, 0600, without a restart.
        std::fs::write(&seed, "host-token-0123456789\n").unwrap();
        assert_eq!(t.authorize(Some("host-token-0123456789")), Auth::Authorized);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap().trim(),
            "host-token-0123456789"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(t.authorize(Some("wrong")), Auth::Denied);

        // A token the guest already has is never replaced by the seed.
        let mut keeps = mk(Some("guest-token-abcdefghij"));
        assert_eq!(keeps.authorize(Some("host-token-0123456789")), Auth::Denied);
        assert_eq!(
            keeps.authorize(Some("guest-token-abcdefghij")),
            Auth::Authorized
        );

        // A short or malformed seed is ignored.
        std::fs::write(&seed, "short\n").unwrap();
        let mut bad = mk(None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(bad.authorize(Some("short")), Auth::NoToken);
    }

    #[test]
    fn vsock_sockaddr_listens_on_any_cid_at_the_requested_port() {
        let (addr, len) = vsock::sockaddr(17777);
        // Copy out of the packed struct before comparing.
        let (family, cid, port) = (addr.svm_family, addr.svm_cid, addr.svm_port);
        assert_eq!(family as i32, libc::AF_VSOCK);
        assert_eq!(cid, u32::MAX);
        assert_eq!(port, 17777);
        assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_vm>());
        #[cfg(target_os = "macos")]
        {
            // Darwin's sockaddr_vm is packed: len(1) family(1) reserved(2) port(4) cid(4).
            assert_eq!(len, 12);
            assert_eq!(addr.svm_len, 12);
        }
    }

    #[test]
    fn exec_timeout_returns_even_when_a_descendant_holds_the_pipes() {
        let t = Instant::now();
        let r = exec_with_timeout("sleep 60 & wait", Duration::from_secs(1));
        assert!(matches!(r, AgentResponse::Error { .. }), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    }

    #[test]
    fn exec_returns_when_a_background_child_keeps_the_pipes_open() {
        let t = Instant::now();
        let r = exec_with_timeout("sleep 20 & echo hi", Duration::from_secs(30));
        match r {
            AgentResponse::Exec {
                exit_code, stdout, ..
            } => {
                assert_eq!(exit_code, 0);
                assert_eq!(stdout, "hi\n");
            }
            other => panic!("{other:?}"),
        }
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    }

    #[test]
    fn hostname_validation() {
        assert!(valid_hostname("vm-fork-1"));
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("-x"));
        assert!(!valid_hostname("a.b"));
        assert!(!valid_hostname(&"a".repeat(64)));
    }

    #[test]
    fn put_file_creates_parent_dirs_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.yaml");
        let resp = put_file(path.to_str().unwrap(), &B64.encode(b"hello"), Some(0o600));
        assert!(matches!(resp, AgentResponse::FileWritten));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn put_file_rejects_invalid_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        let resp = put_file(path.to_str().unwrap(), "not-valid-base64!!!", None);
        assert!(matches!(resp, AgentResponse::Error { .. }));
        assert!(!path.exists());
    }

    #[test]
    fn get_file_round_trips_put_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        put_file(
            path.to_str().unwrap(),
            &B64.encode(b"hello world"),
            Some(0o640),
        );

        match get_file(path.to_str().unwrap()) {
            AgentResponse::FileContent {
                content_base64,
                mode,
            } => {
                assert_eq!(B64.decode(&content_base64).unwrap(), b"hello world");
                assert_eq!(mode, 0o640);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn get_file_errors_on_missing_path() {
        let resp = get_file("/nonexistent/path/for/sure");
        assert!(matches!(resp, AgentResponse::Error { .. }));
    }

    /// A connected AF_UNIX socketpair stands in for the two vsock-connection
    /// halves `handle_connection` would otherwise split off a real
    /// `AF_VSOCK` fd — same `std::fs::File`-over-raw-fd shape, so
    /// `open_shell` runs completely unmodified in this test.
    fn socketpair() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        // SOCK_CLOEXEC: this whole test simulates "guest agent" and "vsock
        // client" as two threads of ONE process (a real deployment has them
        // as separate processes in separate kernels — host vs. guest — with
        // no fd table in common at all). Without CLOEXEC here, the client
        // thread's own end of the pair leaks into every child the agent
        // thread forks (fork() copies the whole process's fd table, not
        // just the calling thread's fds), so the agent's read on its own
        // end never sees EOF even after the client side closes — a
        // same-process test artifact, not a real cross-VM vsock bug.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(
            rc,
            0,
            "socketpair failed: {}",
            std::io::Error::last_os_error()
        );
        // No SOCK_CLOEXEC on Darwin: set it after the fact on both ends.
        for fd in fds {
            set_cloexec(fd);
        }
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    #[test]
    fn an_unparsable_request_gets_an_error_answer_not_a_silent_close() {
        let (agent_end, client_end) = socketpair();
        let dir = tempfile::tempdir().unwrap();
        let tokens = std::sync::Mutex::new(state(dir.path(), "tok-0123456789abcdef"));
        let mut client_reader = BufReader::new(client_end.try_clone().unwrap());
        let mut client_writer = client_end;
        std::thread::scope(|scope| {
            let agent = scope.spawn(|| handle_connection(agent_end, &tokens, -1));
            client_writer
                .write_all(b"{\"op\":\"a-request-from-a-newer-host\"}\n")
                .unwrap();
            client_writer.flush().unwrap();
            let mut answer = String::new();
            client_reader.read_line(&mut answer).unwrap();
            assert!(!answer.is_empty(), "the agent closed without answering");
            let parsed: AgentResponse = decode_line(&answer).unwrap();
            match parsed {
                AgentResponse::Error { message } => {
                    assert!(message.contains("unrecognised request"), "{message}");
                    assert!(
                        !message.contains("a-request-from-a-newer-host"),
                        "must not echo the request: {message}"
                    );
                }
                other => panic!("expected an error, got {other:?}"),
            }
            agent.join().unwrap().unwrap();
        });
    }

    #[test]
    fn open_shell_runs_a_real_pty_shell_round_trip() {
        let (agent_end, client_end) = socketpair();
        let agent_end2 = agent_end.try_clone().unwrap();

        let shell = std::thread::spawn(move || {
            open_shell(agent_end, agent_end2, 80, 24, &[]).unwrap();
        });

        let mut client_reader = BufReader::new(client_end.try_clone().unwrap());
        let mut client_writer = client_end;

        // First line back is the ShellOpened ack, still JSON-framed.
        let mut ack = String::new();
        client_reader.read_line(&mut ack).unwrap();
        assert!(ack.contains("shell-opened"), "unexpected ack: {ack:?}");

        // Everything the client writes from here on must be a `PtyFrame`
        // (see the module-level doc comment) — echo a marker and read until
        // we see it (the shell's own echo of our input, then its command's
        // output, both arrive on the same raw, unframed output stream).
        client_writer
            .write_all(
                &fluxvm_guest_protocol::PtyFrame::Data(b"echo PTY_ECHO_TEST_MARKER\n".to_vec())
                    .encode(),
            )
            .unwrap();
        client_writer.flush().unwrap();

        unsafe {
            use std::os::fd::AsRawFd;
            let fd = client_reader.get_ref().as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let marker_seen = loop {
            if std::time::Instant::now() > deadline {
                break false;
            }
            let mut buf = [0u8; 1024];
            let r = std::io::Read::read(&mut client_reader, &mut buf);
            match r {
                Ok(0) => break false,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains("PTY_ECHO_TEST_MARKER") {
                        break true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break false,
            }
        };
        assert!(
            marker_seen,
            "never saw the marker in PTY output; got: {:?}",
            String::from_utf8_lossy(&seen)
        );

        // Both ends must close for the peer to see EOF — client_reader
        // holds a dup'd fd of the same socket, so dropping client_writer
        // alone leaves the connection open from the agent's point of view.
        drop(client_writer);
        drop(client_reader);
        shell.join().unwrap();
    }

    /// A live `PtyFrame::Resize` mid-session must actually reach the PTY —
    /// not just be accepted without error. `stty size` (prints "rows cols")
    /// run *inside* the shell after the resize is the real, kernel-level
    /// proof: it reads the PTY's current `TIOCGWINSZ`, so this only passes
    /// if the resize frame's `ioctl(TIOCSWINSZ)` genuinely landed on the
    /// same PTY the shell is attached to.
    #[test]
    fn open_shell_resize_frame_changes_the_ptys_window_size() {
        let (agent_end, client_end) = socketpair();
        let agent_end2 = agent_end.try_clone().unwrap();

        let shell = std::thread::spawn(move || {
            open_shell(agent_end, agent_end2, 80, 24, &[]).unwrap();
        });

        let mut client_reader = BufReader::new(client_end.try_clone().unwrap());
        let mut client_writer = client_end;

        let mut ack = String::new();
        client_reader.read_line(&mut ack).unwrap();
        assert!(ack.contains("shell-opened"), "unexpected ack: {ack:?}");

        client_writer
            .write_all(
                &fluxvm_guest_protocol::PtyFrame::Resize {
                    cols: 123,
                    rows: 45,
                }
                .encode(),
            )
            .unwrap();
        client_writer
            .write_all(&fluxvm_guest_protocol::PtyFrame::Data(b"stty size\n".to_vec()).encode())
            .unwrap();
        client_writer.flush().unwrap();

        unsafe {
            use std::os::fd::AsRawFd;
            let fd = client_reader.get_ref().as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        // `stty size` prints "<rows> <cols>" — the resize was applied
        // *before* the command ran, so its own output must reflect it.
        let saw_resized_output = loop {
            if std::time::Instant::now() > deadline {
                break false;
            }
            let mut buf = [0u8; 1024];
            match std::io::Read::read(&mut client_reader, &mut buf) {
                Ok(0) => break false,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains("45 123") {
                        break true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break false,
            }
        };
        assert!(
            saw_resized_output,
            "never saw the resized `stty size` output (\"45 123\"); got: {:?}",
            String::from_utf8_lossy(&seen)
        );

        drop(client_writer);
        drop(client_reader);
        shell.join().unwrap();
    }
}
