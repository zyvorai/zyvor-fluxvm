// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac system info` and `mac system version`: the `docker info` / `docker version` of `fluxctl mac`.
//!
//! Nothing here measures anything new. It gathers what `mac doctor` (the checks), `mac system df` (disk) and `mac ps` (instances)
//! already read, asks the daemon for its version (`GET /v1/openapi.json`, `info.version`), and lays it out once. It works with
//! no daemon: that line says how to connect, and the rest is still shown. The pure parts ([`document`], [`render`],
//! [`version_document`], [`render_version`]) take their inputs as values and are unit tested; the probes are not run in tests.
//!
//! `--json` documents carry `schema_version` 1 and are pinned by `mac_json_golden/system-info.json` and `system-version.json`.

use crate::mac_doctor::{self, Facts};
use crate::mac_image_ops::{self as ops, Df};
use crate::mac_inspect::human_bytes;
use crate::vz_doctor::{Check, Status};
use anyhow::Result;
use serde_json::{Value, json};

pub const SCHEMA_VERSION: u32 = 1;
pub const FLUXCTL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How fluxctl reached the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Daemon {
    /// No `--server`, `FLUXVM_URL` or context.
    NotConfigured,
    Unreachable {
        url: String,
    },
    Up {
        url: String,
        /// `None` when the daemon answered `/healthz` but not the version document.
        version: Option<String>,
    },
}

/// Everything `system info` shows, as values.
pub struct Inputs {
    pub daemon: Daemon,
    pub facts: Facts,
    /// `sw_vers -buildVersion`; `None` off macOS or when it could not be read.
    pub os_build: Option<String>,
    /// The daemon's `fluxctl mac` instances; `None` without a reachable daemon.
    pub instances: Option<Vec<Value>>,
    /// Image and instance disk use, or why it could not be read.
    pub df: Result<Df, String>,
}

const NO_DAEMON_FIX: &str = "pass --server http://127.0.0.1:7788 (and --server-token), set FLUXVM_URL, or `fluxctl context add`; start the daemon with `fluxvm serve`";

fn daemon_fix(d: &Daemon) -> Option<String> {
    match d {
        Daemon::NotConfigured => Some(format!("not connected: {NO_DAEMON_FIX}")),
        Daemon::Unreachable { url } => Some(format!(
            "not reachable at {url}: check that the daemon runs there (`fluxvm serve`), the address, and the token (--server-token)"
        )),
        Daemon::Up { .. } => None,
    }
}

fn active(instances: &[Value]) -> usize {
    instances
        .iter()
        .filter(|v| mac_doctor::is_active(v))
        .count()
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Warn => "warn",
        Status::Fail => "fail",
    }
}

fn counts(checks: &[Check]) -> (usize, usize, usize) {
    let n = |s: Status| checks.iter().filter(|c| c.status == s).count();
    (n(Status::Ok), n(Status::Warn), n(Status::Fail))
}

/// The `--json` document.
pub fn document(i: &Inputs) -> Value {
    let checks = mac_doctor::evaluate(&i.facts);
    let (ok, warn, fail) = counts(&checks);
    let (state, url, dversion) = match &i.daemon {
        Daemon::NotConfigured => ("not_configured", Value::Null, Value::Null),
        Daemon::Unreachable { url } => ("unreachable", json!(url), Value::Null),
        Daemon::Up { url, version } => ("up", json!(url), json!(version)),
    };
    let df = i.df.as_ref().ok();
    json!({
        "schema_version": SCHEMA_VERSION,
        "fluxctl": {"version": FLUXCTL_VERSION},
        "daemon": {"state": state, "url": url, "version": dversion, "next_step": daemon_fix(&i.daemon)},
        "host": {
            "macos": i.facts.is_macos,
            "os": std::env::consts::OS,
            "os_version": (!i.facts.os_version.is_empty()).then_some(&i.facts.os_version),
            "os_build": i.os_build,
            "arch": i.facts.arch,
        },
        "instances": {
            "known": i.instances.is_some(),
            "active": i.instances.as_deref().map(active),
            "total": i.instances.as_ref().map(Vec::len),
            "max": mac_doctor::MAX_SLOTS,
        },
        "images": {
            "count": df.map(|d| d.image_names),
            "in_use": df.map(|d| d.images_active),
            "bytes": df.map(|d| d.image_bytes),
        },
        "disk": {
            "instance_bytes": df.map(|d| d.instance_bytes),
            "reclaimable_bytes": df.map(|d| d.image_reclaimable + d.instance_reclaimable + d.orphan_bytes),
            "free_gb": i.facts.store_free_gb,
            "error": i.df.as_ref().err(),
        },
        "store": {"path": i.facts.store},
        "doctor": {
            "ok": ok, "warn": warn, "fail": fail,
            "problems": checks.iter().filter(|c| c.status != Status::Ok).map(|c| json!({
                "check": c.name, "status": status_word(c.status), "detail": c.detail, "fix": c.fix,
            })).collect::<Vec<_>>(),
        },
    })
}

