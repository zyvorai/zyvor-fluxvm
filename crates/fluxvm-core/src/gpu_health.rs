// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//!
//! GPU health from what the host can see without a vendor agent: whether
//! the function still answers config reads, PCIe AER error counters and
//! link width, plus NVIDIA's own counters when `nvidia-smi` is installed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::gpu::valid_pci_bdf;

/// A GPU at or above this temperature is reported degraded.
pub const HOT_CELSIUS: u32 = 90;
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AerCounts {
    pub correctable: u64,
    pub nonfatal: u64,
    pub fatal: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PcieLink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
}

/// One GPU's line of `nvidia-smi --query-gpu`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NvidiaStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecc_uncorrected: Option<u64>,
    /// `Enabled` or `Disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mig_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_pages_pending: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GpuHealth {
    pub bdf: String,
    pub status: HealthStatus,
    /// Why the status is not healthy, worst first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aer: Option<AerCounts>,
    pub link: PcieLink,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvidia: Option<NvidiaStats>,
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The `TOTAL_ERR_*` line of an `aer_dev_*` file.
fn aer_total(path: &Path) -> Option<u64> {
    let raw = fs::read_to_string(path).ok()?;
    raw.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let key = it.next()?;
        key.starts_with("TOTAL_ERR_")
            .then(|| it.next()?.parse().ok())
            .flatten()
    })
}

fn aer(dev: &Path) -> Option<AerCounts> {
    let correctable = aer_total(&dev.join("aer_dev_correctable"));
    let nonfatal = aer_total(&dev.join("aer_dev_nonfatal"));
    let fatal = aer_total(&dev.join("aer_dev_fatal"));
    if correctable.is_none() && nonfatal.is_none() && fatal.is_none() {
        return None;
    }
    Some(AerCounts {
        correctable: correctable.unwrap_or(0),
        nonfatal: nonfatal.unwrap_or(0),
        fatal: fatal.unwrap_or(0),
    })
}

/// False when the config space vendor ID reads all ones: the device has
/// dropped off the bus. `None` when config cannot be read.
fn responds(dev: &Path) -> Option<bool> {
    let mut f = fs::File::open(dev.join("config")).ok()?;
    let mut id = [0u8; 2];
    f.read_exact(&mut id).ok()?;
    Some(id != [0xff, 0xff])
}

fn is_gpu_class(class: &str) -> bool {
    let Ok(c) = u32::from_str_radix(class.trim_start_matches("0x"), 16) else {
        return false;
    };
    matches!(c >> 8, 0x0300 | 0x0302)
}

/// Judges one function from its sysfs directory and optional NVIDIA stats.
pub fn assess(bdf: &str, dev: &Path, nvidia: Option<NvidiaStats>) -> GpuHealth {
    let mut unhealthy = Vec::new();
    let mut degraded = Vec::new();
    if responds(dev) == Some(false) {
        unhealthy.push("device does not respond to config reads (fell off the bus)".to_string());
    }
    let aer = aer(dev);
    if let Some(a) = &aer {
        if a.fatal > 0 {
            unhealthy.push(format!("{} fatal PCIe AER errors", a.fatal));
        }
        if a.nonfatal > 0 {
            degraded.push(format!("{} non-fatal PCIe AER errors", a.nonfatal));
        }
    }
    let width = |f: &str| read_trim(&dev.join(f)).and_then(|s| s.parse::<u32>().ok());
    let link = PcieLink {
        current_speed: read_trim(&dev.join("current_link_speed")),
        max_speed: read_trim(&dev.join("max_link_speed")),
        current_width: width("current_link_width"),
        max_width: width("max_link_width"),
    };
    // Link speed drops at idle by design; a narrower link does not.
    if let (Some(cur), Some(max)) = (link.current_width, link.max_width)
        && cur > 0
        && cur < max
    {
        degraded.push(format!("PCIe link runs x{cur} of x{max}"));
    }
    if let Some(n) = &nvidia {
        if let Some(e) = n.ecc_uncorrected.filter(|e| *e > 0) {
            unhealthy.push(format!("{e} uncorrected ECC errors"));
        }
        if n.retired_pages_pending == Some(true) {
            degraded.push("retired memory pages pending a GPU reset".into());
        }
        if let Some(t) = n.temperature_c.filter(|t| *t >= HOT_CELSIUS) {
            degraded.push(format!("temperature {t}C"));
        }
    }
    let status = if !unhealthy.is_empty() {
        HealthStatus::Unhealthy
    } else if !degraded.is_empty() {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    };
    unhealthy.extend(degraded);
    GpuHealth {
        bdf: bdf.to_string(),
        status,
        reasons: unhealthy,
        aer,
        link,
        nvidia,
    }
}

