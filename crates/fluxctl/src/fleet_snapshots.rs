// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet snapshot | snapshots | restore | snapshot-rm | clone`: VM snapshots and clones through central
//! (`/fleet/vms/{node}/{id}/snapshots`, `…/clone`). VMs are found by name or ID prefix like every other fleet verb.

use super::*;

pub(super) async fn run(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: FleetCommand,
) -> Result<()> {
    match command {
        FleetCommand::Snapshot { vm, name, memory } => {
            let (node, id, vm_name) = resolve_vm(central, token, &vm).await?;
            let mut body = json!({"include_memory": memory});
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/vms/{}/{}/snapshots", esc(&node), esc(&id)),
                Some(body),
            )
            .await
            .map_err(|e| hint(e, &vm_name))?;
            if !out.human {
                return out.json(&v);
            }
            let snap = v.get("snapshot").cloned().unwrap_or_default();
            let what = if snap.get("memory").and_then(Value::as_bool) == Some(true) {
                "with memory"
            } else {
                "disk only"
            };
            out.done(format!(
                "Took snapshot {} of {vm_name} ({what}, {}).",
                out.bold(s(&snap, "name")),
                size(u(&snap, "size_bytes"))
            ));
            out.next(format!(
                "fluxctl fleet restore {vm_name} {}   (or `fluxctl fleet clone {vm_name} <new-name> --snapshot {}`)",
                s(&snap, "name"),
                s(&snap, "name")
            ));
            Ok(())
        }
        FleetCommand::Snapshots { vm } => {
            let (node, id, vm_name) = resolve_vm(central, token, &vm).await?;
            let v = call(
                central,
                token,
                Method::GET,
                &format!("/fleet/vms/{}/{}/snapshots", esc(&node), esc(&id)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("{vm_name} has no snapshots.");
                out.next(format!("fluxctl fleet snapshot {vm_name} --name <name>"));
                return Ok(());
            }
            snapshot_table(&list).print(out);
            println!(
                "{}",
                out.dim(format!(
                    "{} of {} snapshots kept for this VM.",
                    list.len(),
                    u(&v, "limit")
                ))
            );
            Ok(())
        }
        FleetCommand::Restore { vm, snapshot, yes } => {
            let (node, id, vm_name) = resolve_vm(central, token, &vm).await?;
            if out.human
                && !yes
                && !confirm(&format!(
                    "Restore {vm_name} to snapshot {snapshot}? It is stopped and started again, and everything since the snapshot is lost."
                ))?
            {
                println!("Left {vm_name} as it was.");
                return Ok(());
            }
            let v = call(
                central,
                token,
                Method::POST,
                &format!(
                    "/fleet/vms/{}/{}/snapshots/{}/restore",
                    esc(&node),
                    esc(&id),
                    esc(&snapshot)
                ),
                None,
            )
            .await
            .map_err(|e| hint(e, &vm_name))?;
            if !out.human {
                return out.json(&v);
            }
            let vm = v.get("vm").cloned().unwrap_or_default();
            out.done(format!(
                "{vm_name} restored to {snapshot}; it is {}.",
                vm_status(&vm).0
            ));
            Ok(())
        }
        FleetCommand::SnapshotRm { vm, snapshot, yes } => {
            let (node, id, vm_name) = resolve_vm(central, token, &vm).await?;
            if out.human
                && !yes
                && !confirm(&format!(
                    "Delete snapshot {snapshot} of {vm_name}? It cannot be restored afterwards."
                ))?
            {
                println!("Kept {snapshot}.");
                return Ok(());
            }
            call(
                central,
                token,
                Method::DELETE,
                &format!(
                    "/fleet/vms/{}/{}/snapshots/{}",
                    esc(&node),
                    esc(&id),
                    esc(&snapshot)
                ),
                None,
            )
            .await
            .map_err(|e| hint(e, &vm_name))?;
            if !out.human {
                return out.json(&json!({"ok": true, "vm": id, "snapshot": snapshot}));
            }
            out.done(format!("Deleted snapshot {snapshot} of {vm_name}."));
            Ok(())
        }
        FleetCommand::CloneVm {
            vm,
            new_name,
            snapshot,
        } => {
            let (node, id, vm_name) = resolve_vm(central, token, &vm).await?;
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/vms/{}/{}/clone", esc(&node), esc(&id)),
                Some(json!({"name": new_name, "snapshot": snapshot})),
            )
            .await
            .map_err(|e| hint(e, &vm_name))?;
            if !out.human {
                return out.json(&v);
            }
            let from = snapshot
                .map(|s| format!("snapshot {s} of {vm_name}"))
                .unwrap_or(vm_name);
            launched(out, &v, Some(format!("cloned from {from}")));
            Ok(())
        }
        _ => unreachable!("only snapshot commands are routed here"),
    }
}

