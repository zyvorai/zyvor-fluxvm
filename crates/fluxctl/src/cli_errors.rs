// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl explain` codes for the command-line conveniences: `config get|set`, `upgrade`, `templates` and
//! `completion`. Same shape as `supply_errors`; `explain` looks here after the other tables.

use crate::mac_errors::MacCode;

pub const CODES: &[MacCode] = &[
    MacCode {
        code: "USAGE_BAD_QUERY",
        status: Some(400),
        summary: "`fluxctl usage` (or the Usage page) was asked for something it cannot group or filter by.",
        causes: &[
            "`--by` was not `vm`, `day` or `label:KEY` (a bare `label` needs the label name)",
            "`--since` was not a length like `7d`, `24h`, `2w` or a date like `2026-11-01`",
            "the label filter is not written like `env=dev,team`",
        ],
        fixes: &[
            "group by a label: `fluxctl usage --by label:team`",
            "go back a week: `fluxctl usage --since 7d`",
            "see which labels exist: `fluxctl ps --json`",
        ],
        aliases: &["usage-bad-query"],
        signatures: &[],
    },
    MacCode {
        code: "USAGE_UNAVAILABLE",
        status: None,
        summary: "The usage numbers could not be read from the daemon.",
        causes: &[
            "the FluxVM daemon is not running or not reachable at the address in use",
            "the daemon is older than this fluxctl and has no usage report",
            "a token is needed (`--server-token`)",
        ],
        fixes: &[
            "check the daemon: `fluxctl doctor`",
            "update the daemon to the same version as fluxctl: `fluxctl upgrade`",
        ],
        aliases: &["usage-unavailable"],
        signatures: &[],
    },
    MacCode {
        code: "VM_PROTECTED",
        status: Some(409),
        summary: "A VM was not deleted because it is protected.",
        causes: &["the VM has the label `fluxvm.io/protected=true` (set by `fluxctl protect`)"],
        fixes: &[
            "remove the protection: `fluxctl unprotect NAME`, then delete it",
            "see what is protected: `fluxctl protect --list`",
        ],
        aliases: &["vm-protected"],
        signatures: &[],
    },
    MacCode {
        code: "PROTECT_NO_TARGET",
        status: None,
        summary: "`fluxctl protect` or `unprotect` had no VM to act on.",
        causes: &[
            "no VM name or id was given and `-l` was not used",
            "the label selector given with `-l` matches no VM",
        ],
        fixes: &[
            "name the VM: `fluxctl protect web`",
            "or use a selector: `fluxctl protect -l env=prod` (check with `fluxctl ps --label env=prod`)",
        ],
        aliases: &["protect-no-target"],
        signatures: &[],
    },
    MacCode {
        code: "DAEMON_REQUIRED",
        status: None,
        summary: "`fluxctl protect`, `unprotect`, `df`, `prune` or `top` could not reach the FluxVM daemon.",
        causes: &[
            "the daemon is not running or not reachable at the address in use",
            "the daemon is older than this fluxctl and lacks the route",
            "a token is needed (`--server-token`)",
        ],
        fixes: &[
            "check the daemon: `fluxctl doctor`",
            "start it: `fluxctl serve` (or `fluxctl service install`)",
        ],
        aliases: &["daemon-required"],
        signatures: &[],
    },
    MacCode {
        code: "PRUNE_BAD_AGE",
        status: None,
        summary: "`fluxctl prune --older-than` was not a length of time.",
        causes: &["the value was not like `30m`, `12h`, `7d`, `2w` or `0s`"],
        fixes: &[
            "preview everything stopped: `fluxctl prune --older-than 0s`",
            "keep the last week: `fluxctl prune --older-than 7d`",
        ],
        aliases: &["prune-bad-age"],
        signatures: &[],
    },
    MacCode {
        code: "TOP_BAD_SORT",
        status: None,
        summary: "`fluxctl top` was given a sort key or interval it cannot use.",
        causes: &[
            "`--sort` was not `cpu`, `mem`, `disk`, `net` or `name`",
            "`--interval` was outside 0.05 to 3600 seconds",
        ],
        fixes: &[
            "sort by memory: `fluxctl top --sort mem`",
            "refresh every 5 seconds: `fluxctl top --interval 5`",
        ],
        aliases: &["top-bad-sort"],
        signatures: &[],
    },
    MacCode {
        code: "CONFIG_UNKNOWN_KEY",
        status: None,
        summary: "That setting does not exist, so `fluxctl config set` did not write it (the daemon would silently ignore it).",
        causes: &["the key is misspelled or sits under the wrong section"],
        fixes: &[
            "list the settings that are in use: `fluxctl config ls`",
            "check the spelling against the suggestion in the message, or the docs for the section",
        ],
        aliases: &["config-unknown"],
        signatures: &[],
    },
    MacCode {
        code: "CONFIG_BAD_VALUE",
        status: None,
        summary: "The value cannot work for that setting, so nothing was written.",
        causes: &[
            "a number was expected and text was given, or the other way round",
            "the new value breaks a rule the config check enforces",
        ],
        fixes: &[
            "write the value the way TOML does: `true`, `42`, `\"text\"`, `[\"a\", \"b\"]`",
            "see the rules: `fluxctl config check`",
        ],
        aliases: &["config-bad-value"],
        signatures: &[],
    },
    MacCode {
        code: "CONFIG_NOT_WRITABLE",
        status: None,
        summary: "The config file cannot be written by this user.",
        causes: &[
            "the file is root-owned (a system install at /etc/fluxvm)",
            "the folder does not exist and cannot be made",
        ],
        fixes: &[
            "use your own file: `fluxctl --config ~/.config/fluxvm/config.toml config set KEY VALUE`",
            "or run the command with the privileges that own the file",
        ],
        aliases: &["config-not-writable"],
        signatures: &[],
    },
    MacCode {
        code: "UPGRADE_NO_CHANNEL",
        status: None,
        summary: "`fluxctl upgrade` could not find out what the latest release is.",
        causes: &[
            "no network, or the release server is not reachable from here",
            "the release URL is wrong or private (the default is the public GitHub releases page of FluxVM)",
        ],
        fixes: &[
            "check the network, then run `fluxctl upgrade --check` again",
            "to use a mirror: `fluxctl config set cli.upgrade_url https://your.mirror/releases` (or `--url`, or FLUXVM_RELEASE_BASE)",
        ],
        aliases: &["upgrade-no-channel"],
        signatures: &[],
    },
    MacCode {
        code: "UPGRADE_UNVERIFIED",
        status: None,
        summary: "The download has no checksum to check it against, so it was not installed. `fluxctl upgrade` never replaces the binary without one.",
        causes: &[
            "the release has no `.sha256` file for your platform",
            "the checksum file does not hold a SHA-256",
        ],
        fixes: &[
            "wait for the release to be complete, or pick another with `--to VERSION`",
            "install from the signed channel instead: scripts/install.sh --mirror URL --pubkey FILE",
        ],
        aliases: &["upgrade-unverified"],
        signatures: &[],
    },
    MacCode {
        code: "UPGRADE_CHECKSUM_MISMATCH",
        status: None,
        summary: "The downloaded file does not match its checksum. It was thrown away and this binary was not changed.",
        causes: &[
            "a damaged download",
            "the file or the checksum on the server was replaced",
        ],
        fixes: &[
            "run `fluxctl upgrade` again; if it keeps failing, do not use that release and tell whoever publishes it",
        ],
        aliases: &["upgrade-checksum"],
        signatures: &[],
    },
    MacCode {
        code: "UPGRADE_NOT_WRITABLE",
        status: None,
        summary: "This binary is installed where this user cannot replace it, or by a package manager that owns it.",
        causes: &["fluxctl lives under a system folder, or in a Homebrew Cellar"],
        fixes: &[
            "Homebrew: `brew upgrade fluxctl`",
            "otherwise run the installer: `curl -fsSL https://raw.githubusercontent.com/zyvorai/zyvor-fluxvm/main/scripts/install.sh | sh` (it installs under /opt/fluxvm or your prefix)",
        ],
        aliases: &["upgrade-not-writable"],
        signatures: &[],
    },
    MacCode {
        code: "TEMPLATE_UNKNOWN",
        status: None,
        summary: "There is no template with that name.",
        causes: &["a typo, or a template from a newer fluxctl"],
        fixes: &["see the names: `fluxctl templates ls`"],
        aliases: &["template-unknown"],
        signatures: &[],
    },
    MacCode {
        code: "TEMPLATE_NEEDS_DOCKER",
        status: None,
        summary: "This template is a Compose stack and `docker compose` is not available or did not start it.",
        causes: &[
            "the docker command is not installed",
            "no Docker engine is running (on a Mac: `fluxctl docker up`)",
        ],
        fixes: &[
            "on a Mac: `fluxctl docker up`, then `eval \"$(fluxctl docker env)\"`, then run the template again",
            "or write the file out with `fluxctl templates show NAME > compose.yaml` and start it yourself",
        ],
        aliases: &["template-needs-docker"],
        signatures: &[],
    },
    MacCode {
        code: "TEMPLATE_NEEDS_KUBECTL",
        status: None,
        summary: "This template is for a k3s cluster and kubectl could not apply it.",
        causes: &[
            "kubectl is not installed",
            "there is no FluxVM k3s cluster yet, or it is not running",
        ],
        fixes: &[
            "make one: `fluxctl kube up`",
            "with several clusters, name one: `fluxctl templates run NAME --cluster NAME`",
        ],
        aliases: &["template-needs-kubectl"],
        signatures: &[],
    },
    MacCode {
        code: "TEMPLATE_VM_BLOCKED",
        status: None,
        summary: "`fluxctl templates run` or `down` would not act on this VM environment; nothing was changed.",
        causes: &[
            "this host cannot run it (for example `amd64` needs Rosetta on a Mac, `kvm` needs nested virtualization)",
            "a VM with that name exists and this template did not make it",
            "not enough free disk or memory, or a published port is already in use",
            "another `fluxctl templates` command is working on the same VM",
        ],
        fixes: &[
            "read the [blocked] lines of the plan: `fluxctl templates run NAME --dry-run`",
            "use another VM name: `fluxctl templates run NAME --vm OTHER`",
        ],
        aliases: &["template-vm-blocked"],
        signatures: &[],
    },
    MacCode {
        code: "TEMPLATE_VM_NOT_READY",
        status: None,
        summary: "The VM of an environment is running but a readiness check failed. It is left as it is.",
        causes: &[
            "first-boot setup is still installing packages, or failed (usually no internet or a slow mirror)",
            "a check in your own template file does not pass in this image",
        ],
        fixes: &[
            "run the same command again: it only waits and checks (`--timeout SECONDS` waits longer)",
            "look in the guest: `fluxctl ssh NAME`, then `sudo tail -n 50 /var/log/cloud-init-output.log`",
            "start over, deleting its disk: `fluxctl templates run NAME --recreate`",
        ],
        aliases: &["template-vm-not-ready"],
        signatures: &[],
    },
    MacCode {
        code: "COMPLETION_SHELL_UNKNOWN",
        status: None,
        summary: "fluxctl could not tell which shell to install completion for.",
        causes: &[
            "$SHELL is not set, or is a shell without a completion script here (bash, zsh and fish are supported)",
        ],
        fixes: &["say which: `fluxctl completion install --shell zsh`"],
        aliases: &["completion-shell-unknown"],
        signatures: &[],
    },
];

