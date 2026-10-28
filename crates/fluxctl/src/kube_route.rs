//! `fluxctl kube route`: let this Mac reach pod and Service addresses directly.
//!
//! The cluster VM sits behind vmnet NAT (192.168.64.0/24) and routes its own pod network (10.42.0.0/16) and Service
//! network (10.43.0.0/16), but the Mac has no route to either, so a pod IP or a ClusterIP only answers inside the
//! cluster. Two static routes through the VM's address fix that: the Mac sends the packet to the VM, and the VM's
//! forwarding, flannel and kube-proxy do the rest. Changing routes needs root, so this prints the commands and runs
//! them only with `--install` (the same pattern as `kube trust`).

use anyhow::{Context, Result, bail};

use crate::kube_host::KubeOptions;
use crate::run::VmApi;

/// k3s's default pod and Service ranges. Custom ranges (`--cluster-cidr`, `--service-cidr`) are not supported here.
pub(crate) const POD_CIDR: &str = "10.42.0.0/16";
pub(crate) const SERVICE_CIDR: &str = "10.43.0.0/16";

/// A dotted IPv4 address, because it is placed into a shell line.
pub(crate) fn ip_ok(ip: &str) -> bool {
    let parts: Vec<&str> = ip.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|c| c.is_ascii_digit())
                && p.parse::<u8>().is_ok()
        })
}

/// The commands that route both ranges through the VM. Each deletes first, so running it again after the VM's address
/// changed replaces the old route instead of failing with "route already in table".
pub(crate) fn add_commands(ip: &str) -> Vec<String> {
    [POD_CIDR, SERVICE_CIDR]
        .iter()
        .map(|net| {
            format!(
                "sudo route -n delete -net {net} >/dev/null 2>&1; sudo route -n add -net {net} {ip}"
            )
        })
        .collect()
}

/// The commands that take both routes out again.
pub(crate) fn delete_commands() -> Vec<String> {
    [POD_CIDR, SERVICE_CIDR]
        .iter()
        .map(|net| format!("sudo route -n delete -net {net}"))
        .collect()
}

/// Whether the routes are in place, from `netstat -rn -f inet`.
#[derive(Debug, PartialEq)]
pub(crate) enum State {
    /// Neither range is routed.
    Absent,
    /// Both ranges go through the VM's current address.
    Current,
    /// A range is missing or goes through another address (the VM got a new one), with what was found.
    Stale(String),
}

/// `netstat` prints a range as `10.42/16`, so match that spelling as well as the full one.
fn gateway_of(netstat: &str, net: &str) -> Option<String> {
    let short = net.trim_end_matches(".0.0/16").to_owned() + "/16";
    netstat.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let dest = f.next()?;
        (dest == net || dest == short).then(|| f.next().map(str::to_owned))?
    })
}

pub(crate) fn state(netstat: &str, ip: &str) -> State {
    let pod = gateway_of(netstat, POD_CIDR);
    let svc = gateway_of(netstat, SERVICE_CIDR);
    match (pod.as_deref(), svc.as_deref()) {
        (None, None) => State::Absent,
        (Some(p), Some(s)) if p == ip && s == ip => State::Current,
        (p, s) => State::Stale(format!(
            "pods via {}, Services via {}, but the VM is at {ip}",
            p.unwrap_or("nothing"),
            s.unwrap_or("nothing")
        )),
    }
}

