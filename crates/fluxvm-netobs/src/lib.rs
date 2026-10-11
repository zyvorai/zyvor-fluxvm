// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Native guest-network observability for FluxVM.
//!
//! Frames a guest sends or receives are handed to an [`Observer`] (directly via [`Observer::ingest`], or through
//! a non-blocking [`NetTap`] from a datapath hot loop). The observer parses them ([`packet`]), tracks bidirectional
//! flows ([`flow`]) with TCP health ([`health`]), extracts DNS/TLS/HTTP/QUIC metadata ([`l7`], with
//! [`l7::Redaction`]), maintains RED roll-ups with bounded history ([`rollup`]) and an opt-in, TTL-bound pcap ring
//! ([`capture`]), and publishes versioned JSON/Prometheus/SSE views ([`export`]).
//!
//! Design stance: metadata only by default; packet bytes are retained only by an explicit, filtered, expiring
//! capture session. Everything is bounded; nothing here panics on hostile input.

pub mod capture;
pub mod export;
pub mod flow;
pub mod health;
pub mod l7;
pub mod observer;
pub mod packet;
pub mod rollup;
pub mod source;

pub use capture::{CaptureError, CaptureFilter, CaptureManager, CaptureRequest};
pub use export::{NetEvent, SCHEMA_VERSION, Summary, render_prometheus};
pub use flow::{Direction, DropReason, FrameMeta, Identity, Verdict};
pub use l7::{Redact, Redaction};
pub use observer::{FlowQuery, Observer, ObserverConfig, global, install_global};
pub use source::{FrameSource, NetTap, Tap, TapReceiver, now_us, spawn_pump};
