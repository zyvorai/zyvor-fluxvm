// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Resumable transfer of large files to and from a session's guest.
//!
//! `files/content` moves a whole file as base64 inside one JSON body, which tops out at a few
//! megabytes. An *upload* is a sequence of chunks written as part files in the guest, then
//! assembled, size- and checksum-verified, and renamed into place in one step, so a reader never
//! sees a half-written file and an interrupted transfer continues from the byte it reached. A
//! *range read* returns a slice of a file, with its size and, on request, its SHA-256.
//!
//! The pure parts live here (the record, the chunk rules and the guest scripts) so they can be
//! tested without a daemon; the routes are in `agent_sessions.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest file one upload may assemble.
pub(crate) const MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest chunk (raw bytes) one `PUT` may carry.
pub(crate) const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Largest slice a range read returns.
pub(crate) const MAX_RANGE_BYTES: u64 = 4 * 1024 * 1024;
/// Uploads one session may have open at once.
pub(crate) const MAX_OPEN_UPLOADS: usize = 8;
/// Where parts live in the guest, per upload.
pub(crate) const PARTS_ROOT: &str = "/var/tmp/.fluxvm-uploads";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UploadState {
    Open,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Upload {
    pub id: Uuid,
    /// Where the file ends up.
    pub path: String,
    /// The size the caller declared; `complete` refuses anything else.
    pub size: u64,
    /// Lowercase hex SHA-256 the caller declared, checked on `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Bytes accepted so far; the next chunk must start here.
    pub received: u64,
    /// Chunks accepted so far (the next part's index).
    pub parts: u32,
    pub state: UploadState,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// What to do with a chunk that claims to start at `offset` and carry `len` bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Chunk {
    /// Write it as part `index`.
    Accept {
        index: u32,
    },
    /// The caller is behind or ahead: it should continue at `received`.
    Resume {
        received: u64,
    },
    TooBig(String),
}

impl Upload {
    pub(crate) fn dir(&self) -> String {
        format!("{PARTS_ROOT}/{}", self.id)
    }

    pub(crate) fn part_path(&self, index: u32) -> String {
        format!("{}/{index:08}", self.dir())
    }

    pub(crate) fn check_chunk(&self, offset: u64, len: usize) -> Chunk {
        if offset != self.received {
            return Chunk::Resume {
                received: self.received,
            };
        }
        if len == 0 {
            return Chunk::TooBig("a chunk must carry at least one byte".into());
        }
        if len > MAX_CHUNK_BYTES {
            return Chunk::TooBig(format!("a chunk is at most {MAX_CHUNK_BYTES} bytes"));
        }
        if self.received + len as u64 > self.size {
            return Chunk::TooBig(format!(
                "the chunk would pass the declared size of {} bytes",
                self.size
            ));
        }
        Chunk::Accept { index: self.parts }
    }
}

/// A `sha256` the caller declares: 64 hex digits, lowercased.
pub(crate) fn normalize_sha256(s: &str) -> Option<String> {
    let t = s.trim().to_ascii_lowercase();
    (t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())).then_some(t)
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The guest line that assembles the parts into `dest`, checks size (and SHA-256 when declared)
/// and renames it into place. Exits 0 on success; 3 size mismatch, 4 checksum mismatch, 5 no
/// checksum tool, 6 a part is missing.
pub(crate) fn complete_script(u: &Upload) -> String {
    let dir = q(&u.dir());
    let dest = q(&u.path);
    let tmp = q(&format!("{}.fluxvm-upload-{}", u.path, u.id.simple()));
    let mut s = format!(
        "set -e; d={dir}; t={tmp}; [ -d \"$d\" ] || exit 6; \
         n=$(ls \"$d\" | wc -l | tr -d ' '); [ \"$n\" = {parts} ] || exit 6; \
         cat \"$d\"/* > \"$t\"; \
         [ \"$(wc -c < \"$t\" | tr -d ' ')\" = {size} ] || {{ rm -f \"$t\"; exit 3; }}; ",
        parts = u.parts,
        size = u.size,
    );
    if let Some(sum) = &u.sha256 {
        s.push_str(&format!(
            "if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum \"$t\" | cut -d' ' -f1); \
             elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 \"$t\" | cut -d' ' -f1); \
             else rm -f \"$t\"; exit 5; fi; \
             [ \"$h\" = {sum} ] || {{ rm -f \"$t\"; exit 4; }}; ",
            sum = q(sum)
        ));
    }
    if let Some(mode) = u.mode {
        s.push_str(&format!("chmod {mode:o} \"$t\"; "));
    }
    s.push_str(&format!("mv -f \"$t\" {dest}; rm -rf \"$d\""));
    s
}

/// The guest line that prepares the parts directory.
pub(crate) fn open_script(u: &Upload) -> String {
    format!("mkdir -p {} && chmod 700 {}", q(&u.dir()), q(&u.dir()))
}

pub(crate) fn abort_script(u: &Upload) -> String {
    format!("rm -rf {}", q(&u.dir()))
}

/// The guest line that reads `len` bytes of `path` from `offset`: `SIZE:<bytes>`, then (when
/// `checksum`) `SHA256:<hex>`, then `DATA:<base64>`.
pub(crate) fn range_script(path: &str, offset: u64, len: u64, checksum: bool) -> String {
    let f = q(path);
    let sum = if checksum {
        format!(
            "if command -v sha256sum >/dev/null 2>&1; then echo \"SHA256:$(sha256sum {f} | cut -d' ' -f1)\"; \
             elif command -v shasum >/dev/null 2>&1; then echo \"SHA256:$(shasum -a 256 {f} | cut -d' ' -f1)\"; fi; "
        )
    } else {
        String::new()
    };
    format!(
        "[ -f {f} ] || exit 2; echo \"SIZE:$(wc -c < {f} | tr -d ' ')\"; {sum}\
         printf 'DATA:'; tail -c +{from} {f} | head -c {len} | base64 | tr -d '\\n'; echo",
        from = offset + 1,
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub size: u64,
    pub sha256: Option<String>,
    pub data_base64: String,
}

pub(crate) fn parse_range(stdout: &str) -> Option<Range> {
    let (mut size, mut sha256, mut data) = (None, None, None);
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("SIZE:") {
            size = v.trim().parse::<u64>().ok();
        } else if let Some(v) = line.strip_prefix("SHA256:") {
            sha256 = normalize_sha256(v);
        } else if let Some(v) = line.strip_prefix("DATA:") {
            data = Some(v.trim().to_string());
        }
    }
    Some(Range {
        size: size?,
        sha256,
        data_base64: data?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn upload(path: &str, size: u64, parts: u32, sha: Option<&str>, mode: Option<u32>) -> Upload {
        Upload {
            id: Uuid::new_v4(),
            path: path.into(),
            size,
            sha256: sha.map(str::to_string),
            mode,
            received: size,
            parts,
            state: UploadState::Open,
            created_at: Utc::now(),
            completed_at: None,
        }
    }

    fn sh(script: &str) -> (i32, String) {
        let out = Command::new("sh").arg("-c").arg(script).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn chunks_must_continue_where_the_upload_stopped() {
        let mut u = upload("/x", 10, 0, None, None);
        u.received = 0;
        assert_eq!(u.check_chunk(0, 4), Chunk::Accept { index: 0 });
        u.received = 4;
        u.parts = 1;
        assert_eq!(u.check_chunk(4, 6), Chunk::Accept { index: 1 });
        // Behind (a retry of a chunk that landed) and ahead both say where to continue.
        assert_eq!(u.check_chunk(0, 4), Chunk::Resume { received: 4 });
        assert_eq!(u.check_chunk(8, 2), Chunk::Resume { received: 4 });
        // Past the declared size, empty and oversize chunks are refused.
        assert!(matches!(u.check_chunk(4, 7), Chunk::TooBig(_)));
        assert!(matches!(u.check_chunk(4, 0), Chunk::TooBig(_)));
        u.size = u64::MAX;
        assert!(matches!(
            u.check_chunk(4, MAX_CHUNK_BYTES + 1),
            Chunk::TooBig(_)
        ));
    }

    #[test]
    fn checksums_are_hex_sha256() {
        let h = "A".repeat(64);
        assert_eq!(normalize_sha256(&h).unwrap(), "a".repeat(64));
        assert!(normalize_sha256("abc").is_none());
        assert!(normalize_sha256(&"g".repeat(64)).is_none());
    }

    #[test]
    fn the_assembly_script_verifies_and_renames_atomically() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out.bin");
        // Parts live where the upload says; point it at the temp dir.
        let mut u = upload(dest.to_str().unwrap(), 11, 3, None, Some(0o640));
        let parts = t.path().join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        for (i, b) in [b"hello ".as_slice(), b"wor", b"ld"].iter().enumerate() {
            std::fs::write(parts.join(format!("{i:08}")), b).unwrap();
        }
        let script = complete_script(&u).replace(&u.dir(), parts.to_str().unwrap());
        let (code, _) = sh(&script);
        assert_eq!(code, 0);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        assert!(!parts.exists(), "parts are removed after the rename");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o640);
        }

        // A wrong size leaves no file at the destination and no temp file behind.
        let dest2 = t.path().join("bad.bin");
        u = upload(dest2.to_str().unwrap(), 99, 1, None, None);
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::write(parts.join("00000000"), b"short").unwrap();
        let s2 = complete_script(&u).replace(&u.dir(), parts.to_str().unwrap());
        assert_eq!(sh(&s2).0, 3);
        assert!(!dest2.exists());
        // Only out.bin and the kept parts directory: no bad.bin and no temp file.
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_declared_checksum_must_match() {
        if sh("command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1").0 != 0 {
            return; // no checksum tool on this machine
        }
        let t = tempfile::tempdir().unwrap();
        let parts = t.path().join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::write(parts.join("00000000"), b"abc").unwrap();
        // SHA-256("abc")
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let run = |sum: &str, name: &str| {
            let dest = t.path().join(name);
            std::fs::create_dir_all(&parts).unwrap();
            std::fs::write(parts.join("00000000"), b"abc").unwrap();
            let u = upload(dest.to_str().unwrap(), 3, 1, Some(sum), None);
            let s = complete_script(&u).replace(&u.dir(), parts.to_str().unwrap());
            (sh(&s).0, dest)
        };
        let (code, dest) = run(good, "ok.bin");
        assert_eq!(code, 0);
        assert_eq!(std::fs::read(dest).unwrap(), b"abc");
        let (code, dest) = run(&"0".repeat(64), "bad.bin");
        assert_eq!(code, 4);
        assert!(!dest.exists());
    }

    #[test]
    fn a_missing_part_is_detected_before_assembly() {
        let t = tempfile::tempdir().unwrap();
        let parts = t.path().join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::write(parts.join("00000000"), b"a").unwrap();
        let u = upload(t.path().join("o").to_str().unwrap(), 2, 2, None, None);
        let s = complete_script(&u).replace(&u.dir(), parts.to_str().unwrap());
        assert_eq!(sh(&s).0, 6);
        assert!(!t.path().join("o").exists());
    }

    #[test]
    fn ranges_are_read_from_an_offset_with_size_and_checksum() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f.bin");
        std::fs::write(&f, b"0123456789").unwrap();
        let (code, out) = sh(&range_script(f.to_str().unwrap(), 3, 4, false));
        assert_eq!(code, 0);
        let r = parse_range(&out).unwrap();
        assert_eq!(r.size, 10);
        assert_eq!(r.sha256, None);
        assert_eq!(r.data_base64, "MzQ1Ng=="); // "3456"
        // Past the end: empty data, size still reported.
        let (_, out) = sh(&range_script(f.to_str().unwrap(), 20, 4, false));
        assert_eq!(parse_range(&out).unwrap().data_base64, "");
        // Missing file: a distinct exit status.
        assert_eq!(sh(&range_script("/no/such/file", 0, 1, false)).0, 2);
        // A path with a quote is not an injection.
        let weird = t.path().join("it's.bin");
        std::fs::write(&weird, b"x").unwrap();
        assert_eq!(
            parse_range(&sh(&range_script(weird.to_str().unwrap(), 0, 4, false)).1)
                .unwrap()
                .data_base64,
            "eA=="
        );
        if sh("command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1").0 == 0 {
            let (_, out) = sh(&range_script(f.to_str().unwrap(), 0, 1, true));
            let r = parse_range(&out).unwrap();
            assert_eq!(
                r.sha256.as_deref(),
                Some("84d89877f0d4041efb6bf91a16f0248f2fd573e6af05c19f96bedb9f882f7882")
            );
        }
    }
}
