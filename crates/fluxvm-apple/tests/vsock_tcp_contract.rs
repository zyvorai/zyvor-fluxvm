// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//! `apple.vsock_services[].tcp`: a guest vsock port relayed to a loopback TCP server on the Mac (a local
//! OpenAI-compatible model server). Nothing here opens a socket; the relay itself runs in the Swift runner and was
//! verified on an M4 against a stand-in and a real `llama-server` (docs/macos.md).
use std::{fs, path::PathBuf};

fn read(rel: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn runner_relay_connects_to_a_constant_loopback_address() {
    let swift = read("runner/Runner.swift");
    assert!(swift.contains("let tcp_port: UInt16?"));
    let f = swift
        .find("func connectLoopback(_ port: UInt16)")
        .expect("connectLoopback");
    let body = &swift[f..f + 900];
    assert!(body.contains("UInt32(0x7f000001).bigEndian"));
    assert!(
        !body.contains("getaddrinfo"),
        "no name resolution in the loopback relay"
    );
    // The relay only runs for a non-privileged port taken from the validated config.
    assert!(
        swift.contains("let port = svc.tcp_port, port >= 1024, let u = self.connectLoopback(port)")
    );
}

#[test]
fn runner_config_carries_a_port_not_a_host() {
    let rs = read("src/runner.rs");
    assert!(rs.contains("pub tcp_port: Option<u16>"));
    assert!(rs.contains("loopback_tcp_port(t)"));
    assert!(!rs.contains("tcp_host"));
}

#[test]
fn spec_round_trips_and_defaults_to_no_tcp() {
    let s: fluxvm_core::model::AppleVsockService =
        serde_json::from_str(r#"{"port":5100,"tcp":"127.0.0.1:11434"}"#).unwrap();
    assert_eq!(s.tcp.as_deref(), Some("127.0.0.1:11434"));
    assert!(s.socket.is_none() && s.builtin.is_none());
    let none: fluxvm_core::model::AppleVsockService =
        serde_json::from_str(r#"{"port":5000,"socket":"x"}"#).unwrap();
    assert!(none.tcp.is_none());
    assert!(!serde_json::to_string(&none).unwrap().contains("tcp"));
}
