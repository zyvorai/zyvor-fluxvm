// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Native offline file injection into raw ext4 disk images.
//!
//! No mount, no loop/nbd device, no helper VM, no libguestfs: the raw image
//! file is parsed (GPT/MBR), the Linux root ext4 filesystem is located, and
//! small files are written directly into free blocks/inodes with all
//! bitmaps, group descriptors, superblock counters and metadata checksums
//! kept coherent. Works on macOS (and anywhere else).
//!
//! The image must be **raw**, **not attached to a running VM**, and the
//! filesystem must be cleanly unmounted (no journal replay pending). Anything
//! the writer does not fully understand is refused with
//! [`Error::Unsupported`] / [`Error::Corrupt`] *before* any byte is written:
//! all changes are staged in memory and flushed at the end.
//!
//! Known limits: see the notes on [`inject_files`].

mod crc;
mod ext4;
mod partition;

use std::fmt;
use std::fs::OpenOptions;
use std::path::Path;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Caller error (bad path, bad hostname, ...).
    Invalid(String),
    /// Valid input that this injector deliberately does not handle.
    Unsupported(String),
    /// On-disk structures are inconsistent.
    Corrupt(String),
    NoSpace(String),
    /// No ext4 root filesystem could be selected.
    NoRoot(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Invalid(s) => write!(f, "invalid input: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Corrupt(s) => write!(f, "filesystem inconsistent: {s}"),
            Error::NoSpace(s) => write!(f, "no space: {s}"),
            Error::NoRoot(s) => write!(f, "no root filesystem: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// One file to place inside the guest root filesystem.
#[derive(Debug, Clone)]
pub struct InjectFile {
    /// Absolute guest path. Missing parent directories are created.
    pub path: String,
    pub contents: Vec<u8>,
    /// Permission bits (e.g. `0o600`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Instead of replacing, append `contents` as a line unless an identical
    /// line is already present (idempotent; `authorized_keys` semantics).
    pub append_line: bool,
    /// Mode for parent directories this call has to create.
    pub dir_mode: u32,
    /// Also force `dir_mode` on the immediate parent when it already exists
    /// (`/root/.ssh` must be 0700).
    pub enforce_parent_mode: bool,
}

impl InjectFile {
    /// Regular file, mode 0644, owner root, parents 0755.
    pub fn new(path: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        InjectFile {
            path: path.into(),
            contents: contents.into(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            append_line: false,
            dir_mode: 0o755,
            enforce_parent_mode: false,
        }
    }
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }
    pub fn append_line(mut self, yes: bool) -> Self {
        self.append_line = yes;
        self
    }
    pub fn parent_mode(mut self, mode: u32, enforce_existing: bool) -> Self {
        self.dir_mode = mode;
        self.enforce_parent_mode = enforce_existing;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenFile {
    pub path: String,
    pub inode: u32,
    pub created: bool,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// 1-based partition number of the filesystem used (0 = bare filesystem).
    pub partition_index: u32,
    pub partition_offset: u64,
    pub files: Vec<WrittenFile>,
    pub dirs_created: Vec<String>,
    pub inodes_allocated: u32,
    pub blocks_allocated: u64,
    pub blocks_freed: u64,
}

fn validate_path(p: &str) -> Result<()> {
    if !p.starts_with('/') || p.len() < 2 || p.ends_with('/') {
        return Err(Error::Invalid(format!(
            "path {p:?} must be absolute and name a file"
        )));
    }
    for c in p.split('/').skip(1) {
        if c.is_empty() || c == "." || c == ".." || c.len() > 255 || c.contains('\0') {
            return Err(Error::Invalid(format!(
                "path {p:?} has a bad component {c:?}"
            )));
        }
    }
    Ok(())
}

/// Writes `files` into the root ext4 filesystem of the raw disk image `disk`.
///
/// Behaviour and limits:
/// * raw images only (qcow2 is refused); GPT, MBR or a bare filesystem;
/// * root partition = the ext4 partition containing `/etc` (ranked by
///   Linux-root GPT type, then presence of `/etc/os-release`); ambiguous
///   choices are refused;
/// * ext4 with extents + filetype and either `metadata_csum` or no group
///   checksums (legacy `uninit_bg` crc16 is refused); 1/2/4 KiB blocks;
/// * refused: needs_recovery / unreplayed journal, non-empty orphan list or
///   orphan file, quota, bigalloc, inline_data, encryption, casefold,
///   meta_bg, MMP, external journal, mounted/errored state flag;
/// * space comes only from groups that are not `BLOCK_UNINIT`/`INODE_UNINIT`;
/// * files up to 16 MiB, in at most four inline extents; directories are
///   grown only while their extent tree is inline; indexed (htree)
///   directories are demoted to linear directories when an entry is added;
/// * no SELinux labels / xattrs are written (guests that enforce SELinux on
///   the target path need a relabel at boot);
/// * backup superblocks/descriptors are not updated (as the kernel does not).
///
/// On any error nothing is written to the image.
pub fn inject_files(disk: &Path, files: &[InjectFile]) -> Result<Report> {
    if files.is_empty() {
        return Err(Error::Invalid("no files to inject".into()));
    }
    for f in files {
        validate_path(&f.path)?;
    }
    let file = OpenOptions::new().read(true).write(true).open(disk)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::Invalid(format!(
            "{} is not a regular file",
            disk.display()
        )));
    }
    let cands = partition::candidates(&file, meta.len())?;

    struct Found {
        fs: ext4::Fs,
        cand: partition::Candidate,
        score: u32,
    }
    let mut found: Vec<Found> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for c in cands {
        match ext4::Fs::open(file.try_clone()?, &c) {
            Ok(mut fs) => {
                if !fs.dir_exists("/etc") {
                    errors.push(format!("partition {}: ext4 but no /etc", c.index));
                    continue;
                }
                let os = fs.exists("/etc/os-release") || fs.exists("/etc/fstab");
                let score = c.root_rank as u32 * 2 + os as u32;
                found.push(Found { fs, cand: c, score });
            }
            Err(e) => errors.push(format!("partition {}: {e}", c.index)),
        }
    }
    if found.is_empty() {
        return Err(Error::NoRoot(errors.join("; ")));
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.score));
    if found.len() > 1 && found[0].score == found[1].score {
        return Err(Error::NoRoot(format!(
            "ambiguous: partitions {} and {} both look like a root filesystem",
            found[0].cand.index, found[1].cand.index
        )));
    }
    let Found { mut fs, cand, .. } = found.swap_remove(0);

    let mut report = Report {
        partition_index: cand.index,
        partition_offset: cand.offset,
        ..Report::default()
    };
    for f in files {
        let data: Vec<u8> = if f.append_line {
            let line = String::from_utf8_lossy(&f.contents).trim().to_string();
            match fs.read_file(&f.path)? {
                Some(existing) => {
                    let text = String::from_utf8_lossy(&existing).to_string();
                    if text.lines().any(|l| l.trim() == line) {
                        existing
                    } else {
                        let mut t = text;
                        if !t.is_empty() && !t.ends_with('\n') {
                            t.push('\n');
                        }
                        t.push_str(&line);
                        t.push('\n');
                        t.into_bytes()
                    }
                }
                None => format!("{line}\n").into_bytes(),
            }
        } else {
            f.contents.clone()
        };
        let (ino, created) = fs.put_file(
            &f.path,
            &data,
            f.mode,
            f.uid,
            f.gid,
            f.dir_mode,
            f.enforce_parent_mode,
            &mut report.dirs_created,
        )?;
        report.files.push(WrittenFile {
            path: f.path.clone(),
            inode: ino,
            created,
            bytes: data.len(),
        });
    }
    report.inodes_allocated = fs.stats.inodes_allocated;
    report.blocks_allocated = fs.stats.blocks_allocated;
    report.blocks_freed = fs.stats.blocks_freed;
    fs.commit()?;
    Ok(report)
}

/// Adds `key` (an OpenSSH public key line) to `/root/.ssh/authorized_keys`
/// (dir 0700, file 0600, root-owned; idempotent). Mirrors the Linux-host
/// injector in `fluxvm-image`.
pub fn inject_ssh_key(disk: &Path, key: &str) -> Result<Report> {
    let key = key.trim();
    if key.is_empty() || key.contains('\n') {
        return Err(Error::Invalid(
            "ssh key must be a single non-empty line".into(),
        ));
    }
    let f = InjectFile::new("/root/.ssh/authorized_keys", key.as_bytes())
        .mode(0o600)
        .append_line(true)
        .parent_mode(0o700, true);
    inject_files(disk, &[f])
}

/// Writes the guest-agent token to
/// [`fluxvm_guest_protocol::TOKEN_FILE_PATH`], mode 0600, root-owned.
pub fn inject_agent_token(disk: &Path, token: &str) -> Result<Report> {
    if token.is_empty() {
        return Err(Error::Invalid("empty token".into()));
    }
    let f = InjectFile::new(fluxvm_guest_protocol::TOKEN_FILE_PATH, token.as_bytes()).mode(0o600);
    inject_files(disk, &[f])
}

/// Writes `/etc/hostname` (mode 0644).
pub fn set_hostname(disk: &Path, hostname: &str) -> Result<Report> {
    let ok = !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !ok {
        return Err(Error::Invalid(format!("invalid hostname {hostname:?}")));
    }
    let f = InjectFile::new("/etc/hostname", format!("{hostname}\n").into_bytes());
    inject_files(disk, &[f])
}

#[cfg(test)]
mod tests;
