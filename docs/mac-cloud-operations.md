# Running the Mac cloud's central server

This page is for the operator of a Mac cloud: how to run `fluxvm-agent central`, what every flag does, what it keeps on
disk, what it exposes, and what to check before real customers use it. For the concepts (tenants, leases, quotas) see
[mac-cloud-tenancy.md](mac-cloud-tenancy.md); for the Macs themselves, [mac-cloud.md](mac-cloud.md).

> Everything here was exercised against central plus simulated Macs (`fluxvm-agent sim-node`, see
> [fleet-quickstart.md](fleet-quickstart.md)). Real Stripe, a real identity provider and a fleet of real Macs have not
> been run; each section says what that leaves open.

## What runs where

| Piece | Runs on | Talks to |
|---|---|---|
| `fluxvm-agent central` | one small server (or several, [highly available](fleet-ha.md)) | every Mac's node API, Stripe, the identity provider, webhook receivers |
| `fluxctl serve` (the node API) and `fluxvm-agent node` (its heartbeat to central) on each Mac | the Macs | registers with and heartbeats to central; answers central's calls |
| `fluxctl fleet`, the console (`/console`), the [SDKs](fleet-sdk.md) and [Terraform provider](fleet-sdk.md#terraform) | anywhere | central only: **tenants never reach a Mac directly** |

Central is the only thing customers connect to. A Mac's node API is reachable from central, not from tenants; SSH and
port tunnels go through central ([tunnels](mac-cloud-tenancy.md#ssh-and-port-tunnels)) for the same reason.

## Starting it

```bash
fluxvm-agent central \
  --listen 127.0.0.1:7799 \
  --state-dir /var/lib/fluxvm-agent \
  --token "$(cat /etc/fluxvm/operator-token)" \
  --public-url https://cloud.example.com
```

Put a TLS-terminating reverse proxy in front (or pass `--tls-cert` and `--tls-key`). `--token` is the operator's
credential for everything under `/fleet/*`; **without it the registry is open to anyone who can reach it (lab only).**

## Flags

Every flag has an environment variable, shown in the second column.

### Core

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--listen ADDR` | `LISTEN` | `0.0.0.0:7799` | Address to serve on. Bind to loopback when a reverse proxy is in front. |
| `--state-dir DIR` | `FLUXVM_AGENT_STATE_DIR` | `/var/lib/fluxvm-agent` | Where state is kept (below). |
| `--token TOKEN` | `FLUXVM_AGENT_TOKEN` | none | The operator's bearer token for `/fleet/*`. Omit only in a lab. |
| `--tls-cert PEM`, `--tls-key PEM` | `FLUXVM_AGENT_TLS_CERT`, `FLUXVM_AGENT_TLS_KEY` | none | Serve HTTPS directly. Both are needed together. |
| `--reconcile-secs N` | `FLUXVM_AGENT_RECONCILE_SECS` | `60` | How often central reconciles its ledger against every Mac and checks lease ends, billing, notifications and warm pools. `0` turns the background pass off (`POST /fleet/reconcile` still runs one). |

### Metrics

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--metrics-interval-secs N` | `FLUXVM_AGENT_METRICS_INTERVAL_SECS` | `15` | How often central samples every Mac and VM for graphs and `/metrics`. |
| `--metrics-token TOKEN` | `FLUXVM_AGENT_METRICS_TOKEN` | none | A token that may only `GET /metrics`, so Prometheus does not need the operator token. See [fleet-metrics.md](fleet-metrics.md). |

### High availability

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--ha-redis URL` | `FLUXVM_AGENT_HA_REDIS` | none | Run several centrals against one Redis; one leads and the rest answer `503` naming the leader. |
| `--ha-cluster NAME` | `FLUXVM_AGENT_HA_CLUSTER` | `default` | Name of the HA group (several fleets can share a Redis). |
| `--advertise-url URL` | `FLUXVM_AGENT_ADVERTISE_URL` | none | This central's URL as clients reach it, and its identity in the election. Required with `--ha-redis`. |
| `--ha-ttl-secs N` | `FLUXVM_AGENT_HA_TTL_SECS` | `10` | How long a dead leader blocks a takeover. |

See [fleet-ha.md](fleet-ha.md). A leader that loses Redis steps down after its TTL rather than serve unfenced.

### Single sign-on

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--oidc-issuer URL` | `FLUXVM_OIDC_ISSUER` | none | OpenID Connect issuer (for example `https://acme.okta.com`). Setting it turns console SSO on. |
| `--oidc-client-id ID` | `FLUXVM_OIDC_CLIENT_ID` | none | The client id registered at the provider. |
| `--oidc-client-secret S` / `--oidc-client-secret-file F` | `FLUXVM_OIDC_CLIENT_SECRET`, `FLUXVM_OIDC_CLIENT_SECRET_FILE` | none | The client secret (omit for a public client; PKCE is always used). Prefer the file. |
| `--public-url URL` | `FLUXVM_PUBLIC_URL` | none | This central's URL **as browsers reach it**. The redirect URI to register at the provider is `{public-url}/auth/callback`. |
| `--oidc-provider-name NAME` | `FLUXVM_OIDC_PROVIDER_NAME` | `SSO` | Text on the sign-in button. |
| `--sso-operator EMAIL` | `FLUXVM_SSO_OPERATORS` | none | An email that signs in as the operator (repeatable). |
| `--session-hours N` | `FLUXVM_SESSION_HOURS` | `12` | Console session length, 1 to 720. |

### Payments

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--stripe-secret-key-file F` | `STRIPE_SECRET_KEY_FILE` (or `STRIPE_SECRET_KEY`) | none | Stripe secret (`sk_live_…`) or restricted (`rk_…`) key. Setting it turns payments on. |
| `--stripe-webhook-secret-file F` | `STRIPE_WEBHOOK_SECRET_FILE` (or `STRIPE_WEBHOOK_SECRET`) | none | Signing secret (`whsec_…`) of the endpoint `POST /stripe/webhook`. Without it the endpoint answers `503`. |
| `--stripe-api-base URL` | `STRIPE_API_BASE` | `https://api.stripe.com` | For a local mock in tests; not for production. |

Setup steps are in [mac-cloud-billing.md](mac-cloud-billing.md#setting-up-stripe). Policy that is not a flag (prices,
sign-up on or off, the default quota, session and tunnel limits) is set through the API and kept in state; see
[mac-cloud-tenancy.md](mac-cloud-tenancy.md).

## What it keeps on disk

All under `--state-dir`:

| File | What | Mode |
|---|---|---|
| `fleet-nodes.json` | The registered Macs and their last heartbeat. | 0600 |
| `fleet-tenancy.json` | Tenants (token hashes only), quotas, keys, leases, the VM ledger, images, prices, webhooks (with their signing secrets), volumes, snapshots, warm pools, SSO members and sessions (token hashes), Stripe invoice records, and policy. | 0600 |
| `fleet-audit.jsonl` | The activity log: who did what through central. | 0600 |

- **Secrets in state.** Tenant tokens, API keys and console session tokens are stored as SHA-256 hashes, so a leaked
  state file does not leak them. Webhook signing secrets and Stripe customer ids are stored in the clear: protect the
  directory and its backups.
- **Back it up.** `fleet-tenancy.json` is the source of truth for who owns which VM and lease; copy it with
  central stopped or atomically (it is replaced by rename). Without HA, central being down stops launches, leases,
  the console and billing runs; VMs already running keep running on their Macs.
- **Not persisted.** Metrics history lives in memory (graphs start empty after a restart and fill from the next
  samples), as do the in-flight sign-up and SSO login state and the set of Stripe event ids already seen.

## What it exposes

`/fleet/*` needs a token. These are reachable **without** one, on purpose:

| Path | Why | Guard |
|---|---|---|
| `GET /healthz`, `GET /fleet/ha`, `GET /fleet/ha/leader` | Liveness and which central leads (for load balancers). | none needed |
| `POST /signup`, `GET /signup/config` | Self-service sign-up, **off unless the operator turns it on** (then `GET /signup/config` says `open: false` otherwise). | Per-address and global hourly limits; optional email-domain allow-list; a signup quota. |
| `POST /stripe/webhook` | Stripe's callbacks. | HMAC signature over the raw body, 300 s tolerance; `503` if no secret is set. |
| `GET /auth/config`, `GET /auth/login`, `GET /auth/callback`, `POST /auth/logout`, `GET /auth/session` | Console SSO. | State, nonce and PKCE; CSRF header and same-origin check on changes. |
| `GET /metrics` | Prometheus. | The operator token or `--metrics-token`; a tenant token gets `401`. |
| `GET /console`, `/console/app.js` | The console's static page and script (served with a strict CSP and `frame-ancestors 'none'`). | Everything they call needs a session or token. |

Behind a reverse proxy, central trusts `X-Forwarded-For` **only from a peer on loopback** (it uses the last entry), so
run the proxy on the same machine and bind central to `127.0.0.1`. Otherwise every sign-up looks like it comes from
the proxy and shares one rate limit.

## Before real customers: a checklist

Marked ✅ when the code already enforces it, ☐ when it is yours to do.

**Transport and secrets**
- ☐ TLS in front of central (a reverse proxy or `--tls-cert`/`--tls-key`), and `--public-url` set to its `https` URL.
- ☐ `--token` set; kept in a secrets manager, not in shell history or unit files. Use the `*-file` flags for the OIDC
  and Stripe secrets.
- ☐ `--state-dir` readable only by the service user; backups encrypted.
- ✅ Tokens, keys and session tokens are shown once and stored only as hashes.
- ✅ SSO cookie is `HttpOnly`, `SameSite=Lax`, and `Secure` when `--public-url` is `https`.

**Tenant isolation**
- ☐ `isolate_tenant_networks` on, if tenants share Macs (see [the policy table](mac-cloud-tenancy.md#why-leases) and [warm pools](warm-pools.md#limits)): with it off, VMs of
  different tenants sit on one vmnet.
- ☐ Prefer [leases](mac-cloud-tenancy.md#leases) (a whole Mac per tenant) for untrusted workloads; shared Macs are for
  workloads you are willing to co-locate.
- ✅ A tenant's tokens reach only its own VMs, volumes, snapshots, pools and tunnels (`404` otherwise).
- ✅ Read-only keys and SSO viewers cannot change anything or open tunnels.
- ✅ A tenant cannot name host paths: volumes are created by central with generated names, extra disks are operator
  only, and warm-pool claims accept only the guest OS.
- ☐ Disk image paths are still taken from the request (`image` on a VM create) unless the Mac restricts where images may
  come from. Set the Mac's allowed image directories before giving tenants direct disk paths; the
  [image catalog](mac-cloud-tenancy.md#image-catalog) avoids it.

**Sign-up and payments**
- ✅ Sign-up confirms the address by a mailed link before anything exists (`signup_require_email_verification`, on by
  default), and cannot be opened without a mail path: set `--signup-mail-webhook` and `--public-url`
  ([mac-cloud-billing.md](mac-cloud-billing.md#email-confirmation)). Test the mail path before opening sign-up.
- ☐ Keep `signup_require_email_verification` on outside a closed lab: off, anyone can sign up as anyone, and Stripe
  `send_invoice` mails invoices to that unconfirmed address.
- ☐ Set a signup default quota you are comfortable giving anonymous accounts (volumes are capped at 100 GiB by default).
- ☐ Require a payment method (`signup_require_payment_method`) before accounts can launch VMs.
- ☐ A Stripe **restricted** key with only the permissions in [mac-cloud-billing.md](mac-cloud-billing.md#setting-up-stripe).
- ✅ Invoices are idempotent per tenant and month, and never bill twice across restarts or failovers.

**Console and tunnels**
- ✅ Viewers get neither live screens nor console logs (`403` from central, and the console says so): both can hold
  secrets. Give someone the member role if they need them.
- ☐ Remember that a tunnel opened from a console session is not closed by logging out or by rotating the tenant's token;
  removing or demoting the member does end it, and so do the idle timeout (60 minutes) and the maximum lifetime (8 hours
  by default). To cut access off at once, delete it (`DELETE /fleet/tunnels/{id}`) or remove the tenant's VM.
- ✅ Tunnels reach only a registered Mac's VM, only ports the policy allows (22 and 1024-65535 by default), and, for
  vz guests, only addresses on that Mac's own VM network.

**Operations**
- ☐ Prometheus scraping `/metrics` with `--metrics-token`, and alerts for stale heartbeats, drained Macs, failed
  invoice runs and webhook delivery failures ([fleet-metrics.md](fleet-metrics.md#prometheus)).
- ☐ Run [HA](fleet-ha.md) if a control-plane outage of a few minutes matters.
- ☐ Test restore: stop central, restore `fleet-tenancy.json` to a clean directory, start it, and check
  `fluxctl fleet status`.

## Upgrading

- Copy `fleet-tenancy.json` before upgrading, as for any database. New state fields are added with defaults, so an
  older file loads in a newer central; going back to an older central is not supported once a newer one has written
  the file.
- Central talks to the node API of each Mac and the routes it needs from newer Macs are additive. Upgrade central
  first, then the Macs, and expect volumes, tunnels, metrics and restore to work on a Mac only once its `fluxctl serve` has the matching
  version.
- With [HA](fleet-ha.md), upgrade the standbys first, then stop the leader; a standby takes over within the TTL.
- The CHANGELOG lists what each release changes; entries marked *Security* are worth reading before you upgrade.

## Not done yet

- Real-hardware runs of the multi-Mac paths (cross-Mac overlay and key rotation, leases across several real Macs).
- A "sign out everywhere" action for console sessions, and mail delivery with retries and bounces (central hands the
  message to your webhook and does not see the result).
- Sessions of tunnels and screens are not tied to console login state; there is no per-tenant absolute tunnel lifetime.
- An allow-list of directories for disk images that central itself enforces (today the Mac does).
