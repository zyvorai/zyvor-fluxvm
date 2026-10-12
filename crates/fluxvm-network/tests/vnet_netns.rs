// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Root + Linux integration test for virtual networks, driven through the
//! same entry points the daemon uses (`record_port` -> `attach_vm`,
//! `apply_config`, `status`, `detach_vm`), with two simulated hosts as network
//! namespaces. Each host gets its own pin root and state dir, so the two copies
//! of the programs and maps are as separate as on two machines. Guests are
//! namespaces behind a veth (TC sees a veth exactly as it sees a tap).
//!
//!   g1 10.50.0.11 (vni 100) ─┐                       ┌─ g2 10.50.0.12 (vni 100)
//!   g4 10.50.0.13 (vni 100) ─┤ host A ══ Geneve ══ host B ├─ g3 10.50.0.11 (vni 200)
//!                            │  192.168.77.1   .2         │
//!                    uplink wA 198.51.100.1
//!                            │
//!                 inet 198.51.100.254 (route 203.0.113.10 via host A)
//!
//! Skipped unless: Linux, root, `ip`/`nsenter`/`ping`/`python3`/`bpftool`/`tc`,
//! and `fluxvm_vnet.bpf.o` in `FLUXVM_TEST_BPF_DIR` (scripts/build-ebpf.sh <dir>).
//!
//!   FLUXVM_TEST_BPF_DIR=/tmp/bpf sudo -E cargo test -p fluxvm-network --test vnet_netns -- --nocapture

use fluxvm_core::{
    config::{Config, DataplaneMode},
    model::{VnetAttach, VnetConfig, VnetEndpoint, VnetFloatingIp},
};
use fluxvm_network::{netns_scope, vnet};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

