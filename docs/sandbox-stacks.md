# Richer stacks and Compose-style networking

Stacks already exist (see macos-stacks.md). This extends them so typical Compose files work with less friction.

```bash
# Import and run a compose file as a FluxVM stack
fluxctl stack up -f docker-compose.yml

# Or define natively
fluxctl sandbox run db:postgres -n lab --name db
fluxctl sandbox run app:myapp -n lab --expose 8080 --gateway-hosts db=db
```

## Improvements

- Better service discovery via private networks + `/etc/hosts`
- Automatic port publishing for stack services
- `depends_on` / health-check ordering
- Volume sharing between services in the same stack
- Compose import improvements (env files, build context pointers to `sandbox build`)

## Status

Design and incremental improvements on top of the existing stack machinery. Full Compose daemon parity is not the goal; covering the common cases so `fluxctl docker` is rarely needed is.

## Acceptance

- [ ] A two-service compose (app + db) runs via `fluxctl stack up`
- [ ] Services can reach each other by name
- [ ] Restart and health policies from compose are honored
- [ ] No Docker required
