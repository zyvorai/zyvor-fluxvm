//! `fluxctl kube doctor`: the things that go wrong for a Kubernetes cluster on a Mac and are not Kubernetes' fault (a VPN
//! that swallowed the VM's DNS, a full disk, a skewed clock, a crash-looping pod), each with the command that fixes it.
//! The parsers and verdicts are pure; `run` only gathers the facts.

use crate::{
    VmApi, kube_checkpoint,
    kube_host::{self, KubeOptions},
};
use anyhow::Result;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    /// What to run or do about it; empty when fine.
    pub fix: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Ok,
            detail: detail.into(),
            fix: String::new(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Warn,
            detail: detail.into(),
            fix: fix.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Fail,
            detail: detail.into(),
            fix: fix.into(),
        }
    }
}

const GIB: u64 = 1 << 30;

/// Free space on the Mac's disk, from `df -k <path>` output.
pub fn parse_df_free(out: &str) -> Option<u64> {
    let line = out.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    // Filesystem 1024-blocks Used Available Capacity ...
    cols.get(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

pub fn mac_disk(free: Option<u64>) -> Check {
    match free {
        None => Check::warn("Mac disk", "could not read free space", "df -h ~"),
        Some(f) if f < 2 * GIB => Check::fail(
            "Mac disk",
            format!(
                "{:.1} GiB free: the VM disk cannot grow and checkpoints will fail",
                f as f64 / GIB as f64
            ),
            "free space, or delete old checkpoints: fluxctl kube checkpoints, fluxctl kube checkpoint-rm LABEL",
        ),
        Some(f) if f < 10 * GIB => Check::warn(
            "Mac disk",
            format!("{:.1} GiB free", f as f64 / GIB as f64),
            "checkpoints cost disk as the cluster changes; lower --keep-auto or remove old ones",
        ),
        Some(f) => Check::ok("Mac disk", format!("{:.0} GiB free", f as f64 / GIB as f64)),
    }
}

/// Use% of the guest's root filesystem, from `df --output=pcent /` output.
pub fn parse_pcent(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.trim().strip_suffix('%')?.trim().parse().ok())
}

pub fn guest_disk(pcent: Option<u32>) -> Check {
    match pcent {
        None => Check::warn(
            "VM disk",
            "could not read usage",
            "fluxctl kube shell, then df -h /",
        ),
        Some(p) if p >= 95 => Check::fail(
            "VM disk",
            format!("{p}% full: pods will be evicted"),
            "fluxctl kube shell, then: sudo k3s crictl rmi --prune",
        ),
        Some(p) if p >= 85 => Check::warn(
            "VM disk",
            format!("{p}% full"),
            "sudo k3s crictl rmi --prune (in fluxctl kube shell) removes unused images",
        ),
        Some(p) => Check::ok("VM disk", format!("{p}% used")),
    }
}

/// Whether the Mac's default route goes through a tunnel interface (`netstat -rn -f inet` output): the sign of a VPN that
/// takes all traffic.
pub fn vpn_default_route(netstat: &str) -> Option<String> {
    netstat.lines().find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        let is_default = cols.first() == Some(&"default");
        let iface = cols
            .iter()
            .find(|c| c.starts_with("utun") || c.starts_with("ipsec"))?;
        is_default.then(|| (*iface).to_string())
    })
}

pub fn vpn(route: Option<String>) -> Check {
    match route {
        Some(i) => Check::warn(
            "VPN",
            format!(
                "all Mac traffic goes through {i}; the VM's DNS and image pulls may fail while it is on"
            ),
            "if image pulls fail: fluxctl kube shell, then check `getent hosts registry-1.docker.io`; or fluxctl kube load IMAGE",
        ),
        None => Check::ok("VPN", "no full-tunnel VPN"),
    }
}

pub fn dns(resolved: bool, reached_registry: Option<String>) -> Check {
    if !resolved {
        return Check::fail(
            "VM DNS",
            "the VM cannot resolve names (a VPN or a changed network does this)",
            "fluxctl kube shell, then: sudo systemctl restart systemd-resolved; or wake the cluster after changing networks: fluxctl kube sleep && fluxctl kube wake",
        );
    }
    match reached_registry.as_deref() {
        Some("200") | Some("401") => Check::ok("VM internet", "reaches the image registry"),
        Some(code) => Check::warn(
            "VM internet",
            format!("names resolve but the registry answered {code}"),
            "a proxy or captive portal may be in the way; use fluxctl kube load IMAGE for local images",
        ),
        None => Check::warn(
            "VM internet",
            "names resolve but the registry is unreachable",
            "check the network or VPN; use fluxctl kube load IMAGE for local images",
        ),
    }
}