fn sh(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn have(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn skip(why: &str) {
    eprintln!("SKIP vnet_netns: {why}");
}

fn ns_cmd(ns: &str, cmd: &[&str]) -> Command {
    let mut c = Command::new("nsenter");
    c.arg(format!("--net=/run/netns/{ns}")).arg("--").args(cmd);
    c
}

fn in_ns(ns: &str, cmd: &[&str]) -> bool {
    ns_cmd(ns, cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ns_out(ns: &str, cmd: &[&str]) -> String {
    ns_cmd(ns, cmd)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
        })
        .unwrap_or_default()
}

fn ping(ns: &str, ip: &str) -> bool {
    in_ns(ns, &["ping", "-c", "2", "-i", "0.2", "-W", "1", ip])
}

struct Cleanup {
    namespaces: Vec<String>,
    pin_roots: Vec<PathBuf>,
    server: Option<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(s) = self.server.as_mut() {
            let _ = s.kill();
            let _ = s.wait();
        }
        for ns in &self.namespaces {
            let _ = sh("ip", &["netns", "del", ns]);
        }
        for root in &self.pin_roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

struct Host {
    ns: String,
    cfg: Config,
}

impl Host {
    fn scope(&self) -> netns_scope::NetnsScope {
        netns_scope::enter(Some(&format!("/run/netns/{}", self.ns)))
    }
}

struct Guest {
    ns: String,
    id: Uuid,
    vni: u32,
    ip: &'static str,
    mac: &'static str,
    host_if: String,
}

/// A guest namespace wired to `host` by a veth, configured the way cloud-init
/// configures a vnet guest (static address, gateway, MTU 1450).
fn guest(
    host: &Host,
    name: &str,
    vni: u32,
    ip: &'static str,
    mac: &'static str,
    c: &mut Cleanup,
) -> Guest {
    let ns = format!("{name}-{}", std::process::id());
    assert!(sh("ip", &["netns", "add", &ns]));
    c.namespaces.push(ns.clone());
    let host_if = format!("v{name}");
    assert!(sh(
        "ip",
        &[
            "-n", &host.ns, "link", "add", &host_if, "type", "veth", "peer", "name", "eth0",
            "netns", &ns,
        ]
    ));
    assert!(sh("ip", &["-n", &host.ns, "link", "set", &host_if, "up"]));
    let cidr = format!("{ip}/24");
    for cmd in [
        vec!["sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=1"],
        vec!["ip", "link", "set", "lo", "up"],
        vec!["ip", "link", "set", "eth0", "address", mac, "mtu", "1450"],
        vec!["ip", "addr", "add", &cidr, "dev", "eth0"],
        vec!["ip", "link", "set", "eth0", "up"],
        vec!["ip", "route", "add", "default", "via", "10.50.0.1"],
    ] {
        assert!(in_ns(&ns, &cmd), "{cmd:?} in {ns}");
    }
    Guest {
        ns,
        id: Uuid::new_v4(),
        vni,
        ip,
        mac,
        host_if,
    }
}

fn attach(host: &Host, g: &Guest) {
    let _scope = host.scope();
    vnet::record_port(
        &host.cfg.state_dir,
        g.id,
        &vnet::PortRecord {
            attach: VnetAttach {
                vni: g.vni,
                ip: g.ip.into(),
                prefix_len: 24,
                gateway: "10.50.0.1".into(),
            },
            mac: g.mac.into(),
            tap: g.host_if.clone(),
            ifindex: 0,
            pin_root: None,
        },
    )
    .unwrap();
    assert!(vnet::attach_vm(&host.cfg, g.id).expect("attach_vm"));
}

fn endpoint(g: &Guest, node_ip: &str) -> VnetEndpoint {
    VnetEndpoint {
        ip: g.ip.into(),
        mac: g.mac.into(),
        node_ip: node_ip.into(),
    }
}

fn diag(hosts: &[&Host]) {
    eprintln!("---- DIAG ----");
    for h in hosts {
        eprintln!(
            "[{}] bpftool net:\n{}",
            h.ns,
            ns_out(&h.ns, &["bpftool", "net"])
        );
        eprintln!(
            "[{}] links:\n{}",
            h.ns,
            ns_out(&h.ns, &["ip", "-s", "-br", "link"])
        );
        let maps = h.cfg.sandbox.dataplane.pin_root.join("vnet/maps");
        for m in [
            "fluxvm_vnet_port",
            "fluxvm_vnet_net",
            "fluxvm_vnet_fib",
            "fluxvm_vnet_nat",
        ] {
            let out = Command::new("bpftool")
                .args(["map", "dump", "pinned"])
                .arg(maps.join(m))
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            eprintln!("[{}] {m}:\n{out}", h.ns);
        }
    }
    eprintln!("---- END DIAG ----");
}

#[test]
fn virtual_networks_route_isolate_and_translate() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return skip("root required");
    }
    let Some(bpf_dir) = std::env::var_os("FLUXVM_TEST_BPF_DIR").map(PathBuf::from) else {
        return skip("FLUXVM_TEST_BPF_DIR not set (build with scripts/build-ebpf.sh <dir>)");
    };
    for tool in ["ip", "nsenter", "ping", "python3", "bpftool", "tc"] {
        if !have(tool) {
            return skip(&format!("{tool} not found"));
        }
    }
    if !bpf_dir.join("fluxvm_vnet.bpf.o").exists() {
        return skip("fluxvm_vnet.bpf.o missing in FLUXVM_TEST_BPF_DIR");
    }
    if !Path::new("/sys/fs/bpf").exists() {
        return skip("bpffs not mounted");
    }

    let pid = std::process::id();
    let work = tempfile::tempdir().unwrap();
    let mut c = Cleanup {
        namespaces: vec![],
        pin_roots: vec![],
        server: None,
    };
    let mk_host = |name: &str, c: &mut Cleanup| {
        let ns = format!("fvvt{pid}-{name}");
        assert!(sh("ip", &["netns", "add", &ns]));
        c.namespaces.push(ns.clone());
        assert!(sh("ip", &["-n", &ns, "link", "set", "lo", "up"]));
        let pin_root = PathBuf::from(format!("/sys/fs/bpf/fvvt{pid}-{name}"));
        c.pin_roots.push(pin_root.clone());
        let mut cfg = Config {
            state_dir: work.path().join(name),
            ..Default::default()
        };
        std::fs::create_dir_all(&cfg.state_dir).unwrap();
        cfg.sandbox.dataplane.mode = DataplaneMode::Ebpf;
        cfg.sandbox.dataplane.bpf_object = bpf_dir.join("fluxvm_tc.bpf.o");
        cfg.sandbox.dataplane.pin_root = pin_root;
        Host { ns, cfg }
    };
    let a = mk_host("a", &mut c);
    let b = mk_host("b", &mut c);
    let inet = format!("fvvt{pid}-inet");
    assert!(sh("ip", &["netns", "add", &inet]));
    c.namespaces.push(inet.clone());

    // underlay between the hosts, and host A's uplink to the "internet"
    assert!(sh(
        "ip",
        &[
            "-n", &a.ns, "link", "add", "uA", "type", "veth", "peer", "name", "uB", "netns", &b.ns
        ]
    ));
    assert!(sh(
        "ip",
        &[
            "-n", &a.ns, "link", "add", "wA", "type", "veth", "peer", "name", "iA", "netns", &inet
        ]
    ));
    for (ns, dev, addr) in [
        (&a.ns, "uA", "192.168.77.1/24"),
        (&b.ns, "uB", "192.168.77.2/24"),
        (&a.ns, "wA", "198.51.100.1/24"),
        (&inet, "iA", "198.51.100.254/24"),
    ] {
        assert!(sh("ip", &["-n", ns, "addr", "add", addr, "dev", dev]));
        assert!(sh("ip", &["-n", ns, "link", "set", dev, "up"]));
    }
    assert!(sh("ip", &["-n", &inet, "link", "set", "lo", "up"]));
    assert!(sh(
        "ip",
        &[
            "-n",
            &inet,
            "route",
            "add",
            "203.0.113.10/32",
            "via",
            "198.51.100.1"
        ]
    ));

    let g1 = guest(&a, "g1", 100, "10.50.0.11", "02:00:00:00:00:11", &mut c);
    let g4 = guest(&a, "g4", 100, "10.50.0.13", "02:00:00:00:00:13", &mut c);
    let g2 = guest(&b, "g2", 100, "10.50.0.12", "02:00:00:00:00:12", &mut c);
    // Same address as g1, different network.
    let g3 = guest(&b, "g3", 200, "10.50.0.11", "02:00:00:00:00:31", &mut c);
    for (h, g) in [(&a, &g1), (&a, &g4), (&b, &g2), (&b, &g3)] {
        attach(h, g);
    }

    let net100 = |uplink: bool| VnetConfig {
        vni: 100,
        cidr: "10.50.0.0/24".into(),
        gateway: "10.50.0.1".into(),
        uplink: uplink.then(|| "wA".into()),
        snat_address: uplink.then(|| "198.51.100.1".into()),
        floating_ips: if uplink {
            vec![VnetFloatingIp {
                address: "203.0.113.10".into(),
                ip: "10.50.0.11".into(),
            }]
        } else {
            vec![]
        },
        endpoints: vec![
            endpoint(&g1, "192.168.77.1"),
            endpoint(&g4, "192.168.77.1"),
            endpoint(&g2, "192.168.77.2"),
        ],
    };
    let net200 = VnetConfig {
        vni: 200,
        cidr: "10.50.0.0/24".into(),
        gateway: "10.50.0.1".into(),
        uplink: None,
        snat_address: None,
        floating_ips: vec![],
        endpoints: vec![endpoint(&g3, "192.168.77.2")],
    };
    {
        let _s = a.scope();
        let st = vnet::apply_config(&a.cfg, &net100(true)).expect("apply vni 100 on A");
        assert_eq!(st.local_endpoints, vec!["10.50.0.11", "10.50.0.13"]);
        assert_eq!(st.remote_endpoints, 1);
        vnet::apply_config(&a.cfg, &net200).expect("apply vni 200 on A");
    }
    {
        let _s = b.scope();
        vnet::apply_config(&b.cfg, &net100(false)).expect("apply vni 100 on B");
        vnet::apply_config(&b.cfg, &net200).expect("apply vni 200 on B");
    }

    let check = |what: &str, ok: bool| {
        if !ok {
            diag(&[&a, &b]);
            panic!("{what}");
        }
        eprintln!("ok: {what}");
    };
    check("same-host route g1 -> g4", ping(&g1.ns, g4.ip));
    check("Geneve route g1 (A) -> g2 (B)", ping(&g1.ns, g2.ip));
    check("Geneve route g2 (B) -> g1 (A)", ping(&g2.ns, g1.ip));
    check(
        "the distributed gateway answers ping",
        ping(&g2.ns, "10.50.0.1"),
    );
    check(
        "vni 200 cannot reach vni 100 (g3 -> 10.50.0.12)",
        !ping(&g3.ns, g2.ip),
    );
    check(
        "vni 200 cannot reach vni 100 on another host (g3 -> 10.50.0.13)",
        !ping(&g3.ns, g4.ip),
    );
    // A source address the network did not assign never leaves the guest's tap.
    assert!(in_ns(
        &g1.ns,
        &["ip", "addr", "add", "10.50.0.99/32", "dev", "eth0"]
    ));
    check(
        "a spoofed source is dropped",
        !in_ns(
            &g1.ns,
            &[
                "ping",
                "-c",
                "2",
                "-i",
                "0.2",
                "-W",
                "1",
                "-I",
                "10.50.0.99",
                g4.ip,
            ],
        ),
    );
    assert!(in_ns(
        &g1.ns,
        &["ip", "addr", "del", "10.50.0.99/32", "dev", "eth0"]
    ));

    // SNAT: a TCP connection to the internet arrives from host A's uplink address.
    let log = work.path().join("http.log");
    let server = ns_cmd(
        &inet,
        &[
            "python3",
            "-u",
            "-m",
            "http.server",
            "--bind",
            "198.51.100.254",
            "8080",
        ],
    )
    .stdout(Stdio::null())
    .stderr(std::fs::File::create(&log).unwrap())
    .spawn()
    .unwrap();
    c.server = Some(server);
    let listening = (0..100).any(|_| {
        let up = in_ns(&inet, &["sh", "-c", "ss -Hltn 'sport = :8080' | grep -q ."]);
        if !up {
            std::thread::sleep(Duration::from_millis(100));
        }
        up
    });
    assert!(
        listening,
        "http.server in the internet namespace never listened"
    );
    check("SNAT ping g4 -> internet", ping(&g4.ns, "198.51.100.254"));
    let fetched = in_ns(
        &g4.ns,
        &[
            "python3",
            "-c",
            "import urllib.request;urllib.request.urlopen('http://198.51.100.254:8080/',timeout=3).read()",
        ],
    );
    let seen = std::fs::read_to_string(&log).unwrap_or_default();
    check("SNAT TCP g4 -> internet", fetched);
    check(
        &format!("the internet sees host A's address, not the guest's: {seen:?}"),
        seen.contains("198.51.100.1 ") && !seen.contains("10.50.0.13"),
    );

    // Floating IP: the internet reaches g1 at 203.0.113.10, and g1's traffic leaves as it.
    check(
        "floating IP DNAT internet -> 203.0.113.10 (g1)",
        ping(&inet, "203.0.113.10"),
    );
    let fetched = in_ns(
        &g1.ns,
        &[
            "python3",
            "-c",
            "import urllib.request;urllib.request.urlopen('http://198.51.100.254:8080/',timeout=3).read()",
        ],
    );
    let seen = std::fs::read_to_string(&log).unwrap_or_default();
    check(
        &format!("g1 leaves as its floating IP: {seen:?}"),
        fetched && seen.contains("203.0.113.10 "),
    );

    {
        let _s = a.scope();
        assert!(
            vnet::remove_config(&a.cfg, 100).is_err(),
            "a network with attached VMs is not removed"
        );
        vnet::detach_vm(&a.cfg.state_dir, g4.id);
        let st = vnet::status(&a.cfg, 100).unwrap();
        assert_eq!(st.local_endpoints, vec!["10.50.0.11"]);
    }
    // g4's address now points at its last known host, which is A itself in the
    // config: a remote entry for our own underlay address. It must not loop back.
    check("a detached VM is unreachable", !ping(&g1.ns, g4.ip));
    {
        let _s = a.scope();
        vnet::detach_vm(&a.cfg.state_dir, g1.id);
        let uplink = a
            .cfg
            .sandbox
            .dataplane
            .pin_root
            .join("vnet/links/uplink-wA");
        assert!(uplink.exists(), "the uplink program is attached");
        assert!(vnet::remove_config(&a.cfg, 200).unwrap());
        assert!(vnet::remove_config(&a.cfg, 100).unwrap());
        assert!(
            !uplink.exists(),
            "the uplink program is detached with its last network"
        );
        assert!(vnet::status(&a.cfg, 100).is_none());
        assert!(!vnet::remove_config(&a.cfg, 100).unwrap());
    }
}
