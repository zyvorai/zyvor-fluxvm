// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet volumes | volume-create | volume-rm | volume-resize`: persistent volumes, disks that keep their data
//! when the VM using them is deleted (`/fleet/volumes`). `fluxctl fleet launch --volume NAME` attaches one.

use super::*;

fn volume_tone(v: &Value) -> (String, Tone) {
    match s(v, "status") {
        "attached" => ("in use".into(), Tone::Ok),
        "attaching" | "creating" => (s(v, "status").into(), Tone::Warn),
        "available" => ("available".into(), Tone::Dim),
        other => (other.into(), Tone::Plain),
    }
}

fn path_of(name: &str) -> String {
    format!("/fleet/volumes/{}", esc(name))
}

/// `fluxctl fleet volumes`.
pub(super) async fn list(
    central: &str,
    token: Option<&str>,
    out: &Out,
    tenant: Option<String>,
) -> Result<()> {
    let path = match &tenant {
        Some(t) => format!("/fleet/volumes?tenant={}", esc(t)),
        None => "/fleet/volumes".to_owned(),
    };
    let v = get(central, token, &path).await?;
    if !out.human {
        return out.json(&v);
    }
    let list = items(&v);
    if list.is_empty() {
        println!("No volumes yet. A volume keeps its data when the VM using it is deleted.");
        out.next("fluxctl fleet volume-create <name> --size 20");
        return Ok(());
    }
    // VM names for the "attached to" column.
    let vms = items(&get(central, token, "/fleet/vms").await.unwrap_or_default());
    let vm_name = |node: &str, id: &str| {
        vms.iter()
            .find(|vm| s(vm, "id") == id && s(vm, "node") == node)
            .map(|vm| s(vm, "name").to_owned())
            .unwrap_or_else(|| id.chars().take(8).collect())
    };
    let operator = list.iter().any(|x| x.get("path").is_some());
    let mut headers = vec!["NAME", "SIZE", "MAC", "STATUS", "ATTACHED TO"];
    if operator {
        headers.push("TENANT");
    }
    headers.push("CREATED");
    if out.wide {
        headers.push("ID");
    }
    let mut t = Table::new(headers);
    for x in &list {
        let (st, tone) = volume_tone(x);
        let attached = match x.get("attached_to").and_then(Value::as_str) {
            Some(id) => vm_name(s(x, "node"), id),
            None => "-".into(),
        };
        let mut row = vec![
            c(s(x, "name")),
            c(format!("{} GiB", u(x, "size_gib"))),
            c(s(x, "node")),
            ct(st, tone),
            c(attached),
        ];
        if operator {
            row.push(match s(x, "tenant") {
                "" => ct("operator", Tone::Dim),
                t => c(t),
            });
        }
        row.push(ct(
            time(x, "created_at").map(rel).unwrap_or_default(),
            Tone::Dim,
        ));
        if out.wide {
            row.push(c(s(x, "id")));
        }
        t.row(row);
    }
    t.print(out);
    let total: u64 = list.iter().map(|x| u(x, "size_gib")).sum();
    println!(
        "{}",
        out.dim(format!(
            "{} GiB in {} volume{}",
            total,
            list.len(),
            if list.len() == 1 { "" } else { "s" }
        ))
    );
    Ok(())
}

/// `fluxctl fleet volume-create`.
pub(super) async fn create(
    central: &str,
    token: Option<&str>,
    out: &Out,
    name: String,
    size: u64,
    node: Option<String>,
    tenant: Option<String>,
) -> Result<()> {
    if size == 0 || size > 2048 {
        bail!("--size is in GiB, 1 to 2048");
    }
    let mut body = json!({"name": name, "size_gib": size});
    if let Some(n) = node {
        body["node"] = json!(n);
    }
    if let Some(t) = tenant {
        body["tenant"] = json!(t);
    }
    let v = match call(central, token, Method::POST, "/fleet/volumes", Some(body)).await {
        Ok(v) => v,
        Err(e) if status_of(&e) == Some(429) => {
            return Err(e.context(
                "hint: see what you hold with `fluxctl fleet volumes`; delete one or ask your operator for more",
            ));
        }
        Err(e) => return Err(e),
    };
    if !out.human {
        return out.json(&v);
    }
    out.done(format!(
        "Created volume {} ({} GiB) on {}.",
        out.bold(s(&v, "name")),
        u(&v, "size_gib"),
        s(&v, "node")
    ));
    kv(
        out,
        &[
            ("  ID", s(&v, "id").to_owned()),
            (
                "  In the guest",
                format!("/dev/disk/by-id/virtio-{}", s(&v, "id")),
            ),
        ],
    );
    out.next(format!(
        "fluxctl fleet launch --image <linux image> --volume {}",
        s(&v, "name")
    ));
    Ok(())
}

/// `fluxctl fleet volume-rm`.
pub(super) async fn remove(
    central: &str,
    token: Option<&str>,
    out: &Out,
    name: &str,
    yes: bool,
) -> Result<()> {
    let v = get(central, token, &path_of(name)).await?;
    let label = s(&v, "name").to_owned();
    if let Some(vm) = v.get("attached_to").and_then(Value::as_str) {
        bail!(
            "volume {label} is attached to VM {vm}\nhint: delete the VM first (`fluxctl fleet rm <vm>`); the volume keeps its data"
        );
    }
    if out.human
        && !yes
        && !confirm(&format!(
            "Delete volume {label} ({} GiB on {}) and all its data?",
            u(&v, "size_gib"),
            s(&v, "node")
        ))?
    {
        println!("Kept {label}.");
        return Ok(());
    }
    // By id: a name can be shared by several tenants' volumes (the operator sees them all).
    call(central, token, Method::DELETE, &path_of(s(&v, "id")), None).await?;
    if !out.human {
        return out.json(&json!({"ok": true, "id": s(&v, "id"), "name": label}));
    }
    out.done(format!("Deleted volume {label}."));
    Ok(())
}

/// `fluxctl fleet volume-resize`.
pub(super) async fn resize(
    central: &str,
    token: Option<&str>,
    out: &Out,
    name: &str,
    size: u64,
) -> Result<()> {
    let v = call(
        central,
        token,
        Method::POST,
        &format!("{}/resize", path_of(name)),
        Some(json!({"size_gib": size})),
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    out.done(format!(
        "Volume {} is now {} GiB.",
        out.bold(s(&v, "name")),
        u(&v, "size_gib")
    ));
    println!(
        "{}",
        out.dim(
            "Grow the file system inside the guest next time it is attached: resize2fs (ext4) or xfs_growfs (XFS)."
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_read_well() {
        assert_eq!(volume_tone(&json!({"status": "attached"})).0, "in use");
        assert_eq!(volume_tone(&json!({"status": "available"})).0, "available");
        assert_eq!(path_of("pg-data"), "/fleet/volumes/pg-data");
    }
}