/// Every GPU under `root` (`/` in production), judged with `nvidia`.
pub fn discover(root: &Path, nvidia: &BTreeMap<String, NvidiaStats>) -> Vec<GpuHealth> {
    let Ok(entries) = fs::read_dir(root.join("sys/bus/pci/devices")) else {
        return vec![];
    };
    let mut out: Vec<GpuHealth> = entries
        .flatten()
        .filter_map(|e| {
            let bdf = e.file_name().to_string_lossy().to_ascii_lowercase();
            let dev = e.path();
            (valid_pci_bdf(&bdf) && read_trim(&dev.join("class")).is_some_and(|c| is_gpu_class(&c)))
                .then(|| assess(&bdf, &dev, nvidia.get(&bdf).cloned()))
        })
        .collect();
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    out
}

pub const NVIDIA_SMI_QUERY: &str = "pci.bus_id,memory.total,memory.used,temperature.gpu,ecc.errors.uncorrected.volatile.total,mig.mode.current,retired_pages.pending";

/// Parses `nvidia-smi --query-gpu=<NVIDIA_SMI_QUERY> --format=csv,noheader,nounits`.
/// Keys are sysfs-style BDFs (`0000:01:00.0`).
pub fn parse_nvidia_smi(out: &str) -> BTreeMap<String, NvidiaStats> {
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 7 {
            continue;
        }
        // nvidia-smi prints an 8-digit domain: 00000000:01:00.0.
        let bus = f[0].to_ascii_lowercase();
        let Some(bdf) = bus
            .get(bus.len().saturating_sub(12)..)
            .filter(|b| valid_pci_bdf(b))
        else {
            continue;
        };
        let num = |s: &str| s.parse::<u64>().ok();
        map.insert(
            bdf.to_string(),
            NvidiaStats {
                memory_total_mib: num(f[1]),
                memory_used_mib: num(f[2]),
                temperature_c: f[3].parse().ok(),
                ecc_uncorrected: num(f[4]),
                mig_mode: (!f[5].starts_with('[') && !f[5].is_empty()).then(|| f[5].to_string()),
                retired_pages_pending: match f[6] {
                    "Yes" => Some(true),
                    "No" => Some(false),
                    _ => None,
                },
            },
        );
    }
    map
}

/// Runs `nvidia-smi` when it is installed; empty otherwise or on failure.
pub fn nvidia_smi() -> BTreeMap<String, NvidiaStats> {
    let child = Command::new("nvidia-smi")
        .arg(format!("--query-gpu={NVIDIA_SMI_QUERY}"))
        .arg("--format=csv,noheader,nounits")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return BTreeMap::new();
    };
    let deadline = Instant::now() + NVIDIA_SMI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return BTreeMap::new(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return BTreeMap::new();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    parse_nvidia_smi(&out)
}

