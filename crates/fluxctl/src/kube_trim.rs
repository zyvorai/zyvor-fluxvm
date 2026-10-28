//! `fluxctl kube trim`: give the space the cluster has freed back to the Mac. Deleting images and data inside the VM
//! leaves its disk file as large as it ever was unless the guest tells the host which blocks are unused (TRIM).
//! Whether the host honors that depends on how the disk is attached, so trim first asks the guest's block device if it
//! accepts discards and refuses with that reason when it does not, instead of running `fstrim` for nothing.

use anyhow::{Result, bail};

use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

/// Prints the root device's discard limit in bytes (`0` means discards are not passed through), or `none` when the
/// device cannot be found.
const PROBE: &str = r#"src=$(findmnt -no SOURCE / 2>/dev/null); [ -n "$src" ] && lsblk -nb -o DISC-MAX "$src" 2>/dev/null | head -n1 | tr -d ' ' || echo none"#;

/// Whether the probe output says the root device accepts discards.
pub(crate) fn discard_supported(probe: &str) -> Result<bool> {
    let word = probe.trim();
    if word.is_empty() || word == "none" {
        bail!("could not find the VM's root block device, so trim cannot tell whether discards work");
    }
    let max: u64 = word
        .parse()
        .map_err(|_| anyhow::anyhow!("unexpected answer from the VM's block device: {word}"))?;
    Ok(max > 0)
}

/// Total bytes `fstrim -v` reports across its lines, e.g. `/: 1.2 GiB (1288490188 bytes) trimmed on /dev/vda1`.
pub(crate) fn trimmed_bytes(out: &str) -> u64 {
    out.lines()
        .filter_map(|l| {
            let open = l.find('(')? + 1;
            let rest = &l[open..];
            rest[..rest.find(" bytes)")?].parse::<u64>().ok()
        })
        .sum()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub async fn run<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    if !discard_supported(&kube_host::guest_sh(api, o, PROBE).await?)? {
        bail!(
            "the VM's disk does not pass discards through to the Mac (its discard limit is 0), so trimming would free \
             nothing on the host; the disk file keeps its size until the cluster is recreated"
        );
    }
    let out = kube_host::guest_sh(api, o, "sudo -n fstrim -av 2>&1").await?;
    let bytes = trimmed_bytes(&out);
    Ok(format!(
        "told the host {} of unused space in {} (APFS gives it back as the disk file shrinks)\n{}",
        human(bytes),
        o.name,
        out.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_limit_decides() {
        assert!(discard_supported("2147450880\n").unwrap());
        assert!(!discard_supported("0\n").unwrap());
        assert!(discard_supported("none").is_err());
        assert!(discard_supported("").is_err());
        assert!(discard_supported("garbage").is_err());
    }

    #[test]
    fn fstrim_output_is_summed() {
        let out = "/boot: 100 MiB (104857600 bytes) trimmed on /dev/vda15\n/: 1.2 GiB (1288490188 bytes) trimmed on /dev/vda1\n";
        assert_eq!(trimmed_bytes(out), 104857600 + 1288490188);
        assert_eq!(trimmed_bytes("fstrim: /: the discard operation is not supported\n"), 0);
    }

    #[test]
    fn sizes_read_plainly() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1288490188), "1.2 GiB");
    }
}
