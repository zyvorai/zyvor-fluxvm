# VM run strategy, crash handling and power schedules

Three optional fields of the VM spec (`CreateVmRequest`). All are unset by default and an unset field keeps the behaviour from before they existed: a VM
whose hypervisor process dies is marked `stopped` and stays so. JSON keys are snake_case; the camelCase spellings (`runStrategy`, `onCrash`,
`powerSchedule`) are accepted too.

## `run_strategy`

What the host does when a running VM's hypervisor process exits without anyone asking it to (a stop through the API or CLI never counts).

| Value | Behaviour |
|---|---|
| `Always` | Restart on every unrequested exit, clean or not. |
| `RerunOnFailure` | Restart unless the guest shut itself down cleanly. An exit whose cause is unknown counts as a failure. |
| `Manual` | Never restart. `enable`d VMs still start when `fluxctl serve` boots. |
| `Halted` | Never restart, and skip autostart. An explicit `start` still works. |

Detection is `reconcile()`, which already runs on every reaper tick and at daemon start: a Running or Paused VM whose pid is gone is marked Stopped, and the
supervisor then decides. It adds to, and does not replace, `start_autostart_vms` (boot-time start of `enable`d, Stopped VMs). A VM that died while the daemon
was down is therefore restarted by the first tick after boot.

Restarts back off: 5 s, 10 s, 20 s, ... capped at 5 min. After 5 consecutive restarts with no run lasting 10 minutes the VM is marked `failed` with a
`crash loop` error and left alone; a later successful run (or a manual `start` that stays up 10 minutes) resets the count. The state is kept in
`<state_dir>/supervisor/<id>.json`, so a pending restart survives a daemon restart. Restarts are performed on the reaper tick, so the effective delay is
rounded up to `reaper_interval_secs`.

Limits: without `on_crash` (QEMU), the reaper cannot see why a process vanished, so a guest-initiated poweroff looks like a failure and `RerunOnFailure`
restarts it. Set `on_crash` on QEMU VMs to get guest shutdowns classified as clean. The strategy applies to every backend; `on_crash` is QEMU only.

## `on_crash` (QEMU)

Adds a `pvpanic` device, an `i6300esb` watchdog, a `-action panic=...,watchdog=...` policy (QEMU 6.0+) and a second, control-mode monitor at
`<run_dir>/qmp/<id>.evt.sock` that the daemon reads for events. Unset adds none of it. The guest needs a pvpanic driver (`CONFIG_PVPANIC`) and a running
watchdog daemon (the `i6300esb` driver and, for example, systemd `RuntimeWatchdogSec`).

| Value | Panic | Watchdog expiry |
|---|---|---|
| `restart` | QEMU exits; the VM is started again (even with no `run_strategy`, never against `Manual`/`Halted`). | Guest reset in place. |
| `poweroff` | QEMU exits; the VM stays stopped, whatever `run_strategy` says. | QEMU exits; stays stopped. |
| `pause` | Guest paused for inspection (the record still says running; use `fluxctl resume` or stop). | Guest paused. |
| `coredump-if-existing` | As `pause`, and a guest core is written to `<workspace>/dumps/<time>.elf` if that directory already exists. Without the directory it is `pause`. | Same. |

Events recorded: `vm.guest-panic`, `vm.watchdog`, `vm.guest-shutdown` (fields `action`, `on_crash`).

## `power_schedule`

```json
{ "power_schedule": { "start": "0 8 * * 1-5", "stop": "0 18 * * 1-5" } }
```

or the labels `fluxvm.io/power-start` and `fluxvm.io/power-stop` (the spec field wins). Five-field cron in UTC (`min hour day month weekday`; `*`, numbers,
`a-b`, lists, `/step`; weekday 0-7 with 0 and 7 Sunday; day-of-month and weekday are OR'd when both are restricted) or `@hourly`, `@daily`, `@weekly`,
`@monthly`, `@yearly`. A start only acts on a Stopped VM and a stop (graceful) only on a Running one. A stop by schedule is a requested stop, so
`run_strategy` does not restart it. A fire time is acted on in the reaper tick that covers it; times missed while the daemon was down are not replayed,
except for the one tick after boot.

Events: `vm.exited`, `vm.restart-scheduled`, `vm.restarted`, `vm.restart-failed`, `vm.crash-loop`, `vm.power-schedule` in `events.jsonl` (`fluxctl events`).

## Code

`fluxvm_scheduler::supervisor` (pure decision, backoff, persisted state), `fluxvm_scheduler::power_schedule` (cron, next fire, tick decision),
`fluxvm_scheduler::run_lifecycle` (the `VmManager` glue), `fluxvm_qemu::crash_watch` (arguments, event parser, watcher), `fluxvm_core::lifecycle` (types).
