# Mac cloud: tenants, leases, drain and metering

`fluxvm-agent central` can run a shared fleet of Macs as a cloud for several teams or customers. This page covers the
tenancy layer: tenants and their tokens, Mac host leases, quotas, scrubbing, draining and usage metering. For the fleet
itself (registry, node heartbeats, placement) start with [mac-cloud.md](mac-cloud.md).

**Status:** implemented with unit and in-process HTTP tests. Not yet run against real Macs.

## Why leases

Apple's macOS licence limits macOS virtualisation on a Mac to two extra copies. It also allows a Mac to be leased only
when one lessee has sole use of it for at least 24 consecutive hours. AWS's 24-hour minimum for EC2 Mac hosts comes from
the same rule. A cloud that hands out macOS VMs per minute on a shared Mac does not fit that, so the unit a tenant rents
here is a **whole Mac, for at least 24 hours**. Inside its lease a tenant can create and delete as many short-lived VMs
as it likes, for example one per CI job. Linux VMs have no such restriction and can still run on shared, unleased nodes.

This is an engineering control, not legal advice. The rules are defaults in `GET/PUT /fleet/policy`:

| Policy field | Default | Effect |
|---|---|---|
| `require_lease_for_macos` | `true` | A tenant's macOS guests run only on Macs that tenant leases |
| `exclusive_leased_hosts` | `true` | A leased Mac runs only its lessee's VMs |
| `min_lease_hours` | `24` | Values below 24 are raised to 24 |
| `ledger_retention_days` | `400` | Ended VMs and leases older than this are dropped by the reconciler (`0` keeps all) |
| `isolate_tenant_networks` | `false` | Each tenant's `vz` VMs join their own vmnet network (`fluxvm-tenant-<id>`, one /24 each) |
| `tenant_subnet_prefix` | `10.230` | Tenant n gets `10.230.n.0/24` (indices are assigned once and kept) |
| `allow_shared_pool_claims` | `false` | Tenants may take VMs from shared warm pools (the operator's). Off: their VMs share a network and image credentials between tenants ([warm-pools.md](warm-pools.md#limits)) |
| `signup_require_email_verification` | `true` | A self-service sign-up is created only after a mailed link is confirmed ([mac-cloud-billing.md](mac-cloud-billing.md#email-confirmation)) |

## Console

`http://<central>/console/` is the cloud's web console, [FluxVM Studio](ui.md). The page needs no token: it asks for one,
keeps it in the tab's session storage and sends it only to this central.

> **Studio and the previous console.** The page list below was written for the previous console, which stays at
> `/console/legacy` for one release. Studio covers the same VM tools (screen and Control, snapshots and clones, commands),
> the image catalog, hosts (cordon, drain, scrub, lease), networks, volumes, warm pools, tunnels, enrollment, operations,
> tenants and keys, members, billing and activity. Page names differ: for example Snapshots, Screen and Run are tabs in a VM's sheet.

With a **tenant token** you get:
- **Overview**: a getting-started checklist (lease a Mac, launch a VM, connect VMs), quota meters, your Macs and recent VMs.
- **My Macs**: each Mac shows its two macOS slots (Apple's licence limit) and how far the lease is through its 24-hour
  minimum. Select one to launch a VM on it or release it once the minimum is over.
- **Virtual machines**: search and filter, and a launch sheet that picks an image from the catalog. It suggests a name
  and size, shows how much quota the VM takes, offers only Macs with a free macOS slot, and can join private networks
  or delete the VM automatically. Select a VM to run commands in it through the guest agent, or delete it after typing
  its name.
- **Networks**: create one across your Macs, see each Mac's address range, add or remove Macs, rotate the key.
- **Screen**: a VM's details show its display, refreshed every 2 seconds while open, so you can watch macOS boot,
  finish setup or see why a job hangs, without VNC. It comes through central from the node's screenshot API.
- **Power and control**: Start, Stop, Restart, Pause and Resume in a VM's details. **Control** opens the screen in a
  large view where your clicks, scrolling and typing go to the guest (macOS VMs), with a box for pasting text and
  buttons for Return, Esc, Tab and ⌘ Space, enough to get through Setup Assistant or a stuck dialog. The console log is
  one click away.
- **Snapshots and clones**: a VM's details list its snapshots (when, how big, disk only or disk and memory). **Take
  snapshot** names one; **Restore…** puts the VM back to it after asking; **Delete…** asks too; **New VM from this…**
  and **Clone VM…** make a new VM on the same Mac. See [Snapshots and clones](#snapshots-and-clones).
- **Activity**: every change made with your token (launches, deletes, leases, networks, commands) and whether it worked.
- **Images** (the catalog) and **Usage** (7, 30 or 90 days, with a CSV download).

The **fleet token** adds the operator's pages:
- every Mac, with what needs attention first (not responding, needs a scrub, finished draining);
- cordon, drain (with a choice of what happens to running VMs), scrub, and lease a Mac to a tenant;
- tenants: create with quotas, edit quotas, disable, issue a new token (shown once), delete;
- catalog editing and the policy.

There is a command palette (⌘K or Ctrl-K) to jump to any page, VM, Mac, network or tenant, and to start any action.
Pages are reached from the top bar's grouped menus (Overview, Cloud, Compute, Billing, Access); ⌘K opens search and
the account menu holds sign-out and team switching. The console follows the system's light or dark mode unless you pick one, and it works on a phone. Links to a page or
an item can be shared (`/console#/vms/<mac>/<id>`).

## Command line

`fluxctl fleet` talks to the same API (`--central`, default `http://127.0.0.1:7799`, or `CENTRAL_URL`; `--token` or
`FLUXVM_AGENT_TOKEN`). On a terminal it prints tables, short confirmations and the next command to run. Piped, or
with `-o json`, it prints the API's JSON unchanged, so scripts keep working. `-o table` and `-o wide` force the tables
(`wide` adds IDs, labels, addresses and disk paths).

```text
$ fluxctl fleet status
Acme Mobile  tenant acme, http://fleet:7799

Macs    ████████████░░░░  2 of 3
VMs     ████████░░░░░░░░  5 of 10
vCPUs   ███████░░░░░░░░░  20 of 48
Memory  ████░░░░░░░░░░░░  48 GiB of 192 GiB

MAC       LEASE                MACOS  VMS  FREE
studio-1  releasable now       ●●     3    10 vCPUs, 156 GiB
mini-1    releasable in 19h    ●○     2    6 vCPUs, 20 GiB

Next: fluxctl fleet vms

$ fluxctl fleet launch --image macos-27-xcode --ttl 8h
✓ Launched macos-27-xcode-1 on mini-1 (macOS 27 with Xcode, 6 vCPUs, 16 GiB)
  ID  0b9c…
Next: fluxctl fleet run macos-27-xcode-1 -- sw_vers
```

| Command | What |
|---|---|
| `fleet status` | A tenant's quota, Macs and next step; for the operator, fleet health and what needs attention |
| `fleet launch [NAME] --image I` | Launch from the catalog (or `--disk PATH --guest macos`); `--mac`, `--network` (repeatable), `--ttl 8h`, `--vcpus`, `--memory-gib` |
| `fleet run VM -- CMD…` | Run a command in a VM found by name or ID prefix; prints the guest's output and exits with its code |
| `fleet screen VM [-f out.png]` | Save a picture of a VM's display |
| `fleet events [--limit N]` | The activity log (with the API key used) |
| `fleet keys [create\|revoke]` | A tenant's named API keys |
| `fleet prices [--set FILE]`, `fleet bill`, `fleet budget [AMOUNT]` | Prices, monthly statements, budgets |
| `fleet notifications`, `fleet webhooks [add\|rm\|test]` | Notifications and webhooks |
| `fleet start\|stop\|restart\|pause\|resume VM` | Power a VM (stop and restart ask first on a terminal) |
| `fleet logs VM [--lines N]` | The end of a VM's console log |
| `fleet snapshot VM [--name N] [--memory]`, `fleet snapshots VM` | Take a snapshot (disk, plus memory with `--memory`); list them |
| `fleet restore VM SNAPSHOT [-y]`, `fleet snapshot-rm VM SNAPSHOT [-y]` | Put a VM back to a snapshot; delete one (both ask on a terminal) |
| `fleet clone VM NEW-NAME [--snapshot S]` | A new VM on the same Mac, from a snapshot or the stopped VM |
| `fleet rm VM` | Delete a VM by name (asks on a terminal; `-y` to skip) |
| `fleet vms`, `fleet nodes`, `fleet node M` | VMs and Macs, with macOS slots (`●○`) and when each lease can be released |
| `fleet lease create\|list\|get\|release` | Leases (`create --hours 72 --min-vcpus 16 --nested`) |
| `fleet images [add\|remove]` | The image catalog |
| `fleet networks [show\|create\|delete\|rotate\|add-mac\|remove-mac]` | Private networks across Macs |
| `fleet tenant create\|list\|get\|me\|update\|rotate-token\|delete` | Tenants (the token is printed once) |
| `fleet drain`, `drain-status`, `scrub`, `cordon`, `uncordon`, `usage`, `reconcile` | Operator tasks |
| `fleet volumes`, `volume-create NAME --size GiB [--node M]`, `volume-resize NAME --size GiB`, `volume-rm NAME` | Persistent volumes; `fleet launch --volume NAME` attaches one (see [Volumes](#volumes)) |

A refused call shows central's own message plus a hint: a 401 says which flag carries the token, a 429 points at the
quota, a standby names the leader, and an unreachable central says how to point `--central` at it.

## Prices and billing

The operator sets a price list; central turns metered use into a statement per tenant per month.

```bash
fluxctl fleet prices --set prices.json      # operator
fluxctl fleet bill                          # this month; --month 2026-09, --csv for invoicing
fluxctl fleet budget 500                    # a tenant's own monthly budget
```

```json
{"currency": "USD",
 "mac_classes": [{"name": "Mac mini", "min_vcpus": 8, "min_memory_gib": 16, "per_hour": 0.85},
                 {"name": "Mac Studio", "min_vcpus": 20, "min_memory_gib": 64, "per_hour": 1.95}],
 "mac_per_hour": 0.60,
 "vm_per_hour": 0.01, "vcpu_per_hour": 0.02, "memory_gib_per_hour": 0.004, "macos_vm_per_hour": 0.10,
 "charge_vms_on_leased_macs": false}
```

- **Leases** are priced by the Mac's class: the largest class (by minimum vCPUs and memory) the Mac meets, else
  `mac_per_hour`. The price is fixed when the lease starts and stored on it, so a price change never re-prices a
  running lease. A lease is billed for at least 24 hours, as before.
- **VMs** are charged only on shared hosts (`vm_per_hour` + `vcpu_per_hour` × vCPUs + `memory_gib_per_hour` × GiB, plus
  `macos_vm_per_hour` for a macOS guest), because a lease already covers everything on the Mac: only the hours a VM
  runs inside the tenant's lease of its Mac are free, any time before or after the lease is charged. Set
  `charge_vms_on_leased_macs` to charge them there too.
- **Statements** (`GET /fleet/billing?month=YYYY-MM[&tenant=T][&format=csv]`) have a line per lease and per kind of VM
  time, a total, and for the current month a forecast at the same rate. Amounts are computed in millionths of the
  currency and rounded to cents only for display, so lines add up exactly. An active lease is billed up to its
  `ends_at` at most. In the CSV, a field that starts with `=`, `+`, `-` or `@` gets a leading `'` so a spreadsheet
  never runs it as a formula.
- **Budgets** are the tenant's own (`PUT /fleet/tenants/me/budget {"monthly": 500}`): the console warns at 80% and when
  over, and `fleet bill` says so. Nothing is stopped.
- The console shows month-to-date spend on a tenant's Overview, a cost estimate when leasing a Mac or launching a VM,
  a Billing page with statements and the budget, and (for the operator) the price list and every tenant's statement.

### Self-service sign-up and Stripe invoices

Central can let people create their own account from the sign-in page (`POST /signup`) and send each tenant's monthly
statement as a Stripe invoice (`POST /fleet/billing/invoices?month=…`), with Stripe Checkout to save a card and a signed
webhook for payment status. Both are off by default; see [mac-cloud-billing.md](mac-cloud-billing.md).

## Snapshots and clones

```bash
fluxctl fleet snapshot ios-build-1 --name before-upgrade            # disk only, in any state
fluxctl fleet snapshot ios-build-1 --name warm --memory             # disk and memory: the VM must run (or be paused)
fluxctl fleet snapshots ios-build-1
fluxctl fleet restore ios-build-1 before-upgrade                    # asks first
fluxctl fleet clone ios-build-1 ios-build-2 --snapshot warm         # a new VM on the same Mac
fluxctl fleet snapshot-rm ios-build-1 warm
```

A snapshot is taken by the VM's Mac and stays there, under the VM's workspace (`snapshots/<name>/`):

- **Disk only** (the default): an APFS clone (`clonefile`, `cp -c`) of the disk and firmware files. It takes seconds
  and uses space only as the VM changes afterwards. A stopped VM is copied as it is; a running one is paused for the
  copy and resumed, so the copy is crash-consistent. Restoring boots the VM cold from it.
- **Disk and memory** (`include_memory`, `--memory`): Virtualization.framework also saves the guest's memory and device
  state (`saveMachineStateTo`, macOS 14 or later). The VM has to be running or paused. Restoring resumes it exactly
  where it was.

**Restore** stops the VM, puts the snapshot's disk and firmware back, and starts it (resuming the saved memory, if any).
Everything written since the snapshot is lost; the snapshot itself is unchanged and can be restored again. **Clone**
makes a new VM on the same Mac with the source's settings, size and networks, from a snapshot's disk (the source may be
running) or from the source's own disk (it must be stopped). A macOS clone gets the source's hardware model and
auxiliary storage but a machine identifier of its own, and needs a free macOS slot on that Mac. A clone belongs to the
source VM's tenant and counts against its quota like a launch; a tenant cannot clone onto a cordoned or draining Mac.

Names are 1-64 letters, digits, `.`, `_` and `-`, starting with a letter or digit (default `snap-<UTC time>`), and
unique per VM. Each VM keeps at most `max_snapshots_per_vm` snapshots: 20 by default, set in the policy (0 turns
snapshots off) and overridable per tenant in its quota (`"quota": {"max_snapshots_per_vm": 5}`). Taking one over the
limit is a `429`; taking one with memory of a stopped VM is a `409`. A snapshot is deleted with its VM.

Central keeps a record of each snapshot (VM, tenant, size, when it was taken and deleted) for the limit and for
billing: with `snapshot_gib_per_hour` in the price list, statements get a **Snapshot storage** line (GiB-hours of each
snapshot's size, as its Mac last reported it, for as long as it was kept, on leased Macs too). Listing a VM's snapshots
brings the record in line with the Mac. Taking, restoring and deleting snapshots and cloning are in the activity log
(with the snapshot's or new VM's name) and send `vm.snapshot`, `vm.restored` and `vm.cloned` notifications.

On the Mac, these are the node API's `POST /v1/vms/{id}/snapshot` (`{"tag", "memory": false}` for disk only),
`GET /v1/vms/{id}/snapshots`, `DELETE /v1/vms/{id}/snapshots/{tag}`, `POST /v1/vms/{id}/restore` and
`POST /v1/vms/{id}/clone` (`{"name", "snapshot"}`).

## Notifications and webhooks

Central turns what happens into notifications, shown under the console's bell and on its Notifications page, and
sends them to webhooks.

| Event | Who | When |
|---|---|---|
| `vm.created`, `vm.deleted` | tenant | a VM is launched or deleted through central |
| `lease.created`, `lease.released` | tenant | a Mac is leased or released |
| `lease.ending` | tenant | a lease with an end time has 24 hours left, then 1 hour |
| `lease.ended` | tenant | the reconciler ended a lease at its end time |
| `budget.near`, `budget.over` | tenant | the month's spend reaches 80%, then 100% of the budget |
| `node.down`, `node.scrub`, `drain.done` | operator | a Mac stops reporting, needs a scrub, finishes draining |
| `volume.created`, `volume.deleted` | tenant | a volume is created or deleted through central |

Time-based events are found by the reconciler (every `--reconcile-secs`, 60 by default) and sent once. The record of
what was sent lives in memory, so after a restart an event that is still true may be sent once more.

```bash
fluxctl fleet webhooks add https://hooks.example.com/fluxvm --event lease.ending --event budget.over
fluxctl fleet webhooks test whk_51a0c2e9b3f1
fluxctl fleet notifications
```

A webhook receives a `POST` with the notification as JSON (`id`, `type`, `time`, `tenant`, `title`, `detail`, `link`)
and the headers `X-FluxVM-Event`, `X-FluxVM-Delivery` and `X-FluxVM-Signature: sha256=<hex>` (HMAC-SHA256 of the body
with the webhook's secret, the same scheme as GitHub's). A delivery is tried up to three times (at once, after 2 s,
after 10 s more); a 2xx answer is success. The last 20 deliveries per webhook are shown in the console.

- A tenant's webhooks get only that tenant's events; the operator's get everything. Each owner has at most 10.
- A tenant's webhook must use `https` and reach a public address. The host is resolved at every delivery: if any of
  its addresses is loopback, private, link-local or otherwise non-public the delivery is refused, and the connection
  is pinned to the address that was checked, so the webhook cannot be turned against the network central runs in.
  Redirects are not followed. The operator's webhooks may point anywhere.
- Secrets are stored in the tenancy state (mode 0600, mirrored with HA) and shown once, at creation.

## API keys

A tenant's primary token is the one the operator hands over. Besides it, a tenant can hold up to 50 named API keys:
one per person, CI system or tool, so the activity log says which one did what and one can be revoked without
breaking the others.

```bash
fluxctl fleet keys create ci-github                   # full access, expires in 90 days; prints the key once
fluxctl fleet keys create grafana --read-only --expires-days 0
fluxctl fleet keys                                    # name, access, last used, expiry
fluxctl fleet keys revoke ci-github
```

- A key acts as its tenant, with the same quotas and visibility. A **read-only** key can only `GET`.
- Only the primary token (or the operator, with `--tenant` / `/fleet/tenants/{id}/keys`) creates and revokes keys, so a
  leaked key cannot mint more. An expired key is refused like an unknown one.
- Revoking takes a key's id or its name (an id wins; names may not look like ids). The console revokes by id.
- Rotating the primary token (`POST /fleet/tenants/{id}/rotate-token`) does **not** revoke named keys: CI keeps
  working. If the primary token leaked, also list the tenant's keys and revoke any nobody recognises (the activity log
  shows who created each). Disabling a tenant refuses its keys too; deleting it deletes them.
- Only a key's hash is stored. `last_used_at` is updated at most once a minute and saved with the next change to the
  tenancy state.
- The console has an **API keys** page for tenants, and the operator manages keys in a tenant's details.

## SSH and port tunnels

Tenants reach their VMs' SSH and other TCP ports through central, so no Mac and no node API has to be reachable from
the tenant's network. Central is the only thing they connect to, with the same token as for everything else.

```bash
export CENTRAL_URL=https://cloud.example.com FLUXVM_AGENT_TOKEN=fvt_…
fluxctl fleet ssh web-1                              # the system ssh, as the image's ssh_user (or admin)
fluxctl fleet ssh debian@web-1 -- -L 5432:localhost:5432
fluxctl fleet port-forward web-1 8080:80             # 127.0.0.1:8080 -> port 80 of web-1; --address to change
fluxctl fleet tunnels                                # open tunnels: VM, port, traffic, idle time
fluxctl fleet tunnels close tun_5c1e09a2b7f4
```

`fluxctl fleet ssh` runs your own `ssh` with `-o ProxyCommand="fluxctl fleet --central … tunnel-stdio <node>/<id> %p"`
and `HostKeyAlias=fluxvm-<id>`, so keys, agents, `scp`, `rsync -e ssh` and `~/.ssh/config` work as usual; the token
reaches the ProxyCommand through the environment, never a command line. The VM inspector in the console shows the
commands and an SSH config block to paste into `~/.ssh/config`:

```
Host web-1
  User admin
  HostKeyAlias fluxvm-<vm id>
  ProxyCommand fluxctl fleet --central https://cloud.example.com tunnel-stdio <node>/<vm id> %p
```

How it works: the client opens a WebSocket to `GET /fleet/vms/{node}/{id}/tunnel?port=N` (default 22) with its bearer
token. Central checks that the caller owns the VM (404 otherwise), that the key is not read-only, that the port is
allowed and that the tenant has a tunnel to spare. Then it opens a WebSocket to the node's own
`GET /v1/vms/{id}/tunnel?port=N` with the fleet token and relays binary messages both ways, each direction waiting for
its write before reading again (backpressure, bounded memory). The node connects to the guest only:

- a VM on `apple.usernet` through the host listener of its own TCP forward for that guest port (add one to
  `apple.usernet.port_forwards`; the guest network is not routable from the Mac);
- a `tap` + `netns` VM from inside its network namespace;
- any other VM at the address the node knows for it: the NAT address the vz runner reports, or its DHCP lease. A vz
  guest's address is what the guest itself printed on its serial console, so it is used only when it lies on one of that
  Mac's own VM networks (a `bridge*` interface's subnet) and no other VM on the node reports the same address. A guest
  that announces a LAN host or another tenant's VM gets `409`, not a connection.

Loopback, unspecified and multicast addresses are refused, and so are link-local ones (cloud metadata services live
there), except the `169.254.x.y` a VM in its own network namespace is given. Central only ever connects to a registered node's API,
so a tunnel cannot reach central's or a Mac's own services. If a node's `fluxctl serve` requires tokens, give it the
fleet token as an admin token, or central's tunnels to it are refused (`502`).

The policy (`GET/PUT /fleet/policy`, **Policy** page in the console) has a `tunnels` block:

```json
"tunnels": {"enabled": true, "ports": ["22", "1024-65535"], "idle_timeout_minutes": 60, "max_lifetime_minutes": 480, "max_per_tenant": 20}
```

- `ports`: single ports and inclusive ranges (at most 64 entries). Other ports get `403`.
- `idle_timeout_minutes` (1-1440): a tunnel with no bytes either way for this long is closed. `ServerAliveInterval` in
  ssh keeps an idle shell open.
- `max_lifetime_minutes` (1-10080, default 480): a tunnel is closed this long after it opened, however busy it is, so
  steady traffic cannot keep one (and the access it was opened with) alive for ever. Reconnect to continue.
- `max_per_tenant` (1-1000): more open tunnels get `429` until one closes. The operator counts as one more tenant.

Opening (method `OPEN`, status `101`, or the refusal's status) and closing (`CLOSE`, with bytes sent and received, how
long, and why: the client disconnected, the VM closed the connection, idle timeout, closed by the tenant or the
operator) are written to the activity log. `GET /fleet/tunnels` lists the open tunnels (a tenant sees its own) with
the policy's limits; `DELETE /fleet/tunnels/{id}` closes one at once. The console's **Tunnels** page shows them per
tenant with a Close action. Tunnels live in the memory of the central that relays them: restarting it, or a failover
to another central, ends them, and clients connect again.

What ends a tunnel: either side closing, the idle timeout, the maximum lifetime, an operator or the tenant closing it,
the VM stopping, the tenant or the API key it was opened with being disabled, expired or made read-only, the person who
opened it from the console being removed or demoted to a viewer (checked every 30 seconds), or central restarting.
What does **not** end one: signing out of the console, or the tenant's primary token being rotated: a tunnel is
re-checked for the tenant, the key, the member's role and the VM, not re-authorised per byte, and it is not tied to a
session token. To cut such access off at once, close the tunnels from the **Tunnels** page (or
`DELETE /fleet/tunnels/{id}`); otherwise the lifetime ends them within eight hours by default.
## Sign-in with SSO and team members

People can sign in to the console with the company's identity provider (OpenID Connect: Okta, Microsoft Entra ID,
Google, Keycloak, Auth0, ...) instead of pasting a token, and each person gets a role in their tenant. Tokens and API
keys keep working as before, and `fluxctl` keeps using them.

### Turn it on

Register an application (web, authorization code) at the provider with the redirect URI
`https://<central>/auth/callback` and the scopes `openid email profile`, then start central with:

```bash
fluxvm-agent central --token "$FLEET_TOKEN" \
  --public-url https://cloud.example.com \
  --oidc-issuer https://acme.okta.com --oidc-client-id 0oa1b2c3 \
  --oidc-client-secret-file /etc/fluxvm/oidc-secret \
  --oidc-provider-name Okta \
  --sso-operator ops@example.com            # repeatable; these emails sign in as the operator
```

| Flag (environment) | Meaning |
|---|---|
| `--oidc-issuer` (`FLUXVM_OIDC_ISSUER`) | The issuer; discovery is read from `{issuer}/.well-known/openid-configuration`. https only (http for localhost). |
| `--oidc-client-id`, `--oidc-client-secret` / `--oidc-client-secret-file` | The application. Without a secret central is a public client; PKCE is used either way. |
| `--public-url` (`FLUXVM_PUBLIC_URL`) | Central's URL as browsers reach it. With https, the session cookie is `Secure`. |
| `--oidc-provider-name` | The button's label: "Sign in with Okta". |
| `--sso-operator` (`FLUXVM_SSO_OPERATORS`, comma-separated) | Operator emails. `operator_emails` in the policy (`PUT /fleet/policy`) adds more without a restart. |
| `--session-hours` (default 12) | How long a console session lasts. |

SSO needs `--token`. The console shows **Sign in with ...** when `GET /auth/config` says SSO is on.

### How a sign-in works

`GET /auth/login` sends the browser to the provider with the authorization code flow and PKCE (S256). `state`,
`nonce` and the PKCE verifier stay in central's memory for ten minutes, and `state` is also bound to the browser by a
short-lived cookie, so a sign-in link cannot be finished in another browser. `GET /auth/callback` swaps the code at the
provider's token endpoint and verifies the ID token:

- the signature, with the provider's JWKS (fetched from discovery, cached for an hour, fetched again for an unknown key
  id at most every 30 seconds): RS256/384/512, PS256/384/512, ES256/384; never `none` or HMAC;
- `iss` is the configured issuer, `aud` contains the client id (and `azp` is the client id when there are several
  audiences), `exp` has not passed (60 seconds of clock skew), `nonce` is this sign-in's;
- `email` is present and `email_verified` is true (providers that do not send `email_verified` are refused).

The verified email decides where the person lands: an operator email signs in as the operator; a member of a tenant
signs in to that tenant with their role (`/auth/login?tenant=acme` picks one for someone in several teams, which the
console's **Switch team** does); anyone else gets a page saying they have no access and whom to ask. Nobody is created
on the fly. Signing in again does not need the password at most providers, since the provider has its own session.

### Sessions

A sign-in issues a session: a random token, stored only as its SHA-256 in the tenancy state (so it survives a restart
and follows HA failover), sent as the `fluxvm_session` cookie (`HttpOnly`, `SameSite=Lax`, `Secure` with an https
public URL, 12 hours by default). Who the person is is looked up again on every request, so a role change applies at
once, and removing a member, deleting their tenant or removing an operator email ends their sessions. Sign-out
(`POST /auth/logout`) deletes the session; it does not sign the person out of the provider. Each person keeps at most
20 sessions (the oldest go first).

A request with a bearer token ignores the cookie. A change (anything but `GET`) made with the cookie must carry the
`X-FluxVM-Console` header, which the console sends and a cross-site form or `fetch` cannot, and, when the browser sends
an `Origin`, come from the public URL; otherwise it is refused with `403`.

### Roles

| Role | May |
|---|---|
| `viewer` | Look at the tenant's VMs, usage graphs, invoices and open tunnels; change nothing, open no tunnel, and **no live VM screens or console logs** (both can contain secrets: a typed password, an environment dump). |
| `member` | Also launch, control, run commands in and delete VMs, take, restore and delete snapshots, clone VMs, open SSH and port tunnels, and manage volumes and private networks. |
| `admin` | Every tenant action: also leases, warm pools, closing tunnels, payment methods, API keys, members, the budget and webhooks. Cannot change or remove an owner, or make someone an owner. |
| `owner` | Everything, including other owners. The last owner cannot be demoted, or removed while others remain. |

The tenant's primary token acts as an owner; a named API key acts as an admin (a read-only key only reads) but cannot
change members, the same way it cannot change keys, so a leaked CI key cannot invite its holder. The console hides what
a role cannot do, and central refuses it anyway (`403` with the reason). New tenant routes are closed to members by
default: a feature that wants members to use it adds its path in `role_refusal` (`crates/fluxvm-agent/src/central_sso.rs`).

Someone who signed up (`POST /signup`) becomes their tenant's owner the first time they sign in with SSO with the same
email, as long as the tenant has no members yet (it is not made a member before the identity provider has verified the
address; the sign-up's token works meanwhile). With email confirmation on (the default) the address was already
confirmed by a mailed link, so the token, keys and webhooks stay. **Without confirmation, at that moment the tenant's
primary token is rotated and its named API keys and webhooks are removed**, because whoever held the sign-up token may
not be that person (anyone can sign up with someone else's address); the new owner mints new ones from the console.

### Members

```bash
fluxctl fleet member-add alex@example.com --role admin      # a tenant's primary token, or the operator with --tenant
fluxctl fleet members                                        # email, role, last sign-in, who added them
fluxctl fleet member-role alex@example.com viewer
fluxctl fleet member-rm alex@example.com
```

The operator adds a new tenant's first owner (console: the tenant's details, **Members**); from then on the team
manages itself on its **Members** page. Adding someone sends nothing: tell them the console's address. A tenant has at
most 500 members, emails are matched lowercase, and one person can be in several tenants.

Every change made in a session is in the activity log with the person's email (`user`), and so is each sign-in
(`POST /auth/session`). A first sign-in by an email nobody added is refused; a self-service sign-up would hook in at
that point (`access_for` in `central_sso.rs`), and is not built.

## Activity log

Central records every change made through it: the time, who (`operator` or the tenant), the method and path, the
name or id from the request body if there is one (a VM name, a tenant id), and the HTTP status. Reads and node
heartbeats are not recorded, a command's body is never read, and nothing else from a request is kept, so the log never
holds a VM spec, a command, a token or a key. A tenant's refused attempt at an operator route (403) is recorded too;
calls without a valid token are not. The newest 5000 events are served by `GET /fleet/events`; all of them are appended to `fleet-audit.jsonl` (mode
0600) in the state directory, which rotates to `fleet-audit.jsonl.1` at 16 MiB. A tenant sees only its own actions.
With HA, each central keeps its own log, so the log of a central that was leader for a while covers that while.

## Image catalog

The catalog lists the images the operator has put on the Macs, so tenants launch by name instead of typing disk paths.
Central does not copy images: pull each one onto every Mac that should run it (`fluxctl vm-image pull`, see
[vm-images.md](vm-images.md)), then add it.

```bash
curl -X PUT $CENTRAL/fleet/images/macos-27-xcode -H "Authorization: Bearer $FLEET_TOKEN" \
  -H 'content-type: application/json' -d '{
  "display_name": "macOS 27 with Xcode", "guest_os": "macos",
  "image": "/Users/fluxvm/vms/macos-27-xcode/disk.raw", "vcpus": 6, "memory_mib": 16384,
  "source": "ghcr.io/acme/macos-xcode:27"}'
```

`vcpus` and `memory_mib` are the defaults the console offers (4 and 8192 if omitted). `ssh_user` (optional) is the
image's login name, which `fluxctl fleet ssh` and the console's SSH config use (`admin` if omitted). Tenants read the catalog with
`GET /fleet/images`. Only the operator can add, replace (`PUT`) or remove (`DELETE /fleet/images/{name}`) entries.

## Turning it on

Tenancy is off until the first tenant exists. Until then central behaves exactly as before. Give central a fleet token
first: that token is the operator, and node heartbeats use it too.

```bash
fluxvm-agent central --token "$FLEET_TOKEN"          # state in /var/lib/fluxvm-agent/fleet-tenancy.json
export CENTRAL_URL=http://fleet-registry:7799 FLUXVM_AGENT_TOKEN="$FLEET_TOKEN"

fluxctl fleet tenant create acme --max-leased-hosts 2 --max-vms 20 --max-memory-mib 98304
# {"tenant": {...}, "token": "fvt_…", "note": "store this token now; …"}
```

Central stores only the SHA-256 of a tenant token. `fluxctl fleet tenant rotate-token acme` issues a new one, and
`fluxctl fleet tenant update acme --disable` blocks the tenant at the door.

A tenant token can call only these routes: `POST/GET /fleet/vms`, `DELETE /fleet/vms/{node}/{id}`,
`POST /fleet/vms/{node}/{id}/exec`, `GET/POST /fleet/leases`, `GET /fleet/leases/{id}`, `POST /fleet/leases/{id}/release`,
`GET /fleet/usage`, `GET /fleet/tenants/me`, `GET /fleet/capacity`, `GET /fleet/images`, `GET /fleet/nodes/{name}` (a Mac
it leases), and its own private networks (`GET/POST /fleet/overlays`, `GET/DELETE /fleet/overlays/{name}`,
`POST /fleet/overlays/{name}/members`, `DELETE /fleet/overlays/{name}/members/{node}`,
`POST /fleet/overlays/{name}/rotate`). Everything else is operator-only. In `GET /fleet/vms`, a tenant's
`unreachable_nodes` names only its own Macs and Macs running its VMs, without the node's error text.

## Leases

As the tenant:

```bash
export FLUXVM_AGENT_TOKEN=fvt_…                      # the tenant's token
fluxctl fleet lease create --min-memory-mib 65536 --nested
fluxctl fleet create --spec macos-ci.json            # lands on the leased Mac
fluxctl fleet lease release lease-…                  # refused before 24 h
```

A lease takes the smallest eligible Mac that is all of:
- healthy;
- not cordoned or draining;
- not leased and not waiting for a scrub;
- running no VMs (sole use);
- large enough and matching any `--node-selector` and `--nested`.

`--node` names a specific Mac instead. An operator can lease for a tenant (`--tenant`), can lease a busy Mac
(`allow_busy`) or a Linux host (`any_node`), and can force an early release (`?force=true`). A forced release is still
billed to the 24-hour minimum.

`hours` sets an end time. The reconciler ends the lease when that time passes. When a lease ends:
1. the tenant's VMs on that Mac are deleted;
2. the Mac is marked **scrub pending** and takes no new lease or placement until an operator scrubs it.

```bash
fluxctl fleet scrub mac-studio-3            # refused while VMs remain; --force deletes them
```

A scrub guarantees that no VM and no VM workspace is left: deleting a VM removes its disks, EFI and auxiliary storage.
Anything host-level beyond that, such as login Keychain items, caches or a full reinstall, belongs in your runbook before
you scrub.

## Quotas and ownership

Every VM created through central is recorded in a ledger with its tenant, node, size and start and end times. Quotas
(`max_vms`, `max_vcpus`, `max_memory_mib`, `max_leased_hosts`, and `max_snapshots_per_vm`, see
[Snapshots and clones](#snapshots-and-clones)) are checked against the ledger before placement, and a
breach returns `429`. A create in flight holds its share of the quota until the node answers, so a burst of parallel
creates cannot overshoot. A tenant's create is always stamped with `"tenant": "<id>"`. Tenants see, delete and exec only VMs
the ledger records as theirs; anything else is a `404`.

The reconciler runs every 60 s (`--reconcile-secs`, or on demand with `fluxctl fleet reconcile`). It closes ledger
entries for VMs a node no longer lists, for example after a TTL or a delete made on the node directly, and ends expired
leases.

## Drain

```bash
fluxctl fleet drain mac-mini-2 --mode evict-ephemeral
fluxctl fleet drain-status mac-mini-2
```

A drain cordons the node, marks it draining, then handles each VM by mode:

| Mode | Ephemeral VMs (TTL, or label `fluxvm.dev/ephemeral=true`) | `"reschedulable": true` VMs | Others |
|---|---|---|---|
| `cordon` (default) | kept | kept | kept |
| `evict-ephemeral` | deleted | kept | kept |
| `reschedule` | deleted | recreated elsewhere, then the old one deleted | kept |

Virtualization.framework has no live migration, so rescheduling recreates the VM from its spec and does not carry disk
state. Only VMs created with `"reschedulable": true` keep their spec, because create bodies can carry secrets. A leased
Mac needs `--force`, because draining it breaks the tenant's sole use. `fluxctl fleet uncordon` ends a drain.

## Metering

```bash
fluxctl fleet usage --from 2026-10-01T00:00:00Z --to 2026-11-01T00:00:00Z --csv
```

Per tenant, for the window:

| Field | Meaning |
|---|---|
| `vm_seconds`, `vcpu_seconds`, `memory_mib_seconds` | VM time, weighted by size |
| `macos_vm_seconds` | VM time spent in macOS guests |
| `lease_seconds` | Actual lease time |
| `billable_lease_seconds` | Lease time with each released lease counted to at least its 24-hour minimum |
| `vms_started`, `leases_started` | Starts within the window |

The CSV reports the same figures in hours. A tenant token always sees only its own row.

The ledger covers VMs created through central. VMs created directly on a node are not metered. Records older than
`ledger_retention_days` are dropped, so export usage before then.

## Volumes

A volume is a disk that keeps its data when the VM using it is deleted: put a database, a build cache or a home
directory on it, delete the VM whenever you like, and launch the next one with the same volume.

```bash
fluxctl fleet volume-create pg-data --size 50          # on a Mac you lease, else the one with the most free memory
fluxctl fleet launch --image debian-13 --volume pg-data
fluxctl fleet volumes
fluxctl fleet rm debian-13-1                           # the VM goes; pg-data keeps its data
fluxctl fleet volume-resize pg-data --size 100         # grow only, while detached
fluxctl fleet volume-rm pg-data                        # asks first; refused while attached
```

- **Where it lives.** A volume is a sparse raw file on one Mac (`<state_dir>/volumes/<id>.raw`, mode 0600, made by that
  Mac's `fluxctl serve`), so it uses space only as the guest writes, and a VM that uses it runs on that Mac. A launch
  with `"volumes": ["pg-data"]` is pinned to the volume's Mac; naming another Mac, or volumes on two Macs, is refused
  (`409`) before anything is created. The Mac must take the VM like any other placement (a tenant cannot use a Mac
  that is cordoned, draining or leased to someone else).
- **In the guest.** Each volume is a virtio disk whose serial is the volume id, so it is always
  `/dev/disk/by-id/virtio-vol-…` however many disks the VM has. Make a file system on it the first time
  (`mkfs.ext4`), then mount it. Volumes attach to `vz` Linux guests only: Virtualization.framework gives macOS guests
  no extra disks here.
- **One VM at a time.** Attaching marks the volume as in use; deleting the VM (through central, by a drain, a lease
  ending or the reconciler noticing the VM is gone) frees it. A volume in use cannot be deleted, resized or attached
  to a second VM (`409`). A VM with volumes cannot be `reschedulable`: its volumes do not move.
- **Names and sizes.** Names are 1-40 lowercase letters, digits and `-`, unique per tenant; the routes take the name or
  the id (`vol-` and 12 hex characters). Sizes are 1-2048 GiB. Resizing only grows, and only while detached; grow the
  file system inside the guest afterwards (`resize2fs`, `xfs_growfs`).
- **Quota and billing.** `max_volume_gib` caps the GiB of volumes a tenant holds (`429` beyond it; no limit when unset,
  `fluxctl fleet tenant update acme --max-volume-gib 500`). The price list's `volume_gib_per_hour` charges each GiB for as
  long as the volume exists, attached or not, at its current size. A tenant that still has volumes cannot be deleted.
- **Isolation.** A tenant sees and touches only its own volumes (another tenant's is a `404`); the Mac-local path is
  shown to the operator only. A create body that names a volume's file in `apple.extra_disks` directly is refused, and a tenant may not name
  `apple.extra_disks` at all (those are host files, devices or NBD servers on the Mac): tenants attach storage as volumes.
  On the node, `policy.allowed_image_dirs` (when set) applies to every extra disk of a create as it does to `image`, with
  the host's volumes directory allowed too; a volume file is refused when another VM on the Mac already uses it, and a
  clone does not take its source's volumes.
  `volume.created` and `volume.deleted` notifications go to the tenant.
- **On the Mac.** `GET/POST /v1/volumes`, `GET/DELETE /v1/volumes/{name}` and `POST /v1/volumes/{name}/resize` on
  `fluxctl serve` (admin only). The node refuses to delete or resize a file that any VM record on it still names.

## Warm pools

A tenant (or the operator) can keep VMs of a catalog image booted and idle, so a launch of that image takes one and
starts in seconds. A tenant's pool runs on the Macs it leases and counts against its quota; a shared pool runs on Macs
nobody leases and is billed to whoever takes a VM, from then on. See [warm-pools.md](warm-pools.md).

## API summary

The API is described in `schemas/fleet-openapi.yaml`; to script it from Go, Python, TypeScript or Terraform see
[fleet-sdk.md](fleet-sdk.md).

A tenant can also `GET /fleet/nodes/{name}` for a Mac it leases. It sees the Mac's size, free capacity and Apple
capabilities, but not its address, labels or security report.

| Route | Who |
|---|---|
| `GET/POST /fleet/tenants`, `GET/PATCH/DELETE /fleet/tenants/{id}`, `POST /fleet/tenants/{id}/rotate-token` | operator |
| `GET /fleet/tenants/me` | tenant |
| `GET/POST /fleet/leases`, `GET /fleet/leases/{id}`, `POST /fleet/leases/{id}/release[?force=true]` | tenant, operator |
| `POST /fleet/nodes/{name}/scrub[?force=true]` | operator |
| `POST /fleet/nodes/{name}/drain?mode=…[&force=true]`, `GET /fleet/nodes/{name}/drain` | operator |
| `POST /fleet/macos-pool/checkout`, `POST /fleet/macos-pool/{node}/{id}/return` (the per-Mac macOS pool slots, picked fleet-wide by idle count; the reply names the node) | operator |
| `GET /fleet/rebalance/plan[?high_water=0.85&max_moves=10]` (dry run: which reschedulable or ephemeral VMs could leave an overloaded Mac, and where they fit) | operator |
| `POST /fleet/vms/{node}/{id}/exec` (`{"command": "…", "timeout_seconds": N}`) | tenant (own VMs), operator |
| `GET /fleet/usage?from=&to=&tenant=&format=json\|csv` | tenant (own), operator |
| `POST /fleet/reconcile`, `GET/PUT /fleet/policy` | operator |
| `GET /fleet/images` | tenant, operator |
| `POST /fleet/vms/{node}/{id}/{start,stop,restart,pause,resume}` | tenant (own VMs), operator |
| `POST /fleet/vms/{node}/{id}/input` (`{"actions": [...]}`, as the node's `/v1/vms/{id}/input`) | tenant (own VMs; not with a read-only key), operator; not written to the activity log per event |
| `GET /fleet/vms/{node}/{id}/logs[?lines=N]` | tenant (own VMs), operator |
| `GET/POST /fleet/vms/{node}/{id}/snapshots` (`{"name": "…", "include_memory": false}`) | tenant (own VMs; `POST` not with a read-only key), operator |
| `DELETE /fleet/vms/{node}/{id}/snapshots/{name}`, `POST …/snapshots/{name}/restore` | tenant (own VMs), operator |
| `POST /fleet/vms/{node}/{id}/clone` (`{"name": "…", "snapshot": "…"}`) | tenant (own VMs; counts against its quota), operator |
| `GET /fleet/vms/{node}/{id}/screen[?max_width=N]` | tenant (own VMs), operator: the display as PNG; `409` when the VM has none |
| `GET/POST /fleet/tenants/me/keys`, `DELETE /fleet/tenants/me/keys/{id or name}` | tenant (primary token for changes) |
| `GET/POST /fleet/tenants/{id}/keys`, `DELETE …/keys/{key}` | operator |
| `GET /fleet/prices` | tenant, operator; `PUT` operator |
| `GET /fleet/billing[?month=YYYY-MM&tenant=T&format=csv]` | tenant (own), operator |
| `PUT /fleet/tenants/me/budget`, `PUT /fleet/tenants/{id}/budget` | tenant (primary token or a full key), operator |
| `GET /fleet/notifications[?limit=N]` | tenant (own), operator |
| `GET/POST /fleet/webhooks`, `DELETE /fleet/webhooks/{id}`, `POST /fleet/webhooks/{id}/test` | tenant (own), operator |
| `GET /fleet/events[?limit=N&tenant=T]` | tenant (own actions), operator (all, or one tenant's) |
| `PUT/DELETE /fleet/images/{name}` | operator |
| `GET /fleet/nodes/{name}` | operator; tenant for a Mac it leases (size, capacity, Apple capabilities only) |
| `GET/POST /fleet/volumes[?tenant=T]`, `GET/DELETE /fleet/volumes/{name or id}`, `POST /fleet/volumes/{name}/resize` | tenant (own), operator |
| `GET /fleet/vms/{node}/{id}/tunnel?port=N` (WebSocket) | tenant (own VMs; not with a read-only key), operator |
| `GET /fleet/tunnels`, `DELETE /fleet/tunnels/{id}` | tenant (own), operator (all) |
| `GET/POST /fleet/pools`, `GET/PATCH/DELETE /fleet/pools/{name}`, `GET …/{name}/status` | tenant (own pools; shared pools it may use, read-only), operator. See [warm-pools.md](warm-pools.md) |
| `GET /auth/config`, `GET /auth/login[?tenant=T]`, `GET /auth/callback`, `GET /auth/session`, `POST /auth/logout` | anyone (the browser sign-in; see "Sign-in with SSO") |
| `GET/POST /fleet/tenants/me/members`, `PATCH/DELETE /fleet/tenants/me/members/{email}` (`{"email", "role"}`, `{"role"}`) | tenant (changes: an admin or owner session, or the primary token) |
| `GET/POST /fleet/tenants/{id}/members`, `PATCH/DELETE …/members/{email}` | operator |
| `GET /signup/config`, `POST /signup`, `POST /stripe/webhook` | anyone (sign-up when open; the webhook is signed); see [mac-cloud-billing.md](mac-cloud-billing.md) |
| `GET /fleet/billing/invoices`, `POST /fleet/billing/payment-method` | tenant (own), operator (invoices) |
| `POST /fleet/billing/invoices?month=`, `PUT /fleet/tenants/{id}/billing` | operator |

## Not done yet

- A two-Mac hardware run of leases, scrub and drain.
- Host-level scrubbing beyond VM workspaces.
- High availability is in [fleet-ha.md](fleet-ha.md) (several centrals, one Redis), not yet run on real hosts.
- Tenant vmnet networks (`isolate_tenant_networks`) are per Mac. For one network spanning a tenant's Macs, use an
  overlay: [cross-mac-networks.md](cross-mac-networks.md).
- Lab mode (no `--token`) and tenants do not mix: central refuses to create tenants without `--token`, and if tenants
  exist when it starts without one, every caller except a tenant gets a `401` (node heartbeats included).