/// The text of `system info`.
pub fn render(i: &Inputs) -> String {
    let checks = mac_doctor::evaluate(&i.facts);
    let (ok, warn, fail) = counts(&checks);
    let mut out = String::new();
    let mut row = |k: &str, v: String| out.push_str(&format!("{k:<15} {v}\n"));
    row("fluxctl:", FLUXCTL_VERSION.to_owned());
    row(
        "Daemon:",
        match &i.daemon {
            Daemon::Up { url, version } => format!(
                "{} at {url}",
                version.as_deref().unwrap_or("version unknown")
            ),
            d => daemon_fix(d).unwrap_or_default(),
        },
    );
    row(
        "Mac:",
        if i.facts.is_macos {
            format!(
                "macOS {}{}, {}",
                i.facts.os_version,
                i.os_build
                    .as_deref()
                    .map(|b| format!(" (build {b})"))
                    .unwrap_or_default(),
                i.facts.arch
            )
        } else {
            format!(
                "not a Mac ({} {}): macOS guests run only on a Mac with Apple silicon; point --server at one",
                std::env::consts::OS,
                i.facts.arch
            )
        },
    );
    row(
        "Instances:",
        match &i.instances {
            Some(v) => format!(
                "{} of {} macOS slots in use ({} instances in all)",
                active(v),
                mac_doctor::MAX_SLOTS,
                v.len()
            ),
            None => format!(
                "unknown without the daemon (at most {} run at once)",
                mac_doctor::MAX_SLOTS
            ),
        },
    );
    match &i.df {
        Ok(d) => {
            row(
                "Images:",
                format!(
                    "{} ({} in use), {}",
                    d.image_names,
                    d.images_active,
                    human_bytes(d.image_bytes)
                ),
            );
            row(
                "Instance disks:",
                if i.instances.is_some() {
                    human_bytes(d.instance_bytes)
                } else {
                    "unknown without the daemon".into()
                },
            );
            row(
                "Reclaimable:",
                format!(
                    "{} (see `fluxctl mac system df`, free it with `fluxctl mac system prune`)",
                    human_bytes(d.image_reclaimable + d.instance_reclaimable + d.orphan_bytes)
                ),
            );
        }
        Err(e) => row("Images:", format!("could not read the store: {e}")),
    }
    if let Some(g) = i.facts.store_free_gb {
        row("Free disk:", format!("{g:.0} GiB on the store's volume"));
    }
    row("Store:", i.facts.store.display().to_string());
    row(
        "Doctor:",
        format!("{ok} ok, {warn} warning{}, {fail} failed", plural(warn)),
    );
    for c in checks.iter().filter(|c| c.status != Status::Ok) {
        out.push_str(&format!(
            "  {} {}: {}\n    fix: {}\n",
            status_word(c.status),
            c.name,
            c.detail,
            c.fix.as_deref().unwrap_or("see `fluxctl mac doctor`")
        ));
    }
    if fail + warn > 0 {
        out.push_str("Details: `fluxctl mac doctor`\n");
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The `system version` document.
pub fn version_document(daemon: &Daemon) -> Value {
    let (state, url, v) = match daemon {
        Daemon::NotConfigured => ("not_configured", Value::Null, Value::Null),
        Daemon::Unreachable { url } => ("unreachable", json!(url), Value::Null),
        Daemon::Up { url, version } => ("up", json!(url), json!(version)),
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "client": {"name": "fluxctl", "version": FLUXCTL_VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "server": {"state": state, "url": url, "version": v, "next_step": daemon_fix(daemon)},
    })
}

pub fn render_version(daemon: &Daemon) -> String {
    let mut s = format!(
        "Client:\n Version:  {FLUXCTL_VERSION}\n OS/Arch:  {}/{}\n\nServer:\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match daemon {
        Daemon::Up { url, version } => s.push_str(&format!(
            " Version:  {}\n Address:  {url}\n",
            version.as_deref().unwrap_or("unknown")
        )),
        d => s.push_str(&format!(" {}\n", daemon_fix(d).unwrap_or_default())),
    }
    s
}

// ---- probes (not run in tests) ---------------------------------------------------------------------------------------------

/// The daemon's version from its public OpenAPI document.
async fn daemon_version(r: &crate::remote::Remote) -> Option<String> {
    let (st, body) = r.get_raw("/v1/openapi.json").await.ok()?;
    if !st.is_success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&body).ok()?;
    v["info"]["version"].as_str().map(str::to_owned)
}

async fn probe(r: Option<&crate::remote::Remote>) -> Daemon {
    let Some(r) = r else {
        return Daemon::NotConfigured;
    };
    let up = matches!(r.get_raw("/healthz").await, Ok((st, _)) if st.is_success());
    if !up {
        return Daemon::Unreachable {
            url: r.base().to_owned(),
        };
    }
    Daemon::Up {
        url: r.base().to_owned(),
        version: daemon_version(r).await,
    }
}

#[cfg(target_os = "macos")]
fn os_build() -> Option<String> {
    let o = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-buildVersion")
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn os_build() -> Option<String> {
    None
}

fn read_df(store: &std::path::Path, instances: &[Value]) -> Result<Df, String> {
    if !store.exists() {
        // Nothing pulled yet; do not create the directory just to count nothing.
        return Ok(Df::default());
    }
    let s = crate::mac_store::Store::open(store).map_err(|e| format!("{e:#}"))?;
    let refs: Vec<&Value> = instances.iter().collect();
    ops::compute_df(&s, &refs).map_err(|e| format!("{e:#}"))
}

/// `mac system info`.
pub async fn run_info(json_out: bool, remote: Option<crate::remote::Remote>) -> Result<()> {
    let store = crate::mac_store::default_root()?;
    let daemon = probe(remote.as_ref()).await;
    let (dfacts, instances) = match (&daemon, &remote) {
        (Daemon::Up { .. }, Some(r)) => (
            Some(mac_doctor::probe_daemon(r).await),
            crate::mac_more::instances(r).await.ok(),
        ),
        (_, Some(_)) => (Some(Default::default()), None),
        _ => (None, None),
    };
    let facts = mac_doctor::gather(&store, dfacts);
    let df = read_df(&store, instances.as_deref().unwrap_or(&[]));
    let inputs = Inputs {
        daemon,
        facts,
        os_build: os_build(),
        instances,
        df,
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&document(&inputs))?);
    } else {
        print!("{}", render(&inputs));
    }
    Ok(())
}

