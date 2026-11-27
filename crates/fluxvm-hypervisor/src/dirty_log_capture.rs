// Copyright 2026 Zyvor · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Skeleton for KVM dirty-log capture used by incremental snapshots.
//!
//! Full ioctl wiring (KVM_MEM_LOG_DIRTY_PAGES + KVM_GET_DIRTY_LOG) is still TODO.
//! This module defines the types so the snapshot path can call into it later.

/// A single dirty page recorded during an incremental snapshot.
#[derive(Debug, Clone)]
pub struct DirtyPage {
    /// Guest physical address of the page.
    pub gpa: u64,
    /// Optional page contents (filled by the real capture path).
    pub data: Option<Vec<u8>>,
}

/// Result of a dirty-log capture.
#[derive(Debug, Default)]
pub struct DirtyLog {
    pub pages: Vec<DirtyPage>,
}

impl DirtyLog {
    /// Placeholder capture. Returns an empty log until the KVM ioctls are wired.
    pub fn capture(_vm_fd: i32, _slot: u32) -> Result<Self, String> {
        Err("KVM dirty-log capture not yet implemented".into())
    }
}
