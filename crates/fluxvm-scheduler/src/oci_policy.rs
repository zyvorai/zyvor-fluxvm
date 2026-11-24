// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The image policy at the moment a container sandbox starts. The rules live in `fluxvm_image::supply`; this is the one
//! call both container start paths (`vz` OCI sandboxes and the native Linux runtime) make after the image is pulled.

use anyhow::Result;
use fluxvm_core::config::Config;
use fluxvm_image::oci_registry::PulledImage;
use fluxvm_image::sbom::Subject;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Refuses the start (`PolicyBlocked`, shown as HTTP 403) when `<state_dir>/oci/policy.toml` forbids `image`, unless
/// `override_policy` is set, in which case the start goes ahead and the override is logged. No policy file: no effect.
pub(crate) fn enforce(
    cfg: &Config,
    image: &PulledImage,
    override_policy: bool,
    actor: Option<&str>,
) -> Result<()> {
    let who = actor.unwrap_or("local");
    let result = fluxvm_image::supply::enforce(
        &cfg.state_dir,
        &Subject::from(image),
        override_policy,
        who,
        now(),
    );
    // Every decision the policy makes is kept in the audit log (`fluxctl audit ls --action image.policy`).
    let entry = |outcome| {
        crate::audit::Entry::new("image.policy")
            .target(&image.reference)
            .actor(who)
            .source("api")
            .outcome(outcome)
    };
    match &result {
        Ok(Some(d)) if d.overridden => crate::audit::record(
            entry(crate::audit::Outcome::Ok).detail("note", "policy overridden"),
        ),
        Ok(Some(_)) => {
            crate::audit::record(entry(crate::audit::Outcome::Ok).detail("note", "allowed"))
        }
        Ok(None) => {}
        Err(e) => crate::audit::record(
            entry(crate::audit::Outcome::Denied)
                .detail("error", e.to_string().lines().next().unwrap_or("")),
        ),
    }
    result.map(|_| ())
}
