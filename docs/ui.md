# FluxVM Studio (web UI)

FluxVM ships a web UI in the binary. It needs no install, no Node at runtime and no second port.

| Where | URL | Signs in with |
|---|---|---|
| Node daemon (`fluxctl serve`) | `http://127.0.0.1:7788/console/` | an API token from `[[auth.tokens]]` |
| Fleet central (`fluxvm-agent central`) | `https://<central>/console/` | the operator token, a tenant token or key, or SSO |
| A Kairon node | `https://<node>:<console-port>/studio/console/` | the relay token (see Kairon's `docs/guides/fluxvm-studio.md`) |

`fluxctl dashboard` prints (and, on a Mac, opens) the node URL. The token is typed into the sign-in card; it stays in the
tab's session storage and is never put in a URL.

## What it covers

**Node:** Overview (live events), Machines (create wizard, detail with Console, Guest, Network and egress policy,
Storage, Protect, Migration), Sandboxes, Agent sessions, Network (virtual networks, groups, network policies, services
with apply, flows, inspect, node default egress policy, traffic), Images (catalog, templates, pools, OCI),
Volumes, Branches, Backups, Host (memory, pressure, topology, GPU passthrough on Linux, Apple host and the Mac pool on a Mac), and
Access & audit (quota, events, an API explorer).

**Fleet central:** Overview, Hosts (cordon, drain, scrub, lease), Virtual machines (launch, power, logs, delete),
Networks, Volumes, Warm pools, Tunnels, Images (the catalog), Enrollment (join tokens, node credentials), Operations, Tenants (quota, token
rotation, and each tenant's members) or API keys and Members (a tenant's team), Billing & usage (usage, budgets, invoices when there are any), Activity (audit log, alerts,
webhooks).

A VM's sheet on central has an Overview (power, console log), a **Screen** (the display, refreshed every 2 seconds; for a
macOS guest, **Control** sends your clicks, scrolling and keys to it, and pasted text and Return, Esc, Tab and ⌘ Space
buttons cover Setup Assistant), **Snapshots** (take with or without memory, restore, delete, and clone a VM from a snapshot
or from a stopped VM's disk) and **Run** (a command through the guest agent, 60-second limit). Operators edit the image
catalog on the **Images** page. Leases and warm-pool fill need a Mac node; on a Linux node they are refused by design.

Press **⌘K** (Ctrl+K) from anywhere for every page and every machine. Dark is the default; the sun/moon button switches and
the choice is remembered. Motion honours `prefers-reduced-motion`.

Pages a host cannot serve are hidden, not greyed: GPU passthrough appears only on Linux, the Apple host tab only on a Mac.
Many buttons have a "Copy as fluxctl" twin so the CLI form is one click away.

## How it is built

- `ui/core`: design tokens (from Netra), components, typed API client, SSE-over-fetch, router, theme, palette.
- `ui/node`, `ui/fleet`: the two apps. React 19, TypeScript, Vite; no UI or CSS framework.
- The **built** bundles `ui/node/dist` and `ui/fleet/dist` are committed and embedded with `include_dir`, so
  `cargo build` never runs Node. CI (`.github/workflows/ui-check.yml`) typechecks, tests, rebuilds and fails if a committed
  bundle is stale. To change the UI: `cd ui && npm ci && npm run build`, commit the result.
- File names are fixed and the base is relative, with hash routing, so the UI works under any reverse-proxy prefix.
  Files are served with an ETag and `Cache-Control: no-cache`; a missing file with an extension is a real 404.

### Security model

- The files are served without auth for GET/HEAD; they hold no data. Every API call carries the bearer.
- Strict CSP: `script-src 'self'` (no inline script; the theme bootstrap is an external file), `connect-src 'self'`,
  `frame-ancestors 'none'`. Same-origin only, so there is no CORS.
- Browsers cannot send `Authorization` on a WebSocket. `POST /v1/vms/{id}/ws-ticket` (token required) mints a single-use,
  30-second ticket bound to a VM and a kind (console or serial); `GET /console/ws/{ticket}` redeems it. Agent-session
  terminals use the existing `pty-ticket`.
- Central refuses changes made with an SSO cookie unless the request carries `X-FluxVM-Console`; Studio sends it.

### Development

    cd ui && npm ci
    FLUXVM_DEV_API=http://127.0.0.1:7788 npm run dev          # node UI on :5174, proxies /v1
    npm run dev:fleet                                          # fleet UI on :5175, proxies /fleet and /auth
    npm run typecheck && npm test && npm run build

The previous consoles remain at `/console/legacy` for one release.

### Page smoke test

`ui/e2e/run.sh node|fleet` (or `npm run e2e` in `ui/`) starts a mock API, drives every page in Chrome through
playwright-core at 390 px and 1280 px in both themes, and fails on console errors, failed requests, horizontal
overflow, controls with no accessible name and pages without exactly one `<h1>`. It covers 18 node routes and 13 fleet routes. `ui/e2e/run.sh vmtools` (`npm run e2e:vmtools`) opens a VM's sheet and the Images page in Chrome against the same mock and checks the calls the page makes: the pixel a click lands on, merged keystrokes, snapshot, clone and command requests, and the catalog's PUT and DELETE. It is a regression net, not a substitute
for trying the UI against a real daemon.

### VM tools on a real guest

`scripts/vz-studio-live-test.sh` (Apple silicon only) starts a node daemon with the vz runner, a central and a node agent, boots the
`agent-micro` guest and drives a VM's sheet in Chrome: the screen, a snapshot (take, clone, restore, delete) and a command, each
confirmed on the node's API. Control (clicks and keys) is only offered for macOS guests, so it is covered by the mock-API test
(`ui/e2e/run.sh vmtools`) and the node's `scripts/vz-input-live-test.sh`, not by this one.

### Fleet form flows

`ui/e2e/flows.sh` (or `npm run e2e:flows` in `ui/`) is the real-daemon counterpart for the fleet app's forms. It starts a
`fluxvm-agent central` and one simulated Mac (`BIN`, default `target/debug/fluxvm-agent`; rebuild it after changing the UI,
because the build is embedded), then drives the join-token sheet (validation, create, revoke), the lease sheet (the 24-hour
minimum, a tenant is required, the lease appears with its countdown) and the billing page in Chrome. Every flow is confirmed
through the API afterwards, so a toast alone cannot pass it. To use a central that is already running, set `FLOWS_URL` and
`FLOWS_TOKEN`. The run force-releases its lease and scrubs the Mac, so it can be repeated against the same central.
