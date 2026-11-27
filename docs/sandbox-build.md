# `fluxctl sandbox build`

Status: **not available yet.** `crates/fluxctl/src/sandbox_build.rs` is a scaffold (flags plus a TODO) that is not compiled into
`fluxctl`, so the command below does not exist on `main`. See [DOCKER-FREE.md](DOCKER-FREE.md#1-image-builds) for what works
today and the workarounds.

```bash
fluxctl sandbox build -t myapp:1.0 .
fluxctl sandbox run myapp:1.0 --rm -- echo built
```

See the command implementation in `crates/fluxctl/src/sandbox_build.rs`.
