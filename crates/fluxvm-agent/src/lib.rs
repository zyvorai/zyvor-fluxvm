// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Distributed node-agent for multi-host Zyvor FluxVM deployments. Two
//! halves, both in this one binary (`fluxvm-agent central` /
//! `fluxvm-agent node`): `central` is a fleet registry + create/list/
//! delete proxy across every registered node; `node` is a per-host
//! heartbeat client reporting capacity + VM count to it. Distinct from
//! `fluxvm-kube` (per-node Kubernetes reconciliation against a *local*
//! fluxvm) — this is the non-Kubernetes multi-host story: a caller
//! talks to one central endpoint instead of knowing which host a VM is on.

pub mod billing;
pub mod central;
pub mod gha;
pub mod ha;
pub mod node;
pub mod shared_state;
pub mod sim;
pub mod sim_fleet;
pub mod stripe;
pub mod tenancy;
pub mod volumes;
pub mod warmpool;
