// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl create` without a hand-written spec: `--name` and `--image` start a `vz` VM request, and the `--apple-*`
//! style flags fill in `apple.*` fields. With `--spec` the flags are applied on top of the file.

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum GuestOs {
    Linux,
    Macos,
    Windows,
}

#[derive(Args, Debug, Default)]
pub struct CreateArgs {
    /// JSON spec file (the REST create body). Flags below override its fields.
    #[arg(long, required_unless_present_any = ["name", "image"])]
    pub spec: Option<PathBuf>,
    /// VM name; with `--image` this builds a `vz` request without a spec file.
    #[arg(long)]
    pub name: Option<String>,
    /// Disk image or, for a macOS guest, the IPSW.
    #[arg(long)]
    pub image: Option<PathBuf>,
    #[arg(long)]
    pub vcpus: Option<u8>,
    #[arg(long)]
    pub memory_mib: Option<u64>,
    #[arg(long)]
    pub disk_size_gib: Option<u64>,
    /// Guest OS on the `vz` backend (`apple.guest_os`).
    #[arg(long, value_enum)]
    pub guest: Option<GuestOs>,
    /// Installer ISO attached read-only (`apple.media`); for `--guest windows`, the Windows 11 ARM64 ISO.
    #[arg(long)]
    pub media: Option<PathBuf>,
    /// Windows guests: the virtio-win ISO, attached read-only on USB so the installer can load the disk and network
    /// drivers (`apple.extra_disks`).
    #[arg(long)]
    pub virtio_win: Option<PathBuf>,
    /// Open the guest's console window.
    #[arg(long)]
    pub window: bool,
    /// Initial display, `WIDTHxHEIGHT` or `WIDTHxHEIGHT@PPI` (`apple.display_*`).
    #[arg(long, value_name = "WxH[@PPI]")]
    pub display: Option<String>,
    /// Number of displays for a macOS guest (1-8).
    #[arg(long)]
    pub displays: Option<u8>,
    /// Expose the host Rosetta runtime to a Linux guest.
    #[arg(long)]
    pub rosetta: bool,
    /// Linux guest SPICE clipboard sharing.
    #[arg(long)]
    pub clipboard: bool,
    /// Expose the host microphone to the guest.
    #[arg(long)]
    pub microphone: bool,
    /// Guest→host vsock service, repeatable: `PORT=metadata`, `PORT=telemetry` (answered by the runner) or `PORT=socket:NAME`
    /// (relayed to the host Unix socket for NAME; see `apple.vsock_services`).
    #[arg(long = "vsock-service", value_name = "PORT=KIND")]
    pub vsock_service: Vec<String>,
    /// Do not play guest audio on the host.
    #[arg(long)]
    pub mute: bool,
    /// Nested virtualization for a Linux guest (macOS 15+ host).
    #[arg(long)]
    pub nested_virtualization: bool,
    /// Add an XHCI controller so USB devices can be hot-attached.
    #[arg(long)]
    pub usb_controller: bool,
    /// Number of XHCI controllers (USB buses, 1-4); implies --usb-controller.
    #[arg(long, value_name = "N")]
    pub usb_controllers: Option<u8>,
    /// Sparse ASIF overlay for guest writes (macOS 27+).
    #[arg(long)]
    pub asif_overlay: bool,
    /// Install a macOS guest from the IPSW before first boot.
    #[arg(long)]
    pub install: bool,
    /// Boot a macOS guest into Recovery.
    #[arg(long)]
    pub recovery: bool,
    /// Bridge the guest onto this host interface (for example `en0`).
    #[arg(long, value_name = "IFACE")]
    pub bridge: Option<String>,
    /// macOS 27 first-boot account: full name, user name and a file holding the password.
    #[arg(long)]
    pub provision_full_name: Option<String>,
    #[arg(long, requires = "provision_password_file")]
    pub provision_username: Option<String>,
    #[arg(long, requires = "provision_username")]
    pub provision_password_file: Option<PathBuf>,
    #[arg(long)]
    pub provision_auto_login: bool,
    #[arg(long)]
    pub provision_remote_login: bool,
}