fn routing_table() -> String {
    std::process::Command::new("netstat")
        .args(["-rn", "-f", "inet"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn run_all(commands: &[String]) -> Result<()> {
    for c in commands {
        let status = std::process::Command::new("sh")
            .args(["-c", c])
            .status()
            .context("running the route command")?;
        if !status.success() {
            bail!("the route command failed ({status}): {c}");
        }
    }
    Ok(())
}

/// `fluxctl kube route [--install | --remove]`.
pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    install: bool,
    remove: bool,
) -> Result<String> {
    if remove {
        let cmds = delete_commands();
        if install {
            run_all(&cmds)?;
            return Ok("removed the pod and Service routes".into());
        }
        return Ok(format!(
            "To remove the routes:\n  {}\n\nOr run `fluxctl kube route --remove --install`.",
            cmds.join("\n  ")
        ));
    }
    if o.k3s_args
        .iter()
        .any(|a| a.contains("--cluster-cidr") || a.contains("--service-cidr"))
    {
        bail!(
            "kube route knows k3s's default ranges ({POD_CIDR}, {SERVICE_CIDR}); this cluster sets its own"
        );
    }
    let Some((id, _)) = api.find(&o.name).await? else {
        bail!(
            "no cluster named {}; start it with `fluxctl kube up`",
            o.name
        );
    };
    let Some(ip) = api.guest_ip(id).await? else {
        bail!("{} has no address yet; is it running?", o.name);
    };
    if !ip_ok(&ip) {
        bail!("the VM's address {ip:?} is not an IPv4 address");
    }
    let cmds = add_commands(&ip);
    let now = state(&routing_table(), &ip);
    if install {
        if now == State::Current {
            return Ok(format!("the routes already go through {ip}"));
        }
        run_all(&cmds)?;
        return Ok(format!(
            "pods ({POD_CIDR}) and Services ({SERVICE_CIDR}) now answer from this Mac through {ip}. The VM's address can change when it restarts: run this again if they stop answering. Remove them with `fluxctl kube route --remove --install`."
        ));
    }
    let note = match now {
        State::Current => "The routes are already in place.".to_owned(),
        State::Absent => "No routes yet.".to_owned(),
        State::Stale(why) => format!("The routes are out of date ({why})."),
    };
    Ok(format!(
        "{note}\n\nTo reach pod and Service addresses from this Mac (asks for your password; changes this Mac's routing table):\n  {}\n\nOr run `fluxctl kube route --install`. Remove them later with `fluxctl kube route --remove`.",
        cmds.join("\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "Routing tables\n\nInternet:\nDestination        Gateway            Flags               Netif Expire\ndefault            192.168.1.1        UGScg                 en1\n10.42/16           192.168.64.183     UGSc                  en1\n10.43/16           192.168.64.183     UGSc                  en1\n192.168.64/24      link#28            UC            bridge100      !\n";

    #[test]
    fn addresses_are_plain_dotted_quads() {
        assert!(ip_ok("192.168.64.183"));
        for bad in [
            "",
            "192.168.64",
            "1.2.3.4.5",
            "256.1.1.1",
            "a.b.c.d",
            "1.2.3.4; rm -rf /",
            "1..3.4",
        ] {
            assert!(!ip_ok(bad), "{bad}");
        }
    }

    #[test]
    fn both_ranges_are_routed_through_the_vm_and_replaced_when_it_moves() {
        let c = add_commands("192.168.64.183");
        assert_eq!(c.len(), 2);
        assert!(
            c[0].contains("route -n delete -net 10.42.0.0/16")
                && c[0].ends_with("route -n add -net 10.42.0.0/16 192.168.64.183")
        );
        assert!(c[1].ends_with("route -n add -net 10.43.0.0/16 192.168.64.183"));
        assert_eq!(
            delete_commands()[1],
            "sudo route -n delete -net 10.43.0.0/16"
        );
    }

    #[test]
    fn the_routing_table_says_whether_the_routes_are_current() {
        assert_eq!(state(TABLE, "192.168.64.183"), State::Current);
        assert!(
            matches!(state(TABLE, "192.168.64.190"), State::Stale(w) if w.contains("192.168.64.183") && w.contains("192.168.64.190"))
        );
        assert_eq!(
            state("default 192.168.1.1 UGScg en1\n", "192.168.64.183"),
            State::Absent
        );
        let half = "10.42/16           192.168.64.183     UGSc                  en1\n";
        assert!(
            matches!(state(half, "192.168.64.183"), State::Stale(w) if w.contains("Services via nothing"))
        );
    }
}
