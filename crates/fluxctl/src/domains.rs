// Copyright 2026 Zyvor · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! `fluxctl domains` — list and set local domain mappings.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Domain {
    pub name: String,
    pub http: Option<u16>,
    pub https: Option<u16>,
}

pub fn list() -> Result<Vec<Domain>, String> {
    Ok(vec![])
}

pub fn set(name: &str, http: Option<u16>, https: Option<u16>) -> Result<(), String> {
    if name.is_empty() {
        return Err("name required".into());
    }
    let _ = (http, https);
    Err("domains set is not yet wired to the control plane".into())
}
