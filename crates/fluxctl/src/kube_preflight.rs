// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl kube addons preflight`: will this machine run k3s? Resources, clock, listening ports, cgroup version and
//! controllers, and the limits Apple's Virtualization framework puts on the VM. It reads facts from the guest through a
//! [`GuestOps`], turns them into [`Facts`], and [`evaluate`] (pure) turns facts into verdicts. It is separate from
//! `kube doctor`, which looks at a cluster that is already running.

use crate::kube_addons::{GuestOps, SshGuest};
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sev {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub sev: Sev,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Agent,
}

/// Ports k3s needs free on a server: API, kubelet, and for embedded etcd the client and peer ports.
pub fn required_ports(role: Role, etcd: bool) -> Vec<u16> {
    match (role, etcd) {
        (Role::Agent, _) => vec![10250],
        (Role::Server, false) => vec![6443, 10250],
        (Role::Server, true) => vec![6443, 10250, 2379, 2380],
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub mem_mib: Option<u64>,
    pub cpus: Option<u32>,
    pub disk_free_gib: Option<u64>,
    pub time_synced: Option<bool>,
    pub listening: Option<Vec<u16>>,
    /// `Some("cgroup2fs")` for v2, `Some("tmpfs")` for v1
    pub cgroup_fs: Option<String>,
    pub cgroup_controllers: Option<Vec<String>>,
    /// The Mac, for the VZ limits: logical CPUs and memory in MiB.
    pub host_cpus: Option<u32>,
    pub host_mem_mib: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub role: Role,
    pub etcd: bool,
    /// What the VM was (or will be) configured with, for the VZ checks.
    pub vm_cpus: Option<u32>,
    pub vm_mem_mib: Option<u64>,
}

// ---------------------------------------------------------------- parsers

/// `MemTotal:  4025344 kB` -> MiB.
pub fn parse_meminfo(s: &str) -> Option<u64> {
    let l = s.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = l.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

/// The Available column of `df -Pk PATH` (second line), in GiB.
pub fn parse_df_free_gib(s: &str) -> Option<u64> {
    let l = s.lines().filter(|l| !l.trim().is_empty()).nth(1)?;
    let kb: u64 = l.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb / 1024 / 1024)
}

/// Local ports from `ss -ltnH` (`LISTEN 0 4096 0.0.0.0:6443 0.0.0.0:*`, IPv6 as `[::]:6443`).
pub fn parse_listening(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s
        .lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .filter_map(|a| a.rsplit(':').next()?.parse().ok())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// `timedatectl show -p NTPSynchronized --value` prints `yes` or `no`.
pub fn parse_ntp(s: &str) -> Option<bool> {
    match s.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

pub fn parse_controllers(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

// ---------------------------------------------------------------- verdicts

fn c(name: &'static str, sev: Sev, detail: impl Into<String>) -> Check {
    Check {
        name,
        sev,
        detail: detail.into(),
    }
}

/// Minimums follow the k3s requirements (server 2 CPU / 2 GiB, agent 1 CPU / 512 MiB); the "recommended" level is
/// what an embedded-etcd server with some workloads wants. Memory is compared with 7% slack because the kernel reports
/// less than the configured size.
pub fn evaluate(p: &Plan, f: &Facts) -> Vec<Check> {
    let mut out = vec![];
    let (min_cpu, min_mem, rec_mem, rec_disk) = match p.role {
        Role::Server => (2u32, 2048u64, 4096u64, 20u64),
        Role::Agent => (1, 512, 1024, 10),
    };
    out.push(match f.cpus {
        None => c("cpu", Sev::Warn, "could not read the CPU count"),
        Some(n) if n < min_cpu => c(
            "cpu",
            Sev::Fail,
            format!("{n} CPU(s); k3s needs at least {min_cpu}"),
        ),
        Some(n) => c("cpu", Sev::Ok, format!("{n} CPU(s)")),
    });
    out.push(match f.mem_mib {
        None => c("memory", Sev::Warn, "could not read memory"),
        Some(m) if m * 100 < min_mem * 93 => c(
            "memory",
            Sev::Fail,
            format!("{m} MiB; k3s needs at least {min_mem} MiB"),
        ),
        Some(m) if m * 100 < rec_mem * 93 => c(
            "memory",
            Sev::Warn,
            format!(
                "{m} MiB runs k3s but leaves little for workloads; {rec_mem} MiB is comfortable"
            ),
        ),
        Some(m) => c("memory", Sev::Ok, format!("{m} MiB")),
    });
    out.push(match f.disk_free_gib {
        None => c("disk", Sev::Warn, "could not read free disk space"),
        Some(g) if g < 5 => c(
            "disk",
            Sev::Fail,
            format!("{g} GiB free; images and etcd need more than 5 GiB"),
        ),
        Some(g) if g < rec_disk => c(
            "disk",
            Sev::Warn,
            format!("{g} GiB free; {rec_disk} GiB is comfortable"),
        ),
        Some(g) => c("disk", Sev::Ok, format!("{g} GiB free")),
    });
    out.push(match f.time_synced {
        Some(true) => c("time sync", Sev::Ok, "the clock is synchronised"),
        Some(false) => c("time sync", Sev::Warn, "the clock is not synchronised; certificates and etcd leases are time-sensitive (a VM wakes from sleep with a stale clock)"),
        None => c("time sync", Sev::Warn, "could not tell whether the clock is synchronised (timedatectl missing?)"),
    });
    out.push(match &f.listening {
        None => c(
            "ports",
            Sev::Warn,
            "could not list listening ports (ss missing?)",
        ),
        Some(l) => {
            let busy: Vec<String> = required_ports(p.role, p.etcd)
                .into_iter()
                .filter(|port| l.contains(port))
                .map(|x| x.to_string())
                .collect();
            if busy.is_empty() {
                c("ports", Sev::Ok, "the ports k3s needs are free")
            } else {
                c(
                    "ports",
                    Sev::Fail,
                    format!(
                        "already in use: {} (a running k3s, or another service)",
                        busy.join(", ")
                    ),
                )
            }
        }
    });
    out.push(match f.cgroup_fs.as_deref() {
        Some("cgroup2fs") => c("cgroup", Sev::Ok, "cgroup v2"),
        Some(_) => c("cgroup", Sev::Warn, "cgroup v1; k3s works, but current kernels and Debian use v2 (boot with systemd.unified_cgroup_hierarchy=1)"),
        None => c("cgroup", Sev::Warn, "could not read the cgroup version"),
    });
    if let Some(ctl) = &f.cgroup_controllers {
        let missing: Vec<&str> = ["cpu", "memory", "pids"]
            .into_iter()
            .filter(|n| !ctl.iter().any(|c| c == n))
            .collect();
        out.push(if missing.is_empty() {
            c(
                "cgroup controllers",
                Sev::Ok,
                "cpu, memory and pids are available",
            )
        } else {
            c(
                "cgroup controllers",
                Sev::Fail,
                format!(
                    "missing {}; pod limits cannot be enforced",
                    missing.join(", ")
                ),
            )
        });
    }
    out.extend(vz_checks(p, f));
    out
}

/// Apple's Virtualization framework: a VM cannot have more vCPUs than the Mac has logical CPUs or more memory than
/// the Mac has, and a VM that takes nearly all the Mac's memory starves macOS.
pub fn vz_checks(p: &Plan, f: &Facts) -> Vec<Check> {
    let mut out = vec![];
    if let (Some(v), Some(h)) = (p.vm_cpus, f.host_cpus) {
        out.push(if v > h {
            c(
                "vz cpus",
                Sev::Fail,
                format!("the VM asks for {v} vCPUs; this Mac has {h}"),
            )
        } else if v == h {
            c(
                "vz cpus",
                Sev::Warn,
                format!("the VM takes all {h} logical CPUs; the Mac will feel it"),
            )
        } else {
            c("vz cpus", Sev::Ok, format!("{v} of {h} logical CPUs"))
        });
    }
    if let (Some(v), Some(h)) = (p.vm_mem_mib, f.host_mem_mib) {
        out.push(if v >= h {
            c(
                "vz memory",
                Sev::Fail,
                format!("the VM asks for {v} MiB; this Mac has {h} MiB"),
            )
        } else if h - v < 4096 {
            c(
                "vz memory",
                Sev::Warn,
                format!("{v} of {h} MiB leaves macOS under 4 GiB"),
            )
        } else {
            c("vz memory", Sev::Ok, format!("{v} of {h} MiB"))
        });
    }
    out
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.sev == Sev::Fail)
}

pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for k in checks {
        let tag = match k.sev {
            Sev::Ok => "ok  ",
            Sev::Warn => "warn",
            Sev::Fail => "FAIL",
        };
        s.push_str(&format!("[{tag}] {}: {}\n", k.name, k.detail));
    }
    s
}

// ---------------------------------------------------------------- gathering

/// Asks the guest. A command that fails leaves its fact unknown (a warning), not an error.
pub fn gather(g: &dyn GuestOps) -> Facts {
    let try_run = |cmd: &str| g.run(cmd).ok();
    Facts {
        mem_mib: try_run("cat /proc/meminfo").and_then(|s| parse_meminfo(&s)),
        cpus: try_run("nproc").and_then(|s| s.trim().parse().ok()),
        disk_free_gib: try_run("df -Pk /var/lib 2>/dev/null || df -Pk /")
            .and_then(|s| parse_df_free_gib(&s)),
        time_synced: try_run("timedatectl show -p NTPSynchronized --value")
            .and_then(|s| parse_ntp(&s)),
        listening: try_run("ss -ltnH").map(|s| parse_listening(&s)),
        cgroup_fs: try_run("stat -fc %T /sys/fs/cgroup").map(|s| s.trim().to_string()),
        cgroup_controllers: try_run("cat /sys/fs/cgroup/cgroup.controllers")
            .map(|s| parse_controllers(&s)),
        host_cpus: None,
        host_mem_mib: None,
    }
}

/// The Mac's logical CPU count and memory, from `sysctl` (macOS only).
fn host_facts() -> (Option<u32>, Option<u64>) {
    let sysctl = |k: &str| {
        let o = std::process::Command::new("sysctl")
            .args(["-n", k])
            .output()
            .ok()?;
        String::from_utf8(o.stdout).ok()?.trim().parse::<u64>().ok()
    };
    if !cfg!(target_os = "macos") {
        return (None, None);
    }
    (
        sysctl("hw.logicalcpu").map(|v| v as u32),
        sysctl("hw.memsize").map(|b| b / 1024 / 1024),
    )
}

#[derive(clap::Args, Debug)]
pub struct PreflightArgs {
    /// The machine to check, over ssh ([user@]host).
    #[arg(long)]
    pub ssh: String,
    /// Check for a worker instead of a server.
    #[arg(long)]
    pub agent: bool,
    /// The server will run embedded etcd (checks ports 2379 and 2380 too).
    #[arg(long)]
    pub etcd: bool,
    /// The vCPUs the VM is configured with, for the VZ limit check.
    #[arg(long)]
    pub vm_cpus: Option<u32>,
    /// The VM's memory in MiB, for the VZ limit check.
    #[arg(long)]
    pub vm_memory_mib: Option<u64>,
}

/// Prints the report; returns false when something failed.
pub fn run(a: &PreflightArgs) -> Result<bool> {
    let g = SshGuest::new(a.ssh.clone());
    let mut facts = gather(&g);
    (facts.host_cpus, facts.host_mem_mib) = host_facts();
    let plan = Plan {
        role: if a.agent { Role::Agent } else { Role::Server },
        etcd: a.etcd,
        vm_cpus: a.vm_cpus,
        vm_mem_mib: a.vm_memory_mib,
    };
    let checks = evaluate(&plan, &facts);
    print!("{}", render(&checks));
    Ok(!failed(&checks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_addons::FakeGuest;

    fn good() -> Facts {
        Facts {
            mem_mib: Some(3900),
            cpus: Some(4),
            disk_free_gib: Some(40),
            time_synced: Some(true),
            listening: Some(vec![22, 53]),
            cgroup_fs: Some("cgroup2fs".into()),
            cgroup_controllers: Some(parse_controllers("cpuset cpu io memory hugetlb pids")),
            host_cpus: Some(10),
            host_mem_mib: Some(16384),
        }
    }

    fn plan() -> Plan {
        Plan {
            role: Role::Server,
            etcd: false,
            vm_cpus: Some(4),
            vm_mem_mib: Some(4096),
        }
    }

    fn sev(checks: &[Check], name: &str) -> Sev {
        checks.iter().find(|c| c.name == name).unwrap().sev
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_meminfo("MemFree: 1 kB\nMemTotal:       4025344 kB\n"),
            Some(3931)
        );
        assert_eq!(parse_meminfo("nothing"), None);
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 31000000 5000000 26214400 17% /\n";
        assert_eq!(parse_df_free_gib(df), Some(25));
        let ss = "LISTEN 0 4096 0.0.0.0:6443 0.0.0.0:*\nLISTEN 0 128 [::]:22 [::]:*\nLISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:*\nLISTEN 0 4096 0.0.0.0:6443 0.0.0.0:*\n";
        assert_eq!(parse_listening(ss), vec![22, 53, 6443]);
        assert_eq!(parse_ntp("yes\n"), Some(true));
        assert_eq!(parse_ntp("no"), Some(false));
        assert_eq!(parse_ntp(""), None);
    }

    #[test]
    fn a_healthy_server_passes_without_warnings() {
        let r = evaluate(&plan(), &good());
        assert!(r.iter().all(|c| c.sev == Sev::Ok), "{}", render(&r));
        assert!(!failed(&r));
    }

    #[test]
    fn small_machines_fail_or_warn() {
        let f = Facts {
            cpus: Some(1),
            mem_mib: Some(1500),
            disk_free_gib: Some(3),
            ..good()
        };
        let r = evaluate(&plan(), &f);
        assert_eq!(sev(&r, "cpu"), Sev::Fail);
        assert_eq!(sev(&r, "memory"), Sev::Fail);
        assert_eq!(sev(&r, "disk"), Sev::Fail);
        let f = Facts {
            mem_mib: Some(2100),
            disk_free_gib: Some(12),
            ..good()
        };
        let r = evaluate(&plan(), &f);
        assert_eq!(sev(&r, "memory"), Sev::Warn);
        assert_eq!(sev(&r, "disk"), Sev::Warn);
        // an agent is fine on what a server would warn about
        let agent = Plan {
            role: Role::Agent,
            ..plan()
        };
        assert_eq!(
            sev(
                &evaluate(
                    &agent,
                    &Facts {
                        cpus: Some(1),
                        mem_mib: Some(1100),
                        ..good()
                    }
                ),
                "memory"
            ),
            Sev::Ok
        );
    }

    #[test]
    fn busy_ports_depend_on_role_and_etcd() {
        let f = Facts {
            listening: Some(vec![2379, 6443]),
            ..good()
        };
        let r = evaluate(&plan(), &f);
        let k = r.iter().find(|c| c.name == "ports").unwrap();
        assert_eq!(k.sev, Sev::Fail);
        assert!(k.detail.contains("6443") && !k.detail.contains("2379"));
        let ha = Plan {
            etcd: true,
            ..plan()
        };
        assert!(
            evaluate(&ha, &f)
                .iter()
                .find(|c| c.name == "ports")
                .unwrap()
                .detail
                .contains("2379")
        );
        let agent = Plan {
            role: Role::Agent,
            ..plan()
        };
        assert_eq!(sev(&evaluate(&agent, &f), "ports"), Sev::Ok);
        assert_eq!(
            required_ports(Role::Server, true),
            vec![6443, 10250, 2379, 2380]
        );
    }

    #[test]
    fn time_and_cgroup() {
        let f = Facts {
            time_synced: Some(false),
            cgroup_fs: Some("tmpfs".into()),
            cgroup_controllers: Some(vec!["cpu".into()]),
            ..good()
        };
        let r = evaluate(&plan(), &f);
        assert_eq!(sev(&r, "time sync"), Sev::Warn);
        assert_eq!(sev(&r, "cgroup"), Sev::Warn);
        let k = r.iter().find(|c| c.name == "cgroup controllers").unwrap();
        assert_eq!(k.sev, Sev::Fail);
        assert!(k.detail.contains("memory") && k.detail.contains("pids"));
    }

    #[test]
    fn unknown_facts_warn_rather_than_fail() {
        let r = evaluate(&plan(), &Facts::default());
        assert!(!failed(&r));
        assert!(r.iter().all(|c| c.sev == Sev::Warn));
    }

    #[test]
    fn vz_limits() {
        let f = good();
        let over = Plan {
            vm_cpus: Some(12),
            vm_mem_mib: Some(20000),
            ..plan()
        };
        let r = vz_checks(&over, &f);
        assert!(r.iter().all(|c| c.sev == Sev::Fail));
        let all = Plan {
            vm_cpus: Some(10),
            vm_mem_mib: Some(14000),
            ..plan()
        };
        let r = vz_checks(&all, &f);
        assert!(r.iter().all(|c| c.sev == Sev::Warn));
        assert!(vz_checks(&plan(), &Facts::default()).is_empty());
    }

    #[test]
    fn render_golden() {
        let r = vec![
            c("cpu", Sev::Ok, "4 CPU(s)"),
            c("disk", Sev::Fail, "1 GiB free"),
        ];
        assert_eq!(
            render(&r),
            "[ok  ] cpu: 4 CPU(s)\n[FAIL] disk: 1 GiB free\n"
        );
    }

    #[test]
    fn gather_reads_through_the_guest() {
        let g = FakeGuest {
            answers: vec![
                ("meminfo".into(), "MemTotal: 4096000 kB\n".into()),
                ("nproc".into(), "4\n".into()),
                ("df -Pk".into(), "h\n/dev/x 1 1 41943040 1% /\n".into()),
                ("timedatectl".into(), "yes\n".into()),
                (
                    "ss -ltnH".into(),
                    "LISTEN 0 1 0.0.0.0:22 0.0.0.0:*\n".into(),
                ),
                ("stat -fc".into(), "cgroup2fs\n".into()),
                ("cgroup.controllers".into(), "cpu memory pids\n".into()),
            ],
            ..Default::default()
        };
        let f = gather(&g);
        assert_eq!(f.mem_mib, Some(4000));
        assert_eq!(f.cpus, Some(4));
        assert_eq!(f.disk_free_gib, Some(40));
        assert_eq!(f.time_synced, Some(true));
        assert_eq!(f.listening, Some(vec![22]));
        assert_eq!(f.cgroup_fs.as_deref(), Some("cgroup2fs"));
        assert_eq!(g.calls().len(), 7);
    }
}