/// The live host's GPUs.
pub fn discover_host() -> Vec<GpuHealth> {
    discover(Path::new("/"), &nvidia_smi())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, data: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), data).unwrap();
    }

    fn gpu(root: &Path, bdf: &str) -> std::path::PathBuf {
        let d = root.join("sys/bus/pci/devices").join(bdf);
        write(&d, "class", b"0x030200\n");
        write(&d, "config", &[0xde, 0x10, 0x30, 0x23]);
        write(&d, "current_link_width", b"16\n");
        write(&d, "max_link_width", b"16\n");
        write(&d, "current_link_speed", b"2.5 GT/s PCIe\n");
        write(&d, "max_link_speed", b"16.0 GT/s PCIe\n");
        d
    }

    fn aer_file(total_key: &str, n: u64) -> Vec<u8> {
        format!("Undefined 0\nDLP 0\n{total_key} {n}\n").into_bytes()
    }

    #[test]
    fn an_idle_link_at_low_speed_is_healthy() {
        let t = tempfile::tempdir().unwrap();
        let d = gpu(t.path(), "0000:01:00.0");
        write(&d, "aer_dev_correctable", &aer_file("TOTAL_ERR_COR", 3));
        write(&d, "aer_dev_nonfatal", &aer_file("TOTAL_ERR_NONFATAL", 0));
        write(&d, "aer_dev_fatal", &aer_file("TOTAL_ERR_FATAL", 0));
        let h = assess("0000:01:00.0", &d, None);
        assert_eq!(h.status, HealthStatus::Healthy, "{:?}", h.reasons);
        assert_eq!(
            h.aer,
            Some(AerCounts {
                correctable: 3,
                nonfatal: 0,
                fatal: 0
            })
        );
        assert_eq!(h.link.current_speed.as_deref(), Some("2.5 GT/s PCIe"));
    }

    #[test]
    fn narrow_link_and_nonfatal_errors_degrade() {
        let t = tempfile::tempdir().unwrap();
        let d = gpu(t.path(), "0000:01:00.0");
        write(&d, "current_link_width", b"8\n");
        write(&d, "aer_dev_nonfatal", &aer_file("TOTAL_ERR_NONFATAL", 2));
        let h = assess("0000:01:00.0", &d, None);
        assert_eq!(h.status, HealthStatus::Degraded);
        assert_eq!(
            h.reasons,
            ["2 non-fatal PCIe AER errors", "PCIe link runs x8 of x16"]
        );
    }

    #[test]
    fn fatal_errors_a_dead_device_or_ecc_make_it_unhealthy() {
        let t = tempfile::tempdir().unwrap();
        let d = gpu(t.path(), "0000:01:00.0");
        write(&d, "aer_dev_fatal", &aer_file("TOTAL_ERR_FATAL", 1));
        assert_eq!(
            assess("0000:01:00.0", &d, None).status,
            HealthStatus::Unhealthy
        );

        let d2 = gpu(t.path(), "0000:02:00.0");
        write(&d2, "config", &[0xff, 0xff, 0xff, 0xff]);
        let h = assess("0000:02:00.0", &d2, None);
        assert_eq!(h.status, HealthStatus::Unhealthy);
        assert!(h.reasons[0].contains("fell off the bus"));

        let d3 = gpu(t.path(), "0000:03:00.0");
        let nv = NvidiaStats {
            ecc_uncorrected: Some(4),
            temperature_c: Some(95),
            ..Default::default()
        };
        let h = assess("0000:03:00.0", &d3, Some(nv));
        assert_eq!(h.status, HealthStatus::Unhealthy);
        assert_eq!(h.reasons, ["4 uncorrected ECC errors", "temperature 95C"]);
    }

    #[test]
    fn discover_only_reports_display_class_functions() {
        let t = tempfile::tempdir().unwrap();
        gpu(t.path(), "0000:01:00.0");
        let nic = t.path().join("sys/bus/pci/devices/0000:02:00.0");
        write(&nic, "class", b"0x020000\n");
        let mut nv = BTreeMap::new();
        nv.insert(
            "0000:01:00.0".to_string(),
            NvidiaStats {
                memory_total_mib: Some(81920),
                ..Default::default()
            },
        );
        let all = discover(t.path(), &nv);
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].nvidia.as_ref().unwrap().memory_total_mib,
            Some(81920)
        );
    }

    #[test]
    fn parses_nvidia_smi_csv() {
        let out = "00000000:01:00.0, 81920, 1024, 41, 0, Enabled, No\n\
                   00000000:81:00.0, 81920, [N/A], [N/A], [N/A], [N/A], [N/A]\n\
                   garbage\n";
        let m = parse_nvidia_smi(out);
        assert_eq!(m.len(), 2);
        let a = &m["0000:01:00.0"];
        assert_eq!(
            a,
            &NvidiaStats {
                memory_total_mib: Some(81920),
                memory_used_mib: Some(1024),
                temperature_c: Some(41),
                ecc_uncorrected: Some(0),
                mig_mode: Some("Enabled".into()),
                retired_pages_pending: Some(false),
            }
        );
        let b = &m["0000:81:00.0"];
        assert_eq!(
            (
                b.memory_used_mib,
                b.mig_mode.as_deref(),
                b.retired_pages_pending
            ),
            (None, None, None)
        );
    }
}
