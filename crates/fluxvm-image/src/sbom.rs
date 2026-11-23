// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A software bill of materials read out of an OCI image's layers: no network, no daemon, only the layer blobs already
//! in the local blob store. Layers are walked in order, whiteouts are honoured, and the package databases that
//! survive are read:
//!
//! - Debian/Ubuntu: `var/lib/dpkg/status` and `var/lib/dpkg/status.d/*` (distroless)
//! - Alpine: `lib/apk/db/installed`
//! - Python: `*.dist-info/METADATA`, `*.egg-info/PKG-INFO` under `site-packages` / `dist-packages`
//! - Node: `node_modules/<name>/package.json`
//!
//! Not read: RPM databases (sqlite / Berkeley DB), Go and Rust binaries, Java jars. When an RPM database is present the
//! SBOM says so in `notes` instead of pretending the image has no packages.

use crate::oci_registry::{BlobStore, Compression, Layer};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// Largest single file read from a layer (a dpkg status file of a big image is a few MiB).
const MAX_FILE: u64 = 64 * 1024 * 1024;
/// Most small metadata files kept while walking (python and node packages).
const MAX_FILES: usize = 100_000;

/// The image an SBOM or scan is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub reference: String,
    pub manifest_digest: String,
    pub architecture: String,
    pub layers: Vec<Layer>,
}

