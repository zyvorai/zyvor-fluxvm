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
        code: "MAC_TEMPLATE_SYNTAX",
        status: None,
        summary: "`--format` was given template syntax that fluxctl does not implement, or a template that is not well formed.",
        causes: &[
            "fluxctl implements a small subset of Go templates, not all of them: `if`, `with`, `printf`, `len`, `eq`, variables (`$x`), whitespace trimming (`{{-`) and functions other than `json` and `index` are refused",
            "a `{{` without its `}}`, a `{{range}}` without `{{end}}`, or a quote that is not closed",
            "`range` was used on a field that is text or a number, or `index` on a list with a position that does not exist",
        ],
        fixes: &[
            "use only `{{.Field.Path}}`, `{{json X}}` (or `X | json`), `{{index X KEY}}` and `{{range X}}...{{end}}`; field names ignore case and underscores",
            "see the fields you can use: run the command without `--format` (`fluxctl mac inspect NAME`, `fluxctl mac images --json`) and read the keys",
            "need more logic? print JSON and filter it: `fluxctl mac inspect NAME | jq ...`",
        ],
        aliases: &["template-syntax", "format-syntax", "bad-format"],
        signatures: &["unsupported template syntax"],
    },
    MacCode {
        code: "MAC_FILTER_INVALID",
        status: None,
        summary: "A `--filter` was not `KEY=VALUE`, used a key the command does not know, or had a value it cannot use.",
        causes: &[
            "a typo in the key, or a key from another command (the keys differ between `mac ps`, `mac images` and `mac events`)",
            "no `=`, an empty value, or a port or status that is not valid",
        ],
        fixes: &[
            "`mac ps --filter`: name, id, status, image, label, project, service, ip, publish (a port)",
            "`mac images --filter`: label, reference (with `*`), source",
            "`mac events --filter`: type (mac or kube), event, name, image, project, status, cluster, label",
            "repeat `--filter` to narrow further; every filter must match",
        ],
        aliases: &["filter-invalid", "bad-filter", "filter"],
        signatures: &["unknown filter", "must be key=value", "has an empty value"],
    },
    MacCode {
        code: "MAC_TIME_INVALID",
        status: None,
        summary: "A `--since` value was not a duration, a timestamp or a date.",
        causes: &[
            "words such as `yesterday`, a bare number, a negative duration, or a timestamp without a time zone",
        ],
        fixes: &[
            "a duration back from now: `30s`, `10m`, `2h`, `1d`, `1h30m` (units s, m, h, d, w)",
            "an RFC 3339 timestamp: `2026-11-21T10:00:00Z` or `2026-11-21T10:00:00+01:00`",
            "a date, taken as midnight UTC: `2026-11-21`",
        ],
        aliases: &["time-invalid", "bad-since", "since"],
        signatures: &["is not a time"],
    },
    MacCode {
        code: "MAC_LOGS_NO_TIMESTAMPS",
        status: None,
        summary: "`mac logs --since`, or `-t` without `-f`, was refused because the VM console log has no time on its lines.",
        causes: &[
            "the log is the VM's raw serial output; the daemon does not record when each line was written, and fluxctl will not invent a time",
        ],
        fixes: &[
            "last lines only: `fluxctl mac logs NAME --tail 200`",
            "times as lines arrive: `fluxctl mac logs -f -t NAME` (the time is when fluxctl received the line; lines already in the log show `-`)",
            "times of what happened to the instance: `fluxctl mac events --since 1h --filter name=NAME`",
            "logs of programs inside the guest, with their own times: `fluxctl mac exec NAME -- log show --last 10m`",
        ],
        aliases: &["logs-no-timestamps", "no-timestamps", "logs-since"],
        signatures: &["has no time on its lines", "no per-line times"],
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
            "a Docker-only key such as `networks` or `deploy` in the service (a macOS service is a VM, not a container)",
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
    MacCode {
        code: "MAC_ARCHIVE_CHECKSUM",
        status: None,
        summary: "An image archive does not match its `.sha256` file, so `mac load` / `mac import` refused to unpack it.",
        causes: &[
            "the copy was cut short or damaged (a full disk, an interrupted transfer)",
            "the archive was changed after `mac save` / `mac export` wrote the checksum",
            "the `.sha256` file belongs to a different archive",
        ],
        fixes: &[
            "copy the archive again and compare: `shasum -a 256 FILE` should print the digest in `FILE.sha256`",
            "make a fresh one on the source Mac: `fluxctl mac save IMAGE --output FILE`",
            "if you made the change on purpose, delete `FILE.sha256` and load again (the archive is then not checked)",
        ],
        aliases: &["archive-checksum", "bad-checksum"],
        signatures: &["does not match its checksum file"],
    },
    MacCode {
        code: "MAC_EXPORT_RUNNING",
        status: None,
        summary: "`mac export` needs a stopped instance: the disk of a running one is changing and would not be consistent.",
        causes: &["the instance is running, starting or paused"],
        fixes: &[
            "stop it first: `fluxctl mac stop NAME`",
            "then export: `fluxctl mac export NAME --output NAME.tar`",
        ],
        aliases: &["export-running"],
        signatures: &["cannot be exported"],
    },
    MacCode {
        code: "MAC_MANIFEST_UNREADABLE",
        status: None,
        summary: "`mac manifest inspect` could not read the image's manifest from the registry.",
        causes: &[
            "the reference is misspelled, or the tag does not exist",
            "the registry needs a login you have not saved (private image)",
            "the registry is unreachable from this Mac (network, proxy, TLS)",
        ],
        fixes: &[
            "check the name and tag: `fluxctl mac search REGISTRY TERM --tags`",
            "log in: `fluxctl mac login REGISTRY -u USER --password-stdin`",
            "test reachability: `curl -I https://REGISTRY/v2/`",
        ],
        aliases: &["manifest-unreadable", "manifest-inspect"],
        signatures: &["could not read the manifest of"],
    },
    MacCode {
        code: "MAC_CHECKPOINT_EXISTS",
        status: None,
        summary: "The instance already has a checkpoint with that name.",
        causes: &[
            "an earlier `mac checkpoint NAME TAG` used the same name",
            "a checkpoint name is only unique within one instance, and this instance has it already",
        ],
        fixes: &[
            "pick another name: `fluxctl mac checkpoint NAME OTHER-TAG` (leave the tag out to get ckpt-<time>)",
            "or replace it: `fluxctl mac checkpoint rm NAME TAG`, then save again",
            "see what exists: `fluxctl mac checkpoints NAME`",
        ],
        aliases: &["checkpoint-exists"],
        signatures: &[
            "already exists for this vm",
            "already has a checkpoint called",
        ],
    },
    MacCode {
        code: "MAC_CHECKPOINT_MISSING",
        status: Some(404),
        summary: "There is no checkpoint with that name (for that instance).",
        causes: &[
            "the name is misspelt, or belongs to another instance (a checkpoint belongs to the instance it was taken from)",
            "it was deleted, or was an automatic one that was replaced (only the newest 3 `auto-` checkpoints are kept)",
        ],
        fixes: &[
            "list what exists, with size and age: `fluxctl mac checkpoints`",
            "name the instance too: `fluxctl mac run --from-checkpoint NAME:TAG`",
            "make one: `fluxctl mac checkpoint NAME TAG`",
        ],
        aliases: &["checkpoint-missing", "checkpoint-not-found"],
        signatures: &[
            "has no checkpoint",
            "no instance has a checkpoint called",
            "not found for vm",
        ],
    },
    MacCode {
        code: "MAC_CHECKPOINT_AMBIGUOUS",
        status: None,
        summary: "More than one instance has a checkpoint with that name.",
        causes: &[
            "`--from-checkpoint TAG` was given without an instance name, and the tag exists on several instances",
        ],
        fixes: &[
            "say which: `fluxctl mac run --from-checkpoint NAME:TAG`",
            "see who has what: `fluxctl mac checkpoints`",
        ],
        aliases: &["checkpoint-ambiguous"],
        signatures: &[
            "mac_checkpoint_ambiguous",
            "more than one instance has a checkpoint",
        ],
    },
    MacCode {
        code: "MAC_CHECKPOINT_TAG",
        status: None,
        summary: "That is not a usable checkpoint name.",
        causes: &[
            "names are 1 to 128 letters, digits, '.', '_' or '-', and cannot start with '.' (no spaces, slashes or colons)",
        ],
        fixes: &[
            "use something like `before-upgrade` or `xcode-15.4`: `fluxctl mac checkpoint NAME before-upgrade`",
        ],
        aliases: &["checkpoint-tag"],
        signatures: &["not a usable checkpoint name", "invalid snapshot tag"],
    },
    MacCode {
        code: "MAC_CHECKPOINT_SAFETY",
        status: Some(409),
        summary: "A restore was refused because your current state could not be saved first. Nothing was changed.",
        causes: &[
            "a restore saves where you are now as `auto-before-restore-<time>` so it can be undone, and that save failed (usually too little free disk)",
        ],
        fixes: &[
            "free disk space: delete checkpoints you do not need (`fluxctl mac checkpoints`, `fluxctl mac checkpoint rm NAME TAG`) or instances (`fluxctl mac rm NAME`), then retry",
            "or, if what is running now does not matter, restore without the safety net: `fluxctl mac checkpoint restore NAME TAG --no-safety-checkpoint`",
        ],
        aliases: &["checkpoint-safety"],
        signatures: &["could not save your current state first"],
    },
    MacCode {
        code: "MAC_CHECKPOINT_NOT_SAVABLE",
        status: None,
        summary: "This instance's devices cannot have their memory saved, so a checkpoint with memory is not possible.",
        causes: &[
            "Virtualization.framework refuses to save a guest with some device configurations (the reason is in the message above)",
            "the runner is too old or the Mac's login session is locked, which a memory save needs",
        ],
        fixes: &[
            "save the disk only, which always works: `fluxctl mac checkpoint NAME TAG --disk-only` (coming back boots the guest instead of resuming it)",
            "unlock the Mac or enable automatic login, then retry: `fluxctl mac host headless-check`",
        ],
        aliases: &["checkpoint-not-savable"],
        signatures: &[
            "this vm cannot be saved",
            "the runner could not save the vm",
        ],
    },
    MacCode {
        code: "MAC_CHECKPOINT_OS_UPDATED",
        status: None,
        summary: "A checkpoint's saved memory was written on a different macOS build than this Mac now runs, so it cannot be resumed.",
        causes: &[
            "the Mac was updated after the checkpoint was taken; saved VM memory is only valid on the macOS build that wrote it",
        ],
        fixes: &[
            "by default the instance boots cold from the checkpoint's disk, which still works: nothing to do",
            "to make that an error instead, set FLUXVM_RESTORE_ON_OS_UPDATE=refuse on the daemon",
            "take a new checkpoint after updating macOS: `fluxctl mac checkpoint NAME TAG`",
        ],
        aliases: &["checkpoint-os-updated"],
        signatures: &["not portable across macos updates"],
    },
    MacCode {
        code: "MAC_CHECKPOINT_NOT_LOCAL",
        status: None,
        summary: "`mac diff` reads the instance's disk files, and they are not on this machine.",
        causes: &[
            "fluxctl is talking to a daemon on another Mac, so the files it names are not here",
            "the checkpoint name does not exist",
        ],
        fixes: &[
            "run `fluxctl mac diff` on the Mac that hosts the instance (over ssh, or at its console)",
            "check the name: `fluxctl mac checkpoints NAME`",
        ],
        aliases: &["checkpoint-not-local"],
        signatures: &[
            "mac_checkpoint_not_local",
            "must run on the mac whose daemon owns",
        ],
    },
    MacCode {
        code: "MAC_FROM_CHECKPOINT",
        status: None,
        summary: "`mac run --from-checkpoint` was combined with a flag that makes a new instance.",
        causes: &[
            "--from-checkpoint brings an existing instance back to a saved state; an IMAGE, --name, -p, --rm and --fleet describe a new one",
        ],
        fixes: &[
            "drop the extra flags: `fluxctl mac run --from-checkpoint NAME:TAG [-- CMD]`",
            "to start a new instance from an image, leave --from-checkpoint out: `fluxctl mac run IMAGE`",
        ],
        aliases: &["from-checkpoint"],
        signatures: &[
            "mac_from_checkpoint",
            "--from-checkpoint brings an existing instance",
        ],
    },
    MacCode {
        code: "MAC_SECRET_MISSING",
        status: None,
        summary: "A build step asks for a secret that was not given, or its source cannot be read.",
        causes: &[
            "a RUN has `--mount=type=secret,id=NAME` but the build command has no `--secret id=NAME,...`",
            "the file named by `--secret id=NAME,src=FILE` does not exist",
            "the variable named by `--secret id=NAME,env=VARIABLE` is not set in this shell",
        ],
        fixes: &[
            "add `--secret id=NAME,src=/path/to/file` to `fluxctl mac build`",
            "or read it from the environment: `export TOKEN=...` then `--secret id=NAME,env=TOKEN`",
            "check the plan first: `fluxctl mac build --dry-run ...` lists which steps mount a secret (names only, never values)",
        ],
        aliases: &["secret-missing", "build-secret"],
        signatures: &["mac_secret_missing"],
    },
    MacCode {
        code: "MAC_SECRET_CLEANUP",
        status: None,
        summary: "A build secret could not be removed from the build instance, so the image was not stored.",
        causes: &[
            "the guest stopped answering SSH right after the step that used the secret",
            "the secret file was made read-only by the step's own commands",
        ],
        fixes: &[
            "run the build again: the instance is deleted, so nothing is kept",
            "if it repeats, find the cause with `fluxctl mac doctor`",
            "do not copy the half-built image anywhere: it was not stored",
        ],
        aliases: &["secret-cleanup"],
        signatures: &["mac_secret_cleanup"],
    },
    MacCode {
        code: "MAC_SSH_AGENT_MISSING",
        status: None,
        summary: "A build step wants your SSH agent, but none is available.",
        causes: &[
            "a RUN has `--mount=type=ssh` but the build command has no `--ssh default`",
            "`--ssh default` was given but no ssh-agent is running in this shell (SSH_AUTH_SOCK is unset or stale)",
            "only `--ssh default` is supported",
        ],
        fixes: &[
            "add `--ssh default` to `fluxctl mac build`",
            "start an agent and load a key: `eval $(ssh-agent)` then `ssh-add`",
            "check it works: `ssh-add -l`",
            "for GitHub, add the host key in the step first: `RUN --mount=type=ssh mkdir -p ~/.ssh && ssh-keyscan github.com >> ~/.ssh/known_hosts && git clone git@github.com:ORG/REPO.git`",
        ],
        aliases: &["ssh-agent", "ssh-agent-missing"],
        signatures: &["mac_ssh_agent_missing"],
    },
    MacCode {
        code: "MAC_PLATFORM_UNSUPPORTED",
        status: None,
        summary: "A macOS image can only be built for darwin/arm64.",
        causes: &[
            "`--platform` named another platform such as linux/amd64 or darwin/amd64",
            "macOS guests on this Mac are Apple silicon only",
        ],
        fixes: &[
            "use `--platform darwin/arm64`, or leave the flag out",
            "for a Linux image use `docker build --platform linux/amd64 ...` with the Docker engine instead",
        ],
        aliases: &["platform-unsupported", "build-platform"],
        signatures: &["mac_platform_unsupported"],
    },
    MacCode {
        code: "MAC_BUILD_TARGET_UNSUPPORTED",
        status: None,
        summary: "`--target` needs a multi-stage build, and a Macfile has one stage.",
        causes: &["a Macfile allows exactly one FROM, so there is no named stage to stop at"],
        fixes: &[
            "leave `--target` out to build the whole Macfile",
            "to get an intermediate image, put those steps in their own Macfile and build it: `fluxctl mac build -t base:1 -f Macfile.base .`, then start the next Macfile with `FROM base:1`",
        ],
        aliases: &["build-target", "target-unsupported"],
        signatures: &["mac_build_target_unsupported"],
    },
    MacCode {
        code: "MAC_BUILD_ARG_UNUSED",
        status: None,
        summary: "A `--build-arg` does nothing, or an ARG has no value.",
        causes: &[
            "the Macfile has no `ARG NAME` line for it (a typo, or a different case: names are case-sensitive)",
            "an `ARG NAME` has no default and no `--build-arg NAME=...`, so `$NAME` expands to an empty string",
            "the name looks like a credential: build args are visible to every RUN and to anyone who can read the command",
        ],
        fixes: &[
            "declare it before the step that needs it: `ARG NAME` or `ARG NAME=default`",
            "fix the spelling shown in the warning",
            "for credentials use `--secret id=NAME,src=FILE` with `RUN --mount=type=secret,id=NAME`",
        ],
        aliases: &["build-arg", "build-arg-unused"],
        signatures: &["mac_build_arg_unused"],
    },
    MacCode {
        code: "MAC_BUILD_FLAG_INVALID",
        status: None,
        summary: "A `fluxctl mac build` flag is written in a form that is not understood.",
        causes: &[
            "`--label` is not KEY=VALUE",
            "`--secret` is not `id=NAME,src=FILE` or `id=NAME,env=VARIABLE`, or the same id is given twice",
            "`--ssh` is anything but `default`",
        ],
        fixes: &[
            "examples: `--label team=ci`  `--secret id=npm,src=$HOME/.npmrc`  `--secret id=tok,env=TOKEN`  `--ssh default`",
            "see all build flags: `fluxctl mac build --help`",
        ],
        aliases: &["build-flag"],
        signatures: &["mac_build_flag_invalid"],
    },
    MacCode {
        code: "FLEET_ADD_BLOCKED",
        status: None,
        summary: "`fluxctl mac-fleet add` stopped on a Mac before it could take jobs; the fix is printed under the step that failed.",
        causes: &[
            "ssh could not connect (key not accepted, host key not trusted, Remote Login off, Mac asleep)",
            "the preflight found a problem (Intel Mac, old macOS, low disk, no login session)",
            "the install or the daemon failed, or the Mac cannot list templates yet",
        ],
        fixes: &[
            "do what the `fix:` line under the failed step says, then run `fluxctl mac-fleet add HOST` again (it resumes where it stopped)",
            "see every check at once: `fluxctl mac-fleet doctor HOST`",
        ],
        aliases: &["fleet-add-blocked", "fleet-add"],
        signatures: &["fleet_add_blocked"],
    },
    MacCode {
        code: "FLEET_DRAIN_TIMEOUT",
        status: None,
        summary: "`fluxctl mac-fleet drain --wait` gave up before the Mac emptied (or could not tell). The Mac stays cordoned.",
        causes: &[
            "a job or a long-lived guest is still running on the Mac (a macOS guest can run for hours)",
            "the Mac became unreachable while waiting, so its guest count is unknown",
        ],
        fixes: &[
            "wait longer: `fluxctl mac-fleet drain HOST --wait --wait-timeout 7200`",
            "see what runs: `fluxctl mac-fleet exec HOST -- fluxctl mac ps`, and stop a guest you do not need: `fluxctl mac stop NAME`",
            "give the Mac back to the scheduler: `fluxctl mac-fleet uncordon HOST`",
        ],
        aliases: &["fleet-drain-timeout", "drain-timeout"],
        signatures: &["fleet_drain_timeout"],
    },
    MacCode {
        code: "FLEET_HOST_THROTTLED",
        status: None,
        summary: "A Mac in the fleet is running slower than normal because macOS is limiting its CPU to stay cool (`fluxctl mac-fleet top`).",
        causes: &[
            "poor airflow, a hot room, or a long run of full-load guests",
            "Low Power Mode, or a Mac mini in a closed rack",
        ],
        fixes: &[
            "check the numbers: `fluxctl mac host status`",
            "stop sending jobs while it cools: `fluxctl mac-fleet cordon HOST`, then `fluxctl mac-fleet uncordon HOST`",
            "fix the cooling; jobs that time out on a throttled Mac are slower, not broken",
        ],
        aliases: &["fleet-throttled", "host-throttled"],
        signatures: &["fleet_host_throttled"],
    },
    MacCode {
        code: "FLEET_DISK_LOW",
        status: None,
        summary: "A Mac in the fleet has little free disk; each macOS guest clones a template of tens of GB (`fluxctl mac-fleet top`).",
        causes: &[
            "old templates and stopped guests pile up",
            "a template was distributed to a Mac with a small disk",
        ],
        fixes: &[
            "see what uses it: `fluxctl mac-fleet exec HOST -- fluxctl mac system df`",
            "remove what is unused: `fluxctl mac-fleet exec HOST -- fluxctl mac system prune --all`",
            "keep jobs away meanwhile: `fluxctl mac-fleet cordon HOST`",
        ],
        aliases: &["fleet-disk-low", "host-disk-low"],
        signatures: &["fleet_disk_low"],
    },
    MacCode {
        code: "MAC_VOLUME_NAME",
        status: None,
        summary: "That is not a valid volume name.",
        causes: &[
            "a volume name may hold letters, digits, `_`, `.` and `-`, start with a letter or digit, and be at most 64 characters",
        ],
        fixes: &[
            "pick a simple name: `fluxctl mac volume create build-cache`",
            "to share a folder instead of a named volume, use a path that starts with `./`, `/` or `~/`: `-v ./src:/src`",
        ],
        aliases: &["volume-name"],
        signatures: &["mac_volume_name"],
    },
    MacCode {
        code: "MAC_VOLUME_NOT_FOUND",
        status: Some(404),
        summary: "There is no volume with that name.",
        causes: &[
            "it was never created, was removed, or the name is misspelled",
            "FLUXVM_MAC_VOLUMES points at a different folder than when it was made",
        ],
        fixes: &[
            "see what exists: `fluxctl mac volume ls`",
            "make it: `fluxctl mac volume create NAME` (or just use `-v NAME:/path` on `mac run`, which creates it)",
        ],
        aliases: &["volume-not-found"],
        signatures: &["mac_volume_not_found"],
    },
    MacCode {
        code: "MAC_VOLUME_IN_USE",
        status: Some(409),
        summary: "A volume cannot be removed while an instance still has it mounted (running or stopped).",
        causes: &[
            "a stopped instance still lists the volume among its shared folders and would mount it again on start",
        ],
        fixes: &[
            "remove the instance named in the message: `fluxctl mac rm NAME`, then retry",
            "see which instances hold each volume: `fluxctl mac volume ls`",
            "to clear everything unused in one go: `fluxctl mac system prune --volumes --dry-run`",
        ],
        aliases: &["volume-in-use"],
        signatures: &["mac_volume_in_use"],
    },
    MacCode {
        code: "MAC_VOLUME_IO",
        status: None,
        summary: "FluxVM could not write the volume folder on this Mac.",
        causes: &[
            "the disk is full or read-only",
            "the folder (default `~/.fluxvm/mac-volumes`) is not writable by this user",
        ],
        fixes: &[
            "check free space: `fluxctl mac system df` (a nearly full Mac says so)",
            "free room: `fluxctl mac system prune --all --dry-run`, then without `--dry-run`",
            "or keep volumes on another disk: `export FLUXVM_MAC_VOLUMES=/Volumes/Big/fluxvm-volumes`",
        ],
        aliases: &["volume-io"],
        signatures: &["mac_volume_io"],
    },
    MacCode {
        code: "MAC_PORT_IN_USE",
        status: Some(409),
        summary: "The host port you asked to publish with `-p` is already taken.",
        causes: &[
            "another running instance publishes the same host port",
            "another program on this Mac is listening on it",
            "the same host port was given twice in one command",
        ],
        fixes: &[
            "use another host port: `-p 8081:80` (the message suggests a free one)",
            "free the port: `fluxctl mac stop NAME` for an instance, or quit the program the message names",
            "see what each instance publishes: `fluxctl mac port NAME`",
        ],
        aliases: &["port-in-use", "port-conflict"],
        signatures: &["mac_port_in_use"],
    },
    MacCode {
        code: "MAC_PORT_PRIVILEGED",
        status: None,
        summary: "Host ports below 1024 cannot be published: the FluxVM daemon does not run as root.",
        causes: &["a `-p 80:80` style option asks for a low host port"],
        fixes: &[
            "publish a high port: `-p 8080:80`",
            "if you need port 80 itself, put a small proxy or `pf` redirect on the Mac in front of the high port",
        ],
        aliases: &["port-privileged"],
        signatures: &["mac_port_privileged"],
    },
    MacCode {
        code: "MAC_NETWORK_UNSUPPORTED",
        status: None,
        summary: "A macOS guest has one NAT network; creating, removing or joining others is not supported.",
        causes: &[
            "Virtualization.framework's NAT gives each guest one card; guests on it cannot reach each other and there are no user-defined networks or DNS names",
        ],
        fixes: &[
            "see what the network does and does not do: `fluxctl mac network inspect`",
            "to let one instance call another, publish a port on the target (`-p 9000:9000`) and call the Mac's address on it",
            "for Linux guests with private networks see docs/macos-architecture.md, section 6",
        ],
        aliases: &["network-unsupported"],
        signatures: &["mac_network_unsupported"],
    },
    MacCode {
        code: "MAC_EXEC_NOT_RUNNING",
        status: None,
        summary: "`mac exec`, `mac attach` or `mac run -it` found an instance that is stopped or still booting, so there is no guest to connect to.",
        causes: &[
            "the instance was stopped (`mac stop`, a crash, or a `--rm` run that finished)",
            "the instance just started and macOS has not got an address yet",
        ],
        fixes: &[
            "start it: `fluxctl mac start NAME` (see every instance with `fluxctl mac ps -a`)",
            "if it is booting, wait for ssh: `fluxctl mac wait-ready NAME`, then run the command again",
            "to read what a stopped instance printed: `fluxctl mac logs NAME`",
        ],
        aliases: &["exec-not-running", "mac-exec-not-running"],
        signatures: &[
            "not running, so there is nothing to connect to",
            "no address yet (macos is still booting)",
            "not running, so it has no live console",
        ],
    },
    MacCode {
        code: "MAC_GUEST_SSH_FAILED",
        status: None,
        summary: "ssh could not open a session in the macOS guest (ssh itself failed with exit status 255, not the command you ran).",
        causes: &[
            "Remote Login (sshd) is off in the guest, or the guest is not finished booting",
            "the account in `-u USER` does not exist in the guest, or it has no authorised key",
            "the image was built without the FluxVM key, so the generated key is not accepted",
        ],
        fixes: &[
            "check that the guest answers on ssh: `fluxctl mac wait-ready NAME`",
            "run as the image's own account: `fluxctl mac exec NAME -u admin -- whoami`",
            "see ssh's reason by running the printed command yourself: `fluxctl mac exec --print NAME -- true`",
            "check the whole setup: `fluxctl mac doctor`",
        ],
        aliases: &[
            "guest-ssh-failed",
            "exec-ssh-failed",
            "mac-guest-ssh-failed",
        ],
        signatures: &[
            "could not open an ssh session to",
            "ssh exited with exit status: 255",
        ],
    },
    MacCode {
        code: "MAC_COMPOSE_CONDITION",
        status: None,
        summary: "A `depends_on` entry in the compose file has a condition or key that is not understood.",
        causes: &[
            "the condition is misspelt, or is a Docker one that does not exist (only service_started, service_healthy and service_completed_successfully do)",
            "the entry has a key other than condition, required or restart",
        ],
        fixes: &[
            "write one of: service_started, service_healthy, service_completed_successfully",
            "check the file: `fluxctl mac compose config --quiet`",
        ],
        aliases: &["compose-condition", "compose-depends-on"],
        signatures: &["explain mac_compose_condition"],
    },
    MacCode {
        code: "MAC_COMPOSE_HEALTHCHECK",
        status: None,
        summary: "The `healthcheck:` of a compose service cannot be used.",
        causes: &[
            "there is no `test:`, or it is empty or a list that does not start with CMD, CMD-SHELL or NONE",
            "a duration such as `interval: 5x` is not a number with s, m or h",
            "`retries` is 0",
        ],
        fixes: &[
            "write `healthcheck: {test: [CMD-SHELL, \"pg_isready\"], interval: 5s, retries: 5}`",
            "to turn the image's check off use `disable: true`",
        ],
        aliases: &["compose-healthcheck"],
        signatures: &["explain mac_compose_healthcheck"],
    },
    MacCode {
        code: "MAC_COMPOSE_SCALE",
        status: None,
        summary: "`--scale` could not be applied to this compose file.",
        causes: &[
            "the value is not SERVICE=N with N from 1 to 64",
            "the service is not selected (check `profiles:`), runs to completion, or publishes a fixed host port that every copy would want on one Mac",
            "the copies would need more than 2 macOS VMs at once on this Mac",
        ],
        fixes: &[
            "write `--scale web=2`",
            "drop the fixed host port from `ports:` of a scaled service",
            "spread the copies over several Macs: `fluxctl mac compose up --fleet hosts.yaml --scale web=3`",
        ],
        aliases: &["compose-scale"],
        signatures: &["explain mac_compose_scale"],
    },
    MacCode {
        code: "MAC_COMPOSE_PLACEMENT",
        status: None,
        summary: "The `placement:` of a compose service is wrong.",
        causes: &[
            "a label in `labels:` is not a valid inventory label (letters, digits, and the characters = . _ -)",
            "`placement:` has a key other than host, labels, ac_only or disk_gib",
        ],
        fixes: &[
            "write `placement: {host: mini-1, labels: [xcode=15], ac_only: true, disk_gib: 100}`",
            "`placement:` only matters with `compose up --fleet`; on one Mac it is ignored",
        ],
        aliases: &["compose-placement"],
        signatures: &["explain mac_compose_placement"],
    },
    MacCode {
        code: "MAC_COMPOSE_RESTART",
        status: None,
        summary: "The `restart:` policy of a compose service has a count that does not make sense.",
        causes: &[
            "only `on-failure` takes a count (`on-failure:3`)",
            "the count is not a number",
        ],
        fixes: &[
            "write `restart: on-failure:3`, or `restart: always` / `unless-stopped` / `no`",
            "the count is enforced while `fluxctl mac compose watch` runs",
        ],
        aliases: &["compose-restart"],
        signatures: &["explain mac_compose_restart"],
    },
    MacCode {
        code: "MAC_COMPOSE_WATCH_RULE",
        status: None,
        summary: "A `develop: watch:` rule in the compose file cannot be used.",
        causes: &[
            "the action is not sync, sync+restart, restart or rebuild",
            "a sync rule has no absolute `target:` in the guest",
            "a rebuild rule is on a service with no `build:`",
            "the watched path does not exist on this Mac",
        ],
        fixes: &[
            "write `develop: {watch: [{action: sync+restart, path: ./src, target: /Users/admin/app}]}`",
            "create the path, or fix `path:` (relative to the compose file)",
        ],
        aliases: &["compose-watch-rule"],
        signatures: &["explain mac_compose_watch_rule"],
    },
    MacCode {
        code: "MAC_COMPOSE_WATCH_NONE",
        status: None,
        summary: "`compose watch` has nothing to watch.",
        causes: &["no service has `develop: watch:` rules and none has a `restart:` policy"],
        fixes: &[
            "add rules: `develop: {watch: [{action: sync, path: ./src, target: /Users/admin/app}]}`",
            "or add `restart: on-failure:3` (or always) to a service, which `compose watch` then applies",
        ],
        aliases: &["compose-watch-none"],
        signatures: &["explain mac_compose_watch_none"],
    },
    MacCode {
        code: "MAC_COMPOSE_DEP_FAILED",
        status: None,
        summary: "A service that others wait for with `service_completed_successfully` did not finish successfully, so they were not started.",
        causes: &[
            "its command exited with an error, or the guest did not answer on SSH",
            "it has no `command:` and its image has no CMD, so there is nothing to run to the end",
        ],
        fixes: &[
            "look at what it printed: `fluxctl mac logs NAME` (a failed one-shot stays up for this)",
            "fix the command, then `fluxctl mac compose down` and `up` again",
        ],
        aliases: &["compose-dep-failed"],
        signatures: &["explain mac_compose_dep_failed"],
    },
    MacCode {
        code: "MAC_COMPOSE_FLEET_VOLUMES",
        status: None,
        summary: "`volumes:`, `secrets:` or `configs:` cannot be used with `compose up --fleet`.",
        causes: &[
            "they are folders or files on this Mac, and the other Macs of a fleet cannot see them",
        ],
        fixes: &[
            "put the files in the image (Macfile COPY), push it, and use that image",
            "pass a value in `environment:`: with --fleet it travels over ssh stdin, not in the command line",
            "or run on this Mac only (leave out --fleet)",
        ],
        aliases: &["compose-fleet-volumes"],
        signatures: &["explain mac_compose_fleet_volumes"],
    },
    MacCode {
        code: "MAC_COMPOSE_FLEET_NO_ROOM",
        status: None,
        summary: "No Mac of the inventory can take a service of the compose project.",
        causes: &[
            "every Mac already runs 2 macOS VMs (Apple's licence limit), or lacks the image, or fails a `placement:` rule, or is drained or unreachable",
        ],
        fixes: &[
            "see the reason for each Mac: `fluxctl mac compose up --dry-run --fleet hosts.yaml`",
            "free a slot, add a Mac to the inventory, or distribute the image: `fluxctl mac-fleet templates distribute`",
            "relax `placement:` (host, labels, ac_only, disk_gib)",
        ],
        aliases: &["compose-fleet-no-room"],
        signatures: &["explain mac_compose_fleet_no_room"],
    },
    MacCode {
        code: "MAC_COMPOSE_FLEET_START",
        status: None,
        summary: "Starting a compose service on a fleet Mac failed.",
        causes: &[
            "ssh to the Mac failed, FluxVM is not installed there, or its daemon refused (for example a slot taken meanwhile)",
            "an environment variable name cannot be sent (PATH, HOME, DYLD_* and a few others are reserved)",
        ],
        fixes: &[
            "the message names the Mac and the reason; check it with `fluxctl mac-fleet doctor -f hosts.yaml`",
            "what already started stays: `fluxctl mac compose down --fleet hosts.yaml` removes it",
        ],
        aliases: &["compose-fleet-start"],
        signatures: &["explain mac_compose_fleet_start"],
    },
    MacCode {
        code: "MAC_COMPOSE_FLEET_INVENTORY",
        status: None,
        summary: "The Macs for `compose --fleet` could not be read.",
        causes: &[
            "the inventory file is missing or is not valid YAML or JSON, or lists the same Mac twice",
        ],
        fixes: &[
            "check the file with `fluxctl mac-fleet status -f hosts.yaml`",
            "see docs/macos-containers.md for the inventory format",
        ],
        aliases: &["compose-fleet-inventory"],
        signatures: &["explain mac_compose_fleet_inventory"],
    },
    MacCode {
        code: "SHARE_SERVE_FAILED",
        status: None,
        summary: "`fluxctl share --serve` could not start its one-time local download.",
        causes: &[
            "the port is already used by another program",
            "the address given with --bind does not belong to this machine",
        ],
        fixes: &[
            "leave the port out (`fluxctl share NAME --serve`) and a free one is picked",
            "or share the file by hand: `fluxctl share NAME` writes the bundle and prints the `fluxctl import FILE` line to run on the other machine",
        ],
        aliases: &["share-serve"],
        signatures: &["could not serve the share bundle"],
    },
    MacCode {
        code: "SHARE_FETCH_FAILED",
        status: None,
        summary: "`fluxctl import URL` could not download the bundle.",
        causes: &[
            "the sender stopped `fluxctl share --serve` (it serves one download, then stops, or after its timeout)",
            "the address is not reachable from this machine, or the link was cut short when copied",
        ],
        fixes: &[
            "ask the sender to run `fluxctl share NAME --serve` again and send you the new line",
            "or have them send the file itself and run `fluxctl import FILE`",
        ],
        aliases: &["share-fetch"],
        signatures: &["could not download the shared bundle"],
    },
    MacCode {
        code: "MAC_SCHEDULE_INVALID",
        status: None,
        summary: "A schedule could not be saved: its name, cron expression, image, TTL or webhook is not valid.",
        causes: &[
            "the cron expression does not have five fields (minute hour day-of-month month day-of-week), or a field is out of range",
            "the name has spaces or other characters a schedule name cannot hold, or a schedule with that name exists",
            "the TTL is not a duration such as 30m or 2h, or the webhook is not an http:// or https:// URL",
        ],
        fixes: &[
            "quote the cron expression: `fluxctl mac schedule add nightly \"0 3 * * *\" IMAGE -- CMD` (also @hourly @daily @weekly @monthly)",
            "see the existing schedules: `fluxctl mac schedule ls`, and remove one with `fluxctl mac schedule rm NAME`",
            "the images you can run: `fluxctl mac images`",
        ],
        aliases: &["schedule-invalid", "bad-cron"],
        signatures: &[
            "is not a cron expression",
            "not a usable schedule name",
            "a schedule named",
            "a schedule needs an image",
        ],
    },
    MacCode {
        code: "MAC_SCHEDULE_MISSING",
        status: Some(404),
        summary: "There is no schedule with that name.",
        causes: &[
            "the name is misspelt, or the schedule was removed",
            "this Mac keeps its schedules in its own file; you may be on another Mac or another user account",
        ],
        fixes: &[
            "list them: `fluxctl mac schedule ls`",
            "add it: `fluxctl mac schedule add NAME \"0 3 * * *\" IMAGE -- CMD`",
        ],
        aliases: &["schedule-missing", "schedule-not-found"],
        signatures: &["no schedule named"],
    },
    MacCode {
        code: "MAC_NOTIFY_FAILED",
        status: None,
        summary: "A run finished but its notification could not be delivered (the run itself is not affected).",
        causes: &[
            "macOS blocked the banner: notifications are off for Script Editor / osascript, or there is no logged-in desktop session (a bare ssh login cannot show one)",
            "the webhook URL was revoked, mistyped, or is not reachable from this Mac",
        ],
        fixes: &[
            "check your setup: `fluxctl mac notify test` (add `--webhook URL` to test the webhook, `--no-desktop` to skip the banner)",
            "allow notifications for Script Editor in System Settings > Notifications",
            "give the webhook again: `fluxctl mac schedule rm NAME`, then add it with `--notify-webhook URL`",
        ],
        aliases: &["notify-failed", "notification-failed"],
        signatures: &[
            "could not show a macos notification",
            "macos refused the notification",
            "the webhook answered",
            "could not reach the webhook",
            "a webhook must be an http",
        ],
    },
    MacCode {
        code: "MAC_TTL_REMOVE_FAILED",
        status: None,
        summary: "An instance reached its time limit (--ttl) but could not be stopped and removed.",
        causes: &[
            "the daemon was unreachable or refused the removal",
            "the instance is busy (still starting or stopping)",
        ],
        fixes: &[
            "remove it by hand: `fluxctl mac rm NAME` (or `fluxctl mac kill NAME`, then `rm`)",
            "check the daemon: `fluxctl mac doctor`",
            "`fluxctl mac reap` removes every instance whose TTL passed",
        ],
        aliases: &["ttl-remove-failed"],
        signatures: &["reached its time limit but could not be removed"],
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
    // The most specific signature wins: a generic status line such as "409 conflict" must not hide a precise reason.
    CODES
        .iter()
        .filter_map(|c| {
            c.signatures
                .iter()
                .filter(|s| m.contains(**s))
                .map(|s| s.len())
                .max()
                .map(|len| (len, c))
        })
        .fold(
            None,
            |best: Option<(usize, &'static MacCode)>, (len, c)| match best {
                Some((b, _)) if b >= len => best,
                _ => Some((len, c)),
            },
        )
        .map(|(_, c)| c)
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
            (
                "snapshot \"w\" already exists for this VM",
                "MAC_CHECKPOINT_EXISTS",
            ),
            (
                "POST /v1/vms/1/restore: 409 Conflict: could not save your current state first",
                "MAC_CHECKPOINT_SAFETY",
            ),
            (
                "not enough free disk to save a checkpoint: 900 MiB free",
                "MAC_DISK_FULL",
            ),
            (
                "the saved VM state was written on macOS build 1 but this Mac now runs build 2; Virtualization.framework state is not portable across macOS updates",
                "MAC_CHECKPOINT_OS_UPDATED",
            ),
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
