//! `fluxctl kube doctor`: the things that go wrong for a Kubernetes cluster on a Mac and are not Kubernetes' fault (a VPN
//! that swallowed the VM's DNS, a full disk, a skewed clock, a crash-looping pod), each with the command that fixes it.
//! The parsers and verdicts are pure; `run` only gathers the facts.

use crate::{
    kube_checkpoint,
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::Result;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// A repair `kube doctor --fix` can make by itself: small, reversible, and run inside the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// Delete container images no pod uses.
    PruneImages,
    /// Restart the VM's resolver after a network change.
    RestartResolver,
    /// Step the VM's clock to the Mac's.
    StepClock,
    /// Keep the VM disks out of Time Machine and Spotlight. Runs on the Mac, not in the VM.
    ExcludeFromBackup,
}

impl Remedy {
    pub fn label(self) -> &'static str {
        match self {
            Remedy::PruneImages => "removed images no pod uses",
            Remedy::RestartResolver => "restarted the VM's resolver",
            Remedy::StepClock => "stepped the VM's clock",
            Remedy::ExcludeFromBackup => "excluded the VM folder from Time Machine and Spotlight",
        }
    }

    /// Whether the repair runs on the Mac instead of in the VM.
    pub fn on_mac(self) -> bool {
        matches!(self, Remedy::ExcludeFromBackup)
    }

    /// The script run in the VM. Each one is safe to run twice.
    pub fn script(self, mac_now: i64) -> String {
        match self {
            Remedy::PruneImages => "sudo k3s crictl rmi --prune".into(),
            Remedy::RestartResolver => "sudo systemctl restart systemd-resolved".into(),
            Remedy::StepClock => format!(
                "sudo chronyc makestep >/dev/null 2>&1 || sudo date -u -s @{mac_now} >/dev/null"
            ),
            Remedy::ExcludeFromBackup => String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    /// What to run or do about it; empty when fine.
    pub fix: String,
    /// What `--fix` does about it, when it can.
    pub remedy: Option<Remedy>,
}

impl Check {
    fn fixable(mut self, r: Remedy) -> Self {
        self.remedy = Some(r);
        self
    }

    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Ok,
            detail: detail.into(),
            fix: String::new(),
            remedy: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Warn,
            detail: detail.into(),
            fix: fix.into(),
            remedy: None,
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Fail,
            detail: detail.into(),
            fix: fix.into(),
            remedy: None,
        }
    }
}

const GIB: u64 = 1 << 30;

/// Where the daemon keeps VM disks, checkpoints and images on a Mac.
pub fn vm_folder() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
        .join("Library/Application Support/FluxVM")
}

/// `Some(true)` when `tmutil isexcluded` says the path is excluded from Time Machine, `Some(false)` when included.
pub fn parse_isexcluded(out: &str) -> Option<bool> {
    if out.contains("[Excluded]") {
        Some(true)
    } else if out.contains("[Included]") {
        Some(false)
    } else {
        None
    }
}

/// Whether Time Machine has a backup destination, from `tmutil destinationinfo`.
pub fn has_destination(out: &str) -> bool {
    out.lines().any(|l| l.trim_start().starts_with("Name"))
}

/// Time Machine and Spotlight on the VM folder: gigabytes of disk images that change all day. `excluded` is Time Machine's
/// answer for the folder, `never_indexed` whether Spotlight has been told to skip it.
pub fn backup(destination: bool, excluded: Option<bool>, never_indexed: bool) -> Check {
    let mut open = vec![];
    if destination && excluded == Some(false) {
        open.push("Time Machine backs it up");
    }
    if !never_indexed {
        open.push("Spotlight indexes it");
    }
    if open.is_empty() {
        return Check::ok(
            "Backups",
            if destination {
                "the VM folder is out of Time Machine and Spotlight"
            } else {
                "the VM folder is out of Spotlight (no Time Machine destination)"
            },
        );
    }
    Check::warn(
        "Backups",
        format!("the VM folder holds large disk images and {}", open.join(" and ")),
        "fluxctl kube doctor --fix (or: tmutil addexclusion \"$HOME/Library/Application Support/FluxVM\")",
    )
    .fixable(Remedy::ExcludeFromBackup)
}