fn norm(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace('-', "_")
}

pub fn find(query: &str) -> Vec<&'static MacCode> {
    let q = norm(query);
    if q.is_empty() {
        return vec![];
    }
    let exact: Vec<&MacCode> = CODES
        .iter()
        .filter(|c| c.code == q || c.aliases.iter().any(|a| norm(a) == q))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    if q.len() >= 4 {
        return CODES
            .iter()
            .filter(|c| c.code.contains(q.as_str()))
            .collect();
    }
    vec![]
}

/// An error that ends with the command that explains it.
pub fn coded(message: impl Into<String>, code: &str) -> anyhow::Error {
    debug_assert!(CODES.iter().any(|c| c.code == code), "unknown code {code}");
    anyhow::anyhow!("{}\n  more: fluxctl explain {code}", message.into())
}

pub fn render(found: &[&MacCode], json: bool) -> anyhow::Result<String> {
    if json {
        let one = |e: &MacCode| serde_json::json!({"code": e.code, "status": e.status, "summary": e.summary, "causes": e.causes, "fixes": e.fixes});
        let v = if found.len() == 1 {
            one(found[0])
        } else {
            serde_json::Value::Array(found.iter().map(|e| one(e)).collect())
        };
        return Ok(serde_json::to_string_pretty(&v)? + "\n");
    }
    let mut out = Vec::new();
    for e in found {
        let mut s = format!("{} (fluxctl)\n  {}\n\ncauses:\n", e.code, e.summary);
        for c in e.causes {
            s.push_str(&format!("  - {c}\n"));
        }
        s.push_str("\nwhat to do:\n");
        for f in e.fixes {
            s.push_str(&format!("  - {f}\n"));
        }
        out.push(s);
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_every_card_has_a_fix() {
        let mut seen = std::collections::HashSet::new();
        for c in CODES {
            assert!(seen.insert(c.code), "{}", c.code);
            assert!(!c.fixes.is_empty() && !c.causes.is_empty());
        }
    }

    #[test]
    fn lookup_accepts_aliases_and_a_coded_error_names_its_card() {
        assert_eq!(find("upgrade-unverified")[0].code, "UPGRADE_UNVERIFIED");
        assert!(find("zzzz").is_empty());
        assert!(
            coded("x", "TEMPLATE_UNKNOWN")
                .to_string()
                .contains("fluxctl explain TEMPLATE_UNKNOWN")
        );
    }
}
