# Richer stacks

Status: helper scaffold only. `crates/fluxctl/src/sandbox_stack.rs` is not compiled into `fluxctl`; the existing `fluxctl up` /
`fluxctl import-compose` path ([macos-stacks.md](macos-stacks.md)) is what runs today. See
[DOCKER-FREE.md](DOCKER-FREE.md#3-docker-networks-and-compose-as-a-daemon).

```bash
fluxctl stack up -f docker-compose.yml
```

Implementation: `crates/fluxctl/src/sandbox_stack.rs`.
