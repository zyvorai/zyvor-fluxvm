// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Integration-style tests. They build real ext4 images with `mke2fs` and
//! verify results with `e2fsck -fn` and `debugfs` (e2fsprogs). If e2fsprogs
//! is not installed the tests print a notice and pass vacuously; the
//! partition/CRC unit tests do not need it.

use super::*;
use std::path::PathBuf;
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("FLUXVM_E2FSPROGS_DIR") {
        dirs.push(d.into());
    }
    for d in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
        "/opt/homebrew/sbin",
        "/usr/local/sbin",
        "/sbin",
        "/usr/sbin",
        "/usr/bin",
    ] {
        dirs.push(d.into());
    }
    if let Ok(p) = std::env::var("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

macro_rules! need_tools {
    () => {
        if tool("mke2fs").is_none() || tool("e2fsck").is_none() || tool("debugfs").is_none() {
            eprintln!("SKIP: e2fsprogs (mke2fs/e2fsck/debugfs) not found");
            return;
        }
    };
}

fn mke2fs(img: &Path, size: &str, args: &[&str], populate: Option<&Path>) {
    let mut c = Command::new(tool("mke2fs").unwrap());
    c.args(["-t", "ext4", "-F", "-q"]);
    c.args(args);
    if let Some(p) = populate {
        c.arg("-d").arg(p);
    }
    c.arg(img).arg(size);
    let o = c.output().unwrap();
    assert!(
        o.status.success(),
        "mke2fs failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn e2fsck(img: &Path) -> (bool, String) {
    let o = Command::new(tool("e2fsck").unwrap())
        .args(["-f", "-n"])
        .arg(img)
        .output()
        .unwrap();
    let s = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.success(), s)
}

fn assert_fsck_clean(img: &Path) {
    let (ok, out) = e2fsck(img);
    assert!(ok, "e2fsck -fn reported problems:\n{out}");
}

fn debugfs(img: &Path, cmd: &str) -> Vec<u8> {
    Command::new(tool("debugfs").unwrap())
        .args(["-R", cmd])
        .arg(img)
        .output()
        .unwrap()
        .stdout
}

fn cat(img: &Path, path: &str) -> Vec<u8> {
    debugfs(img, &format!("cat {path}"))
}

fn stat(img: &Path, path: &str) -> String {
    String::from_utf8_lossy(&debugfs(img, &format!("stat {path}"))).to_string()
}

/// Populated rootfs-ish tree.
fn populate(dir: &Path, etc_files: usize) {
    use std::fs;
    fs::create_dir_all(dir.join("etc")).unwrap();
    fs::create_dir_all(dir.join("root")).unwrap();
    fs::create_dir_all(dir.join("usr/bin")).unwrap();
    fs::write(dir.join("etc/hostname"), "old-host-name-that-is-longer\n").unwrap();
    fs::write(dir.join("etc/os-release"), "ID=testos\n").unwrap();
    fs::write(dir.join("etc/fstab"), "# fstab\n").unwrap();
    for i in 0..etc_files {
        fs::write(
            dir.join(format!(
                "etc/some-reasonably-long-config-file-name-{i:05}.conf"
            )),
            format!("k={i}\n"),
        )
        .unwrap();
    }
}

fn sha(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn setup(size: &str, args: &[&str], etc_files: usize) -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let tree = td.path().join("tree");
    populate(&tree, etc_files);
    let img = td.path().join("root.img");
    mke2fs(&img, size, args, Some(&tree));
    assert_fsck_clean(&img);
    (td, img)
}

fn full_scenario(img: &Path) {
    let rep = inject_agent_token(img, "s3cret-token").unwrap();
    assert!(rep.files[0].created);
    assert_fsck_clean(img);
    assert_eq!(
        cat(img, fluxvm_guest_protocol::TOKEN_FILE_PATH),
        b"s3cret-token"
    );
    let st = stat(img, fluxvm_guest_protocol::TOKEN_FILE_PATH);
    assert!(st.contains("Mode:  0600"), "{st}");
    assert!(st.contains("User:     0   Group:     0"), "{st}");

    inject_ssh_key(img, "ssh-ed25519 AAAAC3Nza test@host").unwrap();
    inject_ssh_key(img, "ssh-ed25519 AAAAC3Nza test@host").unwrap(); // idempotent
    inject_ssh_key(img, "ssh-ed25519 BBBB other@host").unwrap();
    assert_fsck_clean(img);
    assert_eq!(
        cat(img, "/root/.ssh/authorized_keys"),
        b"ssh-ed25519 AAAAC3Nza test@host\nssh-ed25519 BBBB other@host\n"
    );
    assert!(stat(img, "/root/.ssh").contains("Mode:  0700"));
    assert!(stat(img, "/root/.ssh/authorized_keys").contains("Mode:  0600"));

    // overwrite shorter, then much larger (multi-block), then shorter again
    set_hostname(img, "vm1").unwrap();
    assert_eq!(cat(img, "/etc/hostname"), b"vm1\n");
    assert_fsck_clean(img);
    let big = vec![b'x'; 300_000];
    let rep = inject_files(img, &[InjectFile::new("/etc/big.bin", big.clone())]).unwrap();
    assert!(rep.blocks_allocated > 0);
    assert_fsck_clean(img);
    assert_eq!(cat(img, "/etc/big.bin"), big);
    let rep = inject_files(img, &[InjectFile::new("/etc/big.bin", b"tiny".to_vec())]).unwrap();
    assert!(rep.blocks_freed > 0);
    assert_fsck_clean(img);
    assert_eq!(cat(img, "/etc/big.bin"), b"tiny");

    // deep new directories + empty file
    inject_files(
        img,
        &[
            InjectFile::new("/var/lib/fluxvm/state/x", Vec::new()),
            InjectFile::new("/opt/a/b/c/d.txt", b"d".to_vec())
                .owner(1000, 1000)
                .mode(0o640),
        ],
    )
    .unwrap();
    assert_fsck_clean(img);
    assert_eq!(cat(img, "/opt/a/b/c/d.txt"), b"d");
    let st = stat(img, "/opt/a/b/c/d.txt");
    assert!(st.contains("User:  1000   Group:  1000"), "{st}");
}

#[test]
fn default_ext4_4k() {
    need_tools!();
    let (_td, img) = setup("64M", &[], 20);
    full_scenario(&img);
}

#[test]
fn ext4_1k_blocks_no_csum_no_64bit() {
    need_tools!();
    let (_td, img) = setup(
        "32M",
        &[
            "-b",
            "1024",
            "-O",
            "^metadata_csum,^64bit,^orphan_file",
            "-I",
            "256",
        ],
        20,
    );
    full_scenario(&img);
}

#[test]
fn refuses_legacy_uninit_bg() {
    need_tools!();
    let td = tempfile::tempdir().unwrap();
    let tree = td.path().join("tree");
    populate(&tree, 2);
    let img = td.path().join("u.img");
    mke2fs(
        &img,
        "32M",
        &["-O", "^metadata_csum,uninit_bg,^orphan_file"],
        Some(&tree),
    );
    let before = sha(&img);
    let err = inject_agent_token(&img, "t").unwrap_err();
    assert!(
        matches!(err, Error::NoRoot(ref m) if m.contains("uninit_bg")),
        "{err}"
    );
    assert_eq!(before, sha(&img));
}

#[test]
fn ext4_1k_blocks_csum() {
    need_tools!();
    let (_td, img) = setup("32M", &["-b", "1024", "-O", "metadata_csum,64bit"], 20);
    full_scenario(&img);
}

#[test]
fn ext4_128_byte_inodes_csum() {
    need_tools!();
    let (_td, img) = setup("32M", &["-I", "128", "-O", "metadata_csum"], 5);
    full_scenario(&img);
}

#[test]
fn multi_group_lazy_init() {
    need_tools!();
    let (_td, img) = setup(
        "512M",
        &[
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1",
            "-O",
            "metadata_csum,64bit,flex_bg",
        ],
        50,
    );
    full_scenario(&img);
}

#[test]
fn htree_directory_insertions() {
    need_tools!();
    // many entries => multi-block directory; e2fsck -D turns it into an htree.
    let (_td, img) = setup("64M", &[], 400);
    let o = Command::new(tool("e2fsck").unwrap())
        .args(["-f", "-y", "-D"])
        .arg(&img)
        .output()
        .unwrap();
    assert!(o.status.code().unwrap_or(9) <= 1);
    // 0x1000 = EXT4_INDEX_FL: the directory really is an htree now.
    assert!(stat(&img, "/etc").contains("Flags: 0x81000"));
    assert_fsck_clean(&img);
    let files: Vec<InjectFile> = (0..60)
        .map(|i| {
            InjectFile::new(
                format!("/etc/injected-file-with-a-long-name-{i:03}"),
                format!("{i}\n").into_bytes(),
            )
        })
        .collect();
    inject_files(&img, &files).unwrap();
    assert_fsck_clean(&img);
    assert_eq!(
        cat(&img, "/etc/injected-file-with-a-long-name-059"),
        b"59\n"
    );
    assert_eq!(
        cat(
            &img,
            "/etc/some-reasonably-long-config-file-name-00399.conf"
        ),
        b"k=399\n"
    );
}

#[test]
fn directory_growth_new_block() {
    need_tools!();
    let (_td, img) = setup("64M", &[], 3);
    // Far more entries than fit in the single existing block of /newdir
    // (created fresh, 1 block, 4 KiB): ~150 entries of ~40 bytes.
    let files: Vec<InjectFile> = (0..300)
        .map(|i| {
            InjectFile::new(
                format!("/newdir/entry-with-long-padding-name-{i:04}.txt"),
                vec![b'a'; 10],
            )
        })
        .collect();
    let rep = inject_files(&img, &files).unwrap();
    assert_eq!(rep.dirs_created, vec!["/newdir".to_string()]);
    assert_fsck_clean(&img);
    let ls = String::from_utf8_lossy(&debugfs(&img, "ls /newdir")).to_string();
    assert!(ls.contains("entry-with-long-padding-name-0299.txt"));
    assert!(ls.contains("entry-with-long-padding-name-0000.txt"));
}

#[test]
fn gpt_partitioned_disk() {
    need_tools!();
    let td = tempfile::tempdir().unwrap();
    let tree = td.path().join("tree");
    populate(&tree, 5);
    let part = td.path().join("part.img");
    mke2fs(&part, "32M", &[], Some(&tree));
    // Build GPT disk: ESP-ish dummy + root at 1 MiB offset.
    let off: u64 = 1 << 20;
    let pdata = std::fs::read(&part).unwrap();
    let mut disk = vec![0u8; off as usize + pdata.len() + (1 << 20)];
    disk[off as usize..off as usize + pdata.len()].copy_from_slice(&pdata);
    disk[512..520].copy_from_slice(b"EFI PART");
    disk[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
    disk[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
    disk[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
    // Linux filesystem data 0FC63DAF-8483-4772-8E79-3D69D8477DE4 (mixed endian)
    let guid = [
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D,
        0xE4,
    ];
    let e = 1024;
    disk[e..e + 16].copy_from_slice(&guid);
    disk[e + 32..e + 40].copy_from_slice(&(off / 512).to_le_bytes());
    disk[e + 40..e + 48].copy_from_slice(&((off + pdata.len() as u64) / 512 - 1).to_le_bytes());
    let img = td.path().join("disk.img");
    std::fs::write(&img, &disk).unwrap();

    let rep = inject_agent_token(&img, "gpt-token").unwrap();
    assert_eq!(rep.partition_index, 1);
    assert_eq!(rep.partition_offset, off);
    // extract the partition and verify with e2fsprogs
    let out = std::fs::read(&img).unwrap();
    let extracted = td.path().join("extracted.img");
    std::fs::write(&extracted, &out[off as usize..off as usize + pdata.len()]).unwrap();
    assert_fsck_clean(&extracted);
    assert_eq!(
        cat(&extracted, fluxvm_guest_protocol::TOKEN_FILE_PATH),
        b"gpt-token"
    );
    // nothing outside the partition changed
    assert_eq!(&out[..off as usize], &disk[..off as usize]);
    assert_eq!(
        &out[off as usize + pdata.len()..],
        &disk[off as usize + pdata.len()..]
    );
}

#[test]
fn refuses_needs_recovery_and_leaves_image_untouched() {
    need_tools!();
    let (_td, img) = setup("32M", &[], 3);
    let o = Command::new(tool("debugfs").unwrap())
        .args(["-w", "-R", "feature needs_recovery"])
        .arg(&img)
        .output()
        .unwrap();
    assert!(o.status.success());
    let before = sha(&img);
    let err = inject_agent_token(&img, "t").unwrap_err();
    assert!(
        matches!(err, Error::NoRoot(ref m) if m.contains("recovery")),
        "{err}"
    );
    assert_eq!(before, sha(&img));
}

#[test]
fn refuses_unsupported_feature_quota() {
    need_tools!();
    let td = tempfile::tempdir().unwrap();
    let img = td.path().join("q.img");
    mke2fs(
        &img,
        "32M",
        &["-O", "quota", "-E", "quotatype=usrquota"],
        None,
    );
    let before = sha(&img);
    let err = inject_agent_token(&img, "t").unwrap_err();
    assert!(matches!(err, Error::NoRoot(_)), "{err}");
    assert_eq!(before, sha(&img));
}

#[test]
fn failure_midway_leaves_image_untouched() {
    need_tools!();
    let (_td, img) = setup("32M", &[], 3);
    let before = sha(&img);
    // second file's parent is a regular file => error after first was staged
    let err = inject_files(
        &img,
        &[
            InjectFile::new("/etc/new-file", b"x".to_vec()),
            InjectFile::new("/etc/hostname/child", b"y".to_vec()),
        ],
    )
    .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
    assert_eq!(before, sha(&img));
}

#[test]
fn refuses_non_root_disk_and_bad_input() {
    need_tools!();
    let td = tempfile::tempdir().unwrap();
    let img = td.path().join("noetc.img");
    mke2fs(&img, "16M", &[], None);
    assert!(matches!(
        inject_agent_token(&img, "t"),
        Err(Error::NoRoot(_))
    ));
    assert!(matches!(
        set_hostname(&img, "bad host"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        inject_files(&img, &[InjectFile::new("relative", vec![])]),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn symlinked_parent_is_followed() {
    need_tools!();
    let td = tempfile::tempdir().unwrap();
    let tree = td.path().join("tree");
    populate(&tree, 2);
    std::os::unix::fs::symlink("usr/bin", tree.join("bin")).unwrap();
    let img = td.path().join("r.img");
    mke2fs(&img, "32M", &[], Some(&tree));
    inject_files(
        &img,
        &[InjectFile::new("/bin/tool", b"#!/bin/sh\n".to_vec()).mode(0o755)],
    )
    .unwrap();
    assert_fsck_clean(&img);
    assert_eq!(cat(&img, "/usr/bin/tool"), b"#!/bin/sh\n");
}