/// `WIDTHxHEIGHT` or `WIDTHxHEIGHT@PPI`.
fn parse_display(s: &str) -> Result<(u32, u32, Option<u32>)> {
    let (dims, ppi) = match s.split_once('@') {
        Some((d, p)) => (d, Some(p.parse::<u32>().context("display ppi")?)),
        None => (s, None),
    };
    let (w, h) = dims
        .split_once(['x', 'X'])
        .context("display must be WIDTHxHEIGHT or WIDTHxHEIGHT@PPI")?;
    Ok((
        w.parse().context("display width")?,
        h.parse().context("display height")?,
        ppi,
    ))
}

/// The REST create body: the spec file (or a fresh `vz` request) with every flag that was given applied on top.
pub fn create_body(a: &CreateArgs) -> Result<Value> {
    let mut body = match &a.spec {
        Some(p) => serde_json::from_slice::<Value>(
            &std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        )?,
        None => json!({"backend": "vz"}),
    };
    let obj = body
        .as_object_mut()
        .context("the spec must be a JSON object")?;
    if let Some(n) = &a.name {
        obj.insert("name".into(), json!(n));
    }
    if let Some(i) = &a.image {
        obj.insert("image".into(), json!(i));
    } else if matches!(a.guest, Some(GuestOs::Windows))
        && let Some(m) = &a.media
    {
        // A Windows install boots from a blank disk, so there is no base image; the ISO stands in for it.
        obj.insert("image".into(), json!(m));
    }
    if a.spec.is_none() && (!obj.contains_key("name") || !obj.contains_key("image")) {
        bail!("without --spec both --name and --image are required");
    }
    if let Some(v) = a.vcpus {
        obj.insert("vcpus".into(), json!(v));
    }
    if let Some(v) = a.memory_mib {
        obj.insert("memory_mib".into(), json!(v));
    }
    if let Some(v) = a.disk_size_gib {
        obj.insert("disk_size_gib".into(), json!(v));
    }

    let mut apple = serde_json::Map::new();
    if let Some(g) = a.guest {
        apple.insert(
            "guest_os".into(),
            json!(match g {
                GuestOs::Linux => "linux",
                GuestOs::Macos => "macos",
                GuestOs::Windows => "windows",
            }),
        );
    }
    if let Some(m) = &a.media {
        apple.insert("media".into(), json!(m));
    }
    if let Some(v) = &a.virtio_win {
        apple.insert(
            "extra_disks".into(),
            json!([{ "name": "virtio-win", "path": v, "read_only": true, "controller": "usb" }]),
        );
    }
    if let Some(d) = &a.display {
        let (w, h, ppi) = parse_display(d)?;
        apple.insert("display_width".into(), json!(w));
        apple.insert("display_height".into(), json!(h));
        if let Some(p) = ppi {
            apple.insert("display_ppi".into(), json!(p));
        }
    }
    if let Some(n) = a.usb_controllers {
        apple.insert("usb_controllers".into(), json!(n));
    }
    if let Some(n) = a.displays {
        apple.insert("display_count".into(), json!(n));
    }
    for (on, key) in [
        (a.window, "window"),
        (a.rosetta, "rosetta"),
        (a.clipboard, "clipboard"),
        (a.microphone, "microphone"),
        (a.nested_virtualization, "nested_virtualization"),
        (a.usb_controller, "usb_controller"),
        (a.asif_overlay, "asif_overlay"),
        (a.install, "install"),
        (a.recovery, "recovery"),
        (a.provision_auto_login, "provision_auto_login"),
        (a.provision_remote_login, "provision_remote_login"),
    ] {
        if on {
            apple.insert(key.into(), json!(true));
        }
    }
    if a.mute {
        apple.insert("audio_output".into(), json!(false));
    }
    if !a.vsock_service.is_empty() {
        let list = a
            .vsock_service
            .iter()
            .map(|v| parse_vsock_service(v))
            .collect::<Result<Vec<_>>>()?;
        apple.insert("vsock_services".into(), json!(list));
    }
    if let Some(b) = &a.bridge {
        apple.insert("bridge_interface".into(), json!(b));
    }
    if let Some(v) = &a.provision_full_name {
        apple.insert("provision_full_name".into(), json!(v));
    }
    if let Some(v) = &a.provision_username {
        apple.insert("provision_username".into(), json!(v));
    }
    if let Some(p) = &a.provision_password_file {
        apple.insert("provision_password_file".into(), json!(p));
    }
    if !apple.is_empty() {
        let slot = obj.entry("apple").or_insert_with(|| json!({}));
        let existing = slot.as_object_mut().context("apple must be an object")?;
        existing.extend(apple);
    }
    Ok(body)
}

