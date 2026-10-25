//! `fluxctl kube chaos`: break a single-node cluster on purpose, check that the app copes, then put it back from a checkpoint.
//! A checkpoint covers the whole machine, so faults that pod-level chaos tools cannot undo (a stopped control plane, a full disk) are safe.

use anyhow::{Result, bail};

use crate::kube_checkpoint::{checkpoint, restore, run_command, server, tag_of};
use crate::kube_host::{self, KubeOptions, guest_sh};
use crate::run::VmApi;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Scenario {
    /// Delete a random pod outside kube-system.
    KillPod,
    /// Stop k3s: the API server, the scheduler and the kubelet go away while the VM stays up.
    StopK3s,
    /// Fill the server VM's disk with a 90% sized file.
    FillDisk,
}

const START: &str = "chaos-start";

impl Scenario {
    /// The shell script that causes the fault, run in the server VM.
    pub(crate) fn inject(self) -> &'static str {
        match self {
            Scenario::KillPod => {
                "sudo k3s kubectl get pods -A --no-headers -o custom-columns=NS:.metadata.namespace,N:.metadata.name | grep -v '^kube-system ' | shuf -n1 | { read ns n; [ -n \"$n\" ] || { echo 'no pod outside kube-system to kill'; exit 0; }; sudo k3s kubectl -n \"$ns\" delete pod \"$n\" --wait=false >/dev/null && echo \"deleted pod $ns/$n\"; }"
            }
            Scenario::StopK3s => "sudo systemctl stop k3s && echo 'k3s stopped'",
            Scenario::FillDisk => {
                "avail=$(df --output=avail -B1 /var | tail -1); sudo fallocate -l $((avail / 10 * 9)) /var/fluxvm-chaos.fill && echo 'disk 90% full'"
            }
        }
    }

    /// The shell script that undoes the fault where it can be undone without a restore.
    pub(crate) fn heal(self) -> &'static str {
        match self {
            Scenario::KillPod => "true",
            Scenario::StopK3s => "sudo systemctl start k3s",
            Scenario::FillDisk => "sudo rm -f /var/fluxvm-chaos.fill",
        }
    }
}

/// The one-line report.
pub(crate) fn report(fault: &str, hold: u64, check: Option<bool>, restored: bool) -> String {
    format!(
        "{fault}; held {hold}s; check: {}; {}",
        match check {
            None => "none given",
            Some(true) => "passed (the app coped)",
            Some(false) => "FAILED (the app did not cope)",
        },
        if restored {
            "cluster restored to before the fault"
        } else {
            "fault undone, cluster left as is"
        }
    )
}

pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    scenario: Scenario,
    hold: u64,
    check: Option<&str>,
    no_restore: bool,
) -> Result<(String, bool)> {
    let (id, _) = server(api, o).await?;
    if api.has_snapshot(id, &tag_of(START)).await? {
        api.delete_snapshot(id, &tag_of(START)).await?;
    }
    eprintln!("{}", checkpoint(api, o, START).await?);
    let fault = match guest_sh(api, o, scenario.inject()).await {
        Ok(s) => s,
        Err(e) => {
            // the fault may have half happened, so go back before reporting
            let _ = restore(api, o, START).await;
            let _ = api.delete_snapshot(id, &tag_of(START)).await;
            bail!("could not cause the fault: {e:#}");
        }
    };
    eprintln!("{fault}; waiting {hold}s");
    tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
    let passed = match check {
        Some(cmd) => Some(
            run_command(o, "sh", &["-c".to_owned(), cmd.to_owned()])
                .await?
                .success(),
        ),
        None => None,
    };
    if no_restore {
        let _ = guest_sh(api, o, scenario.heal()).await;
    } else {
        eprintln!("{}", restore(api, o, START).await?);
    }
    api.delete_snapshot(id, &tag_of(START)).await?;
    let _ = kube_host::up(api, o).await; // reopen the API port after a restore or a stopped k3s
    Ok((
        report(&fault, hold, passed, !no_restore),
        passed != Some(false),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_has_a_fault_and_a_way_back() {
        for s in [Scenario::KillPod, Scenario::StopK3s, Scenario::FillDisk] {
            assert!(!s.inject().is_empty() && !s.heal().is_empty());
        }
        assert!(Scenario::StopK3s.heal().contains("start k3s"));
        assert!(Scenario::FillDisk.heal().contains("rm -f"));
        // never touches kube-system pods
        assert!(
            Scenario::KillPod
                .inject()
                .contains("grep -v '^kube-system '")
        );
    }

    #[test]
    fn the_report_says_what_happened() {
        let r = report("k3s stopped", 20, Some(false), true);
        assert!(r.contains("FAILED") && r.contains("restored"), "{r}");
        let r = report("x", 5, None, false);
        assert!(r.contains("none given") && r.contains("left as is"), "{r}");
    }
}
