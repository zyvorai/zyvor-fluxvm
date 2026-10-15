// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Two (and a third, wrongly keyed) real switches on one machine, joined by the real AES-256-GCM overlay over loopback
//! UDP: guests on different "Macs" reach each other, learned MACs turn floods into unicast, and a switch holding another
//! key never sees the frames nor injects its own. It exercises the mesh, MTU-sized sealing and key handling that the
//! unit tests cover only with a stand-in cipher. It does not touch a real NIC or a second Mac.

use fluxvm_vz_switch::overlay::{AesGcm, Overlay, OverlayConfig};
use fluxvm_vz_switch::{Hello, send_hello, serve_with_overlay, wait_ack};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MAC_A: [u8; 6] = [2, 0, 0, 0, 0, 0x0a];
const MAC_B: [u8; 6] = [2, 0, 0, 0, 0, 0x0b];
const MAC_C: [u8; 6] = [2, 0, 0, 0, 0, 0x0c];
const BROADCAST: [u8; 6] = [0xff; 6];

fn mac_str(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn dgram_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Joins a guest to the switch at `sock`; returns the guest's frame socket and the control stream (keep it alive).
fn join(sock: &Path, vm: &str, mac: &[u8; 6]) -> (OwnedFd, UnixStream) {
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
            mac: mac_str(mac),
        },
        switch_end.as_raw_fd(),
    )
    .unwrap();
    wait_ack(&stream).unwrap();
    (guest, stream)
}

fn frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
    let mut f = dst.to_vec();
    f.extend_from_slice(&src);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(payload);
    f
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
    let mut b = vec![0u8; 4096];
    let n = unsafe { libc::recv(fd.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), 0) };
    (n > 0).then(|| b[..n as usize].to_vec())
}

fn overlay(key: u8) -> Arc<Overlay> {
    let cfg = OverlayConfig {
        network: "acme-net".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        peers: Vec::new(),
    };
    Arc::new(Overlay::bind(cfg, Box::new(AesGcm::new(&[key; 32]))).unwrap())
}

fn start(dir: &Path, name: &str, ov: Arc<Overlay>) -> std::path::PathBuf {
    let sock = dir.join(format!("{name}.sock"));
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = serve_with_overlay(&s, Duration::from_secs(20), Some(ov));
    });
    sock
}

#[test]
fn guests_on_two_switches_talk_and_a_wrong_key_is_deaf_and_mute() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (overlay(7), overlay(7), overlay(9));
    let addr = |o: &Arc<Overlay>| -> SocketAddr { o.local_addr().unwrap() };
    let (aa, ba, ca) = (addr(&a), addr(&b), addr(&c));
    // A full mesh: each switch knows the other two; C's key differs.
    a.set_peers(vec![ba, ca]);
    b.set_peers(vec![aa, ca]);
    c.set_peers(vec![aa, ba]);

    let sa = start(dir.path(), "a", a);
    let sb = start(dir.path(), "b", b);
    let sc = start(dir.path(), "c", c);
    let (ga, _ka) = join(&sa, "vm-a", &MAC_A);
    let (gb, _kb) = join(&sb, "vm-b", &MAC_B);
    let (gc, _kc) = join(&sc, "vm-c", &MAC_C);
    std::thread::sleep(Duration::from_millis(100));

    // A broadcast (an ARP request would be one) crosses to the other Mac...
    let bcast = frame(BROADCAST, MAC_A, b"who-has");
    send(&ga, &bcast);
    assert_eq!(
        recv(&gb, 2000),
        Some(bcast),
        "the flood reaches the guest on the other switch"
    );
    // ...but never to the wrongly keyed one.
    assert_eq!(
        recv(&gc, 500),
        None,
        "a switch with another key does not open the frames"
    );

    // The reply is unicast; B has learned A's MAC from the flood, so it goes straight over.
    let reply = frame(MAC_A, MAC_B, b"is-at");
    send(&gb, &reply);
    assert_eq!(recv(&ga, 2000), Some(reply));
    assert_eq!(recv(&gc, 300), None);

    // A near-MTU frame survives sealing and the datagram.
    let big = frame(MAC_B, MAC_A, &vec![0xab; 1400]);
    send(&ga, &big);
    assert_eq!(recv(&gb, 2000), Some(big));

    // The wrongly keyed switch's own guest cannot inject: nothing it sends arrives on A or B.
    send(&gc, &frame(BROADCAST, MAC_C, b"intruder"));
    assert_eq!(recv(&ga, 500), None);
    assert_eq!(recv(&gb, 500), None);
}
