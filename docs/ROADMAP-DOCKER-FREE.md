# Docker-free roadmap

| Gap | PR / branch | Status |
|---|---|---|
| Native path as default | #874 (merged) | Done |
| Image builds | feat/sandbox-build | Scaffold only (not compiled); `docker build` on the socket builds macOS images only |
| Bind mounts | feat/sandbox-bind-mounts | Scaffold only (not compiled); named volumes only |
| Stacks / Compose networking | feat/sandbox-stacks-networking | `fluxctl up` + `import-compose` work; Docker network API refused; helper scaffold not compiled |

Details, workarounds and planned work for each gap: [DOCKER-FREE.md](DOCKER-FREE.md#remaining-gaps).
