// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use crate::error::Result;
use crate::util::{read_u64, read_u64_or_max, write_cgroup_file};

/// Controller for the pids cgroup v2 subsystem.
pub struct PidsController {
    path: PathBuf,
}

impl PidsController {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read pids.max (u64::MAX = "max" = unlimited).
    pub fn get_max(&self) -> Result<u64> {
        read_u64_or_max(&self.path.join("pids.max"))
    }

    /// Set pids.max (u64::MAX writes "max").
    pub fn set_max(&self, max: u64) -> Result<()> {
        let file = self.path.join("pids.max");
        let value = if max == u64::MAX {
            "max".to_string()
        } else {
            max.to_string()
        };
        write_cgroup_file(&file, &value)
    }

    /// Read pids.current.
    pub fn get_current(&self) -> Result<u64> {
        read_u64(&self.path.join("pids.current"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_cgroup() -> (tempfile::TempDir, PidsController) {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("pids.max"), "").unwrap();
        fs::write(dir.path().join("pids.current"), "").unwrap();
        let ctl = PidsController::new(dir.path().to_path_buf());
        (dir, ctl)
    }

    #[test]
    fn get_max_maps_max_keyword_to_unlimited() {
        let (dir, ctl) = fake_cgroup();
        fs::write(dir.path().join("pids.max"), "max\n").unwrap();
        assert_eq!(ctl.get_max().unwrap(), u64::MAX);
    }

    #[test]
    fn set_max_unlimited_writes_max_keyword() {
        let (dir, ctl) = fake_cgroup();
        ctl.set_max(u64::MAX).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("pids.max"))
                .unwrap()
                .trim(),
            "max"
        );
    }

    #[test]
    fn set_max_writes_decimal_and_zero_is_a_limit() {
        let (dir, ctl) = fake_cgroup();
        ctl.set_max(512).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("pids.max"))
                .unwrap()
                .trim(),
            "512"
        );
        ctl.set_max(0).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("pids.max"))
                .unwrap()
                .trim(),
            "0"
        );
    }

    #[test]
    fn get_current_reads_counter_and_rejects_max() {
        let (dir, ctl) = fake_cgroup();
        fs::write(dir.path().join("pids.current"), "17\n").unwrap();
        assert_eq!(ctl.get_current().unwrap(), 17);
        fs::write(dir.path().join("pids.current"), "max\n").unwrap();
        assert!(ctl.get_current().is_err());
    }

    #[test]
    fn missing_files_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = PidsController::new(dir.path().join("nope"));
        assert!(ctl.get_current().is_err());
        assert!(ctl.get_max().is_err());
    }
}
