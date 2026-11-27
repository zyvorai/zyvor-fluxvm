// Copyright 2026 Zyvor · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Stub for `fluxctl share` — Ngrok-style port exposure.
//!
//! Real implementation will open a tunnel and print a public URL.
//! Today it only documents the plan.

pub fn share(_port: u16, _name: Option<&str>) -> Result<String, String> {
    Err("fluxctl share is not yet implemented. See docs/SHARE.md for the design.".into())
}
