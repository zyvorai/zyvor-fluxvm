# OIDC and mTLS token exchange

## OIDC (JWT bearer)

Set both keys:

```toml
[auth]
require = true
oidc_issuer = "https://idp.example.com"
oidc_audience = "fluxvm"
```

FluxVM discovers `{issuer}/.well-known/openid-configuration`, fetches JWKS,
and validates `iss`, `aud`, `exp`, and signature (`kid`). Claims:

| Claim | Use |
|-------|-----|
| `fluxvm_role` or `role` | `admin` / `read-only` (default read-only) |
| `fluxvm_tenant` / `tenant` / `org` | stamped tenant |
| `preferred_username` / `email` / `sub` | audit actor |

Static `[[auth.tokens]]` still work and are checked first.

## mTLS

```toml
[tls]
cert = "/etc/fluxvm/tls/server.crt"
key = "/etc/fluxvm/tls/server.key"
client_ca = "/etc/fluxvm/tls/client-ca.crt"
```

`fluxctl serve` uses rustls with `WebPkiClientVerifier`: a client must present a certificate signed by `client_ca`.
The daemon then takes the caller's identity **from that verified certificate**, not from anything the client sends:

- actor: the subject `CN`
- role: an `OU` of the form `fluxvm-role=admin`. Any other value, or no such `OU`, is read-only. The first `fluxvm-role=` `OU` wins.
- tenant: an `OU` of the form `fluxvm-tenant=<name>`

Your CA controls these fields, so issue `fluxvm-role=admin` certificates only to principals you trust as admin. A verified
certificate with no `CN` is rejected as unauthenticated. A bearer token, when presented, is still checked first.

The `X-Client-Cert-*` request headers are **ignored** on a connection the daemon terminated itself, so a client cannot
send `X-Client-Cert-Role: admin` to raise its role.

### mTLS terminated by a reverse proxy

If a reverse proxy terminates mTLS and forwards plain HTTP to the daemon, opt in explicitly:

```toml
[tls]
trust_proxy_headers = true   # default false
```

The daemon then reads the identity from these headers (when it did not terminate mTLS itself on that connection):

- `X-Client-Cert-CN` — actor
- `X-Client-Cert-Role` — `admin` or omitted (read-only)
- `X-Client-Cert-Tenant` — tenant

Enable this only if the proxy verifies the client certificate and **overwrites or strips** these headers on every request,
and the daemon is reachable only from the proxy (private listener or firewall). Anyone who can reach the daemon directly can
forge them. With the default `trust_proxy_headers = false` the headers are never read.

Upgrade note: before this change the headers were honored whenever `client_ca` was set. Deployments that relied on that
behind a proxy must set `trust_proxy_headers = true`; deployments with direct mTLS need client certificates carrying
`OU=fluxvm-role=admin` for admin access (certificates without it are read-only).

## Step-up TOTP

Static bearer tokens can require a second factor for exec and writes with `[[auth.mfa]]`; see [host-hardening.md](host-hardening.md#step-up-mfa-totp-for-exec-and-writes). OIDC and mTLS identities are not covered by it: enforce MFA at the identity provider (for example by requiring an `amr` claim), because FluxVM does not read it yet.
