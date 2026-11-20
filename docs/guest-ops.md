# Guest operations (QEMU guest agent)

Run-time operations that need the guest's cooperation, over the QEMU guest agent (QGA) channel. They need a VM created with `qga.enabled`
(QEMU or Cloud Hypervisor), the guest agent installed and running inside it, and the VM in `running` state. No network path into the guest is needed.

## CLI

```
fluxctl vm guest info <vm>
fluxctl vm guest ssh-key add <vm> [--user root] (--key "ssh-ed25519 AAAA… me") | --key-file keys.pub [--reset]
fluxctl vm guest ssh-key rm  <vm> [--user root] (--key … | --key-file …)
fluxctl vm guest ssh-key ls  <vm> [--user root]
fluxctl vm guest set-password <vm> [--user root] (--password-stdin | --password-file FILE) [--crypted]
fluxctl vm guest time-sync <vm> [--rtc]
```

`<vm>` is an id, a unique id prefix or a name. The commands work against the local daemon or, with `--server`/`--context`, a remote one.

Passwords: there is deliberately no `--password VALUE` flag (it would show in `ps` and shell history). Pipe it in
(`printf %s "$PW" | fluxctl vm guest set-password web --password-stdin`) or point at a file with `--password-file`. Only the first line is used and its
line terminator is dropped. The password is sent base64-encoded (as QGA requires), is not printed, is not part of any error message, and the
request type redacts it from `Debug` output. `--crypted` means the input is already a crypt(3) hash. Note the REST body does carry the password, so use
TLS or a loopback/unix listener for the API.

`time-sync` sets the guest clock from this host's wall clock (`guest-set-time` with the host time). `--rtc` instead asks the guest to reload from
its hardware clock. Run it after a resume, restore or migration, when the guest clock has fallen behind.

## REST

| Route | Role | Notes |
| --- | --- | --- |
| `GET /v1/vms/{id}/guest/info` | read | OS, host name, clock (ns), users, filesystems, vCPUs. Best effort per section: a section the agent could not answer is empty and named in `errors`. |
| `GET /v1/vms/{id}/guest/ssh-keys?username=U` | admin | `{"keys": [...]}` |
| `POST /v1/vms/{id}/guest/ssh-keys` | admin | `{"username","keys":[...],"reset"?}`; 204 |
| `POST /v1/vms/{id}/guest/ssh-keys/remove` | admin | `{"username","keys":[...]}`; 204 |
| `POST /v1/vms/{id}/guest/password` | admin | `{"username","password","crypted"?}`; 204 |
| `POST /v1/vms/{id}/guest/time-sync` | admin | `{"mode"?: "host"\|"rtc"}` (default host) |

Errors: 400 for an invalid username/key/empty password, 409 when the VM has no guest agent configured or is not running, 502 when the agent does
not answer or rejects the command (the agent's own error class and description are returned, never the request arguments).

## Library

`fluxvm_image::qga` gains typed wrappers: `os_info`, `fs_info`, `users`, `host_name`, `get_time_ns`, `set_time_ns`, `sync_time_from_host`,
`set_user_password`, `ssh_add_authorized_keys`, `ssh_remove_authorized_keys`, `ssh_get_authorized_keys`, `vcpus`, `memory_blocks` and the aggregate
`guest_info`. Guest-side `{"error": ...}` replies become `Err` for these (the lower-level `raw` still returns them as values).

## Guest support

Availability follows the guest agent: `guest-ssh-*` and `guest-get-memory-blocks` are Linux-only in upstream QGA, and a distribution may disable
commands (`--block-rpcs`). A command the agent refuses surfaces as a 502 with the agent's message.

## Not done / follow-ups

- Automatic clock resync after `resume`/restore/migration: there is no post-resume hook that is not inside the lifecycle code; call
  `POST .../guest/time-sync` (or `vm guest time-sync`) for now. A best-effort background call from the resume/restore paths is the intended follow-up.
- A guest metadata (MMDS-style) HTTP service is not implemented.
- Nothing here was run against a real guest agent; the tests use a mock QGA socket.
