//! Windows guests: a blank disk for the installer to write to.

use anyhow::{Context, Result, bail};
use fluxvm_core::model::{AppleGuest, CreateVmRequest};
use std::fs;
use std::path::Path;

/// Windows 11 asks for 64 GiB; the installer refuses less than this.
pub const MIN_INSTALL_DISK_GIB: u64 = 32;
pub const DEFAULT_INSTALL_DISK_GIB: u64 = 64;

/// A Windows guest with an installer ISO (`apple.media`) starts from a blank sparse disk instead of a cloned image.
pub fn is_windows_install(req: &CreateVmRequest) -> bool {
    req.apple
        .as_ref()
        .is_some_and(|a| a.guest_os == AppleGuest::Windows && a.media.is_some())
}

/// Admission checks for a Windows install.
pub fn validate(req: &CreateVmRequest) -> Result<()> {
    if !is_windows_install(req) {
        return Ok(());
    }
    let media = req.apple.as_ref().and_then(|a| a.media.as_ref());
    if let Some(m) = media {
        let s = m.to_string_lossy();
        if s.starts_with("http://") || s.starts_with("https://") {
            bail!(
                "a Windows install needs a local ISO path; download it first (URLs are not fetched)"
            );
        }
        if !m.is_absolute() {
            bail!("a Windows install needs an absolute ISO path");
        }
    }
    if req.disk_size_gib.is_some_and(|g| g < MIN_INSTALL_DISK_GIB) {
        bail!("a Windows install needs disk_size_gib of at least {MIN_INSTALL_DISK_GIB}");
    }
    Ok(())
}

/// Creates the sparse root disk the Windows installer writes to.
pub fn create_install_disk(disk: &Path, size_gib: Option<u64>) -> Result<()> {
    let gib = size_gib.unwrap_or(DEFAULT_INSTALL_DISK_GIB);
    if gib < MIN_INSTALL_DISK_GIB {
        bail!("a Windows install needs disk_size_gib of at least {MIN_INSTALL_DISK_GIB}");
    }
    let f = fs::File::create(disk).with_context(|| format!("creating {}", disk.display()))?;
    f.set_len(gib * 1024 * 1024 * 1024)
        .with_context(|| format!("sizing {}", disk.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(apple: &str, disk: Option<u64>) -> CreateVmRequest {
        let mut r: CreateVmRequest = serde_json::from_str(&format!(
            r#"{{"name":"w","image":"/iso/w.iso","backend":"vz","apple":{apple}}}"#
        ))
        .unwrap();
        r.disk_size_gib = disk;
        r
    }

    #[test]
    fn install_means_windows_with_media() {
        assert!(is_windows_install(&req(
            r#"{"guest_os":"windows","media":"/iso/w.iso"}"#,
            None
        )));
        assert!(!is_windows_install(&req(r#"{"guest_os":"windows"}"#, None)));
        assert!(!is_windows_install(&req(
            r#"{"guest_os":"linux","media":"/iso/w.iso"}"#,
            None
        )));
    }

    #[test]
    fn admission() {
        let ok = r#"{"guest_os":"windows","media":"/iso/w.iso"}"#;
        assert!(validate(&req(ok, Some(64))).is_ok());
        assert!(validate(&req(ok, Some(8))).is_err());
        assert!(validate(&req(r#"{"guest_os":"windows","media":"w.iso"}"#, None)).is_err());
        assert!(
            validate(&req(
                r#"{"guest_os":"windows","media":"https://x/w.iso"}"#,
                None
            ))
            .is_err()
        );
    }

    #[test]
    fn blank_disk_is_sparse_and_sized() {
        let dir = std::env::temp_dir().join(format!("fluxvm-win-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let d = dir.join("disk.raw");
        create_install_disk(&d, Some(40)).unwrap();
        assert_eq!(fs::metadata(&d).unwrap().len(), 40 << 30);
        assert!(create_install_disk(&d, Some(1)).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
