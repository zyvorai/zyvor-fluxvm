// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl vz doctor`: a preflight for the `vz` backend. It never needs the daemon: facts are gathered from the OS and
//! the signed runner, then judged by the pure [`evaluate`], so every rule is unit tested without a Mac.

use fluxvm_apple::AppleHostCapabilities;
use serde_json::json;
use std::path::Path;
use std::process::Command;

/// Free space below which a first macOS guest (restore image plus disk) will not fit.
const MIN_FREE_GB_LINUX: f64 = 10.0;
const MIN_FREE_GB_MACOS: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// What to run or change; present for every non-ok check.
    pub fix: Option<String>,
}

fn check(
    name: &'static str,
    status: Status,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

/// Everything `evaluate` needs; `None` means the probe itself could not run.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub is_macos: bool,
    pub arch: String,
    pub os_version: String,
    /// Runner path, or why it was not found.
    pub runner: Option<Result<String, String>>,
    /// `codesign --verify` outcome for the runner: `Ok(())` signed, `Err(msg)` not.
    pub runner_signature: Option<Result<(), String>>,
    pub caps: Option<Result<AppleHostCapabilities, String>>,
    pub free_gb: Option<f64>,
    pub swiftc: Option<bool>,
    /// `Some(true)` when `/healthz` answered.
    pub daemon: Option<bool>,
}

fn major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

/// Judges the facts. Order is the order checks are shown.
pub fn evaluate(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    if !f.is_macos {
        out.push(check(
            "platform",
            Status::Fail,
            "the vz backend needs macOS on Apple silicon",
            Some("run this on a Mac; use the firecracker, cloud-hypervisor or qemu backends on Linux"),
        ));
        return out;
    }
    if f.arch == "arm64" || f.arch == "aarch64" {
        out.push(check(
            "apple silicon",
            Status::Ok,
            format!("arch {}", f.arch),
            None,
        ));
    } else {
        out.push(check(
            "apple silicon",
            Status::Fail,
            format!("arch {} (Intel Macs are not supported)", f.arch),
            Some("use an Apple silicon Mac"),
        ));
    }
    match major(&f.os_version) {
        Some(m) if m >= 14 => out.push(check("macOS version", Status::Ok, format!("macOS {}", f.os_version), None)),
        Some(_) => out.push(check(
            "macOS version",
            Status::Fail,
            format!("macOS {} is older than 14", f.os_version),
            Some("update in System Settings > General > Software Update (FluxVM needs macOS 14 or newer)"),
        )),
        None => out.push(check(
            "macOS version",
            Status::Warn,
            format!("could not read the version ({:?})", f.os_version),
            Some("run `sw_vers -productVersion`"),
        )),
    }
    match &f.runner {
        Some(Ok(p)) => out.push(check("vz runner", Status::Ok, p.clone(), None)),
        Some(Err(e)) => out.push(check(
            "vz runner",
            Status::Fail,
            e.clone(),
            Some(if f.swiftc == Some(false) {
                "install the toolchain first (`xcode-select --install`), then `cargo build -p fluxvm-apple`; or `brew install zyvorai/tap/fluxvm` for a prebuilt, signed runner"
            } else {
                "`cargo build -p fluxvm-apple` (builds and signs the runner), `brew install zyvorai/tap/fluxvm`, or set FLUXVM_VZ_RUNNER"
            }),
        )),
        None => {}
    }
    match &f.runner_signature {
        Some(Ok(())) => out.push(check("runner signature", Status::Ok, "codesign verifies", None)),
        Some(Err(e)) => out.push(check(
            "runner signature",
            Status::Fail,
            e.clone(),
            Some("re-sign with the virtualization entitlement: `cargo build -p fluxvm-apple` signs it, or reinstall the Homebrew package"),
        )),
        None => {}
    }
    match &f.caps {
        Some(Ok(c)) => {
            out.push(check(
                "Virtualization.framework",
                Status::Ok,
                format!(
                    "{} CPUs / {} GiB max per VM",
                    c.maximum_vm_cpus,
                    c.maximum_vm_memory_bytes / (1 << 30)
                ),
                None,
            ));
            match c.rosetta.as_str() {
                "installed" => out.push(check(
                    "Rosetta",
                    Status::Ok,
                    "installed (x86_64 Linux binaries run in guests)",
                    None,
                )),
                "not-installed" => out.push(check(
                    "Rosetta",
                    Status::Warn,
                    "not installed: x86_64 binaries and linux/amd64 images will not run in guests",
                    Some("softwareupdate --install-rosetta --agree-to-license"),
                )),
                "not-supported" => out.push(check(
                    "Rosetta",
                    Status::Warn,
                    "not supported on this Mac",
                    None,
                )),
                _ => out.push(check(
                    "Rosetta",
                    Status::Warn,
                    "the runner did not report it (older runner)",
                    Some("update the runner"),
                )),
            }
            if c.vm_networking_entitlement == Some(false) {
                out.push(check(
                    "vmnet entitlement",
                    Status::Warn,
                    "runner lacks com.apple.vm.networking: vmnet networks and bridged NICs are unavailable (apple.usernet and NAT still work)",
                    Some("use apple.usernet, or the Developer ID bundle FluxVMRunner.app from a release package"),
                ));
            }
        }
        Some(Err(e)) => out.push(check(
            "Virtualization.framework",
            Status::Fail,
            e.clone(),
            Some(
                "the runner started but host-capabilities failed; reinstall or rebuild the runner",
            ),
        )),
        None => {}
    }
    match f.free_gb {
        Some(g) if g >= MIN_FREE_GB_MACOS => out.push(check("free disk", Status::Ok, format!("{g:.0} GB free"), None)),
        Some(g) if g >= MIN_FREE_GB_LINUX => out.push(check(
            "free disk",
            Status::Warn,
            format!("{g:.0} GB free: enough for Linux guests, not for a macOS guest (needs about {MIN_FREE_GB_MACOS:.0} GB for the restore image and disk)"),
            Some("free space, or move the state dir to a larger volume"),
        )),
        Some(g) => out.push(check(
            "free disk",
            Status::Fail,
            format!("{g:.1} GB free: too little for guest disks (needs about {MIN_FREE_GB_LINUX:.0} GB)"),
            Some("free space, or move the state dir to a larger volume"),
        )),
        None => {}
    }
    out.push(check(
        "macOS guest limit",
        Status::Ok,
        "Apple allows at most 2 macOS VMs running per Mac; Linux guests are not limited by this",
        None,
    ));
    match f.daemon {
        Some(true) => out.push(check("daemon", Status::Ok, "answers /healthz", None)),
        Some(false) => out.push(check(
            "daemon",
            Status::Warn,
            "not reachable (fine if you have not started it yet)",
            Some("brew services start fluxvm, or `fluxctl service install` on a headless Mac"),
        )),
        None => {}
    }
    out
}

