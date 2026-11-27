// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl port-forward`: 127.0.0.1:HOST on this machine to a VM's loopback port GUEST, for a VM that already exists.
//!
//! Each host port gets its own detached `ssh -N -L` (the forwarder `fluxctl docker` and `fluxctl kube` use) and a record,
//! `~/.fluxvm/forwards/HOST.pid`: the pid, then what is forwarded as JSON. `--list` and `--stop` read only those records, so
//! they work without the daemon and after the VM is gone. A recorded pid counts as ours only while its command line still
//! carries this exact forward. Nothing restarts a forward: it ends when the connection does (VM stopped, host rebooted).

use crate::docker_host as dh;
use crate::output::{self, Column, OutputFormat, col};
use crate::run;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// How long ssh has to log in and open the local port.
const READY_BUDGET: Duration = Duration::from_secs(30);

#[derive(clap::Args, Clone, Debug)]
pub struct PortForwardArgs {
    /// VM name, id or id prefix. With `--list` it is optional and filters the list.
    #[arg(required_unless_present = "list")]
    pub vm: Option<String>,
    /// `HOST` or `HOST:GUEST` (default guest port: the same number). With `--stop`, the host ports to stop (all of the
    /// VM's forwards when none is given).
    #[arg(value_name = "HOST[:GUEST]")]
    pub ports: Vec<String>,
    /// Show the forwards on this machine and whether each is still running.
    #[arg(long, conflicts_with_all = ["stop", "ports"])]
    pub list: bool,
    /// Stop forwards of the VM.
    #[arg(long)]
    pub stop: bool,
    /// Login user (default: the user the VM's cloud-init created, else root).
    #[arg(long, short = 'l', conflicts_with_all = ["list", "stop"])]
    pub user: Option<String>,
}

impl PortForwardArgs {
    /// The VM argument as an id, for the paths that talk to the daemon.
    pub fn vm_id(&self) -> Result<Uuid> {
        let vm = self.vm.as_deref().context("name the VM")?;
        output::parse_vm_ref(vm).map_err(anyhow::Error::msg)
    }
}

/// The VM a forward connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub vm_id: Uuid,
    pub vm: String,
    pub user: String,
    /// The guest's address, `host` or `host:ssh-port`.
    pub address: String,
    /// Check the guest's host key, as `fluxctl ssh` does for a guest that is not a local NAT or loopback-forwarded one.
    pub check_host_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub vm_id: Uuid,
    pub vm: String,
    pub user: String,
    pub address: String,
    pub host_port: u16,
    pub guest_port: u16,
}

impl Record {
    /// The `-L` value.
    fn spec(&self) -> String {
        format!("127.0.0.1:{}:127.0.0.1:{}", self.host_port, self.guest_port)
    }

