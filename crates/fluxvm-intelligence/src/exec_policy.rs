// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Positive exec allow-list for the VMM Guard: file format, Ed25519 signing
//! and a `learn` inventory helper. Kernel enforcement lives in
//! `bpf/fluxvm_guard.bpf.c`; `guard::apply_policy` loads the resolved identities.
//!
//! A policy is a TOML file with a detached signature in `<file>.sig`
//! (base64 Ed25519 over the exact file bytes), verified against keys the
//! operator trusts.

use anyhow::{Context, Result, bail};
use fluxvm_core::signing::{self, ParsedSigner};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecPolicy {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Absolute paths of executables (and script interpreters) allowed to run.
    pub paths: Vec<PathBuf>,
}

impl ExecPolicy {
    pub fn parse(text: &str) -> Result<Self> {
        let p: ExecPolicy = toml::from_str(text).context("parsing exec policy")?;
        if p.version != SCHEMA_VERSION {
            bail!(
                "unsupported exec policy version {} (expected {SCHEMA_VERSION})",
                p.version
            );
        }
        // An empty allow-list in enforce mode would block every exec, so
        // require the operator to say what is allowed.
        if p.paths.is_empty() {
            bail!("exec policy lists no paths");
        }
        if let Some(bad) = p.paths.iter().find(|p| !p.is_absolute()) {
            bail!("exec policy path {} is not absolute", bad.display());
        }
        Ok(p)
    }

    pub fn render(&self) -> Result<String> {
        toml::to_string_pretty(self).context("rendering exec policy")
    }
}

fn sig_path(policy: &Path) -> PathBuf {
    let mut s = policy.as_os_str().to_owned();
    s.push(".sig");
    PathBuf::from(s)
}

/// Writes `<file>.sig` with an Ed25519 signature over the file's exact bytes.
pub fn sign_detached(file: &Path, private_key_b64: &str) -> Result<PathBuf> {
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let sig = signing::sign_bytes(private_key_b64.trim(), &bytes)?;
    let out = sig_path(file);
    fs::write(&out, format!("{sig}\n")).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
}

/// Reads `file` and verifies `<file>.sig` against `trusted`. Fails closed when
/// no keys are configured, the signature file is absent, or no key matches.
/// Returns the file text and the matching signer's label.
pub fn read_verified(file: &Path, trusted: &[ParsedSigner]) -> Result<(String, String)> {
    if trusted.is_empty() {
        bail!(
            "no trusted signing keys configured; pass --trusted-key or set FLUXVM_GUARD_TRUSTED_KEYS (or use --unsigned)"
        );
    }
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let sig_file = sig_path(file);
    let sig = fs::read_to_string(&sig_file)
        .with_context(|| format!("reading signature {}", sig_file.display()))?;
    let signer = signing::verify_bytes(&bytes, sig.trim(), trusted)
        .with_context(|| format!("signature verification failed for {}", file.display()))?
        .to_string();
    let text = String::from_utf8(bytes).context("file is not UTF-8")?;
    Ok((text, signer))
}

/// Signs `policy_file`'s bytes and writes `<policy_file>.sig`. Refuses to sign
/// something the guard would later reject.
pub fn sign_file(policy_file: &Path, private_key_b64: &str) -> Result<PathBuf> {
    ExecPolicy::parse(&fs::read_to_string(policy_file)?)?;
    sign_detached(policy_file, private_key_b64)
}

/// Loads a policy and verifies its detached signature; returns the policy and
/// the matching signer's label.
pub fn load_verified(policy_file: &Path, trusted: &[ParsedSigner]) -> Result<(ExecPolicy, String)> {
    let (text, signer) = read_verified(policy_file, trusted)?;
    Ok((ExecPolicy::parse(&text)?, signer))
}

/// Loads a policy without checking a signature (lab use).
pub fn load_unsigned(policy_file: &Path) -> Result<ExecPolicy> {
    let text = fs::read_to_string(policy_file)
        .with_context(|| format!("reading {}", policy_file.display()))?;
    ExecPolicy::parse(&text)
}

/// Builds trusted signers from base64 public keys, labelled `key0`, `key1`, ...
pub fn trusted_from_keys(keys: &[String]) -> Result<Vec<ParsedSigner>> {
    keys.iter()
        .enumerate()
        .map(|(i, k)| Ok((format!("key{i}"), signing::parse_public_key(k.trim())?)))
        .collect()
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// For a `#!` script, the interpreter path (each script exec is a second
/// bprm check on the interpreter, so it must be allowed too). `/usr/bin/env`
/// style shebangs resolve to env only.
fn shebang_interpreter(path: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut buf = [0u8; 256];
    let n = fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    let line = head.strip_prefix("#!")?.lines().next()?;
    let interp = line.split_whitespace().next()?;
    interp.starts_with('/').then(|| PathBuf::from(interp))
}

fn walk(dir: &Path, depth: usize, out: &mut BTreeSet<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() && !ft.is_symlink() {
            if depth > 0 {
                walk(&p, depth - 1, out);
            }
        } else if let Ok(real) = fs::canonicalize(&p)
            && is_executable_file(&real)
        {
            out.insert(real);
        }
    }
}

