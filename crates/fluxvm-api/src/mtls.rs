//! Identity from the verified mTLS client certificate.
//!
//! When `fluxctl serve` terminates mTLS itself, [`IdentityAcceptor`] performs the TLS handshake,
//! reads the peer certificate that `WebPkiClientVerifier` already validated, and attaches a
//! [`PeerCert`] request extension to every request on that connection. `auth_middleware` trusts
//! only that extension (never the `X-Client-Cert-*` headers, which a client can forge) unless
//! `tls.trust_proxy_headers` is set.
//!
//! Certificate fields (set by your CA, so the client cannot choose them):
//! - actor: subject `CN`
//! - role: an `OU` of the form `fluxvm-role=admin` (anything else, or no such OU: read-only)
//! - tenant: an `OU` of the form `fluxvm-tenant=<name>`

use axum::Extension;
use axum_server::accept::Accept;
use fluxvm_core::config::Role;
use rustls::ServerConfig;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tower_layer::Layer;

/// Identity derived from a verified client certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub cn: String,
    pub role: Role,
    pub tenant: Option<String>,
}

/// Request extension set by [`IdentityAcceptor`]. `None` means the certificate was verified by
/// the CA but carries no usable CN, so the request is unauthenticated.
#[derive(Debug, Clone)]
pub struct PeerCert(pub Option<PeerIdentity>);

/// Maps a DER certificate to an identity. `None` when it cannot be parsed or has no CN.
pub fn identity_from_cert_der(der: &[u8]) -> Option<PeerIdentity> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let subject = cert.subject();
    let cn = subject
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())
        .filter(|s| !s.is_empty())?
        .to_string();
    let ous = subject
        .iter_organizational_unit()
        .filter_map(|a| a.as_str().ok());
    Some(identity_from_fields(cn, ous))
}

/// Role/tenant from the subject's `OU` values (see the module docs).
fn identity_from_fields<'a>(cn: String, ous: impl Iterator<Item = &'a str>) -> PeerIdentity {
    let mut role = Role::ReadOnly;
    let mut role_seen = false;
    let mut tenant = None;
    for ou in ous {
        if let Some(r) = ou.strip_prefix("fluxvm-role=") {
            // First role OU wins; a later one cannot upgrade it.
            if !role_seen {
                role_seen = true;
                if r == "admin" {
                    role = Role::Admin;
                }
            }
        } else if let Some(t) = ou.strip_prefix("fluxvm-tenant=")
            && tenant.is_none()
            && !t.is_empty()
        {
            tenant = Some(t.to_string());
        }
    }
    PeerIdentity { cn, role, tenant }
}

/// TLS acceptor that completes the handshake and attaches the verified [`PeerCert`] to the
/// connection's service.
#[derive(Clone)]
pub struct IdentityAcceptor {
    tls: TlsAcceptor,
}

impl IdentityAcceptor {
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            tls: TlsAcceptor::from(config),
        }
    }
}

impl<I, S> Accept<I, S> for IdentityAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = <Extension<PeerCert> as Layer<S>>::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let tls = self.tls.clone();
        Box::pin(async move {
            let stream = tokio::time::timeout(Duration::from_secs(10), tls.accept(stream))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")
                })??;
            let peer = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .and_then(|leaf| identity_from_cert_der(leaf.as_ref()));
            Ok((stream, Extension(PeerCert(peer)).layer(service)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_role_ou_wins_and_unknown_roles_are_read_only() {
        let id = |ous: &[&str]| identity_from_fields("c".into(), ous.iter().copied());
        assert_eq!(
            id(&["fluxvm-role=admin", "fluxvm-tenant=a"]).role,
            Role::Admin
        );
        assert_eq!(
            id(&["fluxvm-role=admin", "fluxvm-tenant=a"])
                .tenant
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            id(&["fluxvm-role=x", "fluxvm-role=admin"]).role,
            Role::ReadOnly
        );
        assert_eq!(id(&["fluxvm-role=Admin"]).role, Role::ReadOnly);
        assert_eq!(id(&["eng"]).role, Role::ReadOnly);
        assert_eq!(id(&["fluxvm-tenant="]).tenant, None);
    }
}
