// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Opinionated one-command personal-cloud bootstrap helpers.

use crate::catalog::{CatalogStore, CloudCatalog};
use crate::enroll::{EnrollmentAuthority, UsedNonceStore, apply_join};
use crate::ipv6::Ipv6Overlay;
use crate::keydist::{KeyDistributionStore, default_keydist_path};
use crate::public::{PublicConfigStore, default_public_config_path};
use crate::reconcile::rotate_wireguard_key;
use crate::share::{ShareAuthority, default_share_secret_path};
use crate::{DEFAULT_POOL, MeshError, MeshState, Result, StateStore};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct CloudPaths {
    pub state: PathBuf,
    pub catalog: PathBuf,
    pub key: PathBuf,
    pub enroll_secret: PathBuf,
    pub used_nonces: PathBuf,
    pub keydist: PathBuf,
    pub public_config: PathBuf,
    pub share_secret: PathBuf,
    pub acme_account_key: PathBuf,
    pub acme_marker: PathBuf,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub challenge_dir: PathBuf,
}

impl Default for CloudPaths {
    fn default() -> Self {
        Self {
            state: crate::default_state_path(),
            catalog: crate::catalog::default_catalog_path(),
            key: crate::mesh_dir().join("wireguard.key"),
            enroll_secret: crate::enroll::default_secret_path(),
            used_nonces: crate::enroll::default_nonce_path(),
            keydist: default_keydist_path(),
            public_config: default_public_config_path(),
            share_secret: default_share_secret_path(),
            acme_account_key: crate::acme::default_account_key_path(),
            acme_marker: crate::acme::default_issued_marker_path(),
            tls_cert: crate::acme::default_certificate_path(),
            tls_key: crate::acme::default_certificate_key_path(),
            challenge_dir: crate::ingress::default_challenge_dir(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudStatus {
    pub local_node: String,
    pub ipv4: std::net::Ipv4Addr,
    pub ipv6: std::net::Ipv6Addr,
    pub nodes: usize,
    pub services: usize,
    pub relays: usize,
    pub pending_key_rotations: usize,
    pub public_routes: usize,
}

pub fn up(
    paths: &CloudPaths,
    node_id: &str,
    hostname: &str,
    pool: Option<&str>,
) -> Result<CloudStatus> {
    if !paths.state.exists() {
        let public_key = rotate_wireguard_key(&paths.key)?;
        let prefix = pool.unwrap_or(DEFAULT_POOL).parse()?;
        let state = MeshState::new(prefix, node_id, hostname, public_key)?;
        StateStore::new(&paths.state).save(&state)?;
        CatalogStore::new(&paths.catalog).save(&CloudCatalog::default())?;
        let _ = EnrollmentAuthority::load_or_create(&paths.enroll_secret)?;
        KeyDistributionStore::new(&paths.keydist).save(&Default::default())?;
    }
    status(paths)
}

pub fn status(paths: &CloudPaths) -> Result<CloudStatus> {
    let mesh = StateStore::new(&paths.state).load()?;
    let catalog = CatalogStore::new(&paths.catalog).load(&mesh)?;
    let keys = KeyDistributionStore::new(&paths.keydist).load()?;
    let local = mesh.node(&mesh.local_node)?;
    let public = PublicConfigStore::new(&paths.public_config).load()?;
    let ipv6 = Ipv6Overlay::for_mesh(&mesh).address_for(&mesh.local_node)?;
    Ok(CloudStatus {
        local_node: mesh.local_node.clone(),
        ipv4: local.overlay_ip,
        ipv6,
        nodes: mesh.nodes.len(),
        services: catalog.services.services.len(),
        relays: catalog.relays.relays.len(),
        pending_key_rotations: keys.proposals.len(),
        public_routes: public.ingress.len(),
    })
}

pub fn issue_join_token(
    paths: &CloudPaths,
    node_id: &str,
    hostname: &str,
    public_key: &str,
    ttl: Duration,
) -> Result<String> {
    let authority = EnrollmentAuthority::load_or_create(&paths.enroll_secret)?;
    authority.issue(node_id, hostname, public_key, ttl)
}

pub fn issue_open_join_token(
    paths: &CloudPaths,
    node_id: &str,
    hostname: &str,
    ttl: Duration,
) -> Result<String> {
    issue_join_token(paths, node_id, hostname, "*", ttl)
}

pub fn accept_join_token(paths: &CloudPaths, token: &str) -> Result<std::net::Ipv4Addr> {
    let authority = EnrollmentAuthority::load_or_create(&paths.enroll_secret)?;
    let claims = authority.verify(token, now_ms())?;
    if claims.public_key == "*" {
        return Err(MeshError::InvalidState(
            "open enrollment invitations must be accepted through verified TLS enrollment".into(),
        ));
    }
    UsedNonceStore::new(&paths.used_nonces).consume(&claims.nonce)?;
    let store = StateStore::new(&paths.state);
    let mut mesh = store.load()?;
    let ip = apply_join(&mut mesh, &claims)?;
    store.save(&mesh)?;
    Ok(ip)
}

pub fn ensure_paths(paths: &CloudPaths) -> Result<()> {
    for path in [
        &paths.state,
        &paths.catalog,
        &paths.key,
        &paths.enroll_secret,
        &paths.used_nonces,
        &paths.keydist,
        &paths.public_config,
        &paths.share_secret,
        &paths.acme_account_key,
        &paths.acme_marker,
        &paths.tls_cert,
        &paths.tls_key,
    ] {
        let parent = path
            .parent()
            .ok_or_else(|| MeshError::InvalidState(format!("{} has no parent", path.display())))?;
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub fn issue_share_token(paths: &CloudPaths, route: &str, ttl: Duration) -> Result<String> {
    ShareAuthority::load_or_create(&paths.share_secret)?.issue(route, ttl)
}

pub fn public_store(paths: &CloudPaths) -> PublicConfigStore {
    PublicConfigStore::new(&paths.public_config)
}

pub fn key_path(paths: &CloudPaths) -> &Path {
    &paths.key
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_are_separated_by_function() {
        let p = CloudPaths::default();
        assert_ne!(p.state, p.catalog);
        assert_ne!(p.key, p.enroll_secret);
    }
}
