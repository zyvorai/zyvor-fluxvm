// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Trust for `fluxctl mac` images: Ed25519 signatures over the manifest digest, a local trust store, and the licence guard on
//! public registries.
//!
//! The scheme is the one the image catalog already uses (`fluxvm_core::signing`: Ed25519, base64 keys). What is signed is the
//! text `fluxvm-mac-image-v1\n<manifest digest>`; the manifest digest pins the config and every chunk, so a valid signature
//! covers the whole image. The signature travels as an OCI referrer (artifact type [`SIG_TYPE`]) whose annotations carry the
//! signature and the signer's public key, and as a sidecar file next to the trust store after a pull. A signer is trusted only
//! if its public key is in the trust store (`fluxctl mac keys trust`): the key inside a signature proves nothing by itself.
//!
//! This is not cosign: there is no keyless flow, no transparency log and no certificate chain. Not run against a real registry.

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use fluxvm_core::signing::{self, ParsedSigner};
use fluxvm_image::oci_manage::ContentInfo;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Artifact type of the signature referrer.
pub const SIG_TYPE: &str = "application/vnd.zyvor.fluxvm.signature.v1+json";
pub const ANN_SIGNATURE: &str = "dev.zyvor.fluxvm.signature";
pub const ANN_SIGNER: &str = "dev.zyvor.fluxvm.signer";
pub const ANN_SIGNED_AT: &str = "dev.zyvor.fluxvm.signed-at";
/// Stamped on an image pushed to a public registry with the licence override.
pub const ANN_LICENCE_ACK: &str = "dev.zyvor.fluxvm.licence-ack";
const PAYLOAD_PREFIX: &str = "fluxvm-mac-image-v1\n";

/// Where keys and sidecar signatures live: `FLUXVM_MAC_TRUST`, else `mac-trust` next to the image store. Never inside the
/// store itself (`mac image prune` removes directories it does not know).
pub fn trust_dir(store_root: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_TRUST").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    store_root.parent().unwrap_or(store_root).join("mac-trust")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigRecord {
    pub digest: String,
    /// Base64 Ed25519 signature of [`payload`].
    pub signature: String,
    /// Base64 public key the signer claims. Trusted only when it is in the trust store.
    pub signer: String,
    #[serde(default)]
    pub signed_unix: u64,
}

pub fn payload(digest: &str) -> Vec<u8> {
    format!("{PAYLOAD_PREFIX}{digest}").into_bytes()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn check_digest(digest: &str) -> Result<()> {
    let hex = digest.strip_prefix("sha256:").unwrap_or("");
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "{digest:?} is not a sha256 manifest digest"
    );
    Ok(())
}

/// Sign `digest` with a base64 private key; `public_b64` is the matching public key, recorded so a verifier can name it.
pub fn sign(private_b64: &str, public_b64: &str, digest: &str) -> Result<SigRecord> {
    check_digest(digest)?;
    signing::parse_public_key(public_b64).context("the signer's public key")?;
    Ok(SigRecord {
        digest: digest.into(),
        signature: signing::sign_bytes(private_b64, &payload(digest))?,
        signer: public_b64.trim().into(),
        signed_unix: now_unix(),
    })
}

/// A short, stable name for a public key: the first 16 hex characters of its sha256.
pub fn fingerprint(public_b64: &str) -> String {
    use sha2::{Digest, Sha256};
    let bytes = B64.decode(public_b64).unwrap_or_default();
    Sha256::digest(&bytes)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Some record verifies against a trusted key (named).
    Trusted { signer: String },
    /// No signature at all.
    Unsigned,
    /// Valid signatures exist, but only from keys that are not in the trust store (fingerprints).
    UntrustedSigner { fingerprints: Vec<String> },
    /// Signatures exist and none verifies for this digest.
    Invalid,
}