/// Central's refusal with a next step, for the refusals people hit most; anything else is passed on as it is.
fn hint(e: anyhow::Error, vm: &str) -> anyhow::Error {
    let Some((status, msg)) = e
        .chain()
        .find_map(|c| c.downcast_ref::<HttpError>())
        .map(|h| (h.status.as_u16(), h.message.clone()))
    else {
        return e;
    };
    let tip = match status {
        429 if msg.contains("snapshot") => format!(
            "delete an old one first: `fluxctl fleet snapshots {vm}`, then `fluxctl fleet snapshot-rm {vm} <name>`"
        ),
        409 if msg.contains("running or paused") => format!(
            "start {vm} first (`fluxctl fleet start {vm}`), or leave out --memory for a disk-only snapshot"
        ),
        409 if msg.contains("stopped") => format!(
            "stop {vm} first, or clone one of its snapshots with --snapshot (`fluxctl fleet snapshots {vm}`)"
        ),
        404 if msg.contains("snapshot") => format!("list them with `fluxctl fleet snapshots {vm}`"),
        _ => return e,
    };
    anyhow::anyhow!("{msg}\nhint: {tip}")
}

/// `1.5 GiB`, `512 MiB`.
fn size(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        let g = bytes as f64 / GIB as f64;
        if g >= 100.0 {
            format!("{g:.0} GiB")
        } else {
            format!("{g:.1} GiB").replace(".0 ", " ")
        }
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else if bytes == 0 {
        "–".to_owned()
    } else {
        format!("{bytes} B")
    }
}

fn snapshot_table(list: &[Value]) -> Table {
    let mut t = Table::new(vec!["SNAPSHOT", "TAKEN", "SIZE", "CONTENTS"]);
    for x in list {
        let taken = time(x, "created_at")
            .map(|at| format!("{} ({})", local(at), rel(at)))
            .unwrap_or_default();
        let memory = x.get("memory").and_then(Value::as_bool) == Some(true);
        t.row(vec![
            c(s(x, "name")),
            c(taken),
            c(size(u(x, "size_bytes"))),
            if memory {
                ct("disk + memory", Tone::Ok)
            } else {
                ct("disk", Tone::Plain)
            },
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn sizes_read_well() {
        assert_eq!(size(0), "–");
        assert_eq!(size(3 << 29), "1.5 GiB");
        assert_eq!(size(2 << 30), "2 GiB");
        assert_eq!(size(512 << 20), "512 MiB");
        assert_eq!(size(250 << 30), "250 GiB");
    }

    #[test]
    fn the_table_says_what_a_snapshot_holds() {
        let plain = Out {
            human: true,
            wide: false,
            color: false,
        };
        let list = vec![
            json!({"name": "before-upgrade", "created_at": Utc::now().to_rfc3339(), "size_bytes": 3u64 << 30, "memory": true}),
            json!({"name": "nightly", "created_at": Utc::now().to_rfc3339(), "size_bytes": 1u64 << 30, "memory": false}),
        ];
        let r = snapshot_table(&list).render(&plain);
        assert!(r.starts_with("SNAPSHOT"), "{r}");
        assert!(
            r.contains("before-upgrade") && r.contains("disk + memory"),
            "{r}"
        );
        assert!(r.contains("3 GiB"), "{r}");
    }

    #[test]
    fn commands_parse() {
        let parse = |args: &[&str]| {
            let mut full = vec!["fluxctl", "fleet"];
            full.extend_from_slice(args);
            match crate::Cli::try_parse_from(full).map(|c| c.command) {
                Ok(crate::Command::Fleet { command, .. }) => command,
                Ok(_) => panic!("not a fleet command"),
                Err(e) => panic!("{e}"),
            }
        };
        assert!(matches!(
            parse(&["snapshot", "ios-1", "--name", "pre", "--memory"]),
            FleetCommand::Snapshot { vm, name: Some(n), memory: true } if vm == "ios-1" && n == "pre"
        ));
        assert!(matches!(
            parse(&["snapshots", "ios-1"]),
            FleetCommand::Snapshots { .. }
        ));
        assert!(matches!(
            parse(&["restore", "ios-1", "pre", "-y"]),
            FleetCommand::Restore { yes: true, .. }
        ));
        assert!(matches!(
            parse(&["snapshot-rm", "ios-1", "pre"]),
            FleetCommand::SnapshotRm { yes: false, .. }
        ));
        assert!(matches!(
            parse(&["clone", "ios-1", "ios-2", "--snapshot", "pre"]),
            FleetCommand::CloneVm { new_name, snapshot: Some(s), .. } if new_name == "ios-2" && s == "pre"
        ));
    }
}
