// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl helper-agent vmnet|usb install | uninstall | status`: the two privileged helper LaunchAgents.
//!
//! * `vmnet` (`dev.zyvor.fluxvm.vmnetd`): the broker that owns `com.apple.vm.networking`. Without a Developer ID signature
//!   and a provisioning profile for it, VMs on a vmnet network fail with "Internal Network Error".
//! * `usb` (`dev.zyvor.fluxvm.usbd`): the app that holds `com.apple.developer.accessory-access.usb`.
//!
//! `install` wraps `macos/<helper>/build-install.sh` (from a source checkout) or the release package's copy, after a
//! preflight that says in plain words what will not work, instead of letting the failure surface later. `status` reports
//! loaded state plus the signature check. Never runs sudo by itself: the scripts do, and say so.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(clap::Subcommand)]
pub enum HelperAgentCommand {
    /// The vmnet broker (`dev.zyvor.fluxvm.vmnetd`).
    Vmnet {
        #[command(subcommand)]
        command: HelperVerb,
    },
    /// The USB access app (`dev.zyvor.fluxvm.usbd`).
    Usb {
        #[command(subcommand)]
        command: HelperVerb,
    },
}

#[derive(clap::Subcommand)]
pub enum HelperVerb {
    /// Preflight the signature, then run the helper's build-install.sh and load the LaunchAgent.
    Install {
        /// An unpacked release package (has `libexec/` and `share/launchd/`). Default: build from a source checkout.
        #[arg(long)]
        package_dir: Option<PathBuf>,
        /// Install even when the preflight says the helper cannot work (ad hoc signature).
        #[arg(long)]
        force: bool,
    },
    /// Unload and delete the LaunchAgent plist. Leaves the installed binary alone.
    Uninstall,
    /// Installed, loaded, pid, last exit code, and whether the signature can carry the entitlement.
    Status,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Helper {
    Vmnet,
    Usb,
}

impl Helper {
    pub fn label(self) -> &'static str {
        match self {
            Helper::Vmnet => "dev.zyvor.fluxvm.vmnetd",
            Helper::Usb => "dev.zyvor.fluxvm.usbd",
        }
    }
    fn dir(self) -> &'static str {
        match self {
            Helper::Vmnet => "vmnetd",
            Helper::Usb => "usbd",
        }
    }
    pub fn entitlement(self) -> &'static str {
        match self {
            Helper::Vmnet => "com.apple.vm.networking",
            Helper::Usb => "com.apple.developer.accessory-access.usb",
        }
    }
    /// The signed bundle or binary the LaunchAgent runs.
    fn target(self) -> &'static str {
        match self {
            Helper::Vmnet => "/usr/local/libexec/fluxvm-vmnetd",
            Helper::Usb => "/Applications/FluxVMUSBAccess.app",
        }
    }
    fn log(self, home: &Path) -> PathBuf {
        let name = match self {
            Helper::Vmnet => "fluxvm-vmnetd.err",
            Helper::Usb => "fluxvm-usbd.log",
        };
        home.join("Library/Logs").join(name)
    }
}

/// What `codesign` says about a helper, reduced to what decides whether the entitlement can work.
#[derive(Debug, PartialEq, Eq)]
pub struct SignatureVerdict {
    pub ok: bool,
    pub message: String,
}

/// `dvv` is `codesign -dvv` (stderr), `entitlements` is `codesign -d --entitlements - ` output.
pub fn judge_signature(helper: Helper, dvv: &str, entitlements: &str) -> SignatureVerdict {
    let ent = helper.entitlement();
    if dvv.contains("Signature=adhoc") || dvv.contains("flags=0x2(adhoc)") {
        return SignatureVerdict {
            ok: false,
            message: format!(
                "ad hoc signed: macOS kills or refuses a helper that claims the restricted {ent} entitlement without a Developer \
                 ID signature and provisioning profile (docs/macos-signing.md). Install the release package's signed copy: \
                 `fluxctl helper-agent {} install --package-dir <unpacked release>`",
                if helper == Helper::Vmnet { "vmnet" } else { "usb" }
            ),
        };
    }
    if !dvv.contains("Authority=Developer ID Application") {
        return SignatureVerdict {
            ok: false,
            message: "not signed with a Developer ID Application certificate".into(),
        };
    }
    if !entitlements.contains(ent) {
        return SignatureVerdict {
            ok: false,
            message: format!("signed, but without the {ent} entitlement"),
        };
    }
    SignatureVerdict {
        ok: true,
        message: format!("Developer ID signed with {ent}"),
    }
}