    /// What our forwarder's command line contains, so a stale pid is never mistaken for another process.
    fn needle(&self) -> String {
        format!(
            "-L {} {}@{}",
            self.spec(),
            self.user,
            run::split_endpoint(&self.address).0
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    #[serde(flatten)]
    pub record: Record,
    pub pid: u32,
    pub running: bool,
}

const COLUMNS: &[Column] = &[
    col("HOST", "/host_port"),
    col("VM", "/vm"),
    col("GUEST", "/guest_port"),
    col("USER", "/user"),
    col("ADDRESS", "/address"),
    col("PID", "/pid"),
    col("RUNNING", "/running"),
];

pub fn default_dir() -> Result<PathBuf> {
    Ok(dh::home()?.join(".fluxvm").join("forwards"))
}

fn pid_path(dir: &Path, host_port: u16) -> PathBuf {
    dir.join(format!("{host_port}.pid"))
}

fn log_path(dir: &Path, host_port: u16) -> PathBuf {
    dir.join(format!("{host_port}.log"))
}

/// Reads the VM record (the `GET /v1/vms/{id}` shape) into the connection a forward needs.
pub fn target(vm: &Value, user: Option<String>) -> Result<Target> {
    let name = vm["name"].as_str().unwrap_or_default().to_owned();
    let vm_id = vm["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .context("the VM record has no id")?;
    let status = vm["status"].as_str().unwrap_or_default();
    if status != "running" {
        bail!(
            "VM {name} is {status}, and a forward needs it running (`fluxctl start {name}` starts a stopped VM)"
        );
    }
    let address = vm["guest_ip"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| run::loopback_ssh_endpoint(&vm["request"]))
        .with_context(|| unreachable_reason(&name, vm))?;
    // The same rule as `run::ssh_argv`: Mac NAT guests reuse addresses and recreated loopback forwards change keys.
    let check_host_key = vm["backend"] != "vz" && run::split_endpoint(&address).1.is_none();
    let user = user
        .or_else(|| {
            vm["request"]["cloud_init"]["user"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "root".into());
    Ok(Target {
        vm_id,
        vm: name,
        user,
        address,
        check_host_key,
    })
}

/// Why a VM without an address cannot be reached over SSH, as far as its record tells.
fn unreachable_reason(name: &str, vm: &Value) -> String {
    match vm["request"]["network"]["mode"].as_str() {
        Some("none") => format!(
            "VM {name} has no network card (network.mode \"none\"), so there is no SSH connection to forward over"
        ),
        Some("user") if vm["backend"] != "vz" => format!(
            "VM {name} uses user-mode networking with no SSH forward on this machine's loopback, so it cannot be reached \
             over SSH. Create it with a network.forwards entry for guest_port 22 on host_addr 127.0.0.1 (forwards of \
             this backend are set when the VM is created)"
        ),
        _ => format!("VM {name} has no guest_ip yet: wait for it to boot, then run this again"),
    }
}

fn read_entry(path: &Path) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let record: Record = serde_json::from_str(lines.next()?).ok()?;
    let running = i32::try_from(pid).is_ok_and(|p| p > 1 && dh::is_our_ssh(p, &record.needle()));
    Some(Entry {
        record,
        pid,
        running,
    })
}

/// Every recorded forward, by host port. One whose ssh has ended stays listed (`running: false`) until it is stopped or
/// its port is forwarded again.
pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out: Vec<Entry> = read
        .filter_map(|f| {
            let path = f.ok()?.path();
            (path.extension()? == "pid")
                .then(|| read_entry(&path))
                .flatten()
        })
        .collect();
    out.sort_by_key(|e| e.record.host_port);
    Ok(out)
}

/// The forwards of `vm`: the name it had when the forward started, its id or an id prefix, or `id`, which is what the name
/// resolves to now (so a renamed VM is still found).
pub fn of_vm(entries: Vec<Entry>, vm: &str, id: Option<Uuid>) -> Vec<Entry> {
    entries
        .into_iter()
        .filter(|e| {
            let own = e.record.vm_id.to_string();
            Some(e.record.vm_id) == id
                || e.record.vm == vm
                || own == vm
                || (vm.len() >= 4 && own.starts_with(vm))
        })
        .collect()
}

/// Stops `vm`'s forwards on `ports` (all of them when empty) and forgets them. A port it does not forward is an error, and
/// then nothing is stopped.
pub fn stop(dir: &Path, vm: &str, id: Option<Uuid>, ports: &[u16]) -> Result<Vec<Entry>> {
    let mine = of_vm(list(dir)?, vm, id);
    if mine.is_empty() {
        bail!("there is no forward for VM {vm} (`fluxctl port-forward --list` shows them all)");
    }
    if let Some(p) = ports
        .iter()
        .find(|p| !mine.iter().any(|e| e.record.host_port == **p))
    {
        bail!(
            "VM {vm} has no forward on 127.0.0.1:{p} (`fluxctl port-forward --list {vm}` shows its forwards)"
        );
    }
    let chosen: Vec<Entry> = mine
        .into_iter()
        .filter(|e| ports.is_empty() || ports.contains(&e.record.host_port))
        .collect();
    for e in &chosen {
        forget(dir, &e.record);
    }
    Ok(chosen)
}

fn forget(dir: &Path, r: &Record) {
    dh::stop_recorded_ssh(&pid_path(dir, r.host_port), &r.needle());
    let _ = std::fs::remove_file(log_path(dir, r.host_port));
}

fn answers(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Refuses a host port something already listens on. Connecting also catches a wildcard listener, which on macOS would not
/// make the bind fail.
fn check_free(port: u16) -> Result<()> {
    if answers(port) {
        bail!("127.0.0.1:{port} is already in use on this machine: pick another host port");
    }
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "127.0.0.1:{port} needs root: pick a host port of 1024 or higher, for example {}:GUEST",
            u32::from(port) + 8000
        ),
        Err(e) => Err(e).with_context(|| format!("127.0.0.1:{port} cannot be used")),
    }
}

fn ssh_command(r: &Record, identity: &Option<PathBuf>, check_host_key: bool) -> Result<Command> {
    let base = run::ssh_base_keys(&r.address, &r.user, identity, check_host_key);
    let mut c = dh::forward_from(base, &[r.spec()])?;
    // A shared connection from the user's ssh config would own the forward, and killing our ssh would not close it. OpenSSH
    // reads options after the destination too; the `-L … user@host` text above stays as it is.
    c.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
    Ok(c)
}

fn wait_ready(child: &mut Child, port: u16, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("ssh ended ({status})");
        }
        if answers(port) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "ssh did not open the port within {}s (is the guest reachable?)",
                budget.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn write_record(dir: &Path, r: &Record, pid: u32) -> Result<()> {
    let path = pid_path(dir, r.host_port);
    // `create_new`: when two commands race for a port, the loser never overwrites the winner's record.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(f, "{pid}\n{}", serde_json::to_string(r)?)?;
    Ok(())
}

/// Starts one forward, or returns the running one with the same settings (`false`). The same VM's forward on this port with
/// other settings (another guest port, or the VM came back on another address) is replaced.
fn start_one(
    dir: &Path,
    want: Record,
    command: &dyn Fn(&Record) -> Result<Command>,
    budget: Duration,
) -> Result<(Entry, bool)> {
    let port = want.host_port;
    if let Some(have) = read_entry(&pid_path(dir, port)) {
        if have.running {
            if have.record == want {
                return Ok((have, false));
            }
            if have.record.vm_id != want.vm_id {
                bail!(
                    "127.0.0.1:{port} already forwards to port {} of VM {} (ssh pid {}): stop that first with \
                     `fluxctl port-forward --stop {} {port}`, or pick another host port",
                    have.record.guest_port,
                    have.record.vm,
                    have.pid,
                    have.record.vm
                );
            }
        }
        forget(dir, &have.record);
        if have.running {
            let deadline = Instant::now() + Duration::from_secs(5);
            while answers(port) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    } else {
        // An unreadable record names no process we could signal.
        let _ = std::fs::remove_file(pid_path(dir, port));
    }
    check_free(port)?;
    let log = log_path(dir, port);
    let mut c = command(&want)?;
    c.stderr(Stdio::from(
        std::fs::File::create(&log).with_context(|| format!("writing {}", log.display()))?,
    ));
    let mut child = c
        .spawn()
        .context("starting ssh (is the OpenSSH client installed?)")?;
    let pid = child.id();
    if let Err(e) = write_record(dir, &want, pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    if let Err(why) = wait_ready(&mut child, port, budget) {
        let _ = child.kill();
        let _ = child.wait();
        let said = std::fs::read_to_string(&log).unwrap_or_default();
        forget(dir, &want);
        let said = match said.trim() {
            "" => String::new(),
            t => format!("\nssh said: {t}"),
        };
        bail!(
            "could not forward 127.0.0.1:{port} to port {} of VM {}: {why}{said}\nCheck that `fluxctl ssh {} -l {} true` \
             logs in without asking for a password.",
            want.guest_port,
            want.vm,
            want.vm,
            want.user
        );
    }
    Ok((
        Entry {
            record: want,
            pid,
            running: true,
        },
        true,
    ))
}

/// Starts a forward per `(host, guest)` pair. When one fails, the ones this call started are stopped again.
fn start(
    dir: &Path,
    t: &Target,
    ports: &[(u16, u16)],
    command: &dyn Fn(&Record) -> Result<Command>,
    budget: Duration,
) -> Result<Vec<(Entry, bool)>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut done: Vec<(Entry, bool)> = vec![];
    for &(host_port, guest_port) in ports {
        let want = Record {
            vm_id: t.vm_id,
            vm: t.vm.clone(),
            user: t.user.clone(),
            address: t.address.clone(),
            host_port,
            guest_port,
        };
        match start_one(dir, want, command, budget) {
            Ok(r) => done.push(r),
            Err(e) => {
                for (entry, new) in &done {
                    if *new {
                        forget(dir, &entry.record);
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(done)
}

/// `fluxctl port-forward VM HOST[:GUEST]…`; `vm` is the VM's record.
pub fn open(a: &PortForwardArgs, vm: &Value, format: OutputFormat) -> Result<()> {
    let ports = dh::parse_publish_all(&a.ports)?;
    if ports.is_empty() {
        bail!(
            "give at least one port, for example `fluxctl port-forward {} 8080:80` (or --list, --stop)",
            a.vm.as_deref().unwrap_or("VM")
        );
    }
    let t = target(vm, a.user.clone())?;
    let identity = run::generated_identity();
    let done = start(
        &default_dir()?,
        &t,
        &ports,
        &|r| ssh_command(r, &identity, t.check_host_key),
        READY_BUDGET,
    )?;
    for (e, new) in &done {
        let r = &e.record;
        eprintln!(
            "{} 127.0.0.1:{} -> port {} of {} (ssh pid {}); stop it with `fluxctl port-forward --stop {} {}`",
            if *new {
                "forwarding"
            } else {
                "already forwarding"
            },
            r.host_port,
            r.guest_port,
            r.vm,
            e.pid,
            r.vm,
            r.host_port
        );
    }
    let entries: Vec<&Entry> = done.iter().map(|(e, _)| e).collect();
    output::print_list(format, &entries, COLUMNS)
}

/// `--list` and `--stop`: local records only, no daemon.
pub fn list_or_stop(a: &PortForwardArgs, format: OutputFormat) -> Result<()> {
    let dir = default_dir()?;
    // The name or prefix as it resolves now; a deleted VM resolves to nothing and is matched by its recorded name.
    let id = a.vm.as_deref().and_then(|v| output::parse_vm_ref(v).ok());
    let entries = if a.stop {
        let vm = a.vm.as_deref().context("--stop needs the VM")?;
        let ports: Vec<u16> = dh::parse_publish_all(&a.ports)?
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        let mut stopped = stop(&dir, vm, id, &ports)?;
        for e in &mut stopped {
            e.running = false;
            eprintln!(
                "stopped 127.0.0.1:{} -> port {} of {}",
                e.record.host_port, e.record.guest_port, e.record.vm
            );
        }
        stopped
    } else {
        match &a.vm {
            Some(vm) => of_vm(list(&dir)?, vm, id),
            None => list(&dir)?,
        }
    };
    output::print_list(format, &entries, COLUMNS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU16, Ordering};

    fn vm_target(name: &str, id: u128) -> Target {
        Target {
            vm_id: Uuid::from_u128(id),
            vm: name.into(),
            user: "me".into(),
            address: "192.0.2.10".into(),
            check_host_key: false,
        }
    }

    /// A free port no other test is handed: below the ephemeral ranges, so tests that bind port 0 cannot take it, and
    /// above the range `vm_net` uses. It only connects, so it leaves no socket behind.
    fn free_port() -> u16 {
        static NEXT: AtomicU16 = AtomicU16::new(0);
        let base = 28000 + (std::process::id() % 100) as u16 * 40;
        loop {
            let port = base + NEXT.fetch_add(1, Ordering::Relaxed);
            if !answers(port) {
                return port;
            }
        }
    }

    /// Binds the port `start_one` has just probed. A child another test is starting can hold the probe's socket for a
    /// moment longer than we do, so this tries again.
    fn listen_on(port: u16) -> std::io::Result<TcpListener> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                r => return r,
            }
        }
    }

    /// A stand-in forwarder: a process whose command line reads like ours. `listen` opens the host port from this test, as
    /// ssh would once logged in.
    fn fake<'a>(listen: &'a RefCell<Vec<TcpListener>>) -> impl Fn(&Record) -> Result<Command> + 'a {
        move |r: &Record| {
            listen.borrow_mut().push(listen_on(r.host_port)?);
            let mut c = Command::new("sh");
            // Short sleeps: a child left behind by killing the shell would hold whatever it inherited from the test
            // process (a socket of a test running alongside) for as long as it lives.
            let wait = "i=0; while [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done";
            c.args(["-c", wait, "ssh"])
                .args(r.needle().split(' '))
                .stdout(Stdio::null());
            Ok(c)
        }
    }

    #[test]
    fn target_reads_the_vm_record_like_ssh_does() {
        let vm = json!({"id": Uuid::from_u128(7), "name": "web", "status": "running", "guest_ip": "192.0.2.10",
                        "request": {"cloud_init": {"user": "dev"}}});
        let t = target(&vm, None).unwrap();
        assert_eq!((t.user.as_str(), t.address.as_str()), ("dev", "192.0.2.10"));
        assert_eq!(target(&vm, Some("ops".into())).unwrap().user, "ops");

        let bare = json!({"id": Uuid::nil(), "name": "b", "status": "running", "guest_ip": "192.0.2.11", "request": {}});
        assert_eq!(target(&bare, None).unwrap().user, "root");

        let stopped =
            json!({"id": Uuid::nil(), "name": "s", "status": "stopped", "guest_ip": "192.0.2.12"});
        assert!(
            target(&stopped, None)
                .unwrap_err()
                .to_string()
                .contains("is stopped")
        );
        let booting =
            json!({"id": Uuid::nil(), "name": "n", "status": "running", "guest_ip": null});
        assert!(
            target(&booting, None)
                .unwrap_err()
                .to_string()
                .contains("no guest_ip")
        );
    }

    #[test]
    fn linux_backends_are_reached_or_told_why_not() {
        let vm = |backend: &str, ip: Value, network: Value| {
            json!({"id": Uuid::nil(), "name": "q", "status": "running", "backend": backend, "guest_ip": ip,
                   "request": {"network": network}})
        };
        // QEMU user-mode with a loopback SSH forward: reached through it, host key not remembered.
        let user = json!({"mode": "user", "forwards": [
            {"host_addr": "127.0.0.1", "host_port": 22001, "guest_port": 22}]});
        let t = target(&vm("qemu", Value::Null, user), None).unwrap();
        assert_eq!(
            (t.address.as_str(), t.check_host_key),
            ("127.0.0.1:22001", false)
        );
        // A bridged guest keeps the user's host key checking; a Mac NAT guest does not.
        let tap = json!({"mode": "tap"});
        assert!(
            target(&vm("qemu", json!("192.0.2.5"), tap), None)
                .unwrap()
                .check_host_key
        );
        let nat = json!({"mode": "user"});
        assert!(
            !target(&vm("vz", json!("192.0.2.6"), nat.clone()), None)
                .unwrap()
                .check_host_key
        );
        let why = |v: Value| target(&v, None).unwrap_err().to_string();
        assert!(why(vm("qemu", Value::Null, nat.clone())).contains("guest_port 22"));
        assert!(why(vm("vz", Value::Null, nat)).contains("no guest_ip yet"));
        assert!(
            why(vm("firecracker", Value::Null, json!({"mode": "none"})))
                .contains("no network card")
        );
    }

    #[test]
    fn ssh_command_is_a_detached_loopback_forward() {
        let r = Record {
            vm_id: Uuid::nil(),
            vm: "web".into(),
            user: "me".into(),
            address: "127.0.0.1:22001".into(),
            host_port: 8080,
            guest_port: 80,
        };
        let line = |check| {
            let c = ssh_command(&r, &None, check).unwrap();
            std::iter::once(c.get_program())
                .chain(c.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(!line(true).contains("StrictHostKeyChecking"));
        let line = line(false);
        assert!(line.contains("StrictHostKeyChecking=no"), "{line}");
        assert!(line.contains(" -N "), "{line}");
        assert!(line.contains("ExitOnForwardFailure=yes"), "{line}");
        assert!(line.contains("-p 22001"), "{line}");
        // The needle that identifies the process is in the command line as ssh gets it.
        assert!(
            line.contains("-L 127.0.0.1:8080:127.0.0.1:80 me@127.0.0.1"),
            "{line}"
        );
        assert!(line.contains(&r.needle()), "{line}");
        assert!(line.ends_with("-o ControlPath=none"), "{line}");
    }

    #[test]
    fn needles_do_not_match_a_longer_port() {
        let r = |h, g| Record {
            vm_id: Uuid::nil(),
            vm: "v".into(),
            user: "me".into(),
            address: "192.0.2.1".into(),
            host_port: h,
            guest_port: g,
        };
        assert!(!r(180, 80).needle().contains(&r(80, 80).needle()));
        assert!(!r(8000, 8080).needle().contains(&r(8000, 80).needle()));
    }

    #[test]
    fn start_list_and_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let listen = RefCell::new(vec![]);
        let web = vm_target("web", 1);
        let (a, b) = (free_port(), free_port());
        let wait = Duration::from_secs(5);

        let done = start(
            tmp.path(),
            &web,
            &[(a, 80), (b, 5432)],
            &fake(&listen),
            wait,
        )
        .unwrap();
        assert!(done.iter().all(|(e, new)| *new && e.running));
        let all = list(tmp.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|e| e.running), "{all:?}");

        // The same request again starts nothing.
        let again = start(tmp.path(), &web, &[(a, 80)], &fake(&listen), wait).unwrap();
        assert!(!again[0].1);
        assert_eq!(again[0].0.pid, done[0].0.pid);

        // Another VM cannot take the port.
        let db = vm_target("db", 2);
        let err = start(tmp.path(), &db, &[(a, 80)], &fake(&listen), wait)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already forwards to port 80 of VM web"),
            "{err}"
        );

        // An unknown port stops nothing.
        assert!(stop(tmp.path(), "web", None, &[a, 1]).is_err());
        assert_eq!(list(tmp.path()).unwrap().len(), 2);
        assert!(stop(tmp.path(), "db", None, &[]).is_err());

        let stopped = stop(tmp.path(), "web", None, &[b]).unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(list(tmp.path()).unwrap().len(), 1);
        // By id, which is how a renamed VM is found.
        let stopped = stop(tmp.path(), "new-name", Some(web.vm_id), &[]).unwrap();
        assert_eq!(stopped.len(), 1);
        assert!(list(tmp.path()).unwrap().is_empty());
        for (e, _) in done {
            assert!(!dh::is_our_ssh(e.pid as i32, &e.record.needle()));
        }
    }

    #[test]
    fn a_failed_forward_reports_ssh_and_undoes_the_call() {
        let tmp = tempfile::tempdir().unwrap();
        let listen = RefCell::new(vec![]);
        let (good, bad) = (free_port(), free_port());
        let ok = fake(&listen);
        let command = |r: &Record| {
            if r.host_port == good {
                return ok(r);
            }
            let mut c = Command::new("sh");
            c.args(["-c", "echo 'Permission denied (publickey).' >&2; exit 255"]);
            Ok(c)
        };
        let err = start(
            tmp.path(),
            &vm_target("web", 1),
            &[(good, 80), (bad, 81)],
            &command,
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Permission denied (publickey)"), "{err}");
        assert!(list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_busy_host_port_is_refused_before_ssh_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        let err = start(
            tmp.path(),
            &vm_target("web", 1),
            &[(port, 80)],
            &|_| panic!("ssh must not run"),
            Duration::from_secs(1),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already in use"), "{err}");
    }

    #[test]
    fn a_dead_forward_is_listed_and_a_stale_pid_is_not_signalled() {
        let tmp = tempfile::tempdir().unwrap();
        // This test process stands for an unrelated process that got the recorded pid.
        let r = Record {
            vm_id: Uuid::from_u128(3),
            vm: "gone".into(),
            user: "me".into(),
            address: "192.0.2.10".into(),
            host_port: free_port(),
            guest_port: 80,
        };
        write_record(tmp.path(), &r, std::process::id()).unwrap();
        assert!(
            write_record(tmp.path(), &r, 2).is_err(),
            "never overwritten"
        );
        let all = list(tmp.path()).unwrap();
        assert!(!all[0].running);
        // A deleted VM is still found by its recorded name or id prefix.
        assert_eq!(of_vm(all.clone(), "gone", None).len(), 1);
        assert_eq!(of_vm(all.clone(), &r.vm_id.to_string()[..8], None).len(), 1);
        assert!(of_vm(all, "gon", None).is_empty());
        stop(tmp.path(), "gone", None, &[]).unwrap(); // and we are still here
        assert!(list(tmp.path()).unwrap().is_empty());
    }
}
