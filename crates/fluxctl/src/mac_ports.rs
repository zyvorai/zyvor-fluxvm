// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Checks on `-p [ADDR:]HOST:GUEST` before a VM is created, so a clash is reported in plain words with a port to try instead of as a
//! daemon failure after the VM exists. Three things are refused: a host port below 1024 (the daemon does not run as root), a host
//! port asked for twice, a host port an instance that is running already publishes, and a host port some other program on this Mac
//! is listening on. The listening check binds the port for an instant on this machine, so it only means something when the daemon
//! runs here too; against a remote daemon it is skipped (`FLUXVM_MAC_SKIP_PORT_PROBE=1` skips it always).

use crate::mac_errors::coded;
use crate::mac_inspect::status;
use anyhow::Result;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub addr: Option<String>,
    pub host: u16,
    pub guest: u16,
}

impl Claim {
    fn addr_text(&self) -> &str {
        self.addr.as_deref().unwrap_or("0.0.0.0")
    }
    fn overlaps(&self, other_addr: Option<&str>, other_port: u16) -> bool {
        let wild = |a: Option<&str>| matches!(a, None | Some("0.0.0.0") | Some("::") | Some(""));
        self.host == other_port
            && (wild(self.addr.as_deref())
                || wild(other_addr)
                || self.addr.as_deref() == other_addr)
    }
}

pub fn parse_claims(publish: &[String]) -> Result<Vec<Claim>> {
    publish
        .iter()
        .map(|p| {
            let v = crate::run::parse_port(p)?;
            Ok(Claim {
                addr: v["host_addr"].as_str().map(String::from),
                host: v["host_port"].as_u64().unwrap_or(0) as u16,
                guest: v["guest_port"].as_u64().unwrap_or(0) as u16,
            })
        })
        .collect()
}

/// What is wrong with one claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    Privileged { host: u16 },
    Twice { host: u16 },
    Instance { claim: Claim, name: String },
    Listening { claim: Claim, who: String },
}

/// `busy(addr, port)` says what listens there (`Some("node (pid 123)")`), or `None` when the port is free or cannot be told.
pub fn find_conflicts(
    claims: &[Claim],
    instances: &[&Value],
    busy: &dyn Fn(&str, u16) -> Option<String>,
) -> Vec<Conflict> {
    let mut out = Vec::new();
    for (i, c) in claims.iter().enumerate() {
        if c.host < 1024 {
            out.push(Conflict::Privileged { host: c.host });
            continue;
        }
        if claims[..i]
            .iter()
            .any(|p| c.overlaps(p.addr.as_deref(), p.host))
        {
            out.push(Conflict::Twice { host: c.host });
            continue;
        }
        let holder = instances
            .iter()
            .filter(|v| matches!(status(v), "running" | "paused" | "creating"))
            .find(|v| {
                v["request"]["network"]["forwards"]
                    .as_array()
                    .is_some_and(|a| {
                        a.iter().any(|f| {
                            f["host_port"]
                                .as_u64()
                                .is_some_and(|p| c.overlaps(f["host_addr"].as_str(), p as u16))
                        })
                    })
            });
        if let Some(v) = holder {
            out.push(Conflict::Instance {
                claim: c.clone(),
                name: v["name"].as_str().unwrap_or("?").to_string(),
            });
            continue;
        }
        if let Some(who) = busy(c.addr_text(), c.host) {
            out.push(Conflict::Listening {
                claim: c.clone(),
                who,
            });
        }
    }
    out
}

/// The nearest port above `from` that is free for `claim`, for the "try this" hint.
fn suggest(
    claim_host: u16,
    claims: &[Claim],
    instances: &[&Value],
    busy: &dyn Fn(&str, u16) -> Option<String>,
) -> Option<u16> {
    (claim_host.saturating_add(1)..claim_host.saturating_add(200).max(1024)).find(|p| {
        let probe = Claim {
            addr: None,
            host: *p,
            guest: 0,
        };
        find_conflicts(std::slice::from_ref(&probe), instances, busy).is_empty()
            && !claims.iter().any(|c| c.host == *p)
    })
}

