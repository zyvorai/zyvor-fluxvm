# Recipes: run something useful with one command

> **Status.** Rendering is unit-tested with golden files and the secret handling (0600 files, nothing in argv) is tested with a fake runner.
> **Not run** against a real Docker engine, k3s cluster, GitHub or macOS guest in this change. The commands a recipe prints are the same ones you
> would type; `fluxctl recipe run NAME --dry-run` shows them first.

A recipe is a short, named plan layered over what FluxVM already has: `fluxctl kube add`, `fluxctl mac runner` and a Docker engine
(`fluxctl docker up` on a Mac, Docker on Linux). Each prints **where to open it**, **where its credentials are**, **a backup schedule** and **how to undo it**.

```bash
fluxctl recipe list
fluxctl recipe show registry         # what it would do, nothing runs
fluxctl recipe run registry          # do it
fluxctl recipe credentials registry  # the only command that prints a stored secret
fluxctl recipe uninstall registry
```

Parameters are `--set key=value`; `show` lists them when one is missing.

| Recipe | What you get | Needs | Parameters |
|---|---|---|---|
| `dev-database` ("Dev database") | PostgreSQL, MySQL, MongoDB or Redis on its usual port on 127.0.0.1 (`fluxctl kube add`) | `fluxctl kube up` once | `engine` (default postgres) |
| `website` ("Website") | a folder served by nginx, restarted automatically | a Docker engine | `dir`, `port` (8080) |
| `ci-runner` ("CI runner") | macOS: `fluxctl mac runner`, one fresh VM per job; Linux: an ephemeral runner container | a GitHub token file; macOS: a template image; Linux: a Docker engine | `os`, `repo`, `template` |
| `registry` ("Private registry") | a Docker registry with user `fluxvm` and a generated password | a Docker engine | `port` (5000) |
| `monitoring` ("Monitoring") | Grafana (`fluxctl kube add grafana`) | `fluxctl kube up` once | none |

## Credentials

- A generated password (the registry) is made once, written to `~/.config/fluxvm/recipes/NAME/credentials` (file 0600, directory 0700) **before**
  anything runs, reaches commands only by stdin or a 0600 file, and is never printed by `run`, `show` or logs. Running a recipe again keeps the
  same password so clients are not locked out.
- The `dev-database` and `monitoring` recipes use `kube add`'s **development defaults** (for example postgres user `postgres`, password `dev`; Grafana `admin`/`admin`).
  They are shown as defaults, not stored as secrets, and the ports are open on 127.0.0.1 only. Do not expose them.
- `ci-runner` needs a GitHub token that may create runners. Save it yourself in `~/.config/fluxvm/recipes/ci-runner/github.token` (mode 0600); `run`
  refuses a missing or group/world-readable file and never puts the token in a command line. (This is the one thing nobody can generate for you.)
  The Linux runner is the community image `myoung34/github-runner`: review it before using it on a private repository.
- The registry listens on plain HTTP on this machine (Docker accepts that for `localhost`). Put TLS in front before using it from another machine.

## Backups and undo

`show` prints each recipe's backup line (databases: `fluxctl kube db-backup ENGINE FILE`; the registry: a tar of its volume) with a suggested schedule.
The recipes do not install a scheduler; paste the line into cron or a launchd job. `uninstall` runs the undo commands printed by `show`;
`--keep-data` leaves data volumes and databases' persistent volumes in place.

## Studio

The **Recipes** page lists the catalog, shows each plan and has **Run** (which runs `fluxctl recipe run` on the machine you are looking at) and **Copy as fluxctl**
(the exact commands, without secrets). The page needs the UI bundle rebuilt (see [ui.md](ui.md)); the CLI does not.

## Adding one

A recipe is an entry in `crates/fluxctl/src/recipes.rs` (`CATALOG` and one `match` arm in `render`) plus a golden file in `crates/fluxctl/src/recipes_golden/`
(`UPDATE_GOLDEN=1 cargo test -p fluxctl recipes` writes it). Keep steps free of secrets: use `Step::WriteFile`, stdin, or `--env-file`.