impl From<&crate::oci_registry::PulledImage> for Subject {
    fn from(i: &crate::oci_registry::PulledImage) -> Self {
        Self {
            reference: i.reference.clone(),
            manifest_digest: i.manifest_digest.clone(),
            architecture: i.architecture.clone(),
            layers: i.layers.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// `deb`, `apk`, `pypi` or `npm`.
    pub ecosystem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Package URL, e.g. `pkg:deb/debian/openssl@3.0.11-1?arch=arm64`.
    pub purl: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sbom {
    pub reference: String,
    pub manifest_digest: String,
    pub architecture: String,
    /// `ID` and `VERSION_ID` of `/etc/os-release`, when the image has one.
    #[serde(default)]
    pub distro: Option<String>,
    pub packages: Vec<Package>,
    /// What was not read (for example an RPM database).
    #[serde(default)]
    pub notes: Vec<String>,
    pub generated_unix: u64,
}

fn is_site_packages(p: &str) -> bool {
    p.contains("site-packages/") || p.contains("dist-packages/")
}

/// Which files are worth keeping from a layer.
fn wanted(path: &str) -> bool {
    matches!(
        path,
        "var/lib/dpkg/status" | "lib/apk/db/installed" | "etc/os-release" | "usr/lib/os-release"
    ) || path.starts_with("var/lib/dpkg/status.d/")
        || (is_site_packages(path)
            && (path.ends_with(".dist-info/METADATA") || path.ends_with(".egg-info/PKG-INFO")))
        || is_node_manifest(path)
        || is_rpm_db(path)
}

fn is_rpm_db(path: &str) -> bool {
    matches!(
        path,
        "var/lib/rpm/rpmdb.sqlite"
            | "var/lib/rpm/Packages"
            | "usr/lib/sysimage/rpm/rpmdb.sqlite"
            | "usr/lib/sysimage/rpm/Packages"
    )
}

/// `…/node_modules/name/package.json` or `…/node_modules/@scope/name/package.json`, exactly.
fn is_node_manifest(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let n = parts.len();
    if n < 3 || parts[n - 1] != "package.json" {
        return false;
    }
    (parts[n - 3] == "node_modules" && !parts[n - 2].starts_with('@'))
        || (n >= 4 && parts[n - 4] == "node_modules" && parts[n - 3].starts_with('@'))
}

fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.trim_start_matches("./").trim_start_matches('/').to_string()
}

/// Collect the files of interest from each layer in order, applying whiteouts.
fn walk_layers(store: &BlobStore, layers: &[Layer]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for layer in layers {
        let path = store.path(&layer.digest)?;
        let f = std::fs::File::open(&path).with_context(|| {
            format!(
                "layer {} is not in the local blob store ({}); pull the image first",
                layer.digest,
                path.display()
            )
        })?;
        let reader: Box<dyn Read> = match layer.compression {
            Compression::None => Box::new(f),
            Compression::Gzip => Box::new(flate2::read::GzDecoder::new(f)),
            Compression::Zstd => Box::new(zstd::Decoder::new(f)?),
        };
        let mut ar = tar::Archive::new(reader);
        for entry in ar.entries().context("reading a layer")? {
            let mut entry = entry.context("reading a layer entry")?;
            let path = normalize(&entry.path()?);
            let (dir, base) = match path.rsplit_once('/') {
                Some((d, b)) => (d.to_string(), b.to_string()),
                None => (String::new(), path.clone()),
            };
            if base == ".wh..wh..opq" {
                let prefix = format!("{dir}/");
                files.retain(|k, _| !k.starts_with(&prefix));
                continue;
            }
            if let Some(gone) = base.strip_prefix(".wh.") {
                let target = if dir.is_empty() { gone.to_string() } else { format!("{dir}/{gone}") };
                let prefix = format!("{target}/");
                files.retain(|k, _| k != &target && !k.starts_with(&prefix));
                continue;
            }
            if !entry.header().entry_type().is_file() || !wanted(&path) || entry.size() > MAX_FILE {
                continue;
            }
            if files.len() >= MAX_FILES {
                continue;
            }
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            files.insert(path, buf);
        }
    }
    Ok(files)
}

fn field(line: &str, key: &str) -> Option<String> {
    line.strip_prefix(key).map(|v| v.trim().to_string())
}

fn purl(ecosystem: &str, namespace: Option<&str>, name: &str, version: &str, arch: Option<&str>) -> String {
    let ty = match ecosystem {
        "pypi" => "pypi",
        "npm" => "npm",
        other => other,
    };
    let mut s = format!("pkg:{ty}/");
    if let Some(ns) = namespace {
        s.push_str(&enc(ns));
        s.push('/');
    }
    s.push_str(&enc(name));
    s.push('@');
    s.push_str(&enc(version));
    if let Some(a) = arch {
        s.push_str("?arch=");
        s.push_str(&enc(a));
    }
    s
}

/// Percent-encode what a package URL needs encoded (keeps `@` out of names, `/` for npm scopes handled by the caller).
fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub fn parse_dpkg(text: &str, distro_id: &str) -> Vec<Package> {
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let (mut name, mut version, mut arch, mut status) = (None, None, None, None);
        for line in para.lines() {
            if let Some(v) = field(line, "Package:") {
                name = Some(v);
            } else if let Some(v) = field(line, "Version:") {
                version = Some(v);
            } else if let Some(v) = field(line, "Architecture:") {
                arch = Some(v);
            } else if let Some(v) = field(line, "Status:") {
                status = Some(v);
            }
        }
        let (Some(name), Some(version)) = (name, version) else { continue };
        if status.as_deref().is_some_and(|s| !s.ends_with("installed") || s.contains("not-installed")) {
            continue;
        }
        out.push(Package {
            purl: purl("deb", Some(distro_id), &name, &version, arch.as_deref()),
            name,
            version,
            ecosystem: "deb".into(),
            arch,
        });
    }
    out
}

pub fn parse_apk(text: &str) -> Vec<Package> {
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let (mut name, mut version, mut arch) = (None, None, None);
        for line in para.lines() {
            if let Some(v) = line.strip_prefix("P:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("V:") {
                version = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("A:") {
                arch = Some(v.trim().to_string());
            }
        }
        if let (Some(name), Some(version)) = (name, version) {
            out.push(Package {
                purl: purl("apk", Some("alpine"), &name, &version, arch.as_deref()),
                name,
                version,
                ecosystem: "apk".into(),
                arch,
            });
        }
    }
    out
}

fn parse_python(text: &str) -> Option<Package> {
    let (mut name, mut version) = (None, None);
    for line in text.lines() {
        if line.trim().is_empty() {
            break; // headers end at the first blank line
        }
        if let Some(v) = field(line, "Name:") {
            name.get_or_insert(v);
        } else if let Some(v) = field(line, "Version:") {
            version.get_or_insert(v);
        }
    }
    let (name, version) = (name?, version?);
    Some(Package {
        purl: purl("pypi", None, &name.to_ascii_lowercase().replace('_', "-"), &version, None),
        name,
        version,
        ecosystem: "pypi".into(),
        arch: None,
    })
}

fn parse_node(bytes: &[u8]) -> Option<Package> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let version = v.get("version")?.as_str()?.to_string();
    let p = match name.strip_prefix('@').and_then(|r| r.split_once('/')) {
        Some((scope, n)) => format!("pkg:npm/%40{}/{}@{}", enc(scope), enc(n), enc(&version)),
        None => purl("npm", None, &name, &version, None),
    };
    Some(Package { purl: p, name, version, ecosystem: "npm".into(), arch: None })
}

