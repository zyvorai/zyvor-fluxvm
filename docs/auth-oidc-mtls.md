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

`fluxctl serve` uses rustls with `WebPkiClientVerifier`. After the handshake,
identity can also be passed as headers (for a trusted frontend):

- `X-Client-Cert-CN` — actor
- `X-Client-Cert-Role` — `admin` or omitted (read-only)
- `X-Client-Cert-Tenant` — tenant

Headers are honored only when `tls.client_ca` is configured (mTLS on). They are read from the request as sent: the daemon
does not derive the actor, role or tenant from the verified client certificate itself, so when `fluxctl serve` terminates
TLS directly, any client that holds a certificate your `client_ca` accepts can send `X-Client-Cert-Role: admin` (and any
CN or tenant) and be treated as that identity. Until the daemon binds identity to the certificate, use mTLS only to gate who may connect,
issue client certificates only to principals you would trust as admin, or terminate mTLS in a proxy that overwrites these headers
and keep the daemon on a private listener.
Do not put FluxVM behind a proxy that blindly injects these headers without
verifying the client certificate.

## Step-up TOTP

Static bearer tokens can require a second factor for exec and writes with `[[auth.mfa]]`; see [host-hardening.md](host-hardening.md#step-up-mfa-totp-for-exec-and-writes). OIDC and mTLS identities are not covered by it: enforce MFA at the identity provider (for example by requiring an `amr` claim), because FluxVM does not read it yet.
