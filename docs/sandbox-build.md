# `fluxctl sandbox build`

Status: CLI + API scaffold landed. Full builder microVM wiring is the next commit.

```bash
fluxctl sandbox build -t myapp:1.0 .
fluxctl sandbox run myapp:1.0 --rm -- echo built
```

See the command implementation in `crates/fluxctl/src/sandbox_build.rs`.