/// `PORT=metadata`, `PORT=telemetry`, `PORT=socket:NAME` or `PORT=tcp:127.0.0.1:PORT` → one `apple.vsock_services` entry.
fn parse_vsock_service(s: &str) -> Result<serde_json::Value> {
    let (port, kind) = s
        .split_once('=')
        .with_context(|| format!("--vsock-service {s:?}: expected PORT=KIND"))?;
    let port: u32 = port
        .parse()
        .with_context(|| format!("--vsock-service {s:?}: {port:?} is not a port"))?;
    Ok(if let Some(name) = kind.strip_prefix("socket:") {
        json!({"port": port, "socket": name})
    } else if let Some(addr) = kind.strip_prefix("tcp:") {
        json!({"port": port, "tcp": addr})
    } else {
        json!({"port": port, "builtin": kind})
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> CreateArgs {
        CreateArgs {
            name: Some("mac".into()),
            image: Some("/img/macos.ipsw".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_a_vz_request_from_flags() {
        let mut a = args();
        a.guest = Some(GuestOs::Macos);
        a.display = Some("2560x1440@144".into());
        a.install = true;
        a.mute = true;
        a.vcpus = Some(6);
        let b = create_body(&a).unwrap();
        assert_eq!(b["backend"], "vz");
        assert_eq!(b["name"], "mac");
        assert_eq!(b["vcpus"], 6);
        assert_eq!(b["apple"]["guest_os"], "macos");
        assert_eq!(b["apple"]["display_width"], 2560);
        assert_eq!(b["apple"]["display_ppi"], 144);
        assert_eq!(b["apple"]["install"], true);
        assert_eq!(b["apple"]["audio_output"], false);
        assert!(b["apple"].get("rosetta").is_none());
    }

    #[test]
    fn windows_install_needs_no_image_and_attaches_the_driver_iso() {
        let mut a = CreateArgs {
            name: Some("win".into()),
            ..Default::default()
        };
        a.guest = Some(GuestOs::Windows);
        a.media = Some("/iso/Win11_ARM64.iso".into());
        a.virtio_win = Some("/iso/virtio-win.iso".into());
        let b = create_body(&a).unwrap();
        assert_eq!(b["image"], "/iso/Win11_ARM64.iso");
        assert_eq!(b["apple"]["guest_os"], "windows");
        assert_eq!(b["apple"]["media"], "/iso/Win11_ARM64.iso");
        assert_eq!(b["apple"]["extra_disks"][0]["controller"], "usb");
        assert_eq!(b["apple"]["extra_disks"][0]["read_only"], true);
    }

    #[test]
    fn vsock_services_from_flags() {
        let mut a = args();
        a.vsock_service = vec!["5001=metadata".into(), "5003=socket:echo".into()];
        let b = create_body(&a).unwrap();
        assert_eq!(b["apple"]["vsock_services"][0]["builtin"], "metadata");
        assert_eq!(b["apple"]["vsock_services"][1]["socket"], "echo");
        assert_eq!(b["apple"]["vsock_services"][1]["port"], 5003);
        let t = parse_vsock_service("5100=tcp:127.0.0.1:11434").unwrap();
        assert_eq!(t["tcp"], "127.0.0.1:11434");
        assert!(t.get("builtin").is_none());
        assert!(parse_vsock_service("5001").is_err());
        assert!(parse_vsock_service("x=metadata").is_err());
    }

    #[test]
    fn name_without_image_is_refused() {
        let a = CreateArgs {
            name: Some("x".into()),
            ..Default::default()
        };
        assert!(create_body(&a).is_err());
    }

    #[test]
    fn display_forms() {
        assert_eq!(parse_display("1920x1080").unwrap(), (1920, 1080, None));
        assert_eq!(
            parse_display("5120X2880@218").unwrap(),
            (5120, 2880, Some(218))
        );
        assert!(parse_display("1920").is_err());
    }
}
