// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Signed boot manifest for a `vz` VM start ("attested boot, host claim").
//!
//! At launch the daemon records what it is about to boot (kernel, initrd, command line, root disk, runner binary and
//! the Apple spec), signs that manifest with an ed25519 key the daemon holds under its state directory, and serves it to
//! the guest in the `attestation` field of the `metadata` vsock service.
//!
//! **What this is not.** Virtualization.framework has no TPM and no measured boot, and the signing key is a file on the
//! host (mode 0600), not a Secure Enclave key. The signature attests the *host's claim* about what it started, as made
//! by whoever holds the host key. It does not prove to a remote party that the host really booted those bytes, and it
//! gives no protection against a compromised host. A guest can use it to detect tampering in transit or a different
//! host key than the one it was told to expect, nothing more.

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use fluxvm_core::model::CreateVmRequest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const ATTESTATION_VERSION: u32 = 1;
pub const ALG: &str = "ed25519";
/// Files larger than this are not hashed at launch (a multi-GB root disk would delay every start); their evidence
/// carries `digest: "skipped"` together with size and mtime.
pub const HASH_MAX_BYTES: u64 = 256 << 20;
pub const SKIPPED: &str = "skipped";
/// The claim text embedded in every manifest, so a reader of the JSON alone sees what it is.
pub const CLAIM: &str =
    "host-asserted: signed by the FluxVM host key, not by hardware; no TPM or measured boot";
const KEY_FILE: &str = "vz-attestation.key";
const PUB_FILE: &str = "vz-attestation.pub";

/// Evidence for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvidence {
    pub path: String,
    pub size: u64,
    pub mtime_unix: Option<u64>,
    /// Hex sha256 of the contents, or `"skipped"` when the file is larger than [`HASH_MAX_BYTES`].
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub claim: String,
    pub vm_id: String,
    pub issued_at_unix: u64,
    pub kernel: Option<FileEvidence>,
    pub initrd: Option<FileEvidence>,
    /// sha256 of the kernel command line, when direct kernel boot is used.
    pub cmdline_sha256: Option<String>,
    pub root_disk: FileEvidence,
    pub runner: FileEvidence,
    /// sha256 of the request's `apple` spec as JSON (secret environment values are never part of it).
    pub apple_spec_sha256: String,
}

/// The wire form served to the guest. `payload` is the manifest's exact JSON text and `signature` covers those bytes,
/// so verification never depends on re-serialising.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub alg: String,
    pub payload: String,
    /// Hex ed25519 signature over `payload`.
    pub signature: String,
    /// Hex public key of the signer. Informational: a verifier must compare it against a key it already trusts.
    pub public_key: String,
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        bail!("not a hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow::anyhow!("not a hex string"))
        })
        .collect()
}

