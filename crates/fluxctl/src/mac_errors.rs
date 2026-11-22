// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Friendly explanations for the failures people hit most on `fluxctl mac`, `mac-fleet`, `kube` and `docker context`. `fluxctl explain`
//! looks here after the API and cluster-health tables. [`hint_for`] maps the text of a failure to the code that explains it.

/// One explained failure. Same shape as the other `explain` tables.
pub struct MacCode {
    pub code: &'static str,
    /// The HTTP status it rides on, when it has one.
    pub status: Option<u16>,
    pub summary: &'static str,
    pub causes: &'static [&'static str],
    pub fixes: &'static [&'static str],
    /// Other spellings `explain` accepts.
    pub aliases: &'static [&'static str],
    /// Lower-case text that identifies the failure in an error message.
    pub signatures: &'static [&'static str],
}

/// An error that ends with the command that explains it: `fluxctl explain CODE`.
pub fn coded(message: impl Into<String>, code: &str) -> anyhow::Error {
    debug_assert!(CODES.iter().any(|c| c.code == code), "unknown code {code}");
    anyhow::anyhow!("{}\n  more: fluxctl explain {code}", message.into())
}

pub const CODES: &[MacCode] = &[
    MacCode {
        code: "MAC_SLOT_LIMIT",
        status: Some(409),
        summary: "Both macOS VM slots on this Mac are taken (Apple's licence allows 2 macOS guests at once).",
        causes: &[
            "two macOS instances are already running on this Mac (`fluxctl mac ps`)",
            "an earlier `mac run` was left behind after a crash, so its slot is still held",
        ],
        fixes: &[
            "see what holds the slots: `fluxctl mac ps -a`",
            "remove one you do not need: `fluxctl mac rm NAME`",
            "on a fleet, let the scheduler pick a free Mac: `fluxctl mac-fleet run -f hosts.yaml --template NAME:TAG -- CMD` (add `--queue-timeout 600` to wait for a slot)",
        ],
        aliases: &["slot-limit", "slots", "409"],
        signatures: &[
            "slot limit",
            "2 macos",
            "two macos",
            "already running 2",
            "macos guests at once",
            "409 conflict",
        ],
    },
    MacCode {
        code: "MAC_SESSION_LOCKED",
        status: Some(503),
        summary: "The Mac has no unlocked login session, so macOS VMs cannot start or restore.",
        causes: &[
            "the screen is locked, or nobody is logged in at the console (Virtualization.framework needs a GUI session)",
            "the Mac is a headless mini that rebooted and is sitting at the login window",
        ],
        fixes: &[
            "unlock the Mac, or log in at the console once",
            "for a headless Mac enable automatic login (System Settings > Users & Groups) and keep it awake: `fluxctl mac host keepawake install`",
            "check the state: `fluxctl mac doctor` (the login session row) or `fluxctl mac host headless-check`",
        ],
        aliases: &["session-locked", "locked", "503"],
        signatures: &[
            "screen is locked",
            "session is locked",
            "no unlocked",
            "cgssessionscreenislocked",
            "503 service unavailable",
        ],
    },
    MacCode {
        code: "SSH_HOSTKEY_MISMATCH",
        status: None,
        summary: "ssh refused a Mac because its host key is not the one in known_hosts.",
        causes: &[
            "the Mac was reinstalled or its ssh keys were regenerated, so the key changed",
            "the address now belongs to a different machine (DHCP), or someone is intercepting the connection",
            "first contact with a Mac whose key was never recorded (fleet commands refuse unknown keys by default)",
        ],
        fixes: &[
            "first contact, and you trust the network: add `--accept-new-hostkey` to the command",
            "verify the key on the Mac (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`), then compare with `ssh-keyscan -t ed25519 HOST | ssh-keygen -lf -`",
            "key legitimately changed: remove the old entry with `ssh-keygen -R HOST`, then connect again (a changed key is never accepted automatically)",
        ],
        aliases: &["hostkey", "host-key", "host-key-mismatch", "known-hosts"],
        signatures: &[
            "host key verification failed",
            "remote host identification has changed",
            "host key mismatch",
            "no matching host key",
        ],
    },
    MacCode {
        code: "MAC_TEMPLATE_MISSING",
        status: None,
        summary: "The image (template) you named is not in the local store, or a Mac of the fleet does not hold it.",
        causes: &[
            "a typo in the name or tag (`name:tag`), or it was never pulled or imported on this Mac",
            "the template is on one Mac of the fleet but not the one the job landed on",
            "the template directory is missing one of disk.raw, hardware.bin, auxiliary.bin",
        ],
        fixes: &[
            "list what exists: `fluxctl mac images`",
            "get it: `fluxctl mac pull REGISTRY/REPO:TAG` or `fluxctl mac import DIR NAME:TAG`",
            "fleet: put it on every Mac, digest-verified: `fluxctl mac-fleet templates distribute -f hosts.yaml --template REG/REPO:TAG@sha256:HEX`",
            "the guided flow does this for you: `fluxctl mac quickstart`",
        ],
        aliases: &[
            "template-missing",
            "no-image",
            "missing-template",
            "image-not-found",
        ],
        signatures: &[
            "no such image",
            "image not found",
            "not in the local store",
            "unknown image",
            "template not found",
            "is not in the store",
        ],
    },
    MacCode {
        code: "MAC_DISK_FULL",
        status: None,
        summary: "There is not enough free disk on the Mac for the macOS image, an instance clone or the k3s guest.",
        causes: &[
            "a macOS template is 20 to 60 GiB and every full copy of it counts; clones are free only on the same APFS volume",
            "the store (FLUXVM_MAC_STORE) is on a small or non-APFS volume, so clones become real copies",
            "old instances, images or build cache are still around",
        ],
        fixes: &[
            "see the numbers: `fluxctl mac doctor` (the free disk row)",
            "drop what you do not use: `fluxctl mac rm NAME`, `fluxctl mac rmi NAME:TAG`, `fluxctl mac cache prune`",
            "move the store to a bigger APFS volume: set FLUXVM_MAC_STORE to a directory on it",
            "k3s guest: give back freed space with `fluxctl kube trim`, or create it smaller with `kube up --disk-gib`",
        ],
        aliases: &["disk-full", "no-space", "enospc"],
        signatures: &[
            "no space left on device",
            "not enough free",
            "disk full",
            "enospc",
            "out of disk",
        ],
    },
    MacCode {
        code: "DOCKER_CLI_MISSING",
        status: None,
        summary: "The docker command is not installed, so FluxVM did not make the `fluxvm` docker context.",
        causes: &[
            "only the engine runs in FluxVM; the `docker` client is a separate program that must be on this machine",
        ],
        fixes: &[
            "macOS: `brew install docker`; Linux: `sudo apt install docker.io` or see https://docs.docker.com/engine/install/ (the client alone is enough)",
            "then run `fluxctl docker context install`",
        ],
        aliases: &["docker-cli-missing", "no-docker-cli"],
        signatures: &["docker command is not installed"],
    },
    MacCode {
        code: "DOCKER_CONTEXT_UNWRITABLE",
        status: None,
        summary: "FluxVM could not write the `fluxvm` docker context into the Docker CLI's settings folder.",
        causes: &[
            "the folder (~/.docker, or DOCKER_CONFIG) belongs to another user, for example after a `sudo docker` run",
            "the disk is full or read-only, or neither HOME nor DOCKER_CONFIG is set",
        ],
        fixes: &[
            "check the owner: `ls -ld ~/.docker`; fix it with `sudo chown -R $USER ~/.docker`",
            "or use a folder you own: `export DOCKER_CONFIG=$HOME/.docker`",
            "then run `fluxctl docker context install` again",
        ],
        aliases: &["docker-context-unwritable"],
        signatures: &[
            "could not write the context",
            "cannot tell where the docker cli keeps",
        ],
    },
    MacCode {
        code: "DOCKER_CONFIG_INVALID",
        status: None,
        summary: "The Docker CLI's config.json is not valid JSON, so FluxVM left it alone instead of overwriting it.",
        causes: &["the file was edited by hand or cut short; `docker` will complain about it too"],
        fixes: &[
            "look at it: `cat ~/.docker/config.json` and fix the JSON, or move it aside: `mv ~/.docker/config.json ~/.docker/config.json.bak`",
            "then run `fluxctl docker context install --use` again",
        ],
        aliases: &["docker-config-invalid"],
        signatures: &["config.json is not a json object"],
    },
    MacCode {
        code: "MAC_REGISTRY_LOGIN_FAILED",
        status: None,
        summary: "A registry refused the login you gave `fluxctl mac login` (or the saved one on a pull or push).",
        causes: &[
            "a wrong user name, or the account password where the registry wants an access token",
            "the token expired, was revoked, or lacks the read/write scope for packages",
            "the registry address is a different host from the one that issued the token (ghcr.io versus docker.io)",
        ],
        fixes: &[
            "create a fresh access token in the registry settings and log in again: `fluxctl mac login REGISTRY -u USER` (the token is asked for without echo)",
            "scripts: `printf %s \"$TOKEN\" | fluxctl mac login REGISTRY -u USER --password-stdin`",
            "see which registries are saved and as whom: `fluxctl mac logins`; remove one with `fluxctl mac logout REGISTRY`",
        ],
        aliases: &["registry-login-failed", "login-failed", "registry-login"],
        signatures: &["mac_registry_login_failed"],
    },
    MacCode {
        code: "MAC_REGISTRY_CREDS_PERMISSIONS",
        status: None,
        summary: "The saved registry logins file can be read by other users, so fluxctl refuses to read or write it.",
        causes: &[
            "the file was copied, restored from a backup or edited with a tool that reset its permissions",
            "a umask or a shared folder gave group or other access",
        ],
        fixes: &[
            "tighten it: `chmod 600 ~/.config/fluxvm/mac-registry-auth.json` (the path is in the error message)",
            "or start over: delete the file and run `fluxctl mac login REGISTRY -u USER` again",
            "until then pulls and pushes go out anonymously; or use FLUXVM_REGISTRY_USER and FLUXVM_REGISTRY_PASSWORD",
        ],
        aliases: &[
            "registry-creds-permissions",
            "creds-permissions",
            "login-file-permissions",
        ],
        signatures: &["mac_registry_creds_permissions"],
    },
    MacCode {
        code: "MAC_FORK_SOURCE_RUNNING",
        status: None,
        summary: "`fluxctl mac fork` needs a stopped instance: a running disk is not consistent to clone.",
        causes: &[
            "the instance you named is running, paused or starting",
            "a fork is a cold boot of a copy of the disk; nothing of a running VM (memory, processes) can be carried over on macOS",
        ],
        fixes: &[
            "stop it: `fluxctl mac stop NAME`, then repeat `fluxctl mac fork NAME -n N`",
            "list instances and their state: `fluxctl mac ps -a`",
            "to keep the original running, fork a different stopped instance of the same image instead",
        ],
        aliases: &["fork-source-running", "fork-running"],
        signatures: &["mac_fork_source_running"],
    },
    MacCode {
        code: "MAC_FORK_SLOTS",
        status: None,
        summary: "`fluxctl mac fork --start` would run more macOS VMs than this Mac allows (2 at once).",
        causes: &[
            "other instances already hold macOS slots",
            "you asked to start more forks than there are free slots",
        ],
        fixes: &[
            "fork without --start (forks are created stopped, nothing is used up) and start them one at a time: `fluxctl mac start NAME`",
            "free a slot: `fluxctl mac stop NAME` (see `fluxctl mac ps`)",
            "for many VMs use several Macs: `fluxctl mac-fleet run -f hosts.yaml --template NAME:TAG -- CMD`",
        ],
        aliases: &["fork-slots", "fork-slot-limit"],
        signatures: &["mac_fork_slots"],
    },
    MacCode {
        code: "MAC_COMPOSE_KEY_UNSUPPORTED",
        status: None,
        summary: "The compose file uses a key or option that macOS guests do not support, so it is refused instead of being silently ignored.",
        causes: &[
            "a Docker-only key such as `networks`, `deploy` or `healthcheck:` in the service (a macOS service is a VM, not a container)",
            "a `secrets:` or `configs:` entry that is `external`, `environment`-sourced, inline `content`, or sets `uid`, `gid` or `mode`",
            "an `include:` or `extends:` option other than the file path / `service` and `file`",
        ],
        fixes: &[
            "remove the key the message names, then check the file: `fluxctl mac compose config --quiet`",
            "for a secret or config, write the value to a file and use `file: ./path`; to set who can read it, change the file's permissions on the Mac",
            "the supported keys are listed in docs/macos-containers.md#compose; `fluxctl mac compose config` shows what the tool understood",
        ],
        aliases: &["compose-key-unsupported", "compose-unsupported"],
        signatures: &[
            "not supported for macos guests",
            "only file-based",
            "sources are not supported",
            "inline `content`",
            "`external` is not supported",
            "`mode` is not supported",
            "`uid` is not supported",
            "`gid` is not supported",
            "`extends` option",
            "`include` option",
        ],
    },
    MacCode {
        code: "MAC_COMPOSE_VARIABLE",
        status: None,
        summary: "A `${VAR}` in the compose file is required but not set, or is written in a form this tool does not read.",
        causes: &[
            "the file says `${NAME:?message}` and NAME is neither exported in your shell nor in a `.env` file next to the compose file",
            "a `${` with no closing `}`, or a form such as `${NAME%x}` that is not supported",
        ],
        fixes: &[
            "set it for this command: `NAME=value fluxctl mac compose up`, or put `NAME=value` in a `.env` file next to the compose file",
            "give it a default in the file: `${NAME:-default}`; write `$$` for a literal dollar sign",
            "see the result before starting anything: `fluxctl mac compose config`",
        ],
        aliases: &["compose-variable", "compose-interpolation"],
        signatures: &["is required but not set", "interpolation:"],
    },
    MacCode {
        code: "MAC_COMPOSE_INCLUDE",
        status: None,
        summary: "An `include:` or `extends:` in the compose file cannot be followed.",
        causes: &[
            "the included or extended file does not exist (paths are relative to the file that names them)",
            "files include or extend each other in a loop, or nest more than 8 deep",
            "a service, secret or config has the same name in the main file and an included file",
            "`extends` names a service that the file does not define",
        ],
        fixes: &[
            "fix the path the message names, or create the file",
            "break the loop: move the shared part into its own file that includes nothing",
            "rename one of the clashing names",
            "see the merged result: `fluxctl mac compose config`",
        ],
        aliases: &["compose-include", "compose-extends"],
        signatures: &[
            "`include` or `extends` comes back",
            "`extends` loops",
            "is defined both in",
            "`include` names",
            "`extends` names the file",
            "`include` nests",
            "check the `extends` name",
            "`extends` needs",
        ],
    },
    MacCode {
        code: "MAC_COMPOSE_FILE_MISSING",
        status: None,
        summary: "A file the compose file points at (the compose file itself, an env_file, a secret or a config) is missing or unreadable.",
        causes: &[
            "no mac-compose.yaml, .yml or .json in this directory and no `-f FILE`",
            "an `env_file`, or the `file:` of a secret or config, names a path that does not exist (relative to the compose file)",
            "a secret or config is used by a service but not defined in the top-level `secrets:` / `configs:`",
        ],
        fixes: &[
            "name the file: `fluxctl mac compose -f path/to/file.yaml ...` (or `cd` to its directory)",
            "create the missing file, or make an env_file optional: `env_file: [{path: .env.local, required: false}]`",
            "define the secret: `secrets:` then `NAME:` then `file: ./path` at the top of the compose file",
        ],
        aliases: &["compose-file-missing", "compose-no-file"],
        signatures: &[
            "no compose file",
            "or is not a file; create it",
            "which the top-level `secrets:`",
            "which the top-level `configs:`",
            "cannot be read (",
        ],
    },
    MacCode {
        code: "MAC_COMPOSE_NOT_UP",
        status: None,
        summary: "The service you named has no running instance in this compose project.",
        causes: &[
            "the project was never started, or was removed with `compose down`",
            "the instance is stopped (`compose stop`), or the project name differs (`-p`, `name:` or the directory name)",
            "`compose run` needs the service's dependencies running; it does not start them (a Mac runs at most 2 macOS VMs)",
        ],
        fixes: &[
            "see what exists: `fluxctl mac compose ls -a` and `fluxctl mac compose ps -a`",
            "start it: `fluxctl mac compose up` (new) or `fluxctl mac compose start SERVICE` (stopped)",
            "name the project the instances belong to: `-p NAME`",
        ],
        aliases: &["compose-not-up", "compose-no-instance"],
        signatures: &[
            "has no instance in project",
            "no instance for service",
            "is not running in project",
            "no instance of project",
            "no instances for project",
        ],
    },
    MacCode {
        code: "MAC_FLAG_NOT_APPLICABLE",
        status: None,
        summary: "A Docker flag (`--network`, `--platform`) that has no meaning for a macOS guest was used.",
        causes: &[
            "a macOS VM has one NAT network from Virtualization.framework, so there is no network to choose",
            "every `fluxctl mac` image is macOS on Apple silicon, so there is no platform to choose",
        ],
        fixes: &[
            "leave the flag out",
            "to reach a service in the guest publish a port: `fluxctl mac run -p HOST:GUEST ...`, and find the guest address with `fluxctl mac ps`",
        ],
        aliases: &["no-network-flag", "no-platform-flag"],
        signatures: &["mac_flag_not_applicable"],
    },
    MacCode {
        code: "MAC_PULL_POLICY",
        status: None,
        summary: "`--pull` could not be followed: the image is missing and cannot be pulled, or the pull failed.",
        causes: &[
            "`--pull never` and the image is not in the local store",
            "the name is a local name (no registry host), so there is nowhere to pull it from",
            "the registry refused the login, or the name or tag does not exist there, or the network is down",
        ],
        fixes: &[
            "see what you have: `fluxctl mac images`",
            "pull it yourself to see the registry's answer: `fluxctl mac pull REGISTRY/REPO:TAG` (credentials: FLUXVM_REGISTRY_USER and FLUXVM_REGISTRY_PASSWORD)",
            "run the local copy: `--pull missing` (the default) or `--pull never`",
            "add a local template: `fluxctl mac import DIR NAME:TAG`",
        ],
        aliases: &["pull-policy", "pull-failed"],
        signatures: &["mac_pull_policy"],
    },
    MacCode {
        code: "MAC_HEALTHCHECK_FLAGS",
        status: None,
        summary: "The `--health-*` or `--entrypoint` flags are inconsistent or malformed.",
        causes: &[
            "`--health-interval` and the like were given for an image that has no HEALTHCHECK and without `--health-cmd`",
            "`--no-healthcheck` was combined with `--health-cmd`",
            "a duration is not `30s`/`2m`/`1h`, or an interval, timeout or retry count is 0",
            "`--entrypoint` is an empty array or not valid JSON",
        ],
        fixes: &[
            "say what to run: `--health-cmd \"curl -f http://localhost:8080/health\"`",
            "use either `--no-healthcheck` or the `--health-*` flags, not both",
            "write durations like `30s`, `2m`; retries at least 1",
            "check the result with `fluxctl mac health NAME` once the instance runs",
        ],
        aliases: &["healthcheck-flags", "health-flags"],
        signatures: &["mac_healthcheck_flags"],
    },
    MacCode {
        code: "MAC_CREATE_RUN_ONLY",
        status: None,
        summary: "`mac create` was given a command, environment or other option that only `mac run` can use.",
        causes: &[
            "a macOS VM stores no command or environment: `mac run` sends them over SSH when it starts the VM",
            "`CMD`, `-e`, `--env-file`, `-w`, `-d`, `--entrypoint` or `--rm` were passed to `mac create`",
        ],
        fixes: &[
            "create the VM without them, start it, and run the command: `fluxctl mac create --name ci IMG`, `fluxctl mac start ci`, `fluxctl mac exec ci -e K=V -- CMD`",
            "or do it all at once with `fluxctl mac run ...`",
        ],
        aliases: &["create-run-only", "create-flags"],
        signatures: &["mac_create_run_only"],
    },
    MacCode {
        code: "MAC_UPDATE_NOT_SUPPORTED",
        status: None,
        summary: "The CPU count or memory of an existing macOS VM cannot be changed.",
        causes: &[
            "they are fixed when the VM is created, and the daemon only lets you change an instance's name and labels",
            "this holds for a running and for a stopped instance",
        ],
        fixes: &[
            "keep the disk and make a new instance: `fluxctl mac stop NAME`, `fluxctl mac commit NAME NAME-img:1`, `fluxctl mac rm NAME`, `fluxctl mac create --name NAME --cpus N --memory SIZE NAME-img:1`",
            "what `mac update` can change: `--restart POLICY` and `--label KEY=VALUE`",
        ],
        aliases: &["update-not-supported", "resize", "update-resources"],
        signatures: &["mac_update_not_supported"],
    },
    MacCode {
        code: "MAC_KILL_SIGNAL",
        status: None,
        summary: "`mac kill --signal` was given a signal other than KILL.",
        causes: &[
            "a macOS VM is not a process you can signal: `mac kill` ends its runner process at once, which is KILL",
        ],
        fixes: &[
            "leave `--signal` out, or use `-s KILL`",
            "to ask the guest to shut down cleanly: `fluxctl mac stop NAME`",
            "to signal a process inside the guest: `fluxctl mac exec NAME -- kill -TERM PID`",
        ],
        aliases: &["kill-signal"],
        signatures: &["mac_kill_signal"],
    },
    MacCode {
        code: "MAC_KILL_UNAVAILABLE",
        status: Some(404),
        summary: "The daemon you are talking to has no kill call.",
        causes: &["the daemon is older than this fluxctl (the call is `POST /v1/vms/{id}/kill`)"],
        fixes: &[
            "stop the instance cleanly instead: `fluxctl mac stop NAME`",
            "update the daemon to the same version as fluxctl, then retry",
        ],
        aliases: &["kill-unavailable"],
        signatures: &["mac_kill_unavailable"],
    },
];

