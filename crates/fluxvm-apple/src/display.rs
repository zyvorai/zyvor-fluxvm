// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `display-resize`: resize a running VM's display through the runner control socket
//! (`VZGraphicsDisplay.reconfigure(configuration:)`, macOS 14+). UNVERIFIED on hardware: whether the guest
//! follows the new size depends on its display driver.

use anyhow::{Context, Result, bail};
use fluxvm_core::model::VmRecord;
use serde_json::{Value, json};

/// Bounds shared with `apple.display_width` / `apple.display_height` / `apple.display_ppi` validation.
const WIDTH: std::ops::RangeInclusive<u32> = 800..=5120;
const HEIGHT: std::ops::RangeInclusive<u32> = 600..=2880;
const PPI: std::ops::RangeInclusive<u32> = 72..=300;

/// The one-line request the runner's control loop expects. `ppi` only applies to macOS guests.
pub fn display_resize_request(
    width: u32,
    height: u32,
    display: u32,
    ppi: Option<u32>,
) -> Result<Value> {
    if !WIDTH.contains(&width) {
        bail!("display width must be {}..={}", WIDTH.start(), WIDTH.end());
    }
    if !HEIGHT.contains(&height) {
        bail!(
            "display height must be {}..={}",
            HEIGHT.start(),
            HEIGHT.end()
        );
    }
    if display >= 8 {
        bail!("display index must be 0..=7");
    }
    let mut req =
        json!({"cmd": "display-resize", "width": width, "height": height, "display": display});
    if let Some(p) = ppi {
        if !PPI.contains(&p) {
            bail!("display ppi must be {}..={}", PPI.start(), PPI.end());
        }
        req["ppi"] = json!(p);
    }
    Ok(req)
}

/// Resizes display `display` of a running VM. The runner answers with the applied size or a clear error
/// (not running, macOS older than 14, no such display).
pub async fn display_resize(
    vm: &VmRecord,
    width: u32,
    height: u32,
    display: u32,
    ppi: Option<u32>,
) -> Result<()> {
    let request = display_resize_request(width, height, display, ppi)?;
    let socket = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let reply = crate::control_call_with(socket, request).await?;
    if !reply.ok() {
        bail!(
            "runner refused display-resize: {}",
            reply.error().unwrap_or("unknown error")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_control_request() {
        let r = display_resize_request(1920, 1080, 0, None).unwrap();
        assert_eq!(
            r,
            json!({"cmd":"display-resize","width":1920,"height":1080,"display":0})
        );
        let r = display_resize_request(2560, 1600, 1, Some(220)).unwrap();
        assert_eq!(r["ppi"], 220);
        assert_eq!(r["display"], 1);
    }

    #[test]
    fn rejects_out_of_range_sizes() {
        assert!(display_resize_request(799, 1080, 0, None).is_err());
        assert!(display_resize_request(1920, 5000, 0, None).is_err());
        assert!(display_resize_request(1920, 1080, 8, None).is_err());
        assert!(display_resize_request(1920, 1080, 0, Some(10)).is_err());
    }

    #[test]
    fn swift_runner_handles_the_command_and_preconditions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let swift = std::fs::read_to_string(root.join("runner/Runner.swift")).unwrap();
        assert!(swift.contains("case \"display-resize\""));
        for c in ["pause", "resume", "shutdown", "stop"] {
            assert!(
                swift.contains(&format!("controlPrecondition(\"{c}\")")),
                "{c} precondition missing"
            );
        }
        let modern = std::fs::read_to_string(root.join("runner/ModernFeatures.swift")).unwrap();
        assert!(modern.contains("reconfigure(configuration:"));
        for c in [
            "canStart",
            "canPause",
            "canResume",
            "canRequestStop",
            "canStop",
        ] {
            assert!(modern.contains(c), "{c} check missing");
        }
    }
}
