# terraform-provider-fluxvm

The Terraform provider for the FluxVM Mac cloud (`fluxvm-agent central`). Resources: `fluxvm_tenant`,
`fluxvm_api_key`, `fluxvm_lease`, `fluxvm_vm`, `fluxvm_network`, `fluxvm_webhook`, `fluxvm_volume`, `fluxvm_pool`.
Data sources: `fluxvm_images`, `fluxvm_nodes`, `fluxvm_tenant`. Registry docs are in [docs/](docs/index.md), an
example in [examples/main.tf](examples/main.tf).

It is built on terraform-plugin-framework and the Go fleet client in [`../go/fleet`](../go/fleet) (a `replace`
directive points at it).

```bash
go build -o terraform-provider-fluxvm .
go vet ./... && go test ./...
```

The tests drive the provider over the plugin protocol the way Terraform does (validate, plan with a proposed new
state, apply, refresh, import, destroy) against an in-memory fake central (`go/fleet/fleettest`), so they need no
Terraform binary and no Macs. They check what Terraform checks: that applied values match the plan, that a second
plan is empty, and which changes replace.

To try it with a real Terraform, build it and point Terraform at the binary:

```hcl
# ~/.terraformrc
provider_installation {
  dev_overrides { "zyvorai/fluxvm" = "/path/to/terraform-provider-fluxvm" }
  direct {}
}
```

`go.mod` replaces `golang.org/x/*`, `google.golang.org/*` and `gopkg.in/*` with their GitHub mirrors (same code), so
the module builds with `GOPROXY=direct` on hosts that cannot reach those vanity domains.
