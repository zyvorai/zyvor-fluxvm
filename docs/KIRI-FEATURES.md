# Kiri-style features for FluxVM

Clean, additive notes. No existing files are overwritten.

## Already present

- k3s: `fluxctl cluster` and `fluxctl kube run` (see docs/kube-apps.md)
- Docker API adapter scaffold: crates/fluxvm-docker-api
- Local domains design: docs/LOCAL-DOMAINS.md
- share stub: crates/fluxctl/src/share.rs + docs/SHARE.md

## Next (additive only)

1. HTTP routes for the Docker adapter
2. fluxctl domains command
3. fluxctl scan command
4. Compose mapping notes (existing compose.rs is left untouched)
5. USB passthrough design
6. Import notes

Each item lands as its own focused commit on this branch.