/// Seconds the guest's clock is ahead of (positive) or behind the Mac.
pub fn clock(skew: Option<i64>) -> Check {
    match skew {
        None => Check::warn(
            "VM clock",
            "could not read it",
            "fluxctl kube shell, then: date",
        ),
        Some(s) if s.abs() > 60 => Check::fail(
            "VM clock",
            format!("{s} s off the Mac: certificates and tokens will fail"),
            "fluxctl kube sleep && fluxctl kube wake (the clock is re-synced on every wake and restore)",
        ),
        Some(s) if s.abs() > 5 => Check::warn(
            "VM clock",
            format!("{s} s off the Mac"),
            "it self-corrects within a minute",
        ),
        Some(s) => Check::ok("VM clock", format!("{s} s off the Mac")),
    }
}

/// One pod from `kubectl get pods -A --no-headers -o wide` style output: `NS NAME READY STATUS RESTARTS ...`.
#[derive(Debug, PartialEq, Eq)]
pub struct PodRow {
    pub namespace: String,
    pub name: String,
    pub status: String,
    pub restarts: u32,
}

pub fn parse_pods(out: &str) -> Vec<PodRow> {
    out.lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() < 5 {
                return None;
            }
            // RESTARTS may read "3 (2m ago)"; the number is what matters.
            let restarts = c[4].parse().ok()?;
            Some(PodRow {
                namespace: c[0].to_owned(),
                name: c[1].to_owned(),
                status: c[3].to_owned(),
                restarts,
            })
        })
        .collect()
}

const BAD: [&str; 6] = [
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "OOMKilled",
    "CreateContainerConfigError",
    "Error",
];

pub fn pods(rows: &[PodRow]) -> Vec<Check> {
    let bad: Vec<&PodRow> = rows
        .iter()
        .filter(|p| BAD.contains(&p.status.as_str()))
        .collect();
    if bad.is_empty() {
        let n = rows.len();
        return vec![Check::ok("Pods", format!("{n} pod(s), none failing"))];
    }
    bad.iter()
        .map(|p| {
            let fix = match p.status.as_str() {
                "ImagePullBackOff" | "ErrImagePull" => format!(
                    "image cannot be pulled; build it on the Mac and `fluxctl kube load IMAGE`, or check the VM's internet above ({}/{})",
                    p.namespace, p.name
                ),
                "OOMKilled" => format!(
                    "raise the memory limit, or give the cluster more: fluxctl kube up --memory-mib 8192 ({}/{})",
                    p.namespace, p.name
                ),
                _ => format!("kubectl -n {} logs {} --previous", p.namespace, p.name),
            };
            Check::fail(
                "Pod",
                format!("{}/{} is {} (restarts {})", p.namespace, p.name, p.status, p.restarts),
                fix,
            )
        })
        .collect()
}

/// A short report; the exit code the CLI uses is nonzero when any check failed.
pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let mark = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s.push_str(&format!("{mark}  {:<12} {}\n", c.name, c.detail));
        if !c.fix.is_empty() {
            s.push_str(&format!("      fix: {}\n", c.fix));
        }
    }
    s
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

