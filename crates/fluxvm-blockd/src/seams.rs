// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Extension seams. Documented, deliberately unimplemented.
//!
//! * Encryption: wrap a [`BlockBackend`](crate::BlockBackend) with a type
//!   implementing [`BlockCipher`]; it transforms each 64 KiB block (or a
//!   sub-block sector, XTS-style) on write and read. Place it *below* the
//!   CoW layer to encrypt overlay data, or above it to encrypt the guest view.
//!   Not implemented: key management is undecided.
//! * Lazy pull: a [`LazySource`] is a `BlockBackend` used as the CoW base
//!   that fetches missing blocks (e.g. over HTTP range requests or from a
//!   registry) on first read and caches them. `CowBackend::create_on` accepts
//!   any `Arc<dyn BlockBackend>` as base so no change is needed in the CoW
//!   layer; the persisted overlay header only knows file parents today, so a
//!   lazy base would need a new `parent_kind`. Not implemented.

use std::io;

/// Per-block transform (encryption seam). Not implemented by this crate.
pub trait BlockCipher: Send + Sync {
    /// `block_index` is the tweak/nonce input.
    fn encrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()>;
    fn decrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()>;
}

/// Lazy-pull seam. Not implemented by this crate.
pub trait LazySource: crate::BlockBackend {
    /// Hint that `[off, off+len)` will be read soon.
    fn prefetch(&self, off: u64, len: u64);
}