/// Judge `records` for `digest` against `trusted`. Pure.
pub fn evaluate(digest: &str, records: &[SigRecord], trusted: &[ParsedSigner]) -> Verdict {
    if records.is_empty() {
        return Verdict::Unsigned;
    }
    let msg = payload(digest);
    let mut untrusted = Vec::new();
    for r in records {
        if let Ok(name) = signing::verify_bytes(&msg, &r.signature, trusted) {
            return Verdict::Trusted {
                signer: name.to_string(),
            };
        }
        // Valid under its own claimed key, but that key is not trusted?
        if let Ok(own) = signing::parse_public_key(&r.signer)
            && signing::verify_bytes(&msg, &r.signature, &[("claimed".into(), own)]).is_ok()
        {
            untrusted.push(fingerprint(&r.signer));
        }
    }
    if untrusted.is_empty() {
        Verdict::Invalid
    } else {
        untrusted.sort();
        untrusted.dedup();
        Verdict::UntrustedSigner {
            fingerprints: untrusted,
        }
    }
}

/// Turn a verdict into an error unless it is `Trusted`; returns the signer name.
pub fn require_trusted(reference: &str, v: &Verdict) -> Result<String> {
    match v {
        Verdict::Trusted { signer } => Ok(signer.clone()),
        Verdict::Unsigned => bail!(
            "{reference} has no signature. Sign it (`fluxctl mac push --sign`, or `fluxctl mac sign`), or pull without --verify if you accept an unsigned image"
        ),
        Verdict::UntrustedSigner { fingerprints } => bail!(
            "{reference} is signed, but not by a key you trust (key {}). If you know the signer, add the public key: `fluxctl mac keys trust NAME FILE`",
            fingerprints.join(", ")
        ),
        Verdict::Invalid => bail!(
            "{reference}: a signature is present but it does not verify for this image's digest: refusing it"
        ),
    }
}

