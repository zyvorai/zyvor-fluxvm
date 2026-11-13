// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Offline preflight of an [`ExecPolicy`]: "what would a guest with Landlock ABI N enforce, and will the command even
//! start?" without a VM. Dependency-free and pure.
//!
//! [`preflight`] mirrors the strict/best-effort decision of `fluxvm_procbox::landlock::plan` (ABI requirements per
//! protection). The scheduler crate has a test that runs both over a matrix of policies and ABIs so the two cannot
//! drift. [`lint`] adds the mistakes that make a policy refuse to run, fail to start the shell, or quietly confine
//! less than it appears to.

use crate::{ExecIsolation, ExecPolicy, ExecSeccompMode, ExecTcpRule};
use serde::Serialize;
use serde_json::Value;

/// Highest Landlock ABI the guest agent knows how to use.
pub const MAX_KNOWN_ABI: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preflight {
    pub abi: u32,
    /// Strict mode refuses to run: the reason (all gaps joined). `None` = the command runs.
    pub refused: Option<String>,
    /// Protections that will be active.
    pub enforced: Vec<String>,
    /// Protections the policy asks for that this guest cannot enforce (with `best_effort` they are reported and the
    /// command runs; without it any non-informational gap in `blocking` refuses the run).
    pub not_enforced: Vec<String>,
    /// The subset of `not_enforced` that makes strict mode refuse.
    pub blocking: Vec<String>,
}