fn codesign_verdict(helper: Helper, path: &Path) -> SignatureVerdict {
    if !path.exists() {
        return SignatureVerdict {
            ok: false,
            message: format!("{} is not installed", path.display()),
        };
    }
    let run = |args: &[&str]| -> String {
        Command::new("codesign")
            .args(args)
            .arg(path)
            .output()
            .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
            .unwrap_or_default()
    };
    judge_signature(helper, &run(&["-dvv"]), &run(&["-d", "--entitlements", "-"]))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

fn plist(helper: Helper, home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{}.plist", helper.label()))
}

pub fn status(helper: Helper) -> Result<serde_json::Value> {
    let home = home()?;
    let mut v = crate::launch_agent::status_of(helper.label(), &plist(helper, &home), &helper.log(&home))?;
    let sig = codesign_verdict(helper, Path::new(helper.target()));
    v["label"] = helper.label().into();
    v["binary"] = helper.target().into();
    v["signature_ok"] = sig.ok.into();
    v["signature"] = sig.message.into();
    Ok(v)
}

pub fn uninstall(helper: Helper) -> Result<bool> {
    let home = home()?;
    let p = plist(helper, &home);
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{}", crate::launch_agent::domain(), helper.label())])
        .output();
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
    }
}

/// Finds `macos/<helper>/build-install.sh` in a source checkout (the dir of this binary's workspace, or the current dir).
fn find_script(helper: Helper, package_dir: Option<&Path>) -> Result<PathBuf> {
    let rel = format!("crates/fluxvm-apple/macos/{}/build-install.sh", helper.dir());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = package_dir {
        roots.push(p.join("share/helpers"));
        roots.push(p.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    for r in &roots {
        for cand in [r.join(&rel), r.join(helper.dir()).join("build-install.sh")] {
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    bail!(
        "cannot find {} — run this from a FluxVM source checkout, or pass --package-dir <unpacked release>",
        rel
    )
}

pub fn install(helper: Helper, package_dir: Option<&Path>, force: bool) -> Result<serde_json::Value> {
    if let Some(p) = package_dir {
        let app = p.join("libexec/FluxVMNetworking.app");
        if helper == Helper::Vmnet && app.exists() {
            let v = codesign_verdict(helper, &app);
            if !v.ok && !force {
                bail!("the package's {} cannot be used: {}", app.display(), v.message);
            }
        }
    } else if helper == Helper::Vmnet && !force {
        bail!(
            "building {} from source gives an ad hoc signature, and VMs then report \"Internal Network Error\" on vmnet \
             networks. Use the release package (`--package-dir`), or pass --force to install the ad hoc build anyway",
            helper.label()
        );
    }
    let script = find_script(helper, package_dir)?;
    let mut cmd = Command::new("bash");
    cmd.arg(&script);
    if let Some(p) = package_dir {
        cmd.env("FLUXVM_PACKAGE_DIR", p);
    }
    let st = cmd.status().with_context(|| format!("running {}", script.display()))?;
    if !st.success() {
        bail!("{} failed ({st})", script.display());
    }
    status(helper)
}

pub fn run(cmd: HelperAgentCommand) -> Result<serde_json::Value> {
    let (helper, verb) = match cmd {
        HelperAgentCommand::Vmnet { command } => (Helper::Vmnet, command),
        HelperAgentCommand::Usb { command } => (Helper::Usb, command),
    };
    Ok(match verb {
        HelperVerb::Install { package_dir, force } => install(helper, package_dir.as_deref(), force)?,
        HelperVerb::Uninstall => serde_json::json!({"ok": true, "removed": uninstall(helper)?}),
        HelperVerb::Status => status(helper)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVID: &str = "Authority=Developer ID Application: Zyvor (ABC123)\nTeamIdentifier=ABC123\n";

    #[test]
    fn an_ad_hoc_signature_is_refused_with_the_fix() {
        let v = judge_signature(Helper::Vmnet, "Signature=adhoc\n", "");
        assert!(!v.ok);
        assert!(v.message.contains("helper-agent vmnet install --package-dir"));
        let v = judge_signature(Helper::Usb, "flags=0x2(adhoc)", "");
        assert!(v.message.contains("helper-agent usb install"));
    }

    #[test]
    fn developer_id_without_the_entitlement_is_named() {
        let v = judge_signature(Helper::Vmnet, DEVID, "<key>com.apple.security.virtualization</key>");
        assert!(!v.ok);
        assert!(v.message.contains("com.apple.vm.networking"));
    }

    #[test]
    fn developer_id_with_the_entitlement_passes() {
        let v = judge_signature(Helper::Vmnet, DEVID, "<key>com.apple.vm.networking</key>");
        assert!(v.ok);
        let v = judge_signature(Helper::Usb, DEVID, "<key>com.apple.developer.accessory-access.usb</key>");
        assert!(v.ok);
    }

    #[test]
    fn an_unsigned_or_other_signer_is_not_ok() {
        assert!(!judge_signature(Helper::Usb, "Authority=Apple Development: x", "").ok);
    }

    #[test]
    fn labels_match_the_shipped_plists() {
        assert_eq!(Helper::Vmnet.label(), "dev.zyvor.fluxvm.vmnetd");
        assert_eq!(Helper::Usb.label(), "dev.zyvor.fluxvm.usbd");
    }

    #[test]
    fn a_missing_script_says_where_it_looked() {
        let d = std::env::temp_dir().join("fluxvm-helper-agent-empty");
        std::fs::create_dir_all(&d).unwrap();
        let e = find_script(Helper::Usb, Some(&d)).unwrap_err().to_string();
        assert!(e.contains("usbd/build-install.sh"), "{e}");
    }
}