fn mac_sh(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Runs every check and returns the report. A sleeping or stopped cluster is reported as such and the in-VM checks skipped.
pub async fn run<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Vec<Check>> {
    let mut checks = vec![];
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    checks.push(mac_disk(parse_df_free(&mac_sh("df", &["-k", &home]))));
    checks.push(vpn(vpn_default_route(&mac_sh(
        "netstat",
        &["-rn", "-f", "inet"],
    ))));

    match api.find(&o.name).await? {
        None => {
            checks.push(Check::fail(
                "Cluster",
                format!("no cluster named {}", o.name),
                "fluxctl kube up",
            ));
            return Ok(checks);
        }
        Some((_, status)) if status != "running" => {
            if kube_checkpoint::is_asleep(api, o).await {
                checks.push(Check::ok(
                    "Cluster",
                    "asleep; the next kubectl call wakes it (fluxctl kube wake to do it now)",
                ));
            } else {
                checks.push(Check::fail(
                    "Cluster",
                    format!("VM is {status}"),
                    "fluxctl kube up",
                ));
            }
            return Ok(checks);
        }
        Some(_) => checks.push(Check::ok("Cluster", "VM running")),
    }

    if kube_host::api_answers(o.api_port) {
        checks.push(Check::ok(
            "API port",
            format!("127.0.0.1:{} answers", o.api_port),
        ));
    } else {
        checks.push(Check::fail(
            "API port",
            format!("nothing listens on 127.0.0.1:{}", o.api_port),
            "fluxctl kube up (or fluxctl kube supervise keeps it open)",
        ));
    }

    let guest = |script: &'static str| async move { kube_host::guest_sh(api, o, script).await };
    checks.push(guest_disk(
        guest("df --output=pcent /")
            .await
            .ok()
            .as_deref()
            .and_then(parse_pcent),
    ));
    let resolved = guest("getent hosts registry-1.docker.io").await.is_ok();
    let registry = if resolved {
        guest("curl -s -o /dev/null -m 6 -w '%{http_code}' https://registry-1.docker.io/v2/")
            .await
            .ok()
            .filter(|c| c != "000")
    } else {
        None
    };
    checks.push(dns(resolved, registry));
    let mac_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    checks.push(clock(
        guest("date +%s")
            .await
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .map(|g| g - mac_now),
    ));
    match guest("sudo k3s kubectl get pods -A --no-headers").await {
        Ok(out) => checks.extend(pods(&parse_pods(&out))),
        Err(e) => checks.push(Check::fail(
            "Kubernetes",
            format!("the API does not answer in the VM: {e}"),
            "fluxctl kube shell, then: sudo journalctl -u k3s -n 50",
        )),
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_free_is_read_in_bytes() {
        let out = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s5 971350180 800000000 8388608 99% 1 2 0% /System/Volumes/Data\n";
        assert_eq!(parse_df_free(out), Some(8388608 * 1024));
        assert_eq!(parse_df_free(""), None);
    }

    #[test]
    fn mac_disk_levels() {
        assert_eq!(mac_disk(Some(GIB)).level, Level::Fail);
        assert_eq!(mac_disk(Some(5 * GIB)).level, Level::Warn);
        assert_eq!(mac_disk(Some(50 * GIB)).level, Level::Ok);
        assert_eq!(mac_disk(None).level, Level::Warn);
    }

    #[test]
    fn guest_disk_levels() {
        assert_eq!(parse_pcent("Use%\n 42%\n"), Some(42));
        assert_eq!(parse_pcent("nonsense"), None);
        assert_eq!(guest_disk(Some(96)).level, Level::Fail);
        assert_eq!(guest_disk(Some(88)).level, Level::Warn);
        assert_eq!(guest_disk(Some(10)).level, Level::Ok);
    }

    #[test]
    fn a_tunnel_default_route_is_a_vpn() {
        let vpn_table = "Destination Gateway Flags Netif Expire\ndefault link#25 UCSg utun4\n10.0.0/24 link#6 UC en0\n";
        assert_eq!(vpn_default_route(vpn_table).as_deref(), Some("utun4"));
        let plain = "default 192.168.1.1 UGScg en0\n10.0.0/24 link#6 UC utun2\n";
        assert_eq!(vpn_default_route(plain), None);
        assert_eq!(vpn(None).level, Level::Ok);
        assert_eq!(vpn(Some("utun4".into())).level, Level::Warn);
    }

    #[test]
    fn dns_verdicts() {
        assert_eq!(dns(false, None).level, Level::Fail);
        assert_eq!(dns(true, Some("401".into())).level, Level::Ok);
        assert_eq!(dns(true, Some("407".into())).level, Level::Warn);
        assert_eq!(dns(true, None).level, Level::Warn);
    }

    #[test]
    fn clock_verdicts() {
        assert_eq!(clock(Some(0)).level, Level::Ok);
        assert_eq!(clock(Some(-20)).level, Level::Warn);
        assert_eq!(clock(Some(342)).level, Level::Fail);
        assert_eq!(clock(None).level, Level::Warn);
    }

    #[test]
    fn pods_are_read_and_bad_ones_flagged() {
        let out = "default web-1 1/1 Running 0 5m\ndefault api-2 0/1 CrashLoopBackOff 7 (30s ago) 9m\nkube-system dns 1/1 Running 1 (2m ago) 9m\ndefault big 0/1 OOMKilled 2 3m\ndefault pull 0/1 ImagePullBackOff 0 1m\n";
        let rows = parse_pods(out);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[1].restarts, 7);
        let checks = pods(&rows);
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.level == Level::Fail));
        assert!(checks[0].fix.contains("logs api-2 --previous"));
        assert!(checks[1].fix.contains("--memory-mib"));
        assert!(checks[2].fix.contains("kube load"));
        let healthy = pods(&parse_pods("default web-1 1/1 Running 0 5m\n"));
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].level, Level::Ok);
    }

    #[test]
    fn the_report_shows_fixes_and_exit_state() {
        let checks = vec![mac_disk(Some(50 * GIB)), dns(false, None)];
        let text = render(&checks);
        assert!(text.contains("ok    Mac disk"));
        assert!(text.contains("FAIL  VM DNS"));
        assert!(text.contains("fix: "));
        assert!(failed(&checks));
        assert!(!failed(&checks[..1]));
    }
}