/// The Mac-side repair for [`Remedy::ExcludeFromBackup`].
pub fn exclude_from_backup(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    // A sticky exclusion: it follows the folder and needs no password.
    let out = Command::new("tmutil").arg("addexclusion").arg(dir).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "tmutil addexclusion failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::write(dir.join(".metadata_never_index"), b"")?;
    Ok(())
}

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
            "fluxctl ssh CLUSTER, then df -h /",
        ),
        Some(p) if p >= 95 => Check::fail(
            "VM disk",
            format!("{p}% full: pods will be evicted"),
            "fluxctl ssh CLUSTER, then: sudo k3s crictl rmi --prune",
        )
        .fixable(Remedy::PruneImages),
        Some(p) if p >= 85 => Check::warn(
            "VM disk",
            format!("{p}% full"),
            "sudo k3s crictl rmi --prune (in `fluxctl ssh CLUSTER`) removes unused images",
        )
        .fixable(Remedy::PruneImages),
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
            "if image pulls fail: fluxctl ssh CLUSTER, then check `getent hosts registry-1.docker.io`; or fluxctl kube load IMAGE",
        ),
        None => Check::ok("VPN", "no full-tunnel VPN"),
    }
}

pub fn dns(resolved: bool, reached_registry: Option<String>) -> Check {
    if !resolved {
        return Check::fail(
            "VM DNS",
            "the VM cannot resolve names (a VPN or a changed network does this)",
            "fluxctl ssh CLUSTER, then: sudo systemctl restart systemd-resolved; or wake the cluster after changing networks: fluxctl kube sleep && fluxctl kube wake",
        )
        .fixable(Remedy::RestartResolver);
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
            "fluxctl ssh CLUSTER, then: date",
        ),
        Some(s) if s.abs() > 60 => Check::fail(
            "VM clock",
            format!("{s} s off the Mac: certificates and tokens will fail"),
            "fluxctl kube sleep && fluxctl kube wake (the clock is re-synced on every wake and restore)",
        )
        .fixable(Remedy::StepClock),
        Some(s) if s.abs() > 5 => Check::warn(
            "VM clock",
            format!("{s} s off the Mac"),
            "it self-corrects within a minute",
        ),
        Some(s) => Check::ok("VM clock", format!("{s} s off the Mac")),
    }
}

/// Whether the cluster's kubeconfig exists and points at the forwarded API port. `text` is the file's content, if it could be read.
pub fn kubeconfig(text: Option<&str>, port: u16) -> Check {
    let Some(text) = text else {
        return Check::fail(
            "Kubeconfig",
            "the file is missing, so kubectl cannot reach this cluster",
            "fluxctl kube up (writes it again; the cluster is not recreated)",
        );
    };
    let want = format!("https://127.0.0.1:{port}");
    match text.lines().find_map(|l| l.trim().strip_prefix("server:")) {
        Some(s) if s.trim() == want => Check::ok("Kubeconfig", format!("points at {want}")),
        Some(s) => Check::fail(
            "Kubeconfig",
            format!("points at {} but the API is forwarded to {want}", s.trim()),
            "fluxctl kube up (writes it again; the cluster is not recreated)",
        ),
        None => Check::fail(
            "Kubeconfig",
            "the file has no server address",
            "fluxctl kube up (writes it again; the cluster is not recreated)",
        ),
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

/// Restarts after which a pod that looks fine right now is still worth a warning: a crash loop is "Running" for a moment
/// between attempts, so a single snapshot can miss it.
/// Statuses that are failures too, and that the exact `BAD` list would miss.
const ALSO_BAD: [&str; 7] = [
    "Evicted",
    "ContainerStatusUnknown",
    "Failed",
    "RunContainerError",
    "CreateContainerError",
    "InvalidImageName",
    "ImageInspectError",
];

/// Whether a pod status means it is failing. `Init:CrashLoopBackOff` and the like count, with their `Init:` prefix removed.
pub(crate) fn is_bad(status: &str) -> bool {
    let s = status.strip_prefix("Init:").unwrap_or(status);
    BAD.contains(&s) || ALSO_BAD.contains(&s)
}

const RESTARTS_WARN: u32 = 3;

pub fn pods(rows: &[PodRow]) -> Vec<Check> {
    let mut checks: Vec<Check> = rows
        .iter()
        .filter(|p| is_bad(&p.status))
        .map(|p| {
            let fix = match p.status.strip_prefix("Init:").unwrap_or(&p.status) {
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
                format!(
                    "{}/{} is {} (restarts {})",
                    p.namespace, p.name, p.status, p.restarts
                ),
                fix,
            )
        })
        .collect();
    checks.extend(
        rows.iter()
            .filter(|p| !is_bad(&p.status) && p.restarts >= RESTARTS_WARN)
            .map(|p| {
                Check::warn(
                    "Pod",
                    format!(
                        "{}/{} is {} but has restarted {} times",
                        p.namespace, p.name, p.status, p.restarts
                    ),
                    format!("kubectl -n {} logs {} --previous", p.namespace, p.name),
                )
            }),
    );
    if checks.is_empty() {
        checks.push(Check::ok(
            "Pods",
            format!("{} pod(s), none failing", rows.len()),
        ));
    }
    checks
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
            let auto = if c.remedy.is_some() {
                "  (`--fix` does this)"
            } else {
                ""
            };
            s.push_str(&format!("      fix: {}{auto}\n", c.fix));
        }
    }
    s
}

