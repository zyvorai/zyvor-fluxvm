# OIDC and mTLS token exchange

## OIDC (JWT bearer)

Set both keys:

```toml
[auth]
require = true
oidc_issuer = "https://idp.example.com"
oidc_audience = "fluxvm"
```

Both keys are required: with only `oidc_issuer` the daemon logs a warning and OIDC stays off.

FluxVM discovers `{issuer}/.well-known/openid-configuration`, fetches the JWKS,
and validates `iss`, `aud`, `exp`, and the signature (the token must carry a `kid`; RS256/384/512 and ES256/384 keys are mapped, an
unlisted algorithm is treated as RS256). The JWKS is cached for an hour and refetched when a `kid` is not in the cache. Claims:

| Claim | Use |
|-------|-----|
| `fluxvm_role` or `role` | `admin` (or `administrator`, case-insensitive) is admin; anything else, or absent, is read-only |
| `fluxvm_tenant` / `tenant` / `org` | stamped tenant (first one present, in that order) |
| `preferred_username` / `email` / `sub` | audit actor (first present, in that order) |

Static `[[auth.tokens]]` still work and are checked first; a bearer that matches none of them is then tried as a JWT. Setting both OIDC
keys also makes authentication mandatory (`auth.require` behaves as true). An agent-session token (`fxst_...`) is resolved separately
and is confined to its own session's routes ([agent-sessions-guide.md](agent-sessions-guide.md#4-letting-an-agent-use-a-session-safely)).

## mTLS

```toml
[tls]
cert = "/etc/fluxvm/tls/server.crt"
key = "/etc/fluxvm/tls/server.key"
client_ca = "/etc/fluxvm/tls/client-ca.crt"
trust_proxy_headers = false   # default; see "mTLS terminated by a reverse proxy"
```

`cert` and `key` turn on HTTPS; adding `client_ca` makes `fluxctl serve` require a client certificate chained to that CA bundle at the
TLS handshake (rustls `WebPkiClientVerifier`; a client without one cannot connect, not even to `/healthz`). No certificate revocation
list is consulted, so prefer short-lived client certificates. The daemon then takes the caller's identity **from that verified
certificate** (the leaf's subject), not from anything the client sends:

| Certificate field | Becomes |
|---|---|
| subject `CN` (the first one) | the actor: the audit log name and the rate-limit key |
| an `OU` of the form `fluxvm-role=admin` | role `admin`. Matching is exact and case-sensitive. Any other value (`fluxvm-role=Admin`, `fluxvm-role=root`), or no such `OU`, is **read-only**. The first `fluxvm-role=` `OU` wins; a later one cannot raise it |
| an `OU` of the form `fluxvm-tenant=<name>` | the tenant stamped on the request, like a token's `tenant` (the first one; an empty name is ignored) |

Your CA controls these fields, so issue `fluxvm-role=admin` certificates only to principals you trust as admin. A verified
certificate with no `CN` is rejected as unauthenticated (401). Example subject for an admin of tenant `acme`:
`CN=ci-runner, OU=fluxvm-role=admin, OU=fluxvm-tenant=acme`. This role mapping is not the OIDC one: there `administrator` and any
letter case also count as admin.

### When the certificate identity applies

The certificate is one credential among several, and it is consulted last:

- **A bearer token wins.** If the request carries `Authorization: Bearer ...`, only the bearer is checked (static token, session token,
  then OIDC). An invalid bearer is a 401; the certificate does not rescue it.
- **Auth must be on, and mTLS alone does not turn it on.** `fluxctl serve` refuses to start when authentication is required (`auth.require = true`,
  OIDC configured, or a non-loopback `listen`) but there is no `[[auth.tokens]]` entry and no OIDC: a client certificate does not count as
  a credential there. Configure at least one token or OIDC next to `client_ca`, and certificates then work as an additional credential.
  On a loopback listener with `auth.require = false` and no tokens or OIDC, authentication is off and **every request is anonymous
  admin, certificate or not**: the handshake still demands a client certificate, but its `OU`s are not read.
- `GET /healthz`, `/readyz`, `/v1/openapi.json`, `/preview/...` and the Studio console pages stay exempt from authentication, as without mTLS.

The `X-Client-Cert-*` request headers are **ignored** on a connection the daemon terminated itself, so a client cannot
send `X-Client-Cert-Role: admin` to raise its role (this was the hole closed in 0.4.21).

### mTLS terminated by a reverse proxy

If a reverse proxy terminates mTLS and forwards plain HTTP to the daemon, opt in explicitly:

```toml
[tls]
trust_proxy_headers = true   # default false
```

The daemon then reads the identity from these headers (when it did not terminate mTLS itself on that connection):

- `X-Client-Cert-CN`: actor (required and non-empty; without it the request is unauthenticated)
- `X-Client-Cert-Role`: exactly `admin`, otherwise (or omitted) read-only
- `X-Client-Cert-Tenant`: tenant

Enable this only if the proxy verifies the client certificate and **overwrites or strips** these headers on every request,
and the daemon is reachable only from the proxy (private listener or firewall). Anyone who can reach the daemon directly can
forge them. With the default `trust_proxy_headers = false` the headers are never read. The same rules as above apply: a bearer token is
checked first, and authentication has to be on (a token or OIDC configured). The `[tls]` table itself does not need `cert`/`key` for the proxy case,
since the proxy terminates TLS; `trust_proxy_headers` is read regardless.

### Upgrading to 0.4.21

Before 0.4.21 the headers were honored whenever `client_ca` was set, and the identity came from them even on a connection the daemon
terminated, so any holder of a certificate accepted by `client_ca` could send `X-Client-Cert-Role: admin` and get admin.

- **Direct mTLS clients** (the daemon terminates TLS): admin access needs a certificate that carries `OU=fluxvm-role=admin`. A certificate
  without it is read-only now, whatever header the client sends. Reissue admin client certificates before upgrading, or they lose write access.
- **Behind a proxy**: set `trust_proxy_headers = true`, and make sure the proxy strips client-supplied copies of the three headers.
  Without the opt-in the headers are ignored, and with authentication on those requests get 401.
- **Clients that sent the headers themselves** (scripts, SDK `headers=`): they stop having any effect on a direct connection. The Python SDK's note about
  passing a trusted frontend's identity headers applies only behind such a proxy with `trust_proxy_headers = true`.

The change shipped with tests in `crates/fluxvm-api` (`mtls.rs` and the `mtls_identity` module in `lib.rs`): the certificate parsing rules, forged headers
ignored by default and on a certificate connection, the proxy opt-in, and a real TLS handshake with an admin and a read-only client certificate that also send forged
headers. Nothing in the repository exercises the proxy case with a real proxy in front.

## Step-up TOTP

Static bearer tokens can require a second factor for exec and writes with `[[auth.mfa]]`; see [host-hardening.md](host-hardening.md#step-up-mfa-totp-for-exec-and-writes). OIDC and mTLS identities are not covered by it: enforce MFA at the identity provider (for example by requiring an `amr` claim), because FluxVM does not read it yet.
