// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Host-side QEMU guest-agent helpers (Zyvor/GuestKit Windows agent channel).

use anyhow::{Context, Result, bail};
use guestkit::agent::qga_client::{call_qga_socket, qga_request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QgaExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

pub fn ping(socket: &Path) -> Result<()> {
    let _ = call_qga_socket(
        &socket.display().to_string(),
        "guest-ping",
        None,
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("QGA guest-ping on {}", socket.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QgaIpAddress {
    #[serde(rename = "ip-address")]
    pub ip_address: String,
    #[serde(rename = "ip-address-type")]
    pub ip_address_type: String,
    #[serde(default)]
    pub prefix: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QgaNetworkInterface {
    pub name: String,
    #[serde(rename = "hardware-address", default)]
    pub hardware_address: Option<String>,
    #[serde(rename = "ip-addresses", default)]
    pub ip_addresses: Vec<QgaIpAddress>,
}

/// Real guest-reported network interfaces and IPs via `guest-network-get-interfaces`
/// -- unlike DHCP-lease parsing (the only source `guest_ip` has today), this
/// works for every network mode the guest agent can reach, including
/// `NetworkSpec::User` (SLIRP), which has no lease file to parse at all.
pub fn network_interfaces(socket: &Path) -> Result<Vec<QgaNetworkInterface>> {
    let resp = call_qga_socket(
        &socket.display().to_string(),
        "guest-network-get-interfaces",
        None,
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("QGA guest-network-get-interfaces on {}", socket.display()))?;
    let ifaces = resp.get("return").cloned().unwrap_or(Value::Null);
    serde_json::from_value(ifaces).context("decode guest-network-get-interfaces response")
}

/// Freezes every mounted, writable filesystem inside the guest via
/// `guest-fsfreeze-freeze`, for an application-consistent disk snapshot
/// instead of a merely crash-consistent one -- the same mechanism
/// libvirt/virt-manager use for this. Always pair with a later
/// `fsfreeze_thaw` call (see its own doc comment for why a caller must
/// never leave a frozen guest unresolved). Returns the number of
/// filesystems frozen.
pub fn fsfreeze_freeze(socket: &Path) -> Result<i64> {
    let resp = call_qga_socket(
        &socket.display().to_string(),
        "guest-fsfreeze-freeze",
        None,
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("QGA guest-fsfreeze-freeze on {}", socket.display()))?;
    resp.get("return")
        .and_then(Value::as_i64)
        .context("decode guest-fsfreeze-freeze response")
}

/// Thaws filesystems frozen by `fsfreeze_freeze` via `guest-fsfreeze-thaw`.
/// Idempotent per QGA's own semantics -- calling this against an
/// already-thawed guest succeeds as a no-op rather than erroring, which is
/// exactly what makes it safe for a caller to retry unconditionally, on
/// every later attempt, until it succeeds, rather than ever giving up on
/// a stuck-frozen guest filesystem (a real, guest-visible failure mode
/// otherwise). Returns the number of filesystems thawed.
pub fn fsfreeze_thaw(socket: &Path) -> Result<i64> {
    let resp = call_qga_socket(
        &socket.display().to_string(),
        "guest-fsfreeze-thaw",
        None,
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("QGA guest-fsfreeze-thaw on {}", socket.display()))?;
    resp.get("return")
        .and_then(Value::as_i64)
        .context("decode guest-fsfreeze-thaw response")
}

/// Reports the guest's current freeze state via `guest-fsfreeze-status`
/// -- "frozen" or "thawed".
pub fn fsfreeze_status(socket: &Path) -> Result<String> {
    let resp = call_qga_socket(
        &socket.display().to_string(),
        "guest-fsfreeze-status",
        None,
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("QGA guest-fsfreeze-status on {}", socket.display()))?;
    resp.get("return")
        .and_then(Value::as_str)
        .map(String::from)
        .context("decode guest-fsfreeze-status response")
}

/// Run `path` with `args` via `guest-exec`, wait for completion, return output.
pub fn exec(
    socket: &Path,
    path: &str,
    args: &[String],
    timeout: Duration,
) -> Result<QgaExecResult> {
    let sock = socket.display().to_string();
    let start = call_qga_socket(
        &sock,
        "guest-exec",
        Some(json!({
            "path": path,
            "arg": args,
            "capture-output": true,
        })),
        timeout,
    )
    .context("QGA guest-exec")?;
    let pid = start
        .get("return")
        .and_then(|r| r.get("pid"))
        .and_then(|p| p.as_i64())
        .context("guest-exec response missing return.pid")?;

    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            bail!("QGA guest-exec timed out waiting for pid {pid}");
        }
        let status = call_qga_socket(
            &sock,
            "guest-exec-status",
            Some(json!({ "pid": pid })),
            timeout,
        )
        .context("QGA guest-exec-status")?;
        let ret = status.get("return").cloned().unwrap_or(Value::Null);
        let exited = ret.get("exited").and_then(|v| v.as_bool()).unwrap_or(false);
        if !exited {
            thread::sleep(EXEC_POLL);
            continue;
        }
        let exit_code = ret.get("exitcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        let stdout = decode_b64_field(&ret, "out-data");
        let stderr = decode_b64_field(&ret, "err-data");
        return Ok(QgaExecResult {
            exit_code,
            stdout,
            stderr,
        });
    }
}

fn decode_b64_field(ret: &Value, key: &str) -> String {
    ret.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        })
        .unwrap_or_default()
}

/// Convenience: `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command …`
pub fn powershell(socket: &Path, command: &str, timeout: Duration) -> Result<QgaExecResult> {
    exec(
        socket,
        "powershell.exe",
        &[
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-Command".into(),
            command.into(),
        ],
        timeout,
    )
}

pub fn firewall_open(
    socket: &Path,
    name: &str,
    port: u16,
    protocol: &str,
    timeout: Duration,
) -> Result<QgaExecResult> {
    let proto = protocol.to_ascii_uppercase();
    let cmd = format!(
        "New-NetFirewallRule -DisplayName '{name}' -Direction Inbound -Protocol {proto} \
         -LocalPort {port} -Action Allow -ErrorAction Stop | Out-Null; \
         Write-Output 'opened {name} {proto}/{port}'"
    );
    powershell(socket, &cmd, timeout)
}

pub fn firewall_close(socket: &Path, name: &str, timeout: Duration) -> Result<QgaExecResult> {
    let cmd = format!(
        "Remove-NetFirewallRule -DisplayName '{name}' -ErrorAction Stop; \
         Write-Output 'closed {name}'"
    );
    powershell(socket, &cmd, timeout)
}

/// Low-level raw QGA JSON (for advanced callers).
pub fn raw(
    socket: &Path,
    execute: &str,
    arguments: Option<Value>,
    timeout: Duration,
) -> Result<Value> {
    let _ = qga_request(execute, arguments.clone());
    call_qga_socket(&socket.display().to_string(), execute, arguments, timeout)
}

// ---------------------------------------------------------------------------
// Typed guest operations (osinfo, fsinfo, users, host name, clock, password,
// SSH authorized keys, vCPUs, memory blocks). Each wrapper issues exactly one
// QGA command. Secrets (passwords) are only ever placed in the request body;
// no error context in this section includes request arguments.
// ---------------------------------------------------------------------------

/// Sends `execute`, surfaces a guest-side `{"error": …}` reply as an `Err`
/// (the lower-level client returns it as a normal value), and yields `return`.
/// The context names the command only, never its arguments.
fn call_checked(
    socket: &Path,
    execute: &str,
    arguments: Option<Value>,
    timeout: Duration,
) -> Result<Value> {
    let resp = call_qga_socket(&socket.display().to_string(), execute, arguments, timeout)
        .with_context(|| format!("QGA {execute} on {}", socket.display()))?;
    if let Some(err) = resp.get("error") {
        let class = err.get("class").and_then(Value::as_str).unwrap_or("Error");
        let desc = err.get("desc").and_then(Value::as_str).unwrap_or("");
        bail!("QGA {execute} failed: {class}: {desc}");
    }
    Ok(resp.get("return").cloned().unwrap_or(Value::Null))
}

fn decode<T: serde::de::DeserializeOwned>(execute: &str, v: Value) -> Result<T> {
    serde_json::from_value(v).with_context(|| format!("decode {execute} response"))
}

/// `guest-get-osinfo`. Every field is optional: guests report what they know.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QgaOsInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        rename = "pretty-name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub pretty_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(
        rename = "version-id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub version_id: Option<String>,
    #[serde(
        rename = "kernel-release",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub kernel_release: Option<String>,
    #[serde(
        rename = "kernel-version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub kernel_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

pub fn os_info(socket: &Path) -> Result<QgaOsInfo> {
    decode(
        "guest-get-osinfo",
        call_checked(socket, "guest-get-osinfo", None, DEFAULT_TIMEOUT)?,
    )
}

/// One mounted filesystem from `guest-get-fsinfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QgaFilesystem {
    pub name: String,
    pub mountpoint: String,
    #[serde(rename = "type")]
    pub fs_type: String,
    #[serde(
        rename = "used-bytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub used_bytes: Option<u64>,
    #[serde(
        rename = "total-bytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub total_bytes: Option<u64>,
}

pub fn fs_info(socket: &Path) -> Result<Vec<QgaFilesystem>> {
    decode(
        "guest-get-fsinfo",
        call_checked(socket, "guest-get-fsinfo", None, DEFAULT_TIMEOUT)?,
    )
}

/// One logged-in session from `guest-get-users`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QgaUser {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Seconds since the epoch (fractional).
    #[serde(rename = "login-time", default)]
    pub login_time: f64,
}