pub fn explain(
    conflicts: &[Conflict],
    claims: &[Claim],
    instances: &[&Value],
    busy: &dyn Fn(&str, u16) -> Option<String>,
) -> Option<anyhow::Error> {
    let first = conflicts.first()?;
    let try_instead = |host: u16, guest: u16| match suggest(host, claims, instances, busy) {
        Some(p) => format!(" Pick another host port, for example `-p {p}:{guest}`."),
        None => " Pick another host port.".into(),
    };
    let mut lines = Vec::new();
    let mut code = "MAC_PORT_IN_USE";
    for c in conflicts {
        lines.push(match c {
            Conflict::Privileged { host } => {
                code = "MAC_PORT_PRIVILEGED";
                format!(
                    "host port {host} is below 1024, and the FluxVM daemon does not run as root, so it cannot listen there. Publish a high port instead, for example `-p 8{host:03}:{host}`, and put a proxy in front if you need the low one."
                )
            }
            Conflict::Twice { host } => {
                format!("host port {host} appears twice in -p; each host port can go to one guest port.")
            }
            Conflict::Instance { claim, name } => format!(
                "host port {} is already published by the running instance `{name}`. Stop it (`fluxctl mac stop {name}`) or{}",
                claim.host,
                try_instead(claim.host, claim.guest)
            ),
            Conflict::Listening { claim, who } => format!(
                "host port {} on {} is already in use by {who}. Quit that program or{}",
                claim.host,
                claim.addr_text(),
                try_instead(claim.host, claim.guest)
            ),
        });
    }
    let _ = first;
    Some(coded(
        format!("cannot publish: {}", lines.join("\n  ")),
        code,
    ))
}

/// Does something listen on `addr:port` on this Mac? Binds for an instant; on a clash asks `lsof` who it is (best effort).
pub fn probe_local(addr: &str, port: u16) -> Option<String> {
    if std::env::var("FLUXVM_MAC_SKIP_PORT_PROBE").is_ok_and(|v| v == "1") {
        return None;
    }
    let ip: std::net::IpAddr = addr.parse().ok()?;
    match std::net::TcpListener::bind((ip, port)) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Some(who_listens(port)),
        Err(_) => None,
    }
}

fn who_listens(port: u16) -> String {
    let out = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    parse_lsof(&out).unwrap_or_else(|| "another program".into())
}

/// `lsof -Fpc` output: `p123` then `cnode` lines.
pub fn parse_lsof(text: &str) -> Option<String> {
    let pid = text.lines().find_map(|l| l.strip_prefix('p'))?;
    let cmd = text.lines().find_map(|l| l.strip_prefix('c'))?;
    Some(format!("`{cmd}` (pid {pid})"))
}

