# FluxDisk: `fluxvm-blockd`

`fluxvm-blockd` is a small NBD block server with copy-on-write overlays. On macOS the daemon starts one per VM for
`apple.extra_disks` of `kind: overlay` and attaches each export to the guest through Virtualization.framework's NBD client as an
ordinary `nbd` disk (see [macos.md](macos.md#extra-disks)). The binary also works standalone for branching, rebasing and growing
overlay files.

**Status.** The NBD protocol, overlay format, encryption and lazy fetch are covered by in-tree tests against an in-tree client.
Virtualization.framework's NBD client has been run against it once on an Apple M4 (macOS 27.2, Debian 13 guest): connect,
read, write, persistence across restart, and an immutable base all held. A second run on an Apple M4 (macOS 27, Debian 13 guest, release build) served three exports to one guest through
Virtualization.framework's NBD client: a plain overlay, an encrypted overlay (`encrypt`, a key file) and a lazy overlay on an
HTTP base (`sha256=`). All three read back the base byte-for-byte (the guest's `sha256sum` of each disk equalled the base's); a
900 KB marker written with `O_DIRECT` and `fsync` read back identical after `drop_caches` on all three; the plain and lazy
overlay files held the marker in clear and the encrypted overlay file held none; the base file was unchanged. The lazy export
fetched 6 MiB (in 1 MiB ranges, from a Range-capable server) before the guest read the disk, the rest on demand, and a second
full read caused no new requests. Offline `rebase` and `resize` against a VZ guest's overlay were verified on an M4
(`scripts/vz-blockd-rebase-resize-live-test.sh`). Live rebase/resize while the overlay is being served currently return
success without a file lock (marker survived once); do them offline. Not exercised on hardware: structured replies against
VZ (not confirmed either way), and an encrypted overlay's `branch`. An overlay that has children is frozen and is served
read-only.

## Overlays

An overlay is a sparse file (magic `FLXOVL01`) that sits on a *parent*: a raw image, another overlay, or an http(s) URL.
Reads of blocks the overlay has not written fall through to the parent; writes land in the overlay, which allocates in 64 KiB
blocks. The parent is never modified.

Crash behaviour: data is written before its bitmap bit, so a process crash can lose a write but never exposes unwritten data.
Durability is promised only after a flush (`NBD_CMD_FLUSH`, or FUA). After power loss a set bit could in theory point at unwritten
data, as with a disk that lies about flushes; the guest must flush for durability.

## CLI

```sh
# serve (one or more exports; stops when --watch-pid / --watch-pid-file process exits)
fluxvm-blockd serve --export data=/images/base.raw,overlay=/images/vm1.ovl --listen 127.0.0.1:10809

fluxvm-blockd branch --from vm1.ovl --to vm1-branch.ovl        # new overlay on top of vm1.ovl; the parent is frozen
fluxvm-blockd rebase --overlay vm1.ovl --parent /images/base-v2.raw [--sha256 HEX]
fluxvm-blockd resize --overlay vm1.ovl --size 40G              # offline grow; K, M, G, T suffixes
```

`serve` export spec: `name=base[,overlay=path][,ro][,iops=N][,bps=N][,encrypt][,keyfile=PATH][,sha256=HEX][,parallel=N]
[,timeout=SECS][,cache=PATH]`.

- **serve.** An `overlay=` that does not exist is created on top of `base`. If `base` is itself an overlay file it becomes the
  parent (and is frozen). Without `overlay=`, an overlay `base` is opened directly and a plain image is served raw (`ro` is
  honoured). Exports are named by `name`; that is the NBD export name.
- **branch.** Creates a new overlay whose parent is the source overlay and marks the source frozen: no more writes through it.
  Use it for a snapshot you can return to: keep serving the branch, keep the frozen parent as the checkpoint. Chains are limited
  to 64 layers.
- **rebase.** Repoints an overlay at a different parent without touching its data (`qemu-img rebase -u` semantics: you assert the
  new parent has the same content). The parent path is stored relative to the overlay when possible; `--sha256` sets the expected
  hash of a lazy (URL) parent.
- **resize.** Offline grow only. In place when the reserved bitmap space suffices, otherwise the file is rewritten (allocated
  blocks copied, zero blocks skipped) to a temporary file and renamed over the original, upgrading the header to version 2. Do
  not run it on an overlay that is being served or open elsewhere: nothing stops you today (a live call returned 0 on hardware
  once with the guest marker intact; still treat the result as undefined).

## Encryption and keys

`encrypt` makes a *new* overlay AES-256-XTS encrypted per 64 KiB block (tweak = block index). The key is 32 or 64 raw bytes, or
64/128 hex digits, from `keyfile=` / `--key-file` or the hex `FLUXVM_BLOCKD_KEY` environment variable. A salted key-check value
in the header makes a wrong key fail at open instead of decrypting to garbage. A child of an encrypted parent is created
encrypted with the same key. Keys are never printed or logged.

Limits, inherent to XTS: **no authentication** (someone with write access to the overlay can flip or replace blocks undetected),
and the same plaintext at the same block index under the same key produces the same ciphertext, so a branch and its parent
share keys and tweaks. There is no key rotation or per-overlay key derivation. Only the overlay is encrypted; the base image and
the NBD connection (plaintext TCP) are not.

## Lazy base (http/https)

A `base` that is an `http(s)` URL is read through range requests in 1 MiB chunks, cached in a sparse file plus a persisted bitmap
(`<overlay>.lazy.data` / `.map` next to an overlay, or `cache=PATH` for a direct, read-only export). A chunk's bit is set only after
its data is written and synced, so a restart keeps the cache. Requests have a timeout (default 30 s) and 2 retries; a failed
chunk surfaces as an I/O error (`EIO`) to the guest. With `sha256=` the whole image is checked once every chunk is cached; a
mismatch makes the source permanently failed. That is a completeness check, **not per-read authentication**: bytes read before the
cache completed were not verified. The remote image must be immutable and the server must answer ranges with 206 (a 200
is rejected). `parallel=` (default 4) bounds concurrent chunk fetches.

## NBD export and URL

The server speaks newstyle fixed-handshake NBD over TCP: options `EXPORT_NAME`, `GO`, `INFO`, `LIST`, `ABORT`,
`STRUCTURED_REPLY`; commands `READ`, `WRITE` (+FUA), `FLUSH`, `TRIM`, `WRITE_ZEROES`, `DISC`. No TLS (`STARTTLS` is unsupported)
and no authentication: it listens where you tell it to, default `127.0.0.1:10809`. The URL for export `data` on port `P` is
`nbd://127.0.0.1:P/data`. The daemon's own servers use an ephemeral port on 127.0.0.1 and export each disk under its disk name
(an unnamed disk is `disk<position>`).

## Limits

- 64 KiB overlay block; a layer chain of at most 64 overlays.
- Up to 16 requests in flight per connection, executed in parallel; requests up to 32 MiB; `FLUSH` is a full barrier.
- `iops=` / `bps=` are token buckets with a quarter-second burst; a large request is delayed, never rejected.
- The daemon passes only `overlay`, `ro`, `iops` and `bps` to its servers. **Encryption, lazy bases, branch, rebase and resize
  are CLI features: they are not reachable through `apple.extra_disks`** (a disk whose overlay was prepared with the CLI,
  including an encrypted one, cannot be unlocked by the daemon: the key is not passed). Use the CLI on the files, or run
  `fluxvm-blockd serve` yourself and attach the export as a `kind: nbd` disk.
- An overlay file belongs to one VM: FluxVM refuses to attach or start a VM whose overlay is another record's overlay, base, or
  disk (see [macos.md](macos.md#extra-disks)). Nothing stops a second *process* from opening the same file.
- Not a replacement for backups: the overlay depends on its parent staying put and unchanged (a frozen parent is only frozen
  by convention of this tool).