pub fn users(socket: &Path) -> Result<Vec<QgaUser>> {
    decode(
        "guest-get-users",
        call_checked(socket, "guest-get-users", None, DEFAULT_TIMEOUT)?,
    )
}

pub fn host_name(socket: &Path) -> Result<String> {
    let v = call_checked(socket, "guest-get-host-name", None, DEFAULT_TIMEOUT)?;
    v.get("host-name")
        .and_then(Value::as_str)
        .map(String::from)
        .context("decode guest-get-host-name response")
}

/// Guest clock, nanoseconds since the Unix epoch (`guest-get-time`).
pub fn get_time_ns(socket: &Path) -> Result<i64> {
    call_checked(socket, "guest-get-time", None, DEFAULT_TIMEOUT)?
        .as_i64()
        .context("decode guest-get-time response")
}

/// `guest-set-time`. `Some(ns)` sets the guest clock to that instant (ns since
/// the epoch); `None` asks the guest to reload its clock from the hardware RTC.
pub fn set_time_ns(socket: &Path, ns: Option<i64>) -> Result<()> {
    let args = ns.map(|t| json!({ "time": t }));
    call_checked(socket, "guest-set-time", args, DEFAULT_TIMEOUT)?;
    Ok(())
}

/// Sets the guest clock from this host's wall clock; returns the ns value sent.
pub fn sync_time_from_host(socket: &Path) -> Result<i64> {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("host clock is before the Unix epoch")?
        .as_nanos();
    let ns = i64::try_from(ns).context("host time out of range")?;
    set_time_ns(socket, Some(ns))?;
    Ok(ns)
}

