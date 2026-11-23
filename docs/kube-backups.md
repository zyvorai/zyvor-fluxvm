# Backups and restore of a k3s cluster

A cluster made by `fluxctl up` or `fluxctl kube cluster create` backs itself up. You can also back up, check and restore
by hand, from the terminal or from Studio's **Backups** card on the Home page. Works on a Mac (Apple Virtualization) and on
Linux (QEMU/KVM), because it builds on `fluxctl kube cluster backup` and `restore`, the datastore backup the cluster
lifecycle already had (see [kubernetes-unified.md](kubernetes-unified.md)).

## What you type

    fluxctl kube backup                     # back up now (verified before it is kept)
    fluxctl kube backups                    # list: name, kind, age, size, and the schedule
    fluxctl kube backup verify [BACKUP]     # prove a backup is whole and restorable (default: the newest)
    fluxctl kube restore BACKUP --yes       # replace the cluster's state from a backup
    fluxctl kube restore --latest --yes
    fluxctl kube backup schedule            # show the automatic backups
    fluxctl kube backup schedule --every 6 --keep 14
    fluxctl kube backup schedule --off      # or --on
    fluxctl kube backup prune               # apply retention now
    fluxctl kube backup --to /mnt/big       # also keep a verified copy in another folder
    fluxctl kube backup --to s3://BUCKET/PREFIX   # also upload a copy (see below)

The cluster defaults to `default` (what `fluxctl up` makes), or the only one there is; with several, add `--cluster NAME`
(`--k3s-cluster NAME` for `kube restore`). `kube restore` also still restores a checkpoint by label
(`fluxctl kube restore before-upgrade`); an argument ending in `.tar.gz` or containing a `/`, or `--latest`, means a backup.
`--json` on `backup`, `backups` and `restore` prints the machine-readable document.

## What a backup is

A gzip tar of the cluster's datastore and the token it needs, in `~/.fluxvm/apple-k3s/backups/CLUSTER/` (mode 0600), with a
`.sha256` file next to it. **It contains the cluster token: keep it private.** File names say why it was taken:
`CLUSTER-<time>.tar.gz` (by hand), `CLUSTER-auto-...` (scheduled), `CLUSTER-pre-upgrade-...`, `CLUSTER-pre-restore-...`.

- SQLite clusters (the default single server) are stopped for the copy, a few seconds of API downtime, and started again
  whatever happens. Embedded etcd clusters take a k3s snapshot and keep running.
- Right after it is written, a backup is verified (below). One that would not restore is deleted and you are told why.

## Automatic backups

On by default for every new cluster: one a day, the newest 7 kept, and 3 of the ones taken before upgrades and restores.
The FluxVM daemon checks hourly (first check 5 minutes after it starts) with `fluxctl kube backup run-due`, which does
nothing unless a backup is due; the same command works from cron if the daemon is not running. Clusters created before this
feature have no schedule until you run `fluxctl kube backup schedule --on`. The daemon looks in the home folder of the
user it runs as, so a daemon running as another user does not see your clusters; use cron then.

## Never fills the disk

Before every backup, free space on the disk that holds it must cover the newest backup's size plus half again, plus 2 GiB
left over. If not, old scheduled, pre-upgrade and pre-restore backups are removed, oldest first, to make room (the newest of
each kind stays). If that is still not enough, nothing is taken and the cluster is not touched: `BACKUP_DISK_LOW`.
**Backups you take by hand are never removed automatically.** Retention runs after every backup.

## Verify

`kube backup verify` reads the file without touching the cluster and checks: it is not empty or cut short; it matches its
`.sha256`; it is a complete gzip tar; the cluster token is inside; a datastore is inside (a SQLite file with a valid header,
or an etcd snapshot) and it is the kind this cluster uses. It exits 1 when any check fails.
This shows a backup is intact and holds what a restore needs. Only an actual restore proves the data inside is consistent.

## Before an upgrade

`fluxctl kube cluster upgrade` takes a `pre-upgrade` backup first (as before; `--no-backup` skips it). It now goes through
the same disk check, verify and retention, and the upgrade stops if the backup cannot be made. (The channel upgrade through
the system-upgrade-controller, `--channel`, still uses its own snapshot on etcd and asks for a manual backup on SQLite.)

## Restore

`kube restore BACKUP --yes` checks the backup first (a damaged one is refused before anything changes), saves the current
state as a `pre-restore` backup (if the cluster is too broken for that, it says so and goes on), then replaces the
datastore (`kube cluster restore`). SQLite keeps the old directory in the guest as `db.pre-restore-<time>`. With several
etcd servers the other servers must be wiped and rejoined, as the restore output says. Without `--yes` it only explains.

## Off-site copies

`--to DIR` keeps a second, verified copy in another folder (a bigger or mounted disk). `--to s3://BUCKET/PREFIX` uploads the
backup and its checksum to `PREFIX/CLUSTER/` with the S3 client FluxVM's VM backups use; endpoint and keys come from
`FLUXVM_S3_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` (and `AWS_REGION`, `AWS_SESSION_TOKEN`; set
`FLUXVM_S3_VIRTUAL_HOSTED=1` for bucket-in-hostname addressing). The local copy is always kept. Restoring from S3 means
downloading the file and giving its path to `kube restore`.

## Studio

Home has a **Backups** card per cluster: the schedule and free disk, **Back up now**, a list with age, size and kind,
**Verify** per backup, **Restore** (two steps: a warning, then type the backup's name; the daemon also refuses without the
matching `confirm`), a switch for the automatic backups, and **Copy as fluxctl** for each action. Admin token required.

| Route (admin) | Does |
|---|---|
| `GET /v1/k3s/backups` | clusters with backups, schedule, free disk, and the terminal commands |
| `POST /v1/k3s/{cluster}/backups` | back up now (runs `fluxctl kube backup --json`) |
| `POST /v1/k3s/{cluster}/backups/{name}/verify` | the verify report |
| `POST /v1/k3s/{cluster}/backups/{name}/restore` `{"confirm":"<name>"}` | restore; 428 without the matching name |
| `POST /v1/k3s/{cluster}/backups/schedule` `{"enabled","every_hours","keep","keep_pre_upgrade"}` | change the schedule |

Taking a backup and restoring need ssh into the cluster's guests, so the daemon runs `fluxctl` (next to it, else on PATH);
if it is not there the route answers 501 and the card offers the command to copy.

## When something fails

Every failure says what happened and ends with `more: fluxctl explain CODE`: `BACKUP_DISK_LOW`,
`BACKUP_CHECKSUM_MISMATCH`, `BACKUP_NOT_RESTORABLE`, `BACKUP_NOT_FOUND`, `RESTORE_NEEDS_CONFIRM`.

## What was and was not tested

Unit-tested: the file rules (listing, retention, room, due, verify against good, truncated, altered and wrong-datastore
archives), the schedule, name safety, the S3 upload against an in-memory store, the API routes and the UI helpers.
**Not run:** a real backup, upgrade or restore against a real k3s cluster, an upload to a real S3 service, the daemon's
hourly runner, and the Studio card in a browser.
