// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use fluxvm_core::{config::Config, model::CloudInitSpec, process::run_checked};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn yaml_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `static_net`, when `ci.static_network` is set, is `(guest_cidr, gateway)`
/// -- e.g. `("169.254.12.35/28", "169.254.12.33")` -- from
/// `fluxvm_network::netns::NetnsHandle`/`PreparedNetwork`. Callers must
/// run network prep *before* this so that's available; `None` is only
/// valid when `ci.static_network` is false (checked by the caller, not
/// re-validated here, since which networking modes have a known address is
/// this module's caller's concern, not cloud-init seed-building's).
fn write_seed_files(
    dir: &Path,
    ci: &CloudInitSpec,
    static_net: Option<(&str, &str)>,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>)> {
    let user_data = dir.join("user-data");
    let meta_data = dir.join("meta-data");
    let hostname = ci.hostname.clone().unwrap_or_else(|| "fluxvm-vm".into());
    let user = ci.user.clone().unwrap_or_else(|| "cloud".into());

    let mut body = String::from("#cloud-config\n");
    body.push_str(&format!("hostname: {}\n", yaml_quote(&hostname)));
    body.push_str("users:\n");
    body.push_str("  - default\n");
    body.push_str(&format!(
        "  - name: {}\n    sudo: ALL=(ALL) NOPASSWD:ALL\n    shell: /bin/bash\n",
        yaml_quote(&user)
    ));
    if !ci.ssh_authorized_keys.is_empty() {
        body.push_str("    ssh_authorized_keys:\n");
        for key in &ci.ssh_authorized_keys {
            body.push_str(&format!("      - {}\n", yaml_quote(key)));
        }
    }
    if !ci.packages.is_empty() {
        body.push_str("package_update: true\npackages:\n");
        for p in &ci.packages {
            body.push_str(&format!("  - {}\n", yaml_quote(p)));
        }
    }
    if !ci.runcmd.is_empty() {
        body.push_str("runcmd:\n");
        for cmd in &ci.runcmd {
            body.push_str(&format!("  - [ bash, -lc, {} ]\n", yaml_quote(cmd)));
        }
    }
    if !ci.write_files.is_empty() {
        body.push_str("write_files:\n");
        for f in &ci.write_files {
            body.push_str(&format!("  - path: {}\n", yaml_quote(&f.path)));
            if let Some(perms) = &f.permissions {
                body.push_str(&format!("    permissions: {}\n", yaml_quote(perms)));
            }
            body.push_str("    encoding: b64\n");
            body.push_str(&format!(
                "    content: {}\n",
                yaml_quote(&B64.encode(f.content.as_bytes()))
            ));
        }
    }

    fs::write(&user_data, body).context("writing cloud-init user-data")?;
    fs::write(
        &meta_data,
        format!("instance-id: {}\nlocal-hostname: {}\n", hostname, hostname),
    )?;

    let network_config = dir.join("network-config");
    let nics = static_nic_entries(ci);
    let network_config = if ci.static_network {
        let (cidr, gateway) = static_net
            .context("static_network is set but no address was prepared -- network prep must run before build_seed")?;
        // `match: {name: en*}` rather than a specific interface name: which
        // predictable name (enp0s1, ens3, eth0, ...) a given kernel/udev
        // combination assigns isn't known ahead of boot, and this is the
        // netplan-supported way to say "whichever the single real NIC is".
        let netcfg = format!(
            "network:\n  version: 2\n  ethernets:\n    guestnet0:\n      match:\n        name: en*\n      dhcp4: false\n      addresses:\n        - {cidr}\n      routes:\n        - to: default\n          via: {gateway}\n{nics}"
        );
        fs::write(&network_config, netcfg).context("writing cloud-init network-config")?;
        Some(network_config)
    } else if !nics.is_empty() || ci.mtu.is_some() {
        // The catch-all DHCP match is the one the macOS seed writes by default; the static entries sort before it
        // (`fluxvm-nic*` < `vz-nic`), so a private card is claimed by its own entry and never runs DHCP.
        let mtu = ci
            .mtu
            .map(|m| format!("    mtu: {m}\n"))
            .unwrap_or_default();
        let netcfg = format!(
            "version: 2\nethernets:\n  vz-nic:\n    match:\n      name: \"e*\"\n    dhcp4: true\n    dhcp-identifier: mac\n{mtu}{nics}"
        );
        fs::write(&network_config, netcfg).context("writing cloud-init network-config")?;
        Some(network_config)
    } else {
        None
    };
    Ok((user_data, meta_data, network_config))
}

