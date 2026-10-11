// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Extension seams and their implementations.
//!
//! * Encryption: [`BlockCipher`] transforms each 64 KiB overlay block on
//!   write and read. Implemented by [`crate::crypt::XtsCipher`]
//!   (AES-256-XTS, tweak = block index) and applied by
//!   [`CowBackend`](crate::CowBackend) to the overlay's own data region.
//! * Lazy pull: a [`LazySource`] is a read-only `BlockBackend` used as the CoW
//!   base that fetches missing chunks on first read and caches them.
//!   Implemented by [`crate::lazy::LazyHttp`] (HTTP(S) range reads into a
//!   sparse on-disk cache); recorded in the overlay header as parent kind 2.

use std::io;

/// Per-block transform (encryption seam).
pub trait BlockCipher: Send + Sync {
    /// `block_index` is the tweak/nonce input. `data` is a whole block.
    fn encrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()>;
    fn decrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()>;
    /// Salted key-check value stored in the overlay header so a wrong key is
    /// rejected at open. Must not reveal the key.
    fn key_check(&self, salt: &[u8; 16]) -> [u8; 32];
}

/// Lazy-pull seam.
pub trait LazySource: crate::BlockBackend {
    /// Hint that `[off, off+len)` will be read soon (best effort, async).
    fn prefetch(&self, off: u64, len: u64);
}