/// Rejects usernames the guest agent would mis-handle or that look like injection.
fn validate_username(username: &str) -> Result<()> {
    if username.is_empty() || username.len() > 256 {
        bail!("username must be 1-256 bytes");
    }
    if username.chars().any(|c| c.is_control() || c == ':') {
        bail!("username contains an invalid character");
    }
    Ok(())
}

/// `guest-set-user-password`. The password is base64-encoded as QGA requires;
/// with `crypted` it must already be a crypt(3)/Windows-style hash. It is never
/// logged and never part of an error.
pub fn set_user_password(
    socket: &Path,
    username: &str,
    password: &str,
    crypted: bool,
) -> Result<()> {
    use base64::Engine;
    validate_username(username)?;
    if password.is_empty() {
        bail!("password must not be empty");
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(password.as_bytes());
    call_checked(
        socket,
        "guest-set-user-password",
        Some(json!({ "username": username, "password": b64, "crypted": crypted })),
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// An authorized_keys line must be a single non-empty line.
pub fn validate_ssh_keys(keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        bail!("at least one SSH key is required");
    }
    for k in keys {
        let t = k.trim();
        if t.is_empty() || t.contains(['\n', '\r', '\0']) {
            bail!("SSH keys must be single non-empty lines");
        }
        if t.starts_with('#') || t.split_whitespace().count() < 2 {
            bail!("not an authorized_keys entry (expected `<type> <base64> [comment]`)");
        }
    }
    Ok(())
}

/// `guest-ssh-add-authorized-keys`; `reset` replaces the user's existing keys.
pub fn ssh_add_authorized_keys(
    socket: &Path,
    username: &str,
    keys: &[String],
    reset: bool,
) -> Result<()> {
    validate_username(username)?;
    validate_ssh_keys(keys)?;
    let keys: Vec<&str> = keys.iter().map(|k| k.trim()).collect();
    call_checked(
        socket,
        "guest-ssh-add-authorized-keys",
        Some(json!({ "username": username, "keys": keys, "reset": reset })),
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// `guest-ssh-remove-authorized-keys`.
pub fn ssh_remove_authorized_keys(socket: &Path, username: &str, keys: &[String]) -> Result<()> {
    validate_username(username)?;
    validate_ssh_keys(keys)?;
    let keys: Vec<&str> = keys.iter().map(|k| k.trim()).collect();
    call_checked(
        socket,
        "guest-ssh-remove-authorized-keys",
        Some(json!({ "username": username, "keys": keys })),
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// `guest-ssh-get-authorized-keys`.
pub fn ssh_get_authorized_keys(socket: &Path, username: &str) -> Result<Vec<String>> {
    validate_username(username)?;
    let v = call_checked(
        socket,
        "guest-ssh-get-authorized-keys",
        Some(json!({ "username": username })),
        DEFAULT_TIMEOUT,
    )?;
    decode(
        "guest-ssh-get-authorized-keys",
        v.get("keys").cloned().unwrap_or(Value::Null),
    )
}

/// One vCPU from `guest-get-vcpus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QgaVcpu {
    #[serde(rename = "logical-id")]
    pub logical_id: i64,
    pub online: bool,
    #[serde(rename = "can-offline", default)]
    pub can_offline: bool,
}

pub fn vcpus(socket: &Path) -> Result<Vec<QgaVcpu>> {
    decode(
        "guest-get-vcpus",
        call_checked(socket, "guest-get-vcpus", None, DEFAULT_TIMEOUT)?,
    )
}

/// One memory block from `guest-get-memory-blocks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QgaMemoryBlock {
    #[serde(rename = "phys-index")]
    pub phys_index: u64,
    pub online: bool,
    #[serde(rename = "can-offline", default)]
    pub can_offline: bool,
}

pub fn memory_blocks(socket: &Path) -> Result<Vec<QgaMemoryBlock>> {
    decode(
        "guest-get-memory-blocks",
        call_checked(socket, "guest-get-memory-blocks", None, DEFAULT_TIMEOUT)?,
    )
}

/// Aggregate of the read-only queries. Older or minimal agents lack some
/// commands, so each section is best-effort: a failed one is left empty and
/// its reason recorded in `errors` (keyed by section name).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QgaGuestInfo {
    pub os: Option<QgaOsInfo>,
    pub host_name: Option<String>,
    pub time_ns: Option<i64>,
    pub users: Vec<QgaUser>,
    pub filesystems: Vec<QgaFilesystem>,
    pub vcpus: Vec<QgaVcpu>,
    pub errors: std::collections::BTreeMap<String, String>,
}

pub fn guest_info(socket: &Path) -> Result<QgaGuestInfo> {
    // Fail fast (and with a clear error) when the agent is not reachable at all.
    ping(socket)?;
    let mut info = QgaGuestInfo::default();
    fn take<T>(
        r: Result<T>,
        name: &str,
        errors: &mut std::collections::BTreeMap<String, String>,
    ) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                errors.insert(name.to_string(), format!("{e:#}"));
                None
            }
        }
    }
    info.os = take(os_info(socket), "os", &mut info.errors);
    info.host_name = take(host_name(socket), "host_name", &mut info.errors);
    info.time_ns = take(get_time_ns(socket), "time", &mut info.errors);
    info.users = take(users(socket), "users", &mut info.errors).unwrap_or_default();
    info.filesystems = take(fs_info(socket), "filesystems", &mut info.errors).unwrap_or_default();
    info.vcpus = take(vcpus(socket), "vcpus", &mut info.errors).unwrap_or_default();
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    /// A hermetic QGA: answers each connection (one request line) with the next
    /// canned reply and records the requests it saw.
    struct MockQga {
        _dir: tempfile::TempDir,
        sock: std::path::PathBuf,
        seen: Arc<Mutex<Vec<Value>>>,
    }

    impl MockQga {
        fn new(replies: Vec<&'static str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("qga.sock");
            let listener = UnixListener::bind(&sock).unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let seen2 = seen.clone();
            thread::spawn(move || {
                for reply in replies {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    seen2
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&line).unwrap());
                    let mut s = &stream;
                    writeln!(s, "{reply}").unwrap();
                }
            });
            Self {
                _dir: dir,
                sock,
                seen,
            }
        }
        fn requests(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[test]
    fn os_info_decodes_and_tolerates_missing_fields() {
        let m = MockQga::new(vec![
            r#"{"return":{"id":"ubuntu","pretty-name":"Ubuntu 24.04","kernel-release":"6.8.0","machine":"x86_64"}}"#,
        ]);
        let os = os_info(&m.sock).unwrap();
        assert_eq!(os.pretty_name.as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(os.machine.as_deref(), Some("x86_64"));
        assert_eq!(os.version, None);
        assert_eq!(m.requests()[0]["execute"], "guest-get-osinfo");
    }

    #[test]
    fn guest_error_is_surfaced_without_arguments() {
        let m = MockQga::new(vec![
            r#"{"error":{"class":"CommandNotFound","desc":"the command is disabled"}}"#,
        ]);
        let e = format!("{:#}", users(&m.sock).unwrap_err());
        assert!(
            e.contains("CommandNotFound") && e.contains("disabled"),
            "{e}"
        );
    }

    #[test]
    fn fsinfo_users_hostname_time_vcpus_decode() {
        let m = MockQga::new(vec![
            r#"{"return":[{"name":"vda1","mountpoint":"/","type":"ext4","used-bytes":10,"total-bytes":100,"disk":[]}]}"#,
            r#"{"return":[{"user":"root","login-time":1700000000.5}]}"#,
            r#"{"return":{"host-name":"web-1"}}"#,
            r#"{"return":1700000000123456789}"#,
            r#"{"return":[{"logical-id":0,"online":true,"can-offline":false}]}"#,
            r#"{"return":[{"phys-index":0,"online":true,"can-offline":true}]}"#,
        ]);
        let fs = fs_info(&m.sock).unwrap();
        assert_eq!(
            (fs[0].fs_type.as_str(), fs[0].total_bytes),
            ("ext4", Some(100))
        );
        assert_eq!(users(&m.sock).unwrap()[0].user, "root");
        assert_eq!(host_name(&m.sock).unwrap(), "web-1");
        assert_eq!(get_time_ns(&m.sock).unwrap(), 1_700_000_000_123_456_789);
        assert!(vcpus(&m.sock).unwrap()[0].online);
        assert!(memory_blocks(&m.sock).unwrap()[0].can_offline);
    }

    #[test]
    fn set_time_with_and_without_value() {
        let m = MockQga::new(vec![r#"{"return":{}}"#, r#"{"return":{}}"#]);
        set_time_ns(&m.sock, Some(42)).unwrap();
        set_time_ns(&m.sock, None).unwrap();
        let r = m.requests();
        assert_eq!(r[0]["arguments"]["time"], 42);
        assert!(r[1].get("arguments").is_none());
    }

    #[test]
    fn sync_time_from_host_sends_a_plausible_ns_value() {
        let m = MockQga::new(vec![r#"{"return":{}}"#]);
        let ns = sync_time_from_host(&m.sock).unwrap();
        assert!(ns > 1_700_000_000_000_000_000);
        assert_eq!(m.requests()[0]["arguments"]["time"], ns);
    }

    #[test]
    fn password_is_base64_on_the_wire_and_absent_from_errors() {
        use base64::Engine;
        let m = MockQga::new(vec![
            r#"{"return":{}}"#,
            r#"{"error":{"class":"GenericError","desc":"no such user"}}"#,
        ]);
        set_user_password(&m.sock, "alice", "hunter2!secret", false).unwrap();
        let req = &m.requests()[0];
        assert_eq!(req["execute"], "guest-set-user-password");
        assert_eq!(req["arguments"]["username"], "alice");
        assert_eq!(req["arguments"]["crypted"], false);
        let wire = req["arguments"]["password"].as_str().unwrap();
        assert_ne!(wire, "hunter2!secret");
        let dec = base64::engine::general_purpose::STANDARD
            .decode(wire)
            .unwrap();
        assert_eq!(dec, b"hunter2!secret");
        let e = format!(
            "{:#}",
            set_user_password(&m.sock, "bob", "hunter2!secret", false).unwrap_err()
        );
        assert!(!e.contains("hunter2") && !e.contains(wire), "{e}");
    }

    #[test]
    fn password_and_username_validation_needs_no_socket() {
        let nowhere = Path::new("/nonexistent/qga.sock");
        assert!(set_user_password(nowhere, "", "x", false).is_err());
        assert!(set_user_password(nowhere, "a:b", "x", false).is_err());
        let e = set_user_password(nowhere, "alice", "", false).unwrap_err();
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn ssh_key_validation() {
        let ok = vec!["ssh-ed25519 AAAAC3Nza user@host".to_string()];
        assert!(validate_ssh_keys(&ok).is_ok());
        assert!(validate_ssh_keys(&[]).is_err());
        assert!(validate_ssh_keys(&["ssh-ed25519 AAAA\nssh-rsa BBBB".into()]).is_err());
        assert!(validate_ssh_keys(&["onlyonetoken".into()]).is_err());
        assert!(validate_ssh_keys(&["# ssh-rsa AAAA".into()]).is_err());
        assert!(validate_ssh_keys(&["  ".into()]).is_err());
    }

    #[test]
    fn ssh_add_remove_get_round_trip() {
        let m = MockQga::new(vec![
            r#"{"return":{}}"#,
            r#"{"return":{}}"#,
            r#"{"return":{"keys":["ssh-ed25519 AAAA a@b"]}}"#,
        ]);
        let keys = vec!["  ssh-ed25519 AAAA a@b  ".to_string()];
        ssh_add_authorized_keys(&m.sock, "root", &keys, true).unwrap();
        ssh_remove_authorized_keys(&m.sock, "root", &keys).unwrap();
        let got = ssh_get_authorized_keys(&m.sock, "root").unwrap();
        assert_eq!(got, vec!["ssh-ed25519 AAAA a@b"]);
        let r = m.requests();
        assert_eq!(r[0]["execute"], "guest-ssh-add-authorized-keys");
        assert_eq!(r[0]["arguments"]["reset"], true);
        assert_eq!(r[0]["arguments"]["keys"][0], "ssh-ed25519 AAAA a@b");
        assert_eq!(r[1]["execute"], "guest-ssh-remove-authorized-keys");
        assert!(r[1]["arguments"].get("reset").is_none());
        assert_eq!(r[2]["arguments"]["username"], "root");
    }

    #[test]
    fn guest_info_is_best_effort_per_section() {
        let m = MockQga::new(vec![
            r#"{"return":{}}"#,
            r#"{"return":{"id":"debian"}}"#,
            r#"{"error":{"class":"CommandNotFound","desc":"disabled"}}"#,
            r#"{"return":5}"#,
            r#"{"return":[]}"#,
            r#"{"return":[]}"#,
            r#"{"return":[{"logical-id":0,"online":true}]}"#,
        ]);
        let info = guest_info(&m.sock).unwrap();
        assert_eq!(info.os.unwrap().id.as_deref(), Some("debian"));
        assert_eq!(info.host_name, None);
        assert_eq!(info.time_ns, Some(5));
        assert_eq!(info.vcpus.len(), 1);
        assert!(info.errors["host_name"].contains("disabled"));
        assert_eq!(info.errors.len(), 1);
    }

    #[test]
    fn unreachable_socket_errors_cleanly() {
        assert!(guest_info(Path::new("/nonexistent/qga.sock")).is_err());
    }
}