/// What a guest whose kernel offers Landlock `abi` (0 = none) would do with `p`.
pub fn preflight(p: &ExecPolicy, abi: u32) -> Preflight {
    let mut enforced = Vec::new();
    let mut blocking: Vec<String> = Vec::new();
    let mut info: Vec<String> = Vec::new();
    if abi == 0 {
        blocking.push("filesystem confinement (Landlock is unavailable)".into());
    } else {
        enforced.push("filesystem read/write/execute rules".into());
        if p.exec.is_some() {
            enforced.push("executable allow-list (`exec`)".into());
        }
        if abi < 3 {
            blocking.push(format!(
                "filesystem truncate/refer control (needs Landlock ABI >= 3, have {abi})"
            ));
        }
        if abi < 5 {
            info.push(format!(
                "filesystem device ioctl control (needs Landlock ABI >= 5, have {abi})"
            ));
        }
    }
    for (name, rule) in [("connect", &p.tcp_connect), ("bind", &p.tcp_bind)] {
        if *rule == ExecTcpRule::Any {
            continue;
        }
        if abi >= 4 {
            enforced.push(format!("TCP {name} port rules"));
        } else {
            blocking.push(format!(
                "TCP {name} port rules (needs Landlock ABI >= 4, have {abi})"
            ));
        }
    }
    if p.scope_ipc {
        if abi >= 6 {
            enforced.push("abstract-unix-socket and signal scoping".into());
        } else {
            blocking.push(format!(
                "abstract-unix-socket and signal scoping (needs Landlock ABI >= 6, have {abi})"
            ));
        }
    }
    if p.seccomp.is_some() {
        enforced.push("seccomp syscall denylist".into());
    }
    if p.run_as.is_some() {
        enforced.push("uid/gid drop".into());
    }
    if p.isolation != ExecIsolation::Off {
        enforced.push(match p.isolation {
            ExecIsolation::Strict => {
                "namespace isolation (required: refuses without user namespaces)".into()
            }
            _ => "namespace isolation (when the guest kernel allows it)".into(),
        });
    }
    let refused = (!blocking.is_empty() && !p.best_effort).then(|| blocking.join("; "));
    let mut not_enforced = blocking.clone();
    not_enforced.extend(info);
    Preflight {
        abi,
        refused,
        enforced,
        not_enforced,
        blocking,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub field: String,
    pub message: String,
}

fn f(severity: Severity, field: &str, message: impl Into<String>) -> Finding {
    Finding {
        severity,
        field: field.to_owned(),
        message: message.into(),
    }
}

fn norm(p: &str) -> &str {
    if p.len() > 1 {
        p.trim_end_matches('/')
    } else {
        p
    }
}

/// `entry` grants access to `target` (same path, or a parent directory of it).
fn covers(entry: &str, target: &str) -> bool {
    let e = norm(entry);
    let t = norm(target);
    e == "/" || t == e || t.strip_prefix(e).is_some_and(|rest| rest.starts_with('/'))
}

/// Field names a raw JSON policy may carry. `max_abi` is a procbox field the guest agent drops.
pub fn unknown_fields(raw: &Value) -> Vec<String> {
    let Some(obj) = raw.as_object() else {
        return Vec::new();
    };
    let known: Vec<String> = match serde_json::to_value(ExecPolicy::default()) {
        Ok(Value::Object(m)) => m.keys().cloned().collect(),
        _ => Vec::new(),
    };
    obj.keys().filter(|k| !known.contains(k)).cloned().collect()
}

/// Static lint: mistakes and weakenings that do not depend on the guest kernel. `raw` (the JSON the policy was parsed
/// from) enables the unknown-field check, because the parser ignores unknown fields.
pub fn lint(p: &ExecPolicy, raw: Option<&Value>) -> Vec<Finding> {
    use Severity::*;
    let mut out = Vec::new();
    if let Some(raw) = raw {
        for k in unknown_fields(raw) {
            let msg = if k == "max_abi" {
                "ignored: the guest agent always uses the kernel's real Landlock ABI".to_owned()
            } else {
                "unknown field, ignored: a misspelling here silently drops the setting (an omitted field takes the safe default, but an intended allowance is lost)".to_owned()
            };
            out.push(f(Warning, &k, msg));
        }
    }
    let path_lists: [(&str, &Vec<String>); 2] = [("read", &p.read), ("write", &p.write)];
    let exec_list = p.exec.as_ref();
    let all_lists = path_lists
        .iter()
        .map(|(n, l)| (*n, *l))
        .chain(exec_list.map(|l| ("exec", l)));
    for (name, list) in all_lists {
        let mut seen: Vec<&str> = Vec::new();
        for (i, e) in list.iter().enumerate() {
            let field = format!("{name}[{i}]");
            if e.trim().is_empty() {
                out.push(f(Error, &field, "empty path"));
                continue;
            }
            if !e.starts_with('/') {
                out.push(f(Warning, &field, format!("{e:?} is relative: it resolves against the guest agent's working directory, not yours")));
            }
            if e.split('/').any(|c| c == "..") {
                out.push(f(
                    Warning,
                    &field,
                    format!("{e:?} contains `..`; use the resolved path"),
                ));
            }
            if seen.contains(&norm(e)) {
                out.push(f(Warning, &field, format!("{e:?} is listed twice")));
            }
            seen.push(norm(e));
        }
    }
    // Can the command start? It runs as `/bin/sh -c`.
    let runnable: Vec<&String> = match exec_list {
        Some(x) => x.iter().collect(),
        None => p.read.iter().chain(p.write.iter()).collect(),
    };
    let reach = |t: &str| runnable.iter().any(|e| covers(e, t));
    let src = if exec_list.is_some() {
        "exec"
    } else {
        "read/write"
    };
    if !reach("/bin/sh") && !reach("/usr/bin/sh") {
        out.push(f(
            Error,
            if exec_list.is_some() { "exec" } else { "read" },
            format!("no {src} entry covers /bin/sh: the command runs through `/bin/sh -c`, so the shell cannot start. Add /bin and /usr"),
        ));
    }
    if !["/lib64", "/lib", "/usr/lib", "/usr/lib64"]
        .iter()
        .any(|l| reach(l))
    {
        out.push(f(
            Warning,
            if exec_list.is_some() { "exec" } else { "read" },
            format!("no {src} entry covers a library directory (/lib64, /lib, /usr/lib): dynamically linked programs, including most shells, cannot load their loader"),
        ));
    }
    if exec_list.is_some_and(Vec::is_empty) {
        out.push(f(
            Warning,
            "exec",
            "an empty list means nothing may run, not even the shell",
        ));
    }
    if exec_list.is_none() && !p.write.is_empty() {
        out.push(f(Info, "exec", "paths in `write` are also executable: a program the command writes there can run. Set `exec` to an allow-list to prevent it"));
    }
    for (i, w) in p.write.iter().enumerate() {
        for sys in [
            "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/proc", "/sys", "/dev",
            "/root",
        ] {
            if covers(w, sys) {
                out.push(f(
                    Warning,
                    &format!("write[{i}]"),
                    format!(
                        "{w:?} makes {sys} writable: the command can modify the system it runs on"
                    ),
                ));
                break;
            }
        }
    }
    for (name, rule) in [("tcp_connect", &p.tcp_connect), ("tcp_bind", &p.tcp_bind)] {
        match rule {
            ExecTcpRule::Any => out.push(f(Warning, name, "\"any\" leaves TCP unrestricted")),
            ExecTcpRule::Ports(ports) if ports.is_empty() => {
                out.push(f(Info, name, "an empty port list is the same as \"deny\""))
            }
            ExecTcpRule::Ports(ports) if ports.contains(&0) => {
                out.push(f(Error, name, "port 0 is not a valid port"))
            }
            _ => {}
        }
    }
    if p.allow_udp {
        out.push(f(Warning, "allow_udp", "UDP sockets stay available (DNS, QUIC) even where TCP is restricted: data can leave over UDP"));
    }
    if p.allow_unix {
        out.push(f(Info, "allow_unix", "AF_UNIX sockets stay available: the command can talk to any socket it can reach on disk"));
    }
    if p.allow_namespaces {
        out.push(f(
            Warning,
            "allow_namespaces",
            "the command may create namespaces, a common step in kernel exploits",
        ));
    }
    match p.seccomp {
        None => out.push(f(Warning, "seccomp", "null turns the syscall denylist off")),
        Some(ExecSeccompMode::Kill) => out.push(f(Info, "seccomp", "\"kill\" ends the whole command on the first denied syscall; \"errno\" lets it handle the failure")),
        Some(ExecSeccompMode::Errno) => {}
    }
    if !p.scope_ipc {
        out.push(f(Info, "scope_ipc", "false: the command can signal and connect to abstract sockets of other processes in the guest"));
    }
    if p.best_effort {
        out.push(f(Warning, "best_effort", "fails open: on a guest kernel that cannot enforce a rule the command still runs, with that rule missing. See the per-ABI table for which"));
    }
    if let Some(r) = &p.run_as
        && (r.uid == 0 || r.gid == 0)
    {
        out.push(f(
            Error,
            "run_as",
            "uid or gid 0 is root, not a drop to an unprivileged user",
        ));
    }
    if p.timeout_secs == Some(0) || p.cpu_seconds == Some(0) {
        out.push(f(Error, "timeout_secs", "0 kills the command immediately"));
    }
    if p.max_output_bytes == Some(0) {
        out.push(f(Warning, "max_output_bytes", "0 captures no output"));
    }
    if p.max_memory.is_some_and(|m| m < 16 << 20) {
        out.push(f(Warning, "max_memory", "under 16 MiB of address space: most programs fail to start (RLIMIT_AS counts mappings, not resident memory)"));
    }
    for (i, (k, _)) in p.env.iter().enumerate() {
        if k.is_empty() || k.contains('=') {
            out.push(f(
                Error,
                &format!("env[{i}]"),
                format!("{k:?} is not a valid variable name"),
            ));
        }
    }
    if p.timeout_secs.is_none() {
        out.push(f(
            Info,
            "timeout_secs",
            "no policy timeout: the request's own timeout applies",
        ));
    }
    if p.isolation == ExecIsolation::Strict {
        out.push(f(Info, "isolation", "\"strict\" refuses to run on a guest without unprivileged user namespaces; not checked here"));
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.field.cmp(&b.field))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecRunAs;

    fn good() -> ExecPolicy {
        ExecPolicy {
            read: vec!["/bin".into(), "/usr".into(), "/lib".into(), "/lib64".into()],
            write: vec!["/tmp/work".into()],
            tcp_connect: ExecTcpRule::Ports(vec![443]),
            timeout_secs: Some(30),
            ..ExecPolicy::default()
        }
    }
    fn has(l: &[Finding], sev: Severity, field: &str, needle: &str) -> bool {
        l.iter()
            .any(|f| f.severity == sev && f.field == field && f.message.contains(needle))
    }

    #[test]
    fn abi_thresholds_match_the_documented_requirements() {
        let p = good();
        let r = preflight(&p, 0);
        assert!(
            r.refused
                .as_deref()
                .unwrap()
                .contains("Landlock is unavailable")
        );
        assert!(
            preflight(&p, 2)
                .refused
                .as_deref()
                .unwrap()
                .contains("truncate/refer")
        );
        // ABI 3: filesystem ok but tcp port rules and scope_ipc not
        let r3 = preflight(&p, 3);
        let why = r3.refused.unwrap();
        assert!(
            why.contains("TCP connect") && why.contains("scoping"),
            "{why}"
        );
        let r4 = preflight(&p, 4);
        assert!(r4.refused.as_deref().unwrap().contains("scoping"));
        assert!(r4.enforced.iter().any(|e| e.contains("TCP connect")));
        let r6 = preflight(&p, 6);
        assert_eq!(r6.refused, None);
        assert!(r6.enforced.iter().any(|e| e.contains("scoping")));
        // ABI 5 and 6 still report the missing device-ioctl control only below 5
        assert!(
            preflight(&p, 4)
                .not_enforced
                .iter()
                .any(|n| n.contains("ioctl"))
        );
        assert!(
            !preflight(&p, 5)
                .not_enforced
                .iter()
                .any(|n| n.contains("ioctl"))
        );
    }

    #[test]
    fn scope_ipc_false_makes_abi_4_enough() {
        let mut p = good();
        p.scope_ipc = false;
        assert_eq!(preflight(&p, 4).refused, None);
        // the default tcp_bind is "deny", a port rule: it needs ABI 4 too
        p.tcp_connect = ExecTcpRule::Any;
        assert!(
            preflight(&p, 3)
                .refused
                .as_deref()
                .unwrap()
                .contains("TCP bind")
        );
        p.tcp_bind = ExecTcpRule::Any;
        assert_eq!(preflight(&p, 3).refused, None);
    }

    #[test]
    fn best_effort_runs_and_lists_the_gaps() {
        let mut p = good();
        p.best_effort = true;
        let r = preflight(&p, 3);
        assert_eq!(r.refused, None);
        assert_eq!(r.blocking.len(), 3);
        assert!(r.not_enforced.len() >= 2);
        assert!(r.enforced.iter().any(|e| e.contains("filesystem")));
        // abi 0 still lists filesystem as not enforced under best_effort
        assert!(
            preflight(&p, 0)
                .not_enforced
                .iter()
                .any(|n| n.contains("filesystem"))
        );
    }

    #[test]
    fn default_policy_needs_abi_6() {
        let p = ExecPolicy::default();
        assert!(preflight(&p, 5).refused.is_some());
        assert!(preflight(&p, 6).refused.is_none());
    }

    #[test]
    fn a_good_policy_has_no_errors_or_warnings() {
        let l = lint(&good(), None);
        assert!(l.iter().all(|f| f.severity == Severity::Info), "{l:?}");
    }

    #[test]
    fn shell_must_be_reachable() {
        let mut p = good();
        p.read = vec!["/etc/ssl".into()];
        let l = lint(&p, None);
        assert!(has(&l, Severity::Error, "read", "/bin/sh"));
        assert!(has(&l, Severity::Warning, "read", "library directory"));
        // merged-usr: /usr alone covers /usr/bin/sh
        p.read = vec!["/usr".into()];
        assert!(!has(&lint(&p, None), Severity::Error, "read", "/bin/sh"));
        // root grant covers everything
        p.read = vec!["/".into()];
        assert!(
            !lint(&p, None)
                .iter()
                .any(|f| f.severity >= Severity::Warning && f.field == "read")
        );
        // sibling prefix does not count: /binary is not /bin
        p.read = vec!["/binary".into()];
        assert!(has(&lint(&p, None), Severity::Error, "read", "/bin/sh"));
    }

    #[test]
    fn exec_allow_list_is_what_must_cover_the_shell() {
        let mut p = good();
        p.exec = Some(vec!["/opt/tool".into()]);
        let l = lint(&p, None);
        assert!(has(&l, Severity::Error, "exec", "/bin/sh"));
        p.exec = Some(vec!["/bin".into(), "/lib64".into()]);
        let l = lint(&p, None);
        assert!(
            !l.iter()
                .any(|f| f.severity >= Severity::Warning && f.field == "exec"),
            "{l:?}"
        );
        p.exec = Some(vec![]);
        assert!(has(
            &lint(&p, None),
            Severity::Warning,
            "exec",
            "nothing may run"
        ));
    }

    #[test]
    fn writable_exec_hint_only_without_allow_list() {
        let mut p = good();
        assert!(has(
            &lint(&p, None),
            Severity::Info,
            "exec",
            "also executable"
        ));
        p.exec = Some(vec!["/bin".into(), "/lib64".into()]);
        assert!(!has(
            &lint(&p, None),
            Severity::Info,
            "exec",
            "also executable"
        ));
    }

    #[test]
    fn system_dirs_must_not_be_writable() {
        let mut p = good();
        p.write = vec!["/".into()];
        assert!(has(
            &lint(&p, None),
            Severity::Warning,
            "write[0]",
            "/etc writable"
        ));
        p.write = vec!["/usr/local".into()];
        assert!(
            !lint(&p, None)
                .iter()
                .any(|f| f.field == "write[0]" && f.severity >= Severity::Warning)
        );
        p.write = vec!["/etc/".into()];
        assert!(has(
            &lint(&p, None),
            Severity::Warning,
            "write[0]",
            "/etc writable"
        ));
    }

    #[test]
    fn path_hygiene() {
        let mut p = good();
        p.read.push("rel/dir".into());
        p.read.push("/usr/".into());
        p.read.push("".into());
        p.write.push("/tmp/../etc".into());
        let l = lint(&p, None);
        assert!(has(&l, Severity::Warning, "read[4]", "relative"));
        assert!(has(&l, Severity::Warning, "read[5]", "listed twice"));
        assert!(has(&l, Severity::Error, "read[6]", "empty"));
        assert!(has(&l, Severity::Warning, "write[1]", ".."));
    }

    #[test]
    fn network_and_misc_weakenings() {
        let mut p = good();
        p.tcp_connect = ExecTcpRule::Any;
        p.tcp_bind = ExecTcpRule::Ports(vec![0]);
        p.allow_udp = true;
        p.allow_namespaces = true;
        p.seccomp = None;
        p.best_effort = true;
        p.run_as = Some(ExecRunAs { uid: 0, gid: 1 });
        p.timeout_secs = Some(0);
        p.env = vec![("A=B".into(), "x".into())];
        p.max_memory = Some(1024);
        let l = lint(&p, None);
        for (sev, field, needle) in [
            (Severity::Warning, "tcp_connect", "unrestricted"),
            (Severity::Error, "tcp_bind", "port 0"),
            (Severity::Warning, "allow_udp", "UDP"),
            (Severity::Warning, "allow_namespaces", "namespaces"),
            (Severity::Warning, "seccomp", "off"),
            (Severity::Warning, "best_effort", "fails open"),
            (Severity::Error, "run_as", "root"),
            (Severity::Error, "timeout_secs", "immediately"),
            (Severity::Error, "env[0]", "valid variable"),
            (Severity::Warning, "max_memory", "16 MiB"),
        ] {
            assert!(has(&l, sev, field, needle), "missing {field}: {l:?}");
        }
        assert_eq!(l[0].severity, Severity::Error, "errors sort first");
    }

    #[test]
    fn unknown_fields_are_found_in_raw_json() {
        let raw: Value =
            serde_json::from_str(r#"{"read":["/usr"],"tcp_connects":{"ports":[443]},"max_abi":3}"#)
                .unwrap();
        assert_eq!(
            unknown_fields(&raw),
            vec!["max_abi".to_string(), "tcp_connects".to_string()]
        );
        let p: ExecPolicy = serde_json::from_value(raw.clone()).unwrap();
        let l = lint(&p, Some(&raw));
        assert!(has(&l, Severity::Warning, "tcp_connects", "misspelling"));
        assert!(has(
            &l,
            Severity::Warning,
            "max_abi",
            "always uses the kernel"
        ));
        assert!(unknown_fields(&Value::Null).is_empty());
        // every real field is known
        let full = serde_json::to_value(ExecPolicy::default()).unwrap();
        assert!(unknown_fields(&full).is_empty());
    }

    #[test]
    fn covers_semantics() {
        assert!(covers("/usr", "/usr/bin/sh"));
        assert!(covers("/usr/", "/usr/bin/sh"));
        assert!(covers("/", "/anything"));
        assert!(covers("/bin/sh", "/bin/sh"));
        assert!(!covers("/bin/sh", "/bin"));
        assert!(!covers("/us", "/usr"));
    }
}
