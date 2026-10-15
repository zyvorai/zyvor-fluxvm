# Homebrew package (macOS, Apple silicon)

`fluxvm.rb` installs the prebuilt package from a GitHub release: `fluxctl`, the signed `fluxvm-vz-runner`, `fluxvm-vz-switch`, `fluxvm-blockd` and `fluxvm-usernet` side by side in
`bin`, plus a `brew services` entry for the daemon. The tap is `zyvorai/homebrew-tap` (`Formula/fluxvm.rb`, next to its casks).

## Cutting a release

1. Tag and push: `git tag v0.4.0 && git push origin v0.4.0`. The `Release (macOS package)` workflow builds the package on a hosted
   macOS runner, smoke-tests it, and attaches `fluxvm-0.4.0-macos-arm64.tar.gz` and its `.sha256` to the release for that tag
   (a **draft** release is created if there is none; publish it when you are happy).
2. Fill in the formula from the package (or from the `.sha256` file's hash):
   `scripts/update-homebrew-formula.sh 0.4.0 path/to/fluxvm-0.4.0-macos-arm64.tar.gz`
3. Publish the draft release (a draft's assets cannot be downloaded), then put `fluxvm.rb` in the tap as `Formula/fluxvm.rb`.
   Users then run `brew install zyvorai/tap/fluxvm`.

## What a release publishes

One tag produces every package, and `scripts/install.sh` (the one-liner on the docs site) downloads exactly these assets:

| Asset | Built by | Used by |
|---|---|---|
| `fluxvm-<ver>-macos-arm64.tar.gz` + `.sha256` | `.github/workflows/release-macos.yml` (`scripts/package-macos.sh`) | the Homebrew formula and `install.sh` on Apple silicon |
| `fluxvm-<ver>-linux-x86_64.tar.gz` + `.sha256` | `.github/workflows/release-linux.yml` (`scripts/package-linux.sh`) | `install.sh` on Linux x86_64 |
| `fluxvm-<ver>-linux-aarch64.tar.gz` + `.sha256` | the same workflow, arm64 runner (allowed to fail) | `install.sh` on Linux arm64 |

The installer resolves "latest" with `releases/latest`, which **skips drafts**: publish the draft release, or users
keep getting the previous version. Tag, wait for both workflows, check the assets, publish, then update the formula.
`scripts/test-install.sh` exercises the installer against fake packages (no network) and runs in `site-check.yml`.

## Build the package locally

`scripts/package-macos.sh` (add `--profile debug` for a quick, large, unoptimised build) writes
`target/package/fluxvm-<version>-macos-arm64.tar.gz`. It needs `guestkit` checked out next to this repository, like the macOS CI job.

## Notes

- The runner is deliberately not stripped or re-signed after packaging: its ad-hoc signature carries the
  `com.apple.security.virtualization` entitlement, and without it Virtualization.framework refuses to start a VM.
- `fluxctl` links Homebrew's `hivex`, hence `depends_on "hivex"`.
- The package is not notarised. Downloads through `brew` are not quarantined, so it runs; a tarball downloaded in a browser may need
  `xattr -d com.apple.quarantine`.
