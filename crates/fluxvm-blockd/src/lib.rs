// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! FluxDisk block server: NBD (newstyle, fixed handshake) over TCP, with raw
//! file and copy-on-write overlay backends.
//!
//! VZ attaches with `nbd://127.0.0.1:<port>/<export-name>` via
//! `VZNetworkBlockDeviceStorageDeviceAttachment`. Interop with VZ itself has
//! NOT been verified; the protocol is tested against an in-tree client only.

pub mod backend;
pub mod cow;
pub mod limiter;
pub mod nbd;
pub mod seams;
pub mod sparse;

pub use backend::{BlockBackend, RawFile};
pub use cow::CowBackend;
pub use limiter::Limiter;
pub use nbd::{Export, Server};

/// Overlay allocation granularity.
pub const BLOCK_SIZE: u64 = 64 * 1024;