/// The repairs `--fix` would make: one per kind, for the checks that are not ok.
pub fn plan(checks: &[Check]) -> Vec<Remedy> {
    let mut out: Vec<Remedy> = vec![];
    for c in checks.iter().filter(|c| c.level != Level::Ok) {
        if let Some(r) = c.remedy {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Makes the repairs in the VM and says what each one did; a repair that fails is reported and the others still run.
pub async fn apply<A: VmApi>(api: &A, o: &KubeOptions, remedies: &[Remedy]) -> Vec<String> {
    let mac_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut lines = vec![];
    for r in remedies {
        let done = if r.on_mac() {
            exclude_from_backup(&vm_folder()).map(|_| String::new())
        } else {
            kube_host::guest_sh(api, o, &r.script(mac_now)).await
        };
        match done {
            Ok(_) => lines.push(format!("fixed: {}", r.label())),
            Err(e) => lines.push(format!("could not fix ({}): {e}", r.label())),
        }
    }
    lines
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
    let mut checks = run_checks(api, o).await?;
    for c in &mut checks {
        c.fix = c.fix.replace("CLUSTER", &o.name);
    }
    Ok(checks)
}

async fn run_checks<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Vec<Check>> {
    let mut checks = vec![];
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    checks.push(mac_disk(parse_df_free(&mac_sh("df", &["-k", &home]))));
    checks.push(vpn(vpn_default_route(&mac_sh(
        "netstat",
        &["-rn", "-f", "inet"],
    ))));
    let folder = vm_folder();
    if folder.exists() {
        let folder_arg = folder.display().to_string();
        checks.push(backup(
            has_destination(&mac_sh("tmutil", &["destinationinfo"])),
            parse_isexcluded(&mac_sh("tmutil", &["isexcluded", &folder_arg])),
            folder.join(".metadata_never_index").exists(),
        ));
    }

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

    checks.push(kubeconfig(
        kube_host::kubeconfig_path(o)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .as_deref(),
        o.api_port,
    ));

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
            "fluxctl ssh CLUSTER, then: sudo journalctl -u k3s -n 50",
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
        let between = pods(&parse_pods("default flap 1/1 Running 5 (20s ago) 4m\n"));
        assert_eq!(between.len(), 1);
        assert_eq!(between[0].level, Level::Warn);
        let healthy = pods(&parse_pods("default web-1 1/1 Running 0 5m\n"));
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].level, Level::Ok);
    }

    #[test]
    fn init_container_and_evicted_pods_are_failures_too() {
        for bad in [
            "Init:CrashLoopBackOff",
            "Init:Error",
            "Init:ImagePullBackOff",
            "Evicted",
            "ContainerStatusUnknown",
            "Failed",
            "Error",
        ] {
            assert!(is_bad(bad), "{bad}");
        }
        for fine in [
            "Running",
            "Pending",
            "Init:0/1",
            "Completed",
            "ContainerCreating",
            "Terminating",
        ] {
            assert!(!is_bad(fine), "{fine}");
        }
        let rows = parse_pods("default a 0/1 Init:ImagePullBackOff 0 1m\n");
        let checks = pods(&rows);
        assert_eq!(checks[0].level, Level::Fail);
        assert!(
            checks[0].fix.contains("kube load"),
            "the Init: prefix still picks the pull advice"
        );
    }

    #[test]
    fn only_problems_with_a_safe_repair_are_planned_once_each() {
        let checks = vec![
            guest_disk(Some(96)),
            guest_disk(Some(90)),
            dns(false, None),
            clock(Some(342)),
            clock(Some(1)),
            mac_disk(Some(GIB)),
            vpn(Some("utun4".into())),
        ];
        assert_eq!(
            plan(&checks),
            vec![
                Remedy::PruneImages,
                Remedy::RestartResolver,
                Remedy::StepClock
            ]
        );
        assert!(plan(&[guest_disk(Some(10)), clock(Some(0))]).is_empty());
        let pod = pods(&parse_pods("default a 0/1 CrashLoopBackOff 7 9m\n"));
        assert!(plan(&pod).is_empty(), "a crash loop is the app's to fix");
    }

    #[test]
    fn the_kubeconfig_must_exist_and_point_at_the_forward() {
        let ok = "clusters:\n- cluster:\n    server: https://127.0.0.1:6443\n  name: x\n";
        assert_eq!(kubeconfig(Some(ok), 6443).level, Level::Ok);
        let moved = kubeconfig(Some(ok), 6444);
        assert_eq!(moved.level, Level::Fail);
        assert!(moved.detail.contains("6444") && moved.detail.contains("6443"));
        assert_eq!(kubeconfig(None, 6443).level, Level::Fail);
        assert_eq!(kubeconfig(Some("clusters: []\n"), 6443).level, Level::Fail);
    }

    #[test]
    fn repair_scripts_touch_only_what_they_name() {
        assert_eq!(Remedy::PruneImages.script(0), "sudo k3s crictl rmi --prune");
        assert!(
            Remedy::RestartResolver
                .script(0)
                .contains("systemd-resolved")
        );
        let clock = Remedy::StepClock.script(1_700_000_000);
        assert!(clock.contains("chronyc makestep") && clock.contains("@1700000000"));
    }

    #[test]
    fn the_report_marks_what_fix_would_do() {
        let text = render(&[guest_disk(Some(96)), mac_disk(Some(GIB))]);
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("fix:")).collect();
        assert!(lines[0].contains("`--fix` does this"));
        assert!(!lines[1].contains("`--fix`"));
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

    #[test]
    fn time_machine_answers_are_read() {
        assert_eq!(parse_isexcluded("[Excluded]    /Users/me/Library/Application Support/FluxVM"), Some(true));
        assert_eq!(parse_isexcluded("[Included]    /Users/me/Library/Application Support/FluxVM"), Some(false));
        assert_eq!(parse_isexcluded(""), None);
        assert!(has_destination("====================================================\nName          : Backup Disk\nKind          : Local\n"));
        assert!(!has_destination("tmutil: No destinations configured.\n"));
        assert!(!has_destination(""));
    }

    #[test]
    fn the_vm_folder_is_flagged_only_for_what_is_actually_backed_up_or_indexed() {
        // Backed up and indexed: warn, with the repair.
        let c = backup(true, Some(false), false);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("Time Machine backs it up and Spotlight indexes it"), "{}", c.detail);
        assert_eq!(c.remedy, Some(Remedy::ExcludeFromBackup));
        // No Time Machine destination: only Spotlight can be wrong.
        let c = backup(false, Some(false), false);
        assert!(c.detail.contains("Spotlight indexes it") && !c.detail.contains("Time Machine"), "{}", c.detail);
        // Everything excluded: fine.
        assert_eq!(backup(true, Some(true), true).level, Level::Ok);
        assert_eq!(backup(false, None, true).level, Level::Ok);
        // An unknown Time Machine answer is not treated as a problem.
        assert_eq!(backup(true, None, true).level, Level::Ok);
    }

    #[test]
    fn the_backup_repair_runs_on_the_mac_and_is_planned_once() {
        assert!(Remedy::ExcludeFromBackup.on_mac());
        assert!(!Remedy::StepClock.on_mac());
        let checks = vec![backup(true, Some(false), false), backup(true, Some(false), true)];
        assert_eq!(plan(&checks), vec![Remedy::ExcludeFromBackup]);
        assert!(Remedy::ExcludeFromBackup.label().contains("Time Machine"));
    }
}