fn os_release(files: &BTreeMap<String, Vec<u8>>) -> (String, Option<String>) {
    let text = files
        .get("etc/os-release")
        .or_else(|| files.get("usr/lib/os-release"))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let get = |k: &str| {
        text.lines().find_map(|l| {
            l.strip_prefix(k)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    let id = get("ID").unwrap_or_default();
    let distro = (!id.is_empty()).then(|| match get("VERSION_ID") {
        Some(v) => format!("{id} {v}"),
        None => id.clone(),
    });
    (if id.is_empty() { "debian".into() } else { id }, distro)
}

/// Build the SBOM of `subject` from its layer blobs. `now` stamps `generated_unix`.
pub fn collect(store: &BlobStore, subject: &Subject, now: u64) -> Result<Sbom> {
    let files = walk_layers(store, &subject.layers)?;
    let (distro_id, distro) = os_release(&files);
    let mut packages = Vec::new();
    let mut notes = Vec::new();
    for (path, bytes) in &files {
        let text = || String::from_utf8_lossy(bytes);
        if path == "var/lib/dpkg/status" || path.starts_with("var/lib/dpkg/status.d/") {
            packages.extend(parse_dpkg(&text(), &distro_id));
        } else if path == "lib/apk/db/installed" {
            packages.extend(parse_apk(&text()));
        } else if path.ends_with(".dist-info/METADATA") || path.ends_with(".egg-info/PKG-INFO") {
            packages.extend(parse_python(&text()));
        } else if path.ends_with("/package.json") {
            packages.extend(parse_node(bytes));
        } else if is_rpm_db(path) {
            notes.push(format!(
                "an RPM database ({path}) is present but this tool cannot read it, so RPM packages are not listed"
            ));
        }
    }
    packages.sort();
    packages.dedup();
    if packages.is_empty() && notes.is_empty() {
        notes.push(
            "no package database found (no dpkg, apk, python or node packages): a scratch or statically linked image has nothing to list"
                .into(),
        );
    }
    Ok(Sbom {
        reference: subject.reference.clone(),
        manifest_digest: subject.manifest_digest.clone(),
        architecture: subject.architecture.clone(),
        distro,
        packages,
        notes,
        generated_unix: now,
    })
}

fn cache_path(state_dir: &Path, digest: &str) -> Option<std::path::PathBuf> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| state_dir.join("oci").join("sbom").join(format!("{hex}.json")))
}

/// The cached SBOM for this manifest digest (layers are immutable, so it never goes stale), else build and cache it.
pub fn load_or_collect(state_dir: &Path, subject: &Subject, now: u64) -> Result<Sbom> {
    let cached = cache_path(state_dir, &subject.manifest_digest);
    if let Some(p) = &cached
        && let Ok(b) = std::fs::read(p)
        && let Ok(mut s) = serde_json::from_slice::<Sbom>(&b)
    {
        s.reference = subject.reference.clone();
        return Ok(s);
    }
    let sbom = collect(&BlobStore::for_state_dir(state_dir), subject, now)?;
    if let Some(p) = cached {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension(format!("part-{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&sbom)?).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
    Ok(sbom)
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Output format of `image sbom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Spdx,
    CycloneDx,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "spdx" | "spdx-json" | "spdx2.3" => Ok(Self::Spdx),
            "cyclonedx" | "cdx" | "cyclonedx-json" => Ok(Self::CycloneDx),
            other => anyhow::bail!("{other:?} is not an SBOM format: use spdx or cyclonedx"),
        }
    }
}

pub fn render(sbom: &Sbom, format: Format, tool_version: &str) -> serde_json::Value {
    let created = rfc3339(sbom.generated_unix);
    let uuid = uuid::Uuid::new_v4();
    match format {
        Format::CycloneDx => serde_json::json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "serialNumber": format!("urn:uuid:{uuid}"),
            "version": 1,
            "metadata": {
                "timestamp": created,
                "tools": {"components": [{"type": "application", "name": "fluxctl", "version": tool_version}]},
                "component": {"type": "container", "name": sbom.reference, "version": sbom.manifest_digest},
            },
            "components": sbom.packages.iter().map(|p| serde_json::json!({
                "type": "library",
                "bom-ref": p.purl,
                "name": p.name,
                "version": p.version,
                "purl": p.purl,
            })).collect::<Vec<_>>(),
        }),
        Format::Spdx => {
            let mut packages = vec![serde_json::json!({
                "SPDXID": "SPDXRef-Image",
                "name": sbom.reference,
                "versionInfo": sbom.manifest_digest,
                "downloadLocation": "NOASSERTION",
                "filesAnalyzed": false,
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": "NOASSERTION",
                "copyrightText": "NOASSERTION",
            })];
            let mut relationships = vec![serde_json::json!({
                "spdxElementId": "SPDXRef-DOCUMENT",
                "relationshipType": "DESCRIBES",
                "relatedSpdxElement": "SPDXRef-Image",
            })];
            for (i, p) in sbom.packages.iter().enumerate() {
                let id = format!("SPDXRef-Package-{i}");
                packages.push(serde_json::json!({
                    "SPDXID": id,
                    "name": p.name,
                    "versionInfo": p.version,
                    "downloadLocation": "NOASSERTION",
                    "filesAnalyzed": false,
                    "licenseConcluded": "NOASSERTION",
                    "licenseDeclared": "NOASSERTION",
                    "copyrightText": "NOASSERTION",
                    "externalRefs": [{
                        "referenceCategory": "PACKAGE-MANAGER",
                        "referenceType": "purl",
                        "referenceLocator": p.purl,
                    }],
                }));
                relationships.push(serde_json::json!({
                    "spdxElementId": "SPDXRef-Image",
                    "relationshipType": "CONTAINS",
                    "relatedSpdxElement": id,
                }));
            }
            serde_json::json!({
                "spdxVersion": "SPDX-2.3",
                "dataLicense": "CC0-1.0",
                "SPDXID": "SPDXRef-DOCUMENT",
                "name": sbom.reference,
                "documentNamespace": format!("https://zyvor.dev/spdx/fluxctl/{uuid}"),
                "creationInfo": {"created": created, "creators": [format!("Tool: fluxctl-{tool_version}")]},
                "packages": packages,
                "relationships": relationships,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (p, c) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(c.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, p, c.as_bytes()).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn subject_with(dir: &Path, layers: &[Vec<u8>]) -> Subject {
        use sha2::{Digest, Sha256};
        let store = BlobStore::for_state_dir(dir);
        let mut ls = Vec::new();
        for l in layers {
            let c = gz(l);
            let d = format!("sha256:{}", hex::encode(Sha256::digest(&c)));
            store.put(&d, &c).unwrap();
            ls.push(Layer { digest: d, size: c.len() as u64, compression: Compression::Gzip, diff_id: String::new() });
        }
        Subject {
            reference: "example.test/app:1".into(),
            manifest_digest: format!("sha256:{}", "ab".repeat(32)),
            architecture: "arm64".into(),
            layers: ls,
        }
    }

    const DPKG: &str = "Package: openssl\nStatus: install ok installed\nArchitecture: arm64\nVersion: 3.0.11-1\n\nPackage: gone\nStatus: deinstall ok config-files\nVersion: 1\n\nPackage: libc6\nStatus: install ok installed\nVersion: 2.36-9\n";

    #[test]
    fn reads_dpkg_apk_python_and_node() {
        let d = tempfile::tempdir().unwrap();
        let l1 = tar_of(&[
            ("etc/os-release", "ID=debian\nVERSION_ID=\"12\"\n"),
            ("var/lib/dpkg/status", DPKG),
            ("usr/lib/python3/site-packages/requests-2.31.0.dist-info/METADATA", "Metadata-Version: 2.1\nName: requests\nVersion: 2.31.0\n\nbody Name: x\n"),
            ("app/node_modules/left-pad/package.json", "{\"name\":\"left-pad\",\"version\":\"1.3.0\"}"),
            ("app/node_modules/left-pad/node_modules/deep/package.json", "{\"name\":\"deep\",\"version\":\"2.0.0\"}"),
        ]);
        let l2 = tar_of(&[("lib/apk/db/installed", "P:musl\nV:1.2.4-r2\nA:aarch64\n\nP:busybox\nV:1.36.1-r5\n")]);
        let s = subject_with(d.path(), &[l1, l2]);
        let sbom = collect(&BlobStore::for_state_dir(d.path()), &s, 0).unwrap();
        let names: Vec<_> = sbom.packages.iter().map(|p| format!("{}:{}@{}", p.ecosystem, p.name, p.version)).collect();
        assert!(names.contains(&"deb:openssl@3.0.11-1".to_string()), "{names:?}");
        assert!(names.contains(&"deb:libc6@2.36-9".to_string()));
        assert!(!names.iter().any(|n| n.contains("gone")), "removed packages are skipped");
        assert!(names.contains(&"apk:musl@1.2.4-r2".to_string()));
        assert!(names.contains(&"pypi:requests@2.31.0".to_string()));
        assert!(names.contains(&"npm:left-pad@1.3.0".to_string()));
        assert!(names.contains(&"npm:deep@2.0.0".to_string()));
        assert_eq!(sbom.distro.as_deref(), Some("debian 12"));
        let ssl = sbom.packages.iter().find(|p| p.name == "openssl").unwrap();
        assert_eq!(ssl.purl, "pkg:deb/debian/openssl@3.0.11-1?arch=arm64");
    }

    #[test]
    fn a_whiteout_removes_a_package_database_from_an_earlier_layer() {
        let d = tempfile::tempdir().unwrap();
        let l1 = tar_of(&[("lib/apk/db/installed", "P:musl\nV:1.2.4-r2\n")]);
        let l2 = tar_of(&[("lib/apk/db/.wh.installed", "")]);
        let s = subject_with(d.path(), &[l1, l2]);
        let sbom = collect(&BlobStore::for_state_dir(d.path()), &s, 0).unwrap();
        assert!(sbom.packages.is_empty());
        assert!(sbom.notes[0].contains("no package database"));
    }

    #[test]
    fn a_later_layer_replaces_the_database() {
        let d = tempfile::tempdir().unwrap();
        let l1 = tar_of(&[("lib/apk/db/installed", "P:musl\nV:1.0\n")]);
        let l2 = tar_of(&[("lib/apk/db/installed", "P:musl\nV:2.0\n")]);
        let s = subject_with(d.path(), &[l1, l2]);
        let sbom = collect(&BlobStore::for_state_dir(d.path()), &s, 0).unwrap();
        assert_eq!(sbom.packages.len(), 1);
        assert_eq!(sbom.packages[0].version, "2.0");
    }

    #[test]
    fn an_rpm_database_is_reported_not_ignored() {
        let d = tempfile::tempdir().unwrap();
        let s = subject_with(d.path(), &[tar_of(&[("var/lib/rpm/rpmdb.sqlite", "x")])]);
        let sbom = collect(&BlobStore::for_state_dir(d.path()), &s, 0).unwrap();
        assert!(sbom.notes.iter().any(|n| n.contains("RPM")));
    }

    #[test]
    fn a_missing_layer_says_to_pull() {
        let d = tempfile::tempdir().unwrap();
        let mut s = subject_with(d.path(), &[tar_of(&[("a", "b")])]);
        s.layers[0].digest = format!("sha256:{}", "cd".repeat(32));
        let e = collect(&BlobStore::for_state_dir(d.path()), &s, 0).unwrap_err().to_string();
        assert!(e.contains("pull the image first"), "{e}");
    }

    #[test]
    fn spdx_and_cyclonedx_have_the_required_fields() {
        let d = tempfile::tempdir().unwrap();
        let s = subject_with(d.path(), &[tar_of(&[("var/lib/dpkg/status", DPKG)])]);
        let sbom = collect(&BlobStore::for_state_dir(d.path()), &s, 1_700_000_000).unwrap();
        let cdx = render(&sbom, Format::CycloneDx, "1.0");
        assert_eq!(cdx["bomFormat"], "CycloneDX");
        assert_eq!(cdx["specVersion"], "1.5");
        assert_eq!(cdx["metadata"]["timestamp"], "2023-11-14T22:13:20Z");
        assert_eq!(cdx["components"].as_array().unwrap().len(), 2);
        let spdx = render(&sbom, Format::Spdx, "1.0");
        assert_eq!(spdx["spdxVersion"], "SPDX-2.3");
        assert_eq!(spdx["packages"].as_array().unwrap().len(), 3);
        assert_eq!(spdx["relationships"].as_array().unwrap().len(), 3);
        assert!(Format::parse("xml").is_err());
    }

    #[test]
    fn the_sbom_is_cached_by_manifest_digest() {
        let d = tempfile::tempdir().unwrap();
        let s = subject_with(d.path(), &[tar_of(&[("lib/apk/db/installed", "P:a\nV:1\n")])]);
        let first = load_or_collect(d.path(), &s, 5).unwrap();
        // Layers gone: the cache still answers.
        std::fs::remove_dir_all(d.path().join("oci").join("blobs")).unwrap();
        let again = load_or_collect(d.path(), &s, 9).unwrap();
        assert_eq!(first.packages, again.packages);
        assert_eq!(again.generated_unix, 5);
    }
}