fn sha256_bytes(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

/// Evidence for `path`: size and mtime always, the sha256 when the file is at most `max_hash_bytes`.
pub fn file_evidence(path: &Path, max_hash_bytes: u64) -> Result<FileEvidence> {
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let digest = if meta.len() <= max_hash_bytes {
        let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        hex(&h.finalize())
    } else {
        SKIPPED.to_string()
    };
    Ok(FileEvidence {
        path: path.display().to_string(),
        size: meta.len(),
        mtime_unix,
        digest,
    })
}

/// Builds the manifest for a launch. Blocking: it reads (and hashes) the files.
pub fn collect(
    vm_id: &str,
    req: &CreateVmRequest,
    root_disk: &Path,
    runner: &Path,
    issued_at_unix: u64,
) -> Result<Manifest> {
    collect_with_limit(
        vm_id,
        req,
        root_disk,
        runner,
        issued_at_unix,
        HASH_MAX_BYTES,
    )
}

pub fn collect_with_limit(
    vm_id: &str,
    req: &CreateVmRequest,
    root_disk: &Path,
    runner: &Path,
    issued_at_unix: u64,
    max_hash_bytes: u64,
) -> Result<Manifest> {
    let mut apple = req.apple.clone().unwrap_or_default();
    apple.secret_env.clear();
    let evidence = |p: &Option<PathBuf>| -> Result<Option<FileEvidence>> {
        p.as_deref()
            .map(|p| file_evidence(p, max_hash_bytes))
            .transpose()
    };
    Ok(Manifest {
        version: ATTESTATION_VERSION,
        claim: CLAIM.to_string(),
        vm_id: vm_id.to_string(),
        issued_at_unix,
        kernel: evidence(&req.kernel)?,
        initrd: evidence(&req.initrd)?,
        cmdline_sha256: req.kernel_args.as_ref().map(|a| sha256_bytes(a.as_bytes())),
        root_disk: file_evidence(root_disk, max_hash_bytes)?,
        runner: file_evidence(runner, max_hash_bytes)?,
        apple_spec_sha256: sha256_bytes(&serde_json::to_vec(&apple)?),
    })
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Signs `manifest`.
pub fn sign(manifest: &Manifest, key: &SigningKey) -> Result<Attestation> {
    let payload = serde_json::to_string(manifest)?;
    let sig = key.sign(payload.as_bytes());
    Ok(Attestation {
        alg: ALG.to_string(),
        signature: hex(&sig.to_bytes()),
        public_key: hex(key.verifying_key().as_bytes()),
        payload,
    })
}

/// Checks `attestation_json` (the value of the metadata document's `attestation` field, or the whole attestation
/// object) against `pubkey`, the 32-byte ed25519 public key the caller already trusts, and returns the manifest.
/// The `public_key` field inside the document is never used to verify.
pub fn verify_attestation(attestation_json: &str, pubkey: &[u8]) -> Result<Manifest> {
    let key: [u8; 32] = pubkey
        .try_into()
        .map_err(|_| anyhow::anyhow!("an ed25519 public key is 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&key).context("invalid ed25519 public key")?;
    let value: serde_json::Value =
        serde_json::from_str(attestation_json).context("attestation is not JSON")?;
    // Accept the whole metadata document too.
    let value = value.get("attestation").cloned().unwrap_or(value);
    let att: Attestation =
        serde_json::from_value(value).context("attestation has the wrong shape")?;
    if att.alg != ALG {
        bail!("unsupported attestation algorithm {:?}", att.alg);
    }
    let sig: [u8; 64] = unhex(&att.signature)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes"))?;
    key.verify(att.payload.as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| anyhow::anyhow!("attestation signature does not match the trusted key"))?;
    let manifest: Manifest =
        serde_json::from_str(&att.payload).context("attested manifest is malformed")?;
    if manifest.version != ATTESTATION_VERSION {
        bail!("unsupported attestation version {}", manifest.version);
    }
    Ok(manifest)
}

/// [`verify_attestation`] with the public key as a hex string.
pub fn verify_attestation_hex(attestation_json: &str, pubkey_hex: &str) -> Result<Manifest> {
    verify_attestation(attestation_json, &unhex(pubkey_hex.trim())?)
}

/// The daemon's attestation key under `state_dir`, created (0600, from the OS entropy source) on first use.
/// The public half is written beside it as `vz-attestation.pub` for operators to hand to guests.
pub fn load_or_create_host_key(state_dir: &Path) -> Result<SigningKey> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = state_dir.join(KEY_FILE);
    if path.exists() {
        let mode = fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is readable by others (mode {:o}); chmod 600 it or delete it to generate a new key",
                path.display(),
                mode & 0o777
            );
        }
        let text = fs::read_to_string(&path)?;
        let seed: [u8; 32] = unhex(text.trim())?
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} must hold a 32-byte hex seed", path.display()))?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    fs::create_dir_all(state_dir)?;
    let mut seed = [0u8; 32];
    // SAFETY: `seed` is a valid writable buffer of exactly the requested 32 bytes (getentropy's maximum is 256).
    if unsafe { libc::getentropy(seed.as_mut_ptr().cast(), seed.len()) } != 0 {
        bail!("getentropy: {}", std::io::Error::last_os_error());
    }
    let key = SigningKey::from_bytes(&seed);
    let tmp = state_dir.join(format!("{KEY_FILE}.{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(hex(&seed).as_bytes())?;
        f.sync_all()?;
    }
    // A concurrent creator wins; keep its key so every launch signs with one identity.
    match fs::hard_link(&tmp, &path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            let _ = fs::write(
                state_dir.join(PUB_FILE),
                format!("{}\n", hex(key.verifying_key().as_bytes())),
            );
            Ok(key)
        }
        Err(_) => {
            let _ = fs::remove_file(&tmp);
            load_or_create_host_key(state_dir)
        }
    }
}

/// `metadata_json` (the object the runner serves) with the attestation added as its `attestation` field.
pub fn with_attestation(metadata_json: &str, att: &Attestation) -> Result<String> {
    let mut doc: serde_json::Value =
        serde_json::from_str(metadata_json).context("metadata document is not JSON")?;
    let obj = doc
        .as_object_mut()
        .context("metadata document is not an object")?;
    obj.insert("attestation".into(), serde_json::to_value(att)?);
    Ok(doc.to_string())
}

/// File in a VM's workspace holding the attestation its last launch produced.
pub const DELIVERY_FILE: &str = "vz-attestation.json";

/// Keeps the `attestation` object of `metadata_json` in `workspace` so the API can hand it to an out-of-band verifier.
pub fn persist(workspace: &Path, metadata_json: &str) -> Result<()> {
    let doc: serde_json::Value =
        serde_json::from_str(metadata_json).context("metadata document is not JSON")?;
    let att = doc
        .get("attestation")
        .context("metadata document has no attestation")?;
    let path = workspace.join(DELIVERY_FILE);
    let tmp = workspace.join(format!("{DELIVERY_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(att)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// Forgets a previous launch's attestation (this launch produced none), so a stale one is never served.
pub fn clear(workspace: &Path) {
    let _ = fs::remove_file(workspace.join(DELIVERY_FILE));
}

/// The daemon's attestation public key (hex), read from `vz-attestation.pub` or derived from the key file. Never creates one.
pub fn host_public_key_hex(state_dir: &Path) -> Result<String> {
    if let Ok(text) = fs::read_to_string(state_dir.join(PUB_FILE)) {
        let t = text.trim();
        if unhex(t).is_ok_and(|b| b.len() == 32) {
            return Ok(t.to_string());
        }
    }
    let text = fs::read_to_string(state_dir.join(KEY_FILE))
        .context("this host has not produced an attestation key yet")?;
    let seed: [u8; 32] = unhex(text.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("{KEY_FILE} must hold a 32-byte hex seed"))?;
    Ok(hex(SigningKey::from_bytes(&seed)
        .verifying_key()
        .as_bytes()))
}

/// What `GET /v1/vms/{id}/vz/attestation` serves: the signed attestation of the VM's last launch, the manifest it
/// carries (decoded for convenience; the signature covers `attestation.payload`), and the host public key.
///
/// The key here comes from the same host as the attestation, so checking one against the other only shows they are
/// consistent. A verifier that needs trust must hold the host key from somewhere else (see the module docs).
pub fn delivery(vm_id: &str, workspace: &Path, state_dir: &Path) -> Result<serde_json::Value> {
    let path = workspace.join(DELIVERY_FILE);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "no attestation recorded for this VM (it is produced when the VM starts; {} missing)",
            path.display()
        )
    })?;
    let att: Attestation =
        serde_json::from_str(&text).context("recorded attestation is malformed")?;
    let manifest: Manifest =
        serde_json::from_str(&att.payload).context("recorded manifest is malformed")?;
    if manifest.vm_id != vm_id {
        bail!(
            "recorded attestation is for VM {}, not {vm_id}; start the VM again",
            manifest.vm_id
        );
    }
    Ok(serde_json::json!({
        "vm_id": vm_id,
        "claim": CLAIM,
        "issued_at_unix": manifest.issued_at_unix,
        "attestation": att,
        "manifest": manifest,
        "public_key": host_public_key_hex(state_dir)?,
    }))
}

/// Collects, signs and attaches the attestation for a launch, off the async runtime (it hashes files).
pub async fn attach(
    state_dir: PathBuf,
    metadata_json: String,
    vm_id: String,
    req: CreateVmRequest,
    root_disk: PathBuf,
    runner: PathBuf,
) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let key = load_or_create_host_key(&state_dir)?;
        let manifest = collect(&vm_id, &req, &root_disk, &runner, now_unix())?;
        with_attestation(&metadata_json, &sign(&manifest, &key)?)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kernel: Option<&Path>, args: Option<&str>) -> CreateVmRequest {
        let mut r: CreateVmRequest = serde_json::from_str(
            r#"{"name":"t","backend":"vz","image":"/x.raw","apple":{"vsock_services":[{"port":5001,"builtin":"metadata"}]}}"#,
        )
        .unwrap();
        r.kernel = kernel.map(Path::to_path_buf);
        r.kernel_args = args.map(str::to_string);
        r
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        disk: PathBuf,
        runner: PathBuf,
        kernel: PathBuf,
        state: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("root.raw"), b"disk-bytes").unwrap();
        fs::write(p.join("runner"), b"runner-bytes").unwrap();
        fs::write(p.join("vmlinuz"), b"kernel-bytes").unwrap();
        fs::create_dir(p.join("state")).unwrap();
        Fixture {
            disk: p.join("root.raw"),
            runner: p.join("runner"),
            kernel: p.join("vmlinuz"),
            state: p.join("state"),
            _dir: dir,
        }
    }

    fn signed(f: &Fixture, key: &SigningKey) -> (Manifest, Attestation) {
        let r = req(Some(&f.kernel), Some("console=hvc0 root=/dev/vda"));
        let m = collect("vm-1", &r, &f.disk, &f.runner, 1_700_000_000).unwrap();
        let a = sign(&m, key).unwrap();
        (m, a)
    }

    #[test]
    fn signs_and_verifies() {
        let f = fixture();
        let key = load_or_create_host_key(&f.state).unwrap();
        let (m, a) = signed(&f, &key);
        let json = serde_json::to_string(&a).unwrap();
        let got = verify_attestation(&json, key.verifying_key().as_bytes()).unwrap();
        assert_eq!(got, m);
        assert_eq!(got.vm_id, "vm-1");
        assert_eq!(got.root_disk.digest, sha256_bytes(b"disk-bytes"));
        assert_eq!(got.runner.digest, sha256_bytes(b"runner-bytes"));
        assert_eq!(got.kernel.unwrap().digest, sha256_bytes(b"kernel-bytes"));
        assert_eq!(
            got.cmdline_sha256.unwrap(),
            sha256_bytes(b"console=hvc0 root=/dev/vda")
        );
        assert!(got.claim.contains("not by hardware"));
        // hex entry point and the whole metadata document both verify
        verify_attestation_hex(&json, &hex(key.verifying_key().as_bytes())).unwrap();
        let meta = with_attestation(r#"{"id":"x"}"#, &a).unwrap();
        verify_attestation(&meta, key.verifying_key().as_bytes()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(doc["id"], "x");
        assert!(doc["attestation"]["payload"].is_string());
    }

    #[test]
    fn delivery_serves_the_launch_attestation_and_host_key() {
        let f = fixture();
        let key = load_or_create_host_key(&f.state).unwrap();
        let (m, a) = signed(&f, &key);
        let ws = f._dir.path().join("ws");
        fs::create_dir(&ws).unwrap();
        // Nothing recorded yet.
        assert!(
            delivery("vm-1", &ws, &f.state)
                .unwrap_err()
                .to_string()
                .contains("no attestation")
        );
        let meta = with_attestation(r#"{"id":"x"}"#, &a).unwrap();
        persist(&ws, &meta).unwrap();
        let d = delivery("vm-1", &ws, &f.state).unwrap();
        assert_eq!(d["public_key"], hex(key.verifying_key().as_bytes()));
        assert_eq!(d["manifest"]["vm_id"], "vm-1");
        assert_eq!(d["issued_at_unix"], m.issued_at_unix);
        // What was served verifies under the served key, as the CLI does it.
        let got = verify_attestation_hex(
            &d["attestation"].to_string(),
            d["public_key"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(got, m);
        // Another VM's id, or a cleared launch, is not served.
        assert!(delivery("vm-2", &ws, &f.state).is_err());
        clear(&ws);
        assert!(delivery("vm-1", &ws, &f.state).is_err());
        // A metadata document without an attestation records nothing.
        assert!(persist(&ws, r#"{"id":"x"}"#).is_err());
    }

    #[test]
    fn host_key_is_read_not_created() {
        let f = fixture();
        assert!(host_public_key_hex(&f.state).is_err());
        assert!(!f.state.join(KEY_FILE).exists());
        let key = load_or_create_host_key(&f.state).unwrap();
        let want = hex(key.verifying_key().as_bytes());
        assert_eq!(host_public_key_hex(&f.state).unwrap(), want);
        // Falls back to deriving it from the key file.
        fs::remove_file(f.state.join(PUB_FILE)).unwrap();
        assert_eq!(host_public_key_hex(&f.state).unwrap(), want);
    }

    #[test]
    fn tampering_is_detected() {
        let f = fixture();
        let key = load_or_create_host_key(&f.state).unwrap();
        let (_, a) = signed(&f, &key);
        let pk = key.verifying_key();

        // payload edited
        let mut t = a.clone();
        t.payload = t.payload.replace("vm-1", "vm-2");
        let json = serde_json::to_string(&t).unwrap();
        assert!(verify_attestation(&json, pk.as_bytes()).is_err());

        // signature edited
        let mut t = a.clone();
        let flipped = if t.signature.starts_with('0') {
            "1"
        } else {
            "0"
        };
        t.signature.replace_range(0..1, flipped);
        let json = serde_json::to_string(&t).unwrap();
        assert!(verify_attestation(&json, pk.as_bytes()).is_err());

        // signed by another key, even with the right embedded public_key claim swapped in
        let other = SigningKey::from_bytes(&[7u8; 32]);
        let (_, b) = signed(&f, &other);
        let json = serde_json::to_string(&b).unwrap();
        assert!(verify_attestation(&json, pk.as_bytes()).is_err());
        assert!(verify_attestation(&json, other.verifying_key().as_bytes()).is_ok());

        // wrong algorithm, wrong key length, not JSON
        let mut t = a.clone();
        t.alg = "rsa".into();
        let json = serde_json::to_string(&t).unwrap();
        assert!(verify_attestation(&json, pk.as_bytes()).is_err());
        assert!(verify_attestation(&serde_json::to_string(&a).unwrap(), &[0u8; 5]).is_err());
        assert!(verify_attestation("not json", pk.as_bytes()).is_err());
    }

    #[test]
    fn large_files_are_marked_skipped_not_hashed() {
        let f = fixture();
        let r = req(None, None);
        let m = collect_with_limit("v", &r, &f.disk, &f.runner, 1, 4).unwrap();
        assert_eq!(m.root_disk.digest, SKIPPED);
        assert_eq!(m.root_disk.size, 10);
        assert!(m.root_disk.mtime_unix.is_some());
        assert_eq!(m.runner.digest, SKIPPED);
        assert!(m.kernel.is_none() && m.cmdline_sha256.is_none());
    }

    #[test]
    fn host_key_is_created_once_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let k1 = load_or_create_host_key(&f.state).unwrap();
        let k2 = load_or_create_host_key(&f.state).unwrap();
        assert_eq!(k1.to_bytes(), k2.to_bytes());
        let mode = fs::metadata(f.state.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let public = fs::read_to_string(f.state.join(PUB_FILE)).unwrap();
        assert_eq!(public.trim(), hex(k1.verifying_key().as_bytes()));
        // a key readable by others is refused rather than trusted
        fs::set_permissions(f.state.join(KEY_FILE), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_host_key(&f.state).is_err());
    }

    #[test]
    fn the_spec_digest_ignores_secret_env_and_follows_the_spec() {
        let f = fixture();
        let a = collect("v", &req(None, None), &f.disk, &f.runner, 1).unwrap();
        let mut r = req(None, None);
        r.apple.as_mut().unwrap().display_width = 1920;
        let b = collect("v", &r, &f.disk, &f.runner, 1).unwrap();
        assert_ne!(a.apple_spec_sha256, b.apple_spec_sha256);
        let c = collect("v", &req(None, None), &f.disk, &f.runner, 99).unwrap();
        assert_eq!(a.apple_spec_sha256, c.apple_spec_sha256);
    }
}
