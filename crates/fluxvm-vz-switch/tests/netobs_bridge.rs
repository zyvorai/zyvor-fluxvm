// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The switch exporting observed frames to a `fluxvm-netobs` bridge server in another "process": once through the
//! library (`connect_bridge`) and once through the real `fluxvm-vz-switch --netobs-socket` binary.
#![cfg(feature = "netobs")]

use fluxvm_netobs::bridge::{BridgeServer, BridgeServerStats};
use fluxvm_netobs::packet::build::{self, udp_v4};
use fluxvm_netobs::{FlowQuery, Observer, ObserverConfig};
use fluxvm_vz_switch::{Hello, send_hello, wait_ack};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAC_A: &str = "02:00:00:00:00:01"; // build::GUEST_MAC, the source of every fixture frame
const MAC_B: &str = "02:00:00:00:00:02"; // build::PEER_MAC, its destination
const MAC_C: &str = "02:00:00:00:00:03";

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn dgram_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Joins a guest to the switch at `sock`; returns the guest's end of the frame socket and the control stream.
fn join(sock: &Path, vm: &str, mac: &str) -> (OwnedFd, UnixStream) {
    let (guest, switch_end) = dgram_pair();
    let stream = loop {
        match UnixStream::connect(sock) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    send_hello(
        &stream,
        &Hello {
            vm: vm.into(),
            mac: mac.into(),
        },
        switch_end.as_raw_fd(),
    )
    .unwrap();
    wait_ack(&stream).unwrap();
    (guest, stream)
}

fn send(fd: &OwnedFd, f: &[u8]) {
    let n = unsafe { libc::send(fd.as_raw_fd(), f.as_ptr().cast(), f.len(), 0) };
    assert!(n > 0, "send: {}", std::io::Error::last_os_error());
}

fn recv(fd: &OwnedFd, ms: i32) -> Option<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
        return None;
    }
    let mut b = vec![0u8; 2048];
    let n = unsafe { libc::recv(fd.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), 0) };
    (n > 0).then(|| b[..n as usize].to_vec())
}

fn frame(sport: u16) -> Vec<u8> {
    udp_v4(
        (Ipv4Addr::new(10, 1, 0, 1), sport),
        (Ipv4Addr::new(10, 1, 0, 2), 9000),
        b"through the switch",
    )
}

#[test]
fn library_export_survives_the_observer_starting_late_and_restarting() {
    assert_eq!(
        (build::GUEST_MAC, build::PEER_MAC),
        ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 2])
    );
    let dir = tempfile::tempdir().unwrap();
    let bridge = dir.path().join("obs.sock");
    let net = dir.path().join("net.sock");

    // The switch comes up first; nothing listens on the bridge socket yet.
    assert!(fluxvm_vz_switch::connect_bridge(&bridge, "lib-net"));
    assert!(
        !fluxvm_vz_switch::connect_bridge(&bridge, "other"),
        "a second sink is refused"
    );
    let n2 = net.clone();
    let switch = std::thread::spawn(move || {
        fluxvm_vz_switch::serve(&n2, Duration::from_millis(2500)).unwrap()
    });
    let (a, _a_ctl) = join(&net, "vm-a", MAC_A);
    let (b, _b_ctl) = join(&net, "vm-b", MAC_B);
    std::thread::sleep(Duration::from_millis(100));

    // Frames still forward with the observer absent.
    send(&a, &frame(1111));
    assert_eq!(recv(&b, 1000), Some(frame(1111)));

    // The observer's side starts late.
    let obs = Arc::new(Observer::new(ObserverConfig::default()));
    let running = BridgeServer::bind(&bridge).unwrap().spawn(obs.clone());
    assert!(wait_for(|| {
        send(&a, &frame(2222));
        let _ = recv(&b, 50);
        obs.counters().frames > 0
    }));
    let flows = obs.flows(&FlowQuery::default());
    let f = flows
        .iter()
        .find(|f| f.client.port == 2222)
        .expect("a flow for the frame");
    assert_eq!(
        (f.vm_id.as_str(), f.network.as_str(), f.nic.as_str()),
        ("vm-a", "lib-net", "private")
    );
    assert_eq!(f.mac, MAC_A);
    assert_eq!(f.verdict, "allowed");

    // A spoofed source is observed with the switch's verdict.
    let mut spoof = frame(3333);
    spoof[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 3]);
    assert!(wait_for(|| {
        send(&a, &spoof);
        obs.flows(&FlowQuery::default())
            .iter()
            .any(|f| f.client.port == 3333 && f.verdict == "dropped")
    }));
    let f = obs
        .flows(&FlowQuery::default())
        .into_iter()
        .find(|f| f.client.port == 3333)
        .unwrap();
    assert_eq!(f.drop_reason, Some("spoofed_source"));
    let _ = MAC_C;

    // The observer's process restarts at the same path: the switch's client reconnects on its own.
    running.shutdown().unwrap();
    let obs2 = Arc::new(Observer::new(ObserverConfig::default()));
    let running2 = BridgeServer::bind(&bridge).unwrap().spawn(obs2.clone());
    assert!(wait_for(|| {
        send(&a, &frame(4444));
        let _ = recv(&b, 50);
        obs2.flows(&FlowQuery::default())
            .iter()
            .any(|f| f.client.port == 4444)
    }));
    assert!(
        BridgeServerStats::get(&running2.stats.idents) >= 1,
        "identities are re-announced on reconnect"
    );
    running2.shutdown().unwrap();
    drop((a, b));
    switch.join().unwrap();
}

#[test]
fn the_switch_binary_exports_with_netobs_socket() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = dir.path().join("obs.sock");
    let net = dir.path().join("priv-net.sock");
    let obs = Arc::new(Observer::new(ObserverConfig::default()));
    let running = BridgeServer::bind(&bridge).unwrap().spawn(obs.clone());

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fluxvm-vz-switch"))
        .arg("--socket")
        .arg(&net)
        .arg("--idle-exit-secs")
        .arg("2")
        .arg("--netobs-socket")
        .arg(&bridge)
        .spawn()
        .unwrap();

    let (a, _a_ctl) = join(&net, "vm-a", MAC_A);
    let (b, _b_ctl) = join(&net, "vm-b", MAC_B);
    std::thread::sleep(Duration::from_millis(100));
    assert!(wait_for(|| {
        send(&a, &frame(5555));
        let got = recv(&b, 100).is_some();
        got && obs
            .flows(&FlowQuery::default())
            .iter()
            .any(|f| f.client.port == 5555)
    }));
    let f = obs
        .flows(&FlowQuery::default())
        .into_iter()
        .find(|f| f.client.port == 5555)
        .unwrap();
    // The network name defaults to the stem of `--socket`.
    assert_eq!((f.vm_id.as_str(), f.network.as_str()), ("vm-a", "priv-net"));
    drop((a, b, _a_ctl, _b_ctl));
    let status = {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            match child.try_wait().unwrap() {
                Some(s) => break s,
                None if Instant::now() > end => {
                    let _ = child.kill();
                    panic!("the switch did not exit when idle");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    };
    assert!(status.success(), "{status:?}");
    running.shutdown().unwrap();
}

#[test]
fn bad_usage_is_rejected() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fluxvm-vz-switch"))
        .arg("--netobs-socket")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--netobs-socket"));
}
