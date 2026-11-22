// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! FluxVM's own Linux container runtime (see `docs/linux-runtime.md`).
//!
//! A container is described by a [`spec::ContainerSpec`]. This crate decides how a workload should run
//! ([`runtime`]), reports what the host can do ([`probe`]), renders the container as a systemd service
//! ([`render`]) and, on Linux, launches it with namespaces, cgroup v2, seccomp and dropped capabilities ([`launch`]).
//! Everything except [`launch`] is plain data and compiles and tests on any host.

pub mod probe;
pub mod render;
pub mod runtime;
pub mod spec;

#[cfg(target_os = "linux")]
pub mod launch;

/// Name of the launcher binary that `systemd-run` (or the daemon, without systemd) starts for every container.
pub const LAUNCHER: &str = "fluxvm-container-run";
