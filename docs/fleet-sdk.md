# Scripting the Mac cloud: fleet SDKs and Terraform

Everything the fleet console and `fluxctl fleet` do goes through central's fleet API (`/fleet/*`). This page covers
the ways to script it: an OpenAPI description, clients for Go, Python and TypeScript, and a Terraform provider. For
what the API does (tenants, leases, quotas, billing) see [mac-cloud-tenancy.md](mac-cloud-tenancy.md).

| | Where | Status |
|---|---|---|
| OpenAPI 3.1 | [`schemas/fleet-openapi.yaml`](../schemas/fleet-openapi.yaml) | Written from central's handlers; checked to cover every routed `/fleet` path except node-agent and HA ones |
| Go | [`go/fleet`](https://github.com/zyvorai/zyvor-fluxvm/tree/main/go/fleet) (`github.com/zyvorai/fluxvm/go/fleet`) | Typed; unit tests against an in-memory fake central (`go/fleet/fleettest`) |
| Python | `fluxvm.FleetClient` ([`python/fluxvm/fleet.py`](../python/fluxvm/fleet.py)) | Standard library only; `unittest` against a fake central |
| TypeScript | `FleetClient` ([`typescript/src/fleet.ts`](../typescript/src/fleet.ts)) | No runtime dependencies; `node:test` against a mock server |
| Terraform | [`terraform-provider-fluxvm`](https://github.com/zyvorai/zyvor-fluxvm/tree/main/terraform-provider-fluxvm) | terraform-plugin-framework; tested over the plugin protocol against the Go fake |

None of these has run against a real central with real Macs yet: the tests use fakes that copy central's shapes,
statuses and tenant scoping.

## Tokens

Every client takes one bearer token:

| Token | What it can do |
|---|---|
| A tenant's primary token (`fvt_…`) | everything the tenant owns, including making and revoking API keys |
| A named API key | the same, except keys; a read-only key only reads |
| The fleet token | the operator: tenants, prices, policy, node maintenance, and anything on a tenant's behalf |

Prefer one named key per script or CI system (`fluxctl fleet keys create ci-github`), so the activity log says which
one did what and you can revoke it alone.

## Behaviour shared by all clients

- **Errors** carry central's status and its `{"error": "…"}` message. 401 bad token; 403 an operator route or a
  read-only key; 404 not found, also for another tenant's things; 409 a conflict (a Mac already leased, a lease
  released before its 24 hours, a name in use); 429 over quota; 503 no Mac can take it (or an HA standby).
- **Retries**: GET, PUT and DELETE are retried on 429, 5xx and connection failures (3 times by default, exponential
  backoff from 500 ms with jitter, capped at 10 s, a longer `Retry-After` wins up to the cap). POSTs are never retried,
  because a launch or a lease may have happened even when the answer was lost.
- **No single-VM route**: central lists VMs fleet-wide. `GetVM`/`get_vm`/`getVm` find one in the list and answer
  not-found when it is absent, but *unavailable* (503) when its Mac did not answer, so a script does not mistake an
  unreachable Mac for a deleted VM.
- **Secrets once**: tenant tokens, key tokens and webhook secrets are in the create answer only.

## Go

```go
import "github.com/zyvorai/fluxvm/go/fleet"

c := fleet.NewClient(os.Getenv("FLUXVM_ENDPOINT"), fleet.WithToken(os.Getenv("FLUXVM_TOKEN")))

lease, err := c.CreateLease(ctx, fleet.CreateLeaseRequest{Hours: fleet.Int64(48)})
img, err := c.GetImage(ctx, "macos-27-xcode")
res, err := c.LaunchVM(ctx, fleet.LaunchRequest{
    Name: "ci-1", Image: img.Image, VCPUs: img.VCPUs, MemoryMiB: img.MemoryMiB,
    Node: lease.Lease.Node, Apple: &fleet.AppleOptions{GuestOS: img.GuestOS},
})
out, err := c.Exec(ctx, res.Node, res.VM.ID, fleet.ExecRequest{Command: "xcodebuild -version"})
if errors.Is(err, fleet.ErrQuota) { /* 429 */ }
```

Options: `WithToken`, `WithHTTPClient`, `WithTimeout` (per attempt), `WithRetries`, `WithRetryDelay`,
`WithUserAgent`. `c.Do(ctx, method, path, body, &out)` reaches routes the package does not wrap.
`LaunchRequest.Extra` carries further create-request fields (cloud-init, agent, labels).

`go/fleet/fleettest` is a stateful fake central for your own tests:

```go
srv := fleettest.NewServer()            // two Macs: mac-1, mac-2 (nested virtualization)
defer srv.Close()
tok := srv.AddTenant("acme")
srv.PutImage("macos-27-xcode", "macos", "/vms/xcode.raw", 6, 16384)
srv.Fail("GET", "/fleet/vms", 503, 2)   // inject failures
```

## Python

```python
from fluxvm import FleetClient, Conflict

fleet = FleetClient("https://fleet.example.com", token=os.environ["FLUXVM_TOKEN"])
lease = fleet.create_lease(hours=48)["lease"]
res = fleet.launch_from_catalog("ci-1", "macos-27-xcode", node=lease["node"], networks=["build"])
print(fleet.exec_vm(res["node"], res["vm"]["id"], "sw_vers")["stdout"])
try:
    fleet.release_lease(lease["id"])
except Conflict as e:          # before the 24-hour minimum
    print(e.message)
```

Answers are the API's JSON as dicts. Run the tests with `cd python && python3 -m unittest discover -s tests`.

## TypeScript

```ts
import { FleetClient, Conflict } from "@zyvorai/fluxvm";

const fleet = new FleetClient({ baseUrl: "https://fleet.example.com", token: process.env.FLUXVM_TOKEN });
const { lease } = await fleet.createLease({ hours: 48 });
const { node, vm } = await fleet.launchFromCatalog("ci-1", "macos-27-xcode", { node: lease.node });
console.log((await fleet.execVm(node, vm.id, "sw_vers")).stdout);
```

## Terraform

The provider lives in [`terraform-provider-fluxvm`](https://github.com/zyvorai/zyvor-fluxvm/tree/main/terraform-provider-fluxvm), with registry docs in its
[`docs/`](https://github.com/zyvorai/zyvor-fluxvm/blob/main/terraform-provider-fluxvm/docs/index.md) and a full example in
[`examples/main.tf`](https://github.com/zyvorai/zyvor-fluxvm/blob/main/terraform-provider-fluxvm/examples/main.tf).

```terraform
provider "fluxvm" {}   # FLUXVM_ENDPOINT, FLUXVM_TOKEN

resource "fluxvm_lease" "ci" {
  hours                 = 72
  nested_virtualization = true
}

resource "fluxvm_volume" "cache" {
  name     = "xcode-cache"
  size_gib = 100
  node     = fluxvm_lease.ci.node
}

resource "fluxvm_vm" "runner" {
  name          = "ci-runner-1"
  catalog_image = "macos-27-xcode"
  node          = fluxvm_lease.ci.node
  volumes       = [fluxvm_volume.cache.name]
}

resource "fluxvm_webhook" "alerts" {
  url    = "https://hooks.example.com/fluxvm"
  events = ["lease.ending", "budget.over"]
}
```

| Resource | What | Import id |
|---|---|---|
| `fluxvm_tenant` | a tenant and its quotas (fleet token) | tenant id |
| `fluxvm_api_key` | a named key (`token` is sensitive) | `key_…` or `<tenant>/key_…` |
| `fluxvm_lease` | a whole Mac for at least 24 hours | lease id |
| `fluxvm_vm` | a VM; only `power_state` changes in place | `<node>/<vm_id>` |
| `fluxvm_network` | a private network across Macs; members change in place | name |
| `fluxvm_webhook` | signed notifications (`secret` is sensitive) | `whk_…` |
| `fluxvm_volume` | a block volume; grows in place (requires central with volumes) | name |
| `fluxvm_pool` | a warm pool; changes in place (requires central with pools) | name |

Data sources: `fluxvm_images` (the catalog), `fluxvm_nodes` (every node for the operator, leased Macs for a tenant),
`fluxvm_tenant` (yourself, or a tenant by id).

Things to plan around:
- **A lease cannot be destroyed in its first 24 hours.** Central refuses the release (the macOS licence asks for 24
  hours of sole use), so `terraform destroy` fails with the earliest release time. The operator can set
  `force_release = true`.
- **VMs are replaced, not edited**: Virtualization.framework cannot resize or re-image a VM.
- A VM whose Mac does not answer during a refresh keeps its state (with a warning) instead of being planned again.
- Tokens and secrets are known only to the run that created them, and null after an import. Keep state encrypted.

Build and test:

```bash
cd terraform-provider-fluxvm
GOPROXY=direct GOSUMDB=off go build ./... && go vet ./... && go test ./...
```

The tests drive the provider over the plugin protocol (validate, plan with Terraform's proposed new state, apply,
refresh, import, destroy) against `go/fleet/fleettest`. They check that applied values match the plan, that a second
plan is empty and which changes force a replacement. No Terraform binary is needed.

## Volumes and pools

`/fleet/volumes*` and `/fleet/pools*` come with a central that has volumes and warm pools. The clients and the
provider use these shapes:

- `POST /fleet/volumes {name, size_gib, node?}` returns a volume `{id, name, tenant, node, size_gib, attached_to,
  created_at}`. `GET /fleet/volumes[/{name}]`, `DELETE /fleet/volumes/{name}`, `POST /fleet/volumes/{name}/resize
  {size_gib}`. A launch attaches volumes with `"volumes": ["name"]`.
- `POST /fleet/pools {name, image, vcpus?, memory_mib?, min_ready, max_total?, nodes?}`, `GET /fleet/pools[/{name}]`,
  `PATCH /fleet/pools/{name}`, `DELETE /fleet/pools/{name}`.

Against a central without them, those calls answer 404.

## Not done yet

- No run against a live central with real Macs.
- No packages published (Go module tag, PyPI, npm, the Terraform registry). The provider's `go.mod` uses a `replace`
  to `../go`; a release needs a tagged `go` module first.
- Snapshots, members/SSO, sign-up and invoices, metrics and tunnels are not in the SDKs yet.