/// network-config v2 entries for `ci.static_nics`: matched by MAC, one static address, no DHCP, no link-local and no
/// router advertisements, so the card is configured as soon as networkd starts and is never waited on.
fn static_nic_entries(ci: &CloudInitSpec) -> String {
    let mut out = String::new();
    for (i, nic) in ci.static_nics.iter().enumerate() {
        out.push_str(&format!(
            "  fluxvm-nic{i}:\n    match:\n      macaddress: {}\n    dhcp4: false\n    dhcp6: false\n    accept-ra: false\n    link-local: []\n    addresses:\n      - {}\n",
            yaml_quote(&nic.mac),
            yaml_quote(&nic.address)
        ));
        if let Some(m) = nic.mtu {
            out.push_str(&format!("    mtu: {m}\n"));
        }
    }
    out
}

pub async fn build_seed(
    cfg: &Config,
    dir: &Path,
    ci: &CloudInitSpec,
    static_net: Option<(&str, &str)>,
) -> Result<PathBuf> {
    let (user_data, meta_data, network_config) = write_seed_files(dir, ci, static_net)?;
    let seed = dir.join("seed.img");
    let mut args = vec!["--disk-format".to_string(), "raw".to_string()];
    if let Some(network_config) = network_config.as_ref() {
        args.push("--network-config".into());
        args.push(network_config.display().to_string());
    }
    args.push(seed.display().to_string());
    args.push(user_data.display().to_string());
    args.push(meta_data.display().to_string());

    #[cfg(target_os = "macos")]
    {
        // `cloud-localds` is a Linux package; macOS builds the same NoCloud ("cidata") ISO with hdiutil.
        let _ = (&args, cfg);
        return build_seed_hdiutil(dir, &user_data, &meta_data, network_config.as_deref()).await;
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_checked(&cfg.cloud_localds_binary, &args).await?;
        Ok(seed)
    }
}