/// `mac system version`.
pub async fn run_version(json_out: bool, remote: Option<crate::remote::Remote>) -> Result<()> {
    let daemon = probe(remote.as_ref()).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&version_document(&daemon))?
        );
    } else {
        print!("{}", render_version(&daemon));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_doctor::{DaemonFacts, Session};

    fn mac_facts() -> Facts {
        Facts {
            is_macos: true,
            arch: "arm64".into(),
            os_version: "27.2".into(),
            store: "/Users/me/.fluxvm/mac-images".into(),
            store_writable: Some(Ok(())),
            store_free_gb: Some(400.0),
            store_dev: Some(16),
            instance_dev: Some(16),
            store_fs: Some("apfs".into()),
            session: Some(Session::Unlocked),
            daemon: Some(DaemonFacts {
                reachable: true,
                active_instances: Some(1),
                workspace: None,
            }),
            ssh_keys: vec!["~/.ssh/id_ed25519".into()],
            filevault_on: Some(false),
            ..Facts::default()
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            daemon: Daemon::Up {
                url: "http://127.0.0.1:7788".into(),
                version: Some("0.4.14".into()),
            },
            facts: mac_facts(),
            os_build: Some("25C56".into()),
            instances: Some(vec![
                json!({"name": "ci-1", "status": "running"}),
                json!({"name": "old", "status": "stopped"}),
            ]),
            df: Ok(Df {
                image_names: 3,
                images_active: 1,
                image_bytes: 64 * 1024 * 1024 * 1024,
                image_reclaimable: 20 * 1024 * 1024 * 1024,
                orphan_dirs: 0,
                orphan_bytes: 0,
                instances: 2,
                instances_running: 1,
                instance_bytes: 10 * 1024 * 1024 * 1024,
                instance_reclaimable: 5 * 1024 * 1024 * 1024,
            }),
        }
    }

    fn golden(name: &str, actual: &Value) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/mac_json_golden")
            .join(name);
        let text = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {name}; run with UPDATE_GOLDEN=1"));
        assert_eq!(
            text, want,
            "golden {name} differs: the --json schema changed (UPDATE_GOLDEN=1 to accept, and say so in CHANGELOG.md)"
        );
    }

    /// Host fields that depend on the machine running the test are pinned before the golden is compared.
    fn pinned(mut v: Value) -> Value {
        if let Some(h) = v["host"].as_object_mut() {
            h.insert("os".into(), json!("macos"));
        }
        // The fluxctl version changes with every release; the golden pins the shape, not the number.
        v["fluxctl"]["version"] = json!("0.0.0");
        v
    }

    #[test]
    fn info_json_matches_golden() {
        let doc = document(&inputs());
        assert_eq!(doc["instances"]["active"], 1);
        assert_eq!(doc["doctor"]["fail"], 0);
        golden("system-info.json", &pinned(doc));
    }

    #[test]
    fn version_json_matches_golden() {
        let mut d = version_document(&Daemon::Up {
            url: "http://127.0.0.1:7788".into(),
            version: Some("0.4.14".into()),
        });
        let c = d["client"].as_object_mut().unwrap();
        c.insert("os".into(), json!("macos"));
        c.insert("arch".into(), json!("aarch64"));
        c.insert("version".into(), json!("0.0.0"));
        golden("system-version.json", &d);
    }

    #[test]
    fn info_text_has_every_fact() {
        let t = render(&inputs());
        for want in [
            "fluxctl:",
            "0.4.14 at http://127.0.0.1:7788",
            "macOS 27.2 (build 25C56), arm64",
            "1 of 2 macOS slots in use (2 instances in all)",
            "3 (1 in use), 64.0 GiB",
            "10.0 GiB",
            "25.0 GiB",
            "/Users/me/.fluxvm/mac-images",
            "Doctor:",
        ] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
    }

    #[test]
    fn no_daemon_still_shows_the_rest_and_says_how_to_connect() {
        let mut i = inputs();
        i.daemon = Daemon::NotConfigured;
        i.instances = None;
        i.facts.daemon = None;
        let t = render(&i);
        assert!(t.contains("not connected: pass --server"), "{t}");
        assert!(t.contains("unknown without the daemon"), "{t}");
        assert!(t.contains("Store:"), "{t}");
        let d = document(&i);
        assert_eq!(d["daemon"]["state"], "not_configured");
        assert_eq!(d["instances"]["known"], false);
        assert_eq!(d["instances"]["active"], Value::Null);
    }

    #[test]
    fn unreachable_daemon_names_the_address_and_what_to_check() {
        let mut i = inputs();
        i.daemon = Daemon::Unreachable {
            url: "http://mac:7788".into(),
        };
        let t = render(&i);
        assert!(t.contains("not reachable at http://mac:7788") && t.contains("fluxvm serve"));
        let v = render_version(&i.daemon);
        assert!(v.contains("not reachable at http://mac:7788"), "{v}");
    }

    #[test]
    fn a_linux_host_is_said_plainly_and_doctor_failures_carry_fixes() {
        let mut i = inputs();
        i.facts = Facts {
            is_macos: false,
            arch: "x86_64".into(),
            store: "/home/x/.fluxvm/mac-images".into(),
            ..Facts::default()
        };
        let t = render(&i);
        assert!(t.contains("not a Mac"), "{t}");
        assert!(
            t.contains("1 failed") && t.contains("fix: run `fluxctl mac` on a Mac"),
            "{t}"
        );
        let d = document(&i);
        assert_eq!(d["doctor"]["fail"], 1);
        assert!(d["doctor"]["problems"][0]["fix"].is_string());
    }

    #[test]
    fn an_unreadable_store_is_reported_not_hidden() {
        let mut i = inputs();
        i.df = Err("permission denied".into());
        assert!(render(&i).contains("could not read the store: permission denied"));
        assert_eq!(document(&i)["disk"]["error"], "permission denied");
    }

    #[test]
    fn version_text_is_client_then_server() {
        let t = render_version(&Daemon::Up {
            url: "http://x".into(),
            version: None,
        });
        assert!(t.starts_with("Client:\n Version:  "));
        assert!(t.contains("Server:\n Version:  unknown"));
        assert!(render_version(&Daemon::NotConfigured).contains("not connected"));
    }

    #[test]
    fn read_df_of_a_missing_store_is_zero_and_creates_nothing() {
        let p = std::env::temp_dir().join(format!("fluxctl-sysinfo-{}", std::process::id()));
        let d = read_df(&p, &[]).unwrap();
        assert_eq!(d.image_names, 0);
        assert!(!p.exists());
    }
}
