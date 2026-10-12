// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Where FluxVM's disk space goes (`GET /v1/host/storage`): the volume's free space and, per VM, per branch and for the
//! image cache, what is *allocated* (the blocks a file really occupies, so a sparse 3 GiB disk that holds 700 MiB counts
//! 700 MiB) next to its logical size.
//!
//! One honest limit: copy-on-write clones (APFS `clonefile`, which fork, branches and `vm clone` use) share blocks, and
//! `st_blocks` of each clone reports the shared blocks in full. The per-item figures are therefore upper bounds that
//! can add up to more than the volume holds; the free-space number is the truth about what remains.

use fluxvm_core::model::VmRecord;
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use uuid::Uuid;

/// Bytes a path occupies and how many files it holds.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        self.allocated_bytes = self.allocated_bytes.saturating_add(o.allocated_bytes);
        self.logical_bytes = self.logical_bytes.saturating_add(o.logical_bytes);
        self.files = self.files.saturating_add(o.files);
    }
}

/// Usage of a file, or of everything under a directory. Symlinks are not followed (they would count something that is
/// not ours); unreadable entries are skipped rather than failing the report.
pub fn usage_of(path: &Path) -> Usage {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return Usage::default();
    };
    if md.file_type().is_symlink() {
        return Usage::default();
    }
    if md.is_file() {
        return Usage {
            allocated_bytes: md.blocks().saturating_mul(512),
            logical_bytes: md.len(),
            files: 1,
        };
    }
    let mut total = Usage::default();
    if md.is_dir()
        && let Ok(rd) = std::fs::read_dir(path)
    {
        for e in rd.filter_map(|e| e.ok()) {
            total.add(usage_of(&e.path()));
        }
    }
    total
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Volume {
    pub path: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VmStorage {
    pub vm_id: Uuid,
    pub name: String,
    pub status: String,
    /// The VM's root disk.
    pub disk: Usage,
    /// Everything else in its workspace: snapshots, saved state, seed, logs.
    pub other: Usage,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BranchStorage {
    pub id: Uuid,
    pub name: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageReport {
    pub volume: Volume,
    pub vms: Vec<VmStorage>,
    pub branches: Vec<BranchStorage>,
    /// `<state_dir>/images`: downloaded and imported base images.
    pub images: Usage,
    /// Everything under the state directory.
    pub state_dir: Usage,
    /// Why the per-item figures can exceed what the volume holds.
    pub note: &'static str,
}

pub(crate) fn volume(path: &Path) -> Volume {
    let mut total = None;
    let mut free = None;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        // SAFETY: a zeroed statvfs is a valid out parameter and `c` outlives the call.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } == 0 {
            let fr = vfs.f_frsize as u64;
            total = Some((vfs.f_blocks as u64).saturating_mul(fr));
            free = Some((vfs.f_bavail as u64).saturating_mul(fr));
        }
    }
    Volume {
        path: path.display().to_string(),
        total_bytes: total,
        free_bytes: free,
    }
}

/// Builds the report (blocking: it walks the state directory).
pub fn build(state_dir: &Path, vms: &[VmRecord], branches: &[(Uuid, String)]) -> StorageReport {
    let mut vm_rows: Vec<VmStorage> = vms
        .iter()
        .map(|v| {
            let disk = usage_of(&v.disk);
            let mut all = usage_of(&v.workspace);
            // The root disk lives inside the workspace; report the rest apart from it.
            if v.disk.starts_with(&v.workspace) {
                all.allocated_bytes = all.allocated_bytes.saturating_sub(disk.allocated_bytes);
                all.logical_bytes = all.logical_bytes.saturating_sub(disk.logical_bytes);
                all.files = all.files.saturating_sub(disk.files);
            }
            VmStorage {
                vm_id: v.id,
                name: v.name.clone(),
                status: format!("{:?}", v.status).to_lowercase(),
                disk,
                other: all,
            }
        })
        .collect();
    vm_rows.sort_by(|a, b| {
        (b.disk.allocated_bytes + b.other.allocated_bytes)
            .cmp(&(a.disk.allocated_bytes + a.other.allocated_bytes))
    });
    let mut branch_rows: Vec<BranchStorage> = branches
        .iter()
        .map(|(id, name)| BranchStorage {
            id: *id,
            name: name.clone(),
            usage: usage_of(&state_dir.join("branches").join(id.to_string())),
        })
        .collect();
    branch_rows.sort_by(|a, b| b.usage.allocated_bytes.cmp(&a.usage.allocated_bytes));
    StorageReport {
        volume: volume(state_dir),
        vms: vm_rows,
        branches: branch_rows,
        images: usage_of(&state_dir.join("images")),
        state_dir: usage_of(state_dir),
        note: "allocated bytes count blocks shared with copy-on-write clones in every clone, so the parts can add up to more than the volume holds; free space is what is really left",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_sparse_file_counts_what_it_occupies_not_its_length() {
        let d = tempfile::tempdir().unwrap();
        let sparse = d.path().join("sparse.raw");
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(256 << 20).unwrap();
        let u = usage_of(&sparse);
        assert_eq!(u.logical_bytes, 256 << 20);
        assert!(
            u.allocated_bytes < (8 << 20),
            "allocated {}",
            u.allocated_bytes
        );
        assert_eq!(u.files, 1);

        let dense = d.path().join("dense.bin");
        let mut f = std::fs::File::create(&dense).unwrap();
        f.write_all(&vec![7u8; 4 << 20]).unwrap();
        f.sync_all().unwrap();
        let u = usage_of(&dense);
        assert!(
            u.allocated_bytes >= (4 << 20),
            "allocated {}",
            u.allocated_bytes
        );
    }

    #[test]
    fn a_directory_sums_its_files_and_ignores_symlinks_and_missing_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/one"), vec![1u8; 10_000]).unwrap();
        std::fs::write(d.path().join("a/b/two"), vec![2u8; 20_000]).unwrap();
        std::os::unix::fs::symlink("/etc", d.path().join("a/link")).unwrap();
        let u = usage_of(&d.path().join("a"));
        assert_eq!(u.files, 2);
        assert_eq!(u.logical_bytes, 30_000);
        assert_eq!(usage_of(&d.path().join("nope")), Usage::default());
    }

    #[test]
    fn the_report_has_a_volume_and_lists_branches_biggest_first() {
        let d = tempfile::tempdir().unwrap();
        let small = Uuid::new_v4();
        let big = Uuid::new_v4();
        for (id, n) in [(small, 1_000usize), (big, 400_000)] {
            let p = d.path().join("branches").join(id.to_string());
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("state"), vec![9u8; n]).unwrap();
        }
        let r = build(
            d.path(),
            &[],
            &[(small, "small".into()), (big, "big".into())],
        );
        assert!(r.volume.total_bytes.unwrap_or(0) > 0);
        assert_eq!(r.branches[0].name, "big");
        assert!(r.branches[0].usage.allocated_bytes >= 400_000);
        assert!(r.state_dir.files >= 2);
        assert_eq!(r.images, Usage::default());
    }
}