/// NoCloud seed as an ISO9660/Joliet image labelled `cidata`.
#[cfg(target_os = "macos")]
pub async fn build_seed_hdiutil(
    dir: &Path,
    user_data: &Path,
    meta_data: &Path,
    network_config: Option<&Path>,
) -> Result<PathBuf> {
    let staging = dir.join("seed-src");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    fs::copy(user_data, staging.join("user-data"))?;
    fs::copy(meta_data, staging.join("meta-data"))?;
    match network_config {
        Some(nc) => {
            fs::copy(nc, staging.join("network-config"))?;
        }
        // A MAC-based DHCP identity keeps the guest's address stable across network restarts on first boot.
        None => fs::write(
            staging.join("network-config"),
            "version: 2\nethernets:\n  vz-nic:\n    match:\n      name: \"e*\"\n    dhcp4: true\n    dhcp-identifier: mac\n",
        )?,
    }
    let seed = dir.join("seed.img");
    let _ = fs::remove_file(&seed);
    run_checked(
        "hdiutil",
        &[
            "makehybrid".into(),
            "-quiet".into(),
            "-iso".into(),
            "-joliet".into(),
            "-default-volume-name".into(),
            "cidata".into(),
            "-o".into(),
            seed.display().to_string(),
            staging.display().to_string(),
        ],
    )
    .await?;
    // hdiutil may append `.iso`; keep a stable name for the backend.
    let iso = dir.join("seed.img.iso");
    if iso.exists() {
        fs::rename(&iso, &seed)?;
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(seed)
}

/// Seed Cloud-init's NoCloud datasource inside a flat ext4 VM clone.
/// This avoids cloud-localds and a second virtio disk in the native engine.
pub fn inject_nocloud_raw(
    disk: &Path,
    dir: &Path,
    ci: &CloudInitSpec,
    static_net: Option<(&str, &str)>,
) -> Result<()> {
    let (user_data, meta_data, network_config) = write_seed_files(dir, ci, static_net)?;
    let base = "/var/lib/cloud/seed/nocloud";
    crate::raw_ext4::write_file(
        disk,
        &format!("{base}/user-data"),
        &fs::read(user_data)?,
        0o600,
    )?;
    crate::raw_ext4::write_file(
        disk,
        &format!("{base}/meta-data"),
        &fs::read(meta_data)?,
        0o600,
    )?;
    if let Some(network_config) = network_config {
        crate::raw_ext4::write_file(
            disk,
            &format!("{base}/network-config"),
            &fs::read(network_config)?,
            0o600,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn embeds_nocloud_in_flat_ext4_without_cloud_localds() {
        if Command::new("mkfs.ext4").arg("-V").output().is_err()
            || Command::new("debugfs").arg("-V").output().is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("root.raw");
        fs::File::create(&disk)
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        assert!(
            Command::new("mkfs.ext4")
                .args(["-q", "-F"])
                .arg(&disk)
                .status()
                .unwrap()
                .success()
        );
        inject_nocloud_raw(&disk, dir.path(), &CloudInitSpec::default(), None).unwrap();
        let output = Command::new("debugfs")
            .args(["-R", "cat /var/lib/cloud/seed/nocloud/user-data"])
            .arg(&disk)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("#cloud-config"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::model::CloudInitNic;

    fn spec(nics: Vec<CloudInitNic>) -> CloudInitSpec {
        CloudInitSpec {
            static_nics: nics,
            ..Default::default()
        }
    }

    #[test]
    fn private_cards_get_a_static_entry_and_no_dhcp() {
        let d = tempfile::tempdir().unwrap();
        let ci = spec(vec![CloudInitNic {
            mac: "02:e2:1f:84:d3:72".into(),
            address: "10.89.7.20/24".into(),
            mtu: None,
        }]);
        let (_, _, nc) = write_seed_files(d.path(), &ci, None).unwrap();
        let body = fs::read_to_string(nc.expect("a network-config")).unwrap();
        assert!(body.contains("name: \"e*\"") && body.contains("dhcp4: true"));
        let nic = body.split("fluxvm-nic0:").nth(1).unwrap();
        assert!(nic.contains("macaddress: '02:e2:1f:84:d3:72'"));
        assert!(nic.contains("dhcp4: false") && nic.contains("- '10.89.7.20/24'"));
        assert!(nic.contains("link-local: []") && nic.contains("accept-ra: false"));
        // "fluxvm-nic0" sorts before "vz-nic": networkd picks the first file that matches.
        assert!("fluxvm-nic0" < "vz-nic");
    }

    #[test]
    fn private_card_mtu_is_written_only_when_set() {
        let d = tempfile::tempdir().unwrap();
        let nic = |mtu| CloudInitNic {
            mac: "02:e2:1f:84:d3:72".into(),
            address: "10.89.7.20/24".into(),
            mtu,
        };
        let (_, _, nc) = write_seed_files(d.path(), &spec(vec![nic(Some(9000))]), None).unwrap();
        let body = fs::read_to_string(nc.unwrap()).unwrap();
        assert!(body.split("fluxvm-nic0:").nth(1).unwrap().contains("mtu: 9000"));
        let d2 = tempfile::tempdir().unwrap();
        let (_, _, nc) = write_seed_files(d2.path(), &spec(vec![nic(None)]), None).unwrap();
        let body = fs::read_to_string(nc.unwrap()).unwrap();
        assert!(!body.split("fluxvm-nic0:").nth(1).unwrap().contains("mtu:"));
    }

    #[test]
    fn nat_mtu_goes_into_the_primary_cards_network_config() {
        let d = tempfile::tempdir().unwrap();
        let ci = CloudInitSpec {
            mtu: Some(1400),
            ..Default::default()
        };
        let (_, _, nc) = write_seed_files(d.path(), &ci, None).unwrap();
        let body = fs::read_to_string(nc.expect("a network-config")).unwrap();
        assert!(
            body.contains("dhcp4: true") && body.contains("    mtu: 1400\n"),
            "{body}"
        );
        // Without it the file is unchanged: no network-config at all (the macOS builder writes its own default).
        let (_, _, none) = write_seed_files(d.path(), &CloudInitSpec::default(), None).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn without_private_cards_nothing_changes() {
        let d = tempfile::tempdir().unwrap();
        let (_, _, nc) = write_seed_files(d.path(), &spec(vec![]), None).unwrap();
        assert!(nc.is_none());
    }
}