/// Is the daemon's base URL this machine? Only then does binding a port here say anything about the daemon's Mac.
pub fn is_local(base: &str) -> bool {
    let host = base
        .split("://")
        .nth(1)
        .unwrap_or(base)
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Used by `create_instance`: look at the instances the daemon knows and refuse a clash before the VM exists.
pub async fn preflight(r: &crate::remote::Remote, publish: &[String]) -> Result<()> {
    if publish.is_empty() {
        return Ok(());
    }
    let claims = parse_claims(publish)?;
    // A daemon that cannot list instances will fail the create next with its own message.
    let items = crate::mac_more::instances(r).await.unwrap_or_default();
    let refs: Vec<&Value> = items.iter().collect();
    let local = is_local(r.base());
    let busy = move |a: &str, p: u16| if local { probe_local(a, p) } else { None };
    let conflicts = find_conflicts(&claims, &refs, &busy);
    match explain(&conflicts, &claims, &refs, &busy) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(p: &[&str]) -> Vec<Claim> {
        parse_claims(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn vm(name: &str, st: &str, port: u64) -> Value {
        json!({"name": name, "status": st,
            "request": {"network": {"forwards": [{"host_port": port, "guest_port": 80}]}}})
    }

    fn free(_: &str, _: u16) -> Option<String> {
        None
    }

    #[test]
    fn a_running_instance_holding_the_port_is_named() {
        let web = vm("web", "running", 8080);
        let c = claims(&["8080:80"]);
        let found = find_conflicts(&c, &[&web], &free);
        assert!(matches!(&found[0], Conflict::Instance { name, .. } if name == "web"));
        let msg = explain(&found, &c, &[&web], &free).unwrap().to_string();
        assert!(
            msg.contains("running instance `web`")
                && msg.contains("-p 8081:80")
                && msg.contains("mac stop web")
                && msg.contains("MAC_PORT_IN_USE"),
            "{msg}"
        );
    }

    #[test]
    fn a_stopped_instance_does_not_hold_a_port() {
        let web = vm("web", "stopped", 8080);
        assert!(find_conflicts(&claims(&["8080:80"]), &[&web], &free).is_empty());
    }

    #[test]
    fn loopback_and_wildcard_overlap_but_two_loopbacks_of_different_ports_do_not() {
        let web = json!({"name": "w", "status": "running", "request": {"network": {"forwards": [{"host_port": 9000, "guest_port": 1, "host_addr": "127.0.0.1"}]}}});
        assert_eq!(
            find_conflicts(&claims(&["9000:80"]), &[&web], &free).len(),
            1
        );
        assert_eq!(
            find_conflicts(&claims(&["127.0.0.1:9000:80"]), &[&web], &free).len(),
            1
        );
        assert!(find_conflicts(&claims(&["9001:80"]), &[&web], &free).is_empty());
    }

    #[test]
    fn a_port_asked_for_twice_and_a_low_port_are_refused() {
        let c = claims(&["9100:80", "9100:81"]);
        assert_eq!(
            find_conflicts(&c, &[], &free),
            [Conflict::Twice { host: 9100 }]
        );
        let c = claims(&["80:80"]);
        let f = find_conflicts(&c, &[], &free);
        let msg = explain(&f, &c, &[], &free).unwrap().to_string();
        assert!(
            msg.contains("below 1024")
                && msg.contains("-p 8080:80")
                && msg.contains("MAC_PORT_PRIVILEGED"),
            "{msg}"
        );
        // Different loopback addresses of one port do not clash with each other.
        assert!(
            find_conflicts(
                &claims(&["127.0.0.1:9100:80", "127.0.0.2:9100:80"]),
                &[],
                &free
            )
            .is_empty()
        );
    }

    #[test]
    fn a_listening_program_is_named_and_the_hint_skips_busy_ports() {
        let busy =
            |_: &str, p: u16| (p == 3000 || p == 3001).then(|| "`node` (pid 42)".to_string());
        let c = claims(&["3000:3000"]);
        let f = find_conflicts(&c, &[], &busy);
        let msg = explain(&f, &c, &[], &busy).unwrap().to_string();
        assert!(
            msg.contains("`node` (pid 42)") && msg.contains("-p 3002:3000"),
            "{msg}"
        );
    }

    #[test]
    fn only_a_loopback_daemon_is_probed() {
        assert!(is_local("http://127.0.0.1:7788"));
        assert!(is_local("http://localhost:7788/"));
        assert!(is_local("http://[::1]:7788"));
        assert!(!is_local("http://mac.local:7788"));
    }

    #[test]
    fn lsof_output_is_read() {
        assert_eq!(parse_lsof("p123\ncnode\n").unwrap(), "`node` (pid 123)");
        assert!(parse_lsof("").is_none());
    }

    #[test]
    fn the_local_probe_sees_a_port_this_test_holds() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(probe_local("127.0.0.1", port).is_some());
        drop(l);
        assert!(probe_local("127.0.0.1", port).is_none());
    }
}
