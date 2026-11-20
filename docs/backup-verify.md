# Backup verification and test-restore

For changed-block-tracking backups in object storage (`/v1/vms/{id}/cbt-backup`, see `crates/fluxvm-backup-s3`). A chain is a full backup followed
by incrementals; a restore needs every link, so one bad object or missing manifest makes the newest backups unusable. Two checks find that before a
restore does.

| | reads | restores | catches |
|---|---|---|---|
| `verify` | every manifest and layer | no | missing/corrupt objects, broken chain, inconsistent manifests |
| `test-restore` | the same | into a scratch directory, then checks the image | all of the above, plus layers that do not stack into a valid image |

## verify

```sh
fluxctl backup-verify verify --dir /mnt/backups \
    --manifest ns/db/c1/00000003-b3/manifest.json
fluxctl backup-verify verify --target s3target.json --manifest ... --key-id k1 --key-file key.b64
```

`--manifest` is the manifest key of the newest backup to check (repeatable). The check walks the parent links down to the full backup (or to a
`--synthetic-base` that consolidates a parent) and reports **every** problem, not just the first:

* `manifest-missing` / `manifest-invalid`: a link of the chain cannot be read or parsed.
* `chain-broken`: a parent link names a backup the chain does not hold, sequences do not increase, a layer belongs to another chain, the base is an
  incremental, a layer set lacks a disk the restore point has, or the links loop.
* `manifest-inconsistent`: a manifest stored under a key that does not match its chain/sequence/backup id, a layer key outside its backup's directory,
  duplicate disks, a zero virtual size or a malformed digest.
* `layer-missing`, `size-mismatch`, `digest-mismatch`: the layer object is gone, differs from `stored_size`, or its SHA-256 differs from the
  manifest. For an encrypted layer the digest is of the plaintext, so it is decrypted first (`--key-id`/`--key-file`, the base64 32-byte key in a file,
  never on the command line); `decrypt-failed` means the GCM authentication failed.

Without the key, encrypted layers are only size-checked and counted in `unverified_layers` with a warning; the report is still `ok` if nothing else
is wrong. Nothing is written to the object store; downloads go to a private directory under `--scratch` (default `<state_dir>/backup-verify-work`),
removed afterwards.

## test-restore

```sh
fluxctl backup-verify test-restore --dir /mnt/backups --manifest .../manifest.json
```

Downloads and verifies the chain, stacks the layers with `qemu-img rebase -u`, flattens the top into a standalone qcow2 in the scratch directory,
then runs `qemu-img check` and `qemu-img info` on it and compares the virtual size with the manifest (leaked clusters are reported but are not an
error). `qemu-img` is the binary `qemu_img_binary` names in the config - the same one backups and restores already use. If it cannot be run, the check
falls back to the qcow2 header of every layer (magic, version, virtual size) with a warning, and the report says `imageCheck: "header"`. A chain that
failed verification is not restored. No VM is touched; the scratch files are deleted.

## Scheduled verify jobs

A job names a target, chain points, a schedule and a mode:

```sh
fluxctl backup-verify job add nightly --dir /mnt/backups \
    --manifest ns/db/c1/00000003-b3/manifest.json --every 1d --ignore-verified --outdated-after 7d
fluxctl backup-verify job add s3check --s3-endpoint https://s3.example --s3-bucket backups \
    --access-key-id-env BACKUP_AK --secret-access-key-env BACKUP_SK --key-id k1 --key-env BACKUP_KEY \
    --manifest ... --every 6h --mode test-restore
fluxctl backup-verify job ls | run NAME [--force] | rm NAME
```

* The daemon checks for due jobs every 30 seconds and runs them one at a time. `--every` is at least 60s.
* `--ignore-verified` with `--outdated-after` (PBS `ignore-verified` / `outdated-after`): a chain point that verified **clean** less than that long
  ago is skipped, so a frequent schedule re-reads only what is new or stale. A failed chain point is never skipped. `job run --force` ignores it.
* **Secrets are not stored.** `jobs` live in `<state_dir>/backup-verify-jobs.json`; an S3 job names the *environment variables of the daemon*
  that hold the access key, secret and encryption key. A job whose variable is unset fails with that name in the message.
* Results (per chain point: when, ok, issue count, summary) are kept in `<state_dir>/backup-verify-state.json`, shown by `job ls`.

Each job targets chain points you list: after new backups are taken, update the job (`job add --replace`) to name the new newest manifest.

## Reporting

* **Event** per result in `events.jsonl` / `GET /v1/events`: `backup.verify` or `backup.test_restore` with `job` (`-` for a CLI run), `manifest`,
  `result=ok|failed`, `issues`, `layers` and a one-line `summary`. A job that could not run at all (unset credential, unreachable store) is a
  `failed` event too. Failures also log at warn level.
* **CLI exit status**: 0 when everything checked clean, **3** when a check found a problem, 1 when it could not run (bad arguments, unreadable key
  file). The full report is JSON on stdout, a summary line per chain on stderr.

## Not covered

* Verification trusts the manifests it reads; it cannot know about backups that are absent from the chain you name.
* Only CBT object-store chains. The local qcow2 backups under `state_dir/backups` (`fluxctl backup`) are standalone files and are not checked here.
* The S3 path shares the code of the existing S3 store but, as of this change, was exercised only against directory and in-memory stores in tests.