fn norm(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace('-', "_")
}

/// A code, one of its aliases (including the HTTP status), or a fragment of a code.
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

/// The code that explains an error message, matched on the text ssh, the daemon and fluxctl print.
pub fn hint_for(message: &str) -> Option<&'static MacCode> {
    let m = message.to_ascii_lowercase();
    CODES
        .iter()
        .find(|c| c.signatures.iter().any(|s| m.contains(s)))
}

/// `explain MAC_SLOT_LIMIT` card text.
pub fn card(e: &MacCode) -> String {
    let head = match e.status {
        Some(s) => format!("{} (HTTP {s}, mac)", e.code),
        None => format!("{} (mac)", e.code),
    };
    let mut s = format!("{head}\n  {}\n\ncauses:\n", e.summary);
    for c in e.causes {
        s.push_str(&format!("  - {c}\n"));
    }
    s.push_str("\nwhat to do:\n");
    for f in e.fixes {
        s.push_str(&format!("  - {f}\n"));
    }
    s
}

pub fn entry(e: &MacCode) -> serde_json::Value {
    serde_json::json!({"code": e.code, "status": e.status, "summary": e.summary, "causes": e.causes, "fixes": e.fixes})
}

/// A short hint to print under a failure: the cause and the first fix. `None` when the message is not recognised.
pub fn friendly(message: &str) -> Option<String> {
    let c = hint_for(message)?;
    Some(format!(
        "hint ({}): {}\n  try: {}\n  more: fluxctl explain {}",
        c.code,
        c.summary,
        c.fixes.first().copied().unwrap_or(""),
        c.code
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_cause_and_fix_and_is_found_by_name_and_aliases() {
        for c in CODES {
            assert!(
                !c.causes.is_empty() && !c.fixes.is_empty() && !c.summary.is_empty(),
                "{}",
                c.code
            );
            assert_eq!(find(c.code)[0].code, c.code);
            for a in c.aliases.iter().filter(|a| a.parse::<u16>().is_err()) {
                assert_eq!(find(a)[0].code, c.code, "{a}");
            }
        }
    }

    #[test]
    fn statuses_and_spellings() {
        assert_eq!(find("409")[0].code, "MAC_SLOT_LIMIT");
        assert_eq!(find("503")[0].code, "MAC_SESSION_LOCKED");
        assert_eq!(find("mac-disk-full")[0].code, "MAC_DISK_FULL");
        assert!(find("zzz").is_empty() && find("").is_empty());
    }

    #[test]
    fn messages_map_to_codes() {
        let cases = [
            ("Host key verification failed.", "SSH_HOSTKEY_MISMATCH"),
            (
                "@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @",
                "SSH_HOSTKEY_MISMATCH",
            ),
            ("write: No space left on device", "MAC_DISK_FULL"),
            ("Error: no such image macos-x:1", "MAC_TEMPLATE_MISSING"),
            (
                "the Mac's screen is locked, so snapshot restores fail",
                "MAC_SESSION_LOCKED",
            ),
            ("both macOS slots: slot limit reached", "MAC_SLOT_LIMIT"),
        ];
        for (m, want) in cases {
            assert_eq!(hint_for(m).map(|c| c.code), Some(want), "{m}");
        }
        assert!(hint_for("connection refused").is_none());
        let f = friendly("No space left on device").unwrap();
        assert!(f.contains("fluxctl explain MAC_DISK_FULL") && f.contains("try:"));
    }

    #[test]
    fn card_has_causes_and_fixes() {
        let s = card(&CODES[0]);
        assert!(s.starts_with("MAC_SLOT_LIMIT (HTTP 409, mac)") && s.contains("what to do:"));
    }
}