/// Inventories executables under `dirs` (up to `depth` levels, symlinks
/// resolved) plus each `extra` path, adding script interpreters. Paths that
/// do not exist are skipped.
pub fn learn(dirs: &[PathBuf], extra: &[PathBuf], depth: usize) -> ExecPolicy {
    let mut found = BTreeSet::new();
    for d in dirs {
        walk(d, depth, &mut found);
    }
    for p in extra {
        if let Ok(real) = fs::canonicalize(p)
            && is_executable_file(&real)
        {
            found.insert(real);
        }
    }
    let interpreters: Vec<PathBuf> = found
        .iter()
        .filter_map(|p| shebang_interpreter(p))
        .filter_map(|i| fs::canonicalize(i).ok())
        .collect();
    found.extend(interpreters);
    ExecPolicy {
        version: SCHEMA_VERSION,
        comment: Some("generated by `fluxvm-guard learn-exec`; review before signing".into()),
        paths: found.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn policy_file(dir: &Path, paths: &[&str]) -> PathBuf {
        let p = ExecPolicy {
            version: 1,
            comment: None,
            paths: paths.iter().map(PathBuf::from).collect(),
        };
        let f = dir.join("exec.toml");
        fs::write(&f, p.render().unwrap()).unwrap();
        f
    }

    #[test]
    fn parse_rejects_bad_policies() {
        assert!(ExecPolicy::parse("version = 2\npaths = [\"/bin/ls\"]").is_err());
        assert!(ExecPolicy::parse("version = 1\npaths = []").is_err());
        assert!(ExecPolicy::parse("version = 1\npaths = [\"bin/ls\"]").is_err());
        assert!(ExecPolicy::parse("version = 1\npaths = [\"/bin/ls\"]").is_ok());
    }

    #[test]
    fn signed_policy_verifies_and_tampering_or_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let f = policy_file(dir.path(), &["/usr/bin/true"]);
        let (private, public) = signing::generate_keypair();
        sign_file(&f, &private).unwrap();
        let trusted = trusted_from_keys(&[public]).unwrap();
        let (p, signer) = load_verified(&f, &trusted).unwrap();
        assert_eq!(p.paths, vec![PathBuf::from("/usr/bin/true")]);
        assert_eq!(signer, "key0");

        // tamper: add a path after signing
        let mut text = fs::read_to_string(&f).unwrap();
        text = text.replace("/usr/bin/true", "/usr/bin/true\", \"/bin/sh");
        fs::write(&f, text).unwrap();
        assert!(load_verified(&f, &trusted).is_err());

        // wrong key
        let f2 = policy_file(dir.path(), &["/usr/bin/true"]);
        sign_file(&f2, &private).unwrap();
        let (_, other_pub) = signing::generate_keypair();
        assert!(load_verified(&f2, &trusted_from_keys(&[other_pub]).unwrap()).is_err());
    }

    #[test]
    fn verification_fails_closed_without_keys_or_signature() {
        let dir = tempfile::tempdir().unwrap();
        let f = policy_file(dir.path(), &["/usr/bin/true"]);
        let (_, public) = signing::generate_keypair();
        // no signature file
        assert!(load_verified(&f, &trusted_from_keys(&[public]).unwrap()).is_err());
        // no trusted keys at all
        assert!(load_verified(&f, &[]).is_err());
    }

    #[test]
    fn sign_refuses_an_invalid_policy() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bad.toml");
        fs::write(&f, "version = 1\npaths = []\n").unwrap();
        let (private, _) = signing::generate_keypair();
        assert!(sign_file(&f, &private).is_err());
    }

    #[test]
    fn learn_finds_executables_resolves_symlinks_and_adds_interpreters() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        write_exec(&bin.join("tool"), "\x7fELF-ish");
        write_exec(&bin.join("interp"), "\x7fELF-ish");
        write_exec(
            &bin.join("script.sh"),
            &format!("#!{} -e\necho hi\n", bin.join("interp").display()),
        );
        fs::write(bin.join("data.txt"), "not executable").unwrap();
        symlink(bin.join("tool"), bin.join("tool-link")).unwrap();

        let p = learn(std::slice::from_ref(&bin), &[], 2);
        assert_eq!(
            p.paths,
            vec![bin.join("interp"), bin.join("script.sh"), bin.join("tool")]
        );
        assert!(!p.paths.contains(&bin.join("data.txt")));
        // learned output is itself a valid policy
        ExecPolicy::parse(&p.render().unwrap()).unwrap();
    }

    #[test]
    fn learn_with_extra_path_and_missing_dir_is_tolerant() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        write_exec(&root.join("x"), "bin");
        let p = learn(
            &[root.join("nope")],
            &[root.join("x"), root.join("missing")],
            1,
        );
        assert_eq!(p.paths, vec![root.join("x")]);
    }
}