/// Worst status; drives the exit code.
pub fn worst(checks: &[Check]) -> Status {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Ok
    }
}

pub fn render_text(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        s.push_str(&format!(
            "[{:>4}] {:<26} {}\n",
            c.status.label(),
            c.name,
            c.detail
        ));
        if let Some(fix) = &c.fix {
            s.push_str(&format!("       fix: {fix}\n"));
        }
    }
    let (f, w) = (
        checks.iter().filter(|c| c.status == Status::Fail).count(),
        checks.iter().filter(|c| c.status == Status::Warn).count(),
    );
    s.push_str(&format!(
        "\n{f} failed, {w} warnings, {} checks\n",
        checks.len()
    ));
    s
}

pub fn render_json(checks: &[Check]) -> serde_json::Value {
    json!({
        "ok": worst(checks) != Status::Fail,
        "checks": checks.iter().map(|c| json!({
            "name": c.name,
            "status": c.status.label().to_lowercase(),
            "detail": c.detail,
            "fix": c.fix,
        })).collect::<Vec<_>>(),
    })
}

fn cmd_out(program: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(program).args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Free gigabytes on the volume holding `dir` (its nearest existing parent), via `df -k`.
fn free_gb(dir: &Path) -> Option<f64> {
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let out = cmd_out("/bin/df", &["-k", d.to_str()?])?;
    let kb: f64 = out
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(kb / 1024.0 / 1024.0)
}

/// Gathers facts from this machine. `healthz` is the daemon probe result, supplied by the async caller.
pub fn gather(state_dir: &Path, healthz: Option<bool>) -> Facts {
    let is_macos = cfg!(target_os = "macos");
    let mut f = Facts {
        is_macos,
        arch: std::env::consts::ARCH.to_owned(),
        daemon: healthz,
        ..Facts::default()
    };
    if !is_macos {
        return f;
    }
    f.os_version = cmd_out("/usr/bin/sw_vers", &["-productVersion"]).unwrap_or_default();
    f.free_gb = free_gb(state_dir);
    f.swiftc = Some(Path::new("/usr/bin/swiftc").exists());
    match fluxvm_apple::find_runner() {
        Ok(p) => {
            let sig = Command::new("/usr/bin/codesign")
                .args(["--verify", "--strict"])
                .arg(&p)
                .output()
                .map_err(|e| format!("could not run codesign: {e}"))
                .and_then(|o| {
                    if o.status.success() {
                        Ok(())
                    } else {
                        Err(String::from_utf8_lossy(&o.stderr).trim().to_owned())
                    }
                });
            f.runner = Some(Ok(p.display().to_string()));
            f.runner_signature = Some(sig);
            f.caps = Some(fluxvm_apple::host_capabilities().map_err(|e| e.to_string()));
        }
        Err(e) => f.runner = Some(Err(e.to_string())),
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Facts {
        Facts {
            is_macos: true,
            arch: "arm64".into(),
            os_version: "27.2".into(),
            runner: Some(Ok("/opt/homebrew/bin/fluxvm-vz-runner".into())),
            runner_signature: Some(Ok(())),
            caps: Some(Ok(AppleHostCapabilities {
                maximum_vm_cpus: 10,
                maximum_vm_memory_bytes: 64 << 30,
                rosetta: "installed".into(),
                vm_networking_entitlement: Some(true),
                ..Default::default()
            })),
            free_gb: Some(200.0),
            swiftc: Some(true),
            daemon: Some(true),
        }
    }

    fn find<'a>(c: &'a [Check], name: &str) -> &'a Check {
        c.iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}"))
    }

    #[test]
    fn healthy_mac_has_no_failures_or_warnings() {
        let c = evaluate(&good());
        assert_eq!(worst(&c), Status::Ok);
        assert!(c.iter().all(|c| c.fix.is_none()));
    }

    #[test]
    fn non_macos_fails_fast_with_one_check() {
        let c = evaluate(&Facts {
            arch: "x86_64".into(),
            ..Facts::default()
        });
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].status, Status::Fail);
    }

    #[test]
    fn intel_and_old_macos_fail() {
        let c = evaluate(&Facts {
            arch: "x86_64".into(),
            os_version: "13.6".into(),
            ..good()
        });
        assert_eq!(find(&c, "apple silicon").status, Status::Fail);
        assert_eq!(find(&c, "macOS version").status, Status::Fail);
    }

    #[test]
    fn macos_14_is_the_floor() {
        let c = evaluate(&Facts {
            os_version: "14.0".into(),
            ..good()
        });
        assert_eq!(find(&c, "macOS version").status, Status::Ok);
    }

    #[test]
    fn missing_runner_fix_depends_on_swift_toolchain() {
        let with = |swift| {
            let c = evaluate(&Facts {
                runner: Some(Err("not found".into())),
                swiftc: Some(swift),
                ..good()
            });
            find(&c, "vz runner").fix.clone().unwrap()
        };
        assert!(with(false).contains("xcode-select --install"));
        assert!(!with(true).contains("xcode-select"));
    }

    #[test]
    fn unsigned_runner_fails_with_a_fix() {
        let c = evaluate(&Facts {
            runner_signature: Some(Err("code object is not signed".into())),
            ..good()
        });
        let r = find(&c, "runner signature");
        assert_eq!(r.status, Status::Fail);
        assert!(r.fix.is_some());
    }

    #[test]
    fn rosetta_states() {
        let with = |s: &str| {
            let mut f = good();
            f.caps.as_mut().unwrap().as_mut().unwrap().rosetta = s.into();
            find(&evaluate(&f), "Rosetta").clone()
        };
        assert_eq!(with("installed").status, Status::Ok);
        let missing = with("not-installed");
        assert_eq!(missing.status, Status::Warn);
        assert!(missing.fix.unwrap().contains("install-rosetta"));
        assert_eq!(with("").status, Status::Warn);
    }

    #[test]
    fn vmnet_entitlement_warns_only_when_reported_false() {
        let with = |v| {
            let mut f = good();
            f.caps
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .vm_networking_entitlement = v;
            evaluate(&f).iter().any(|c| c.name == "vmnet entitlement")
        };
        assert!(with(Some(false)));
        assert!(!with(Some(true)));
        assert!(!with(None));
    }

    #[test]
    fn disk_thresholds() {
        let s = |g| {
            find(
                &evaluate(&Facts {
                    free_gb: Some(g),
                    ..good()
                }),
                "free disk",
            )
            .status
        };
        assert_eq!(s(100.0), Status::Ok);
        assert_eq!(s(30.0), Status::Warn);
        assert_eq!(s(5.0), Status::Fail);
    }

    #[test]
    fn stopped_daemon_is_only_a_warning() {
        let c = evaluate(&Facts {
            daemon: Some(false),
            ..good()
        });
        assert_eq!(find(&c, "daemon").status, Status::Warn);
        assert_eq!(worst(&c), Status::Warn);
    }

    #[test]
    fn json_reports_ok_false_only_on_failure() {
        let ok = render_json(&evaluate(&good()));
        assert_eq!(ok["ok"], true);
        let bad = render_json(&evaluate(&Facts {
            os_version: "12.0".into(),
            ..good()
        }));
        assert_eq!(bad["ok"], false);
        let statuses: Vec<_> = bad["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == "macOS version")
            .map(|c| c["status"].clone())
            .collect();
        assert_eq!(statuses, vec![serde_json::json!("fail")]);
    }

    #[test]
    fn text_output_lists_fixes_and_totals() {
        let t = render_text(&evaluate(&Facts {
            free_gb: Some(3.0),
            ..good()
        }));
        assert!(t.contains("[FAIL] free disk"));
        assert!(t.contains("fix: free space"));
        assert!(t.contains("1 failed, 0 warnings"));
    }
}
