// SPDX-License-Identifier: Apache-2.0
//! `apple.macos_vsock` is an experimental, opt-in flag: the Rust spec, the runner config, the Swift runner and the guest
//! agent's LaunchDaemon files agree on its name and default off. Nothing here boots a VM; whether a macOS guest exposes a
//! usable vsock device is UNVERIFIED on hardware.
use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn runner_adds_a_socket_device_to_macos_guests_only_behind_the_flag() {
    let swift = read("runner/Runner.swift");
    assert!(swift.contains("let macos_vsock: Bool?"));
    let start = swift
        .find("c.pointingDevices = [VZMacTrackpadConfiguration()]")
        .expect("macOS branch");
    let branch = &swift[start..];
    let guarded = branch
        .find("if cfg.macos_vsock == true {")
        .expect("macOS socket device must sit behind cfg.macos_vsock");
    let device = branch.find("VZVirtioSocketDeviceConfiguration()").unwrap();
    assert!(guarded < device, "device configured before the flag check");
    // The macOS branch ends at its `} else {`; the device must be inside it.
    let else_at = branch.find("} else {").unwrap();
    assert!(device < else_at, "the guarded device must be in the macOS branch");
    assert!(
        swift.contains("unverified"),
        "the runner comment must say the path is unverified"
    );
}

#[test]
fn flag_is_in_the_runner_config_and_off_by_default() {
    assert!(read("src/runner.rs").contains("macos_vsock: apple.macos_vsock"));
    let spec: fluxvm_core::model::AppleSpec = serde_json::from_str("{}").unwrap();
    assert!(!spec.macos_vsock);
    assert!(!fluxvm_core::model::AppleSpec::default().macos_vsock);
}

#[test]
fn capability_matrix_labels_it_experimental_and_unverified() {
    let row = fluxvm_apple::CAPABILITIES
        .iter()
        .find(|c| c.feature.contains("macos_vsock"))
        .expect("capability row");
    assert!(row.supported);
    assert!(row.note.contains("EXPERIMENTAL") && row.note.contains("unverified"));
}

#[test]
fn launch_daemon_files_match_the_agent_defaults() {
    let plist = read("../fluxvm-guest-agent/macos/dev.zyvor.fluxvm-guest-agent.plist");
    assert!(plist.contains(&format!(
        "<string>{}</string>",
        fluxvm_guest_protocol::DEFAULT_PORT
    )));
    assert!(plist.contains(fluxvm_guest_protocol::TOKEN_FILE_PATH));
    assert!(plist.contains("<key>KeepAlive</key>"));
    let install = read("../fluxvm-guest-agent/macos/install.sh");
    assert!(install.contains(fluxvm_guest_protocol::TOKEN_FILE_PATH));
    assert!(install.contains("dev.zyvor.fluxvm-guest-agent"));
    assert!(install.contains("UNVERIFIED"));
}