/// Signature records out of the annotations of a referrers listing (other artifact types are ignored).
pub fn records_from_referrers(digest: &str, referrers: &[ContentInfo]) -> Vec<SigRecord> {
    referrers
        .iter()
        .filter(|c| c.artifact_type.as_deref() == Some(SIG_TYPE))
        .filter_map(|c| {
            Some(SigRecord {
                digest: digest.into(),
                signature: c.annotations.get(ANN_SIGNATURE)?.clone(),
                signer: c.annotations.get(ANN_SIGNER)?.clone(),
                signed_unix: c
                    .annotations
                    .get(ANN_SIGNED_AT)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

pub fn record_annotations(r: &SigRecord) -> BTreeMap<String, String> {
    BTreeMap::from([
        (ANN_SIGNATURE.to_string(), r.signature.clone()),
        (ANN_SIGNER.to_string(), r.signer.clone()),
        (ANN_SIGNED_AT.to_string(), r.signed_unix.to_string()),
    ])
}

/// Where signatures for a manifest digest are fetched from. The real one asks the registry; tests use a fake.
pub trait SignatureSource {
    /// The manifest digest `reference` currently points at, and the signature records attached to it.
    async fn signatures(&self, reference: &str) -> Result<(String, Vec<SigRecord>)>;
}

pub struct RegistrySource {
    pub creds: Option<fluxvm_image::oci_registry::Credentials>,
}

impl SignatureSource for RegistrySource {
    async fn signatures(&self, reference: &str) -> Result<(String, Vec<SigRecord>)> {
        let rep =
            fluxvm_image::oci_manage::referrers(reference, Some(SIG_TYPE), self.creds.clone())
                .await?;
        let recs = records_from_referrers(&rep.subject, &rep.referrers);
        Ok((rep.subject, recs))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Verified {
    pub digest: String,
    pub signer: String,
    pub records: Vec<SigRecord>,
}

/// Fetch the signatures of `reference` and require a trusted one. Errors when the trust store is empty (nothing could pass).
pub async fn verify_remote<S: SignatureSource>(
    src: &S,
    reference: &str,
    trust: &TrustStore,
) -> Result<Verified> {
    let trusted = trust.parsed()?;
    ensure!(
        !trusted.is_empty(),
        "no trusted keys: add one with `fluxctl mac keys trust NAME FILE`, or create your own with `fluxctl mac keys generate`"
    );
    let (digest, records) = src.signatures(reference).await?;
    let signer = require_trusted(reference, &evaluate(&digest, &records, &trusted))?;
    Ok(Verified {
        digest,
        signer,
        records,
    })
}

/// Names for trusted keys: letters, digits, `.`, `_`, `-`.
fn check_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && !name.starts_with('.'),
        "{name:?} is not a valid key name (letters, digits, . _ -)"
    );
    Ok(())
}

/// The local keypair, trusted public keys and sidecar signatures.
pub struct TrustStore {
    dir: PathBuf,
}

impl TrustStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("trusted"))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn private_path(&self) -> PathBuf {
        self.dir.join("signing.key")
    }

    fn public_path(&self) -> PathBuf {
        self.dir.join("signing.pub")
    }

    /// Create the signing keypair. Refuses to overwrite one. Returns the public key.
    pub fn generate(&self) -> Result<String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        ensure!(
            !self.private_path().exists(),
            "a signing key already exists at {}: remove it first if you really mean to replace it",
            self.private_path().display()
        );
        let (private, public) = signing::generate_keypair();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.private_path())
            .context("writing the private key")?;
        f.write_all(private.as_bytes())?;
        std::fs::write(self.public_path(), format!("{public}\n"))?;
        Ok(public)
    }

    /// Sign `digest` with the local key.
    pub fn sign_digest(&self, digest: &str) -> Result<SigRecord> {
        sign(&self.private_key()?, &self.public_key()?, digest)
    }

    pub fn private_key(&self) -> Result<String> {
        std::fs::read_to_string(self.private_path())
            .map(|s| s.trim().to_string())
            .with_context(|| {
                format!(
                    "no signing key at {}: run `fluxctl mac keys generate`",
                    self.private_path().display()
                )
            })
    }

    pub fn public_key(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.public_path())
            .context("no signing key: run `fluxctl mac keys generate`")?
            .trim()
            .to_string())
    }

    /// Trust `public_b64` under `name`.
    pub fn trust(&self, name: &str, public_b64: &str) -> Result<()> {
        check_name(name)?;
        let key = public_b64.trim();
        signing::parse_public_key(key).context("not an Ed25519 public key (base64, 32 bytes)")?;
        std::fs::write(
            self.dir.join("trusted").join(format!("{name}.pub")),
            format!("{key}\n"),
        )?;
        Ok(())
    }

    pub fn untrust(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let p = self.dir.join("trusted").join(format!("{name}.pub"));
        ensure!(p.is_file(), "no trusted key named {name}");
        std::fs::remove_file(p)?;
        Ok(())
    }

    /// `(name, public key)` of the trusted keys, sorted by name. Your own key counts as `self` once generated.
    pub fn list(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.dir.join("trusted"))?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(name) = n.strip_suffix(".pub") {
                out.push((
                    name.to_string(),
                    std::fs::read_to_string(e.path())?.trim().into(),
                ));
            }
        }
        if let Ok(own) = self.public_key()
            && !out.iter().any(|(_, k)| *k == own)
        {
            out.push(("self".into(), own));
        }
        out.sort();
        Ok(out)
    }

    pub fn parsed(&self) -> Result<Vec<ParsedSigner>> {
        self.list()?
            .into_iter()
            .map(|(n, k)| {
                Ok((
                    n.clone(),
                    signing::parse_public_key(&k).with_context(|| format!("trusted key {n}"))?,
                ))
            })
            .collect()
    }

    fn sig_path(&self, digest: &str) -> Result<PathBuf> {
        check_digest(digest)?;
        Ok(self
            .dir
            .join("signatures")
            .join(format!("{}.json", digest.trim_start_matches("sha256:"))))
    }

    /// Remember signatures for a digest (after a verified pull or a local `--sign`).
    pub fn save_signatures(&self, digest: &str, records: &[SigRecord]) -> Result<()> {
        let p = self.sig_path(digest)?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_vec_pretty(records)?)?;
        Ok(())
    }

    pub fn load_signatures(&self, digest: &str) -> Result<Vec<SigRecord>> {
        match std::fs::read(self.sig_path(digest)?) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Registries where an image is readable by anyone by default.
const PUBLIC_REGISTRIES: &[&str] = &[
    "docker.io",
    "index.docker.io",
    "registry-1.docker.io",
    "registry.hub.docker.com",
    "ghcr.io",
    "quay.io",
    "public.ecr.aws",
    "registry.gitlab.com",
    "mcr.microsoft.com",
    "codeberg.org",
    "registry.k8s.io",
];

/// The host of a push target, lowercase, without a port.
fn registry_host(target: &str) -> String {
    let r = fluxvm_image::oci_registry::ImageRef::parse(target)
        .map(|r| r.registry)
        .unwrap_or_else(|_| target.split('/').next().unwrap_or_default().to_string());
    r.rsplit_once(':')
        .map_or(r.as_str(), |(h, p)| {
            if p.bytes().all(|b| b.is_ascii_digit()) {
                h
            } else {
                &r
            }
        })
        .to_ascii_lowercase()
}

pub fn is_public_registry(target: &str) -> bool {
    let h = registry_host(target);
    PUBLIC_REGISTRIES.contains(&h.as_str())
}

/// The licence guard. `Err` blocks the push; `Ok(Some(text))` is a warning to print (override in use); `Ok(None)` is clear.
pub fn licence_check(target: &str, override_set: bool) -> Result<Option<String>> {
    if !is_public_registry(target) {
        return Ok(None);
    }
    let host = registry_host(target);
    let text = format!(
        "{host} is a well-known public registry. Apple's macOS software licence terms may restrict copying or redistributing macOS \
(including in an image anyone can pull). FluxVM cannot tell what your licence allows. This is not legal advice, and the \
wording of Apple's terms was not verified here: read the licence that applies to you, or use a private registry."
    );
    if override_set {
        Ok(Some(format!(
            "warning: pushing a macOS image to {host} with the licence override. {text}"
        )))
    } else {
        bail!(
            "refusing to push a macOS image to {host}. {text} To push anyway, pass --i-have-checked-the-licence (or set FLUXVM_MAC_ALLOW_PUBLIC_REGISTRY=1)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const D2: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

    fn store() -> (tempfile::TempDir, TrustStore) {
        let d = tempfile::tempdir().unwrap();
        let t = TrustStore::open(d.path().join("trust")).unwrap();
        (d, t)
    }

    #[test]
    fn sign_then_evaluate_is_trusted_only_for_that_digest_and_a_trusted_key() {
        let (_d, t) = store();
        t.generate().unwrap();
        let rec = t.sign_digest(D).unwrap();
        let trusted = t.parsed().unwrap();
        assert_eq!(
            evaluate(D, std::slice::from_ref(&rec), &trusted),
            Verdict::Trusted {
                signer: "self".into()
            }
        );
        // The same signature does not vouch for another digest.
        assert_eq!(
            evaluate(D2, std::slice::from_ref(&rec), &trusted),
            Verdict::Invalid
        );
        // Nobody trusted: valid but untrusted, naming the key.
        match evaluate(D, std::slice::from_ref(&rec), &[]) {
            Verdict::UntrustedSigner { fingerprints } => {
                assert_eq!(fingerprints, vec![fingerprint(&rec.signer)])
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(evaluate(D, &[], &trusted), Verdict::Unsigned);
    }

    #[test]
    fn a_forged_record_claiming_a_trusted_key_is_invalid() {
        let (_d, t) = store();
        t.generate().unwrap();
        let mut rec = t.sign_digest(D).unwrap();
        // Same claimed signer, tampered signature.
        let other = signing::generate_keypair().0;
        rec.signature = signing::sign_bytes(&other, &payload(D)).unwrap();
        assert_eq!(evaluate(D, &[rec], &t.parsed().unwrap()), Verdict::Invalid);
    }

    #[test]
    fn trust_store_round_trip_and_name_checks() {
        let (_d, t) = store();
        let (_, pubkey) = signing::generate_keypair();
        t.trust("ci-bot", &pubkey).unwrap();
        assert_eq!(
            t.list().unwrap(),
            vec![("ci-bot".to_string(), pubkey.clone())]
        );
        assert!(t.trust("../x", &pubkey).is_err());
        assert!(t.trust("ok", "not-a-key").is_err());
        t.untrust("ci-bot").unwrap();
        assert!(t.untrust("ci-bot").is_err());
        assert!(t.list().unwrap().is_empty());
        // Generating twice never replaces the private key.
        t.generate().unwrap();
        assert!(t.generate().is_err());
    }

    #[test]
    fn the_private_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, t) = store();
        t.generate().unwrap();
        let mode = std::fs::metadata(t.private_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn sidecar_signatures_round_trip() {
        let (_d, t) = store();
        t.generate().unwrap();
        let rec = t.sign_digest(D).unwrap();
        assert!(t.load_signatures(D).unwrap().is_empty());
        t.save_signatures(D, std::slice::from_ref(&rec)).unwrap();
        assert_eq!(t.load_signatures(D).unwrap(), vec![rec]);
        assert!(t.load_signatures("sha256:../x").is_err());
    }

    #[test]
    fn referrer_annotations_become_records_and_other_types_are_ignored() {
        let (_d, t) = store();
        t.generate().unwrap();
        let rec = t.sign_digest(D).unwrap();
        let mk = |ty: &str, ann: BTreeMap<String, String>| ContentInfo {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: D2.into(),
            size: 1,
            artifact_type: Some(ty.into()),
            platform: None,
            annotations: ann,
        };
        let got = records_from_referrers(
            D,
            &[
                mk(SIG_TYPE, record_annotations(&rec)),
                mk("application/spdx+json", record_annotations(&rec)),
                mk(SIG_TYPE, BTreeMap::new()),
            ],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].signature, rec.signature);
        assert_eq!(got[0].signed_unix, rec.signed_unix);
    }

    struct Fake(String, Vec<SigRecord>);
    impl SignatureSource for Fake {
        async fn signatures(&self, _: &str) -> Result<(String, Vec<SigRecord>)> {
            Ok((self.0.clone(), self.1.clone()))
        }
    }

    #[tokio::test]
    async fn verify_remote_demands_a_trusted_signature_with_a_fake_registry() {
        let (_d, t) = store();
        // Empty trust store: refused before asking the registry anything useful.
        let e = verify_remote(&Fake(D.into(), vec![]), "r/x:1", &t)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no trusted keys"), "{e}");
        t.generate().unwrap();
        let e = verify_remote(&Fake(D.into(), vec![]), "r/x:1", &t)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no signature"), "{e}");
        let rec = t.sign_digest(D).unwrap();
        let ok = verify_remote(&Fake(D.into(), vec![rec.clone()]), "r/x:1", &t)
            .await
            .unwrap();
        assert_eq!((ok.digest.as_str(), ok.signer.as_str()), (D, "self"));
        // The tag moved to another digest after signing: the old signature does not carry over.
        let e = verify_remote(&Fake(D2.into(), vec![rec]), "r/x:1", &t)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("does not verify"), "{e}");
    }

    #[test]
    fn licence_guard_blocks_public_registries_unless_overridden() {
        for t in [
            "ghcr.io/acme/mac:1",
            "docker.io/acme/mac:1",
            "quay.io/a/b:1",
            "public.ecr.aws/x/y:1",
            "GHCR.IO/acme/mac:1",
        ] {
            let e = licence_check(t, false).unwrap_err().to_string();
            assert!(
                e.contains("not legal advice")
                    && e.contains("--i-have-checked-the-licence")
                    && e.contains("not verified"),
                "{t}: {e}"
            );
            let w = licence_check(t, true).unwrap().unwrap();
            assert!(w.starts_with("warning:"), "{w}");
        }
        for t in [
            "registry.corp.example:5000/mac:1",
            "localhost:5000/mac:1",
            "10.0.0.5:5000/mac:1",
            "myacct.dkr.ecr.eu-west-1.amazonaws.com/mac:1",
        ] {
            assert_eq!(licence_check(t, false).unwrap(), None, "{t}");
        }
    }

    #[test]
    fn digests_must_be_full_sha256() {
        assert!(check_digest(D).is_ok());
        assert!(check_digest("sha256:abc").is_err());
        assert!(check_digest("latest").is_err());
    }
}
