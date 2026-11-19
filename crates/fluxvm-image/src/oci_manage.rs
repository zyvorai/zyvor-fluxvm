// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Registry management over the OCI distribution HTTP API: inspect, list tags, retag, delete, referrers (OCI 1.1
//! API with the tag-schema fallback), attach an artifact to a subject, and build multi-arch indexes. Client-side
//! only; reuses [`ImageRef`] and [`Credentials`] from `oci_registry` and the same bearer-token flow. See
//! docs/oci-registry-tools.md.

use crate::oci_registry::{
    Credentials, ImageRef, parse_bearer_challenge, read_capped, sha256_digest, validate_digest,
};
use anyhow::{Context, Result, bail};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MT_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const MT_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MT_EMPTY: &str = "application/vnd.oci.empty.v1+json";
const ANN_TITLE: &str = "org.opencontainers.image.title";
const ANN_CREATED: &str = "org.opencontainers.image.created";

const MAX_JSON: u64 = 4 * 1024 * 1024;
/// Largest file `attach` will upload (it is sent as one monolithic blob).
pub const MAX_ATTACH_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TAG_PAGES: usize = 1000;

fn accept_all() -> String {
    [
        MT_OCI_INDEX,
        MT_OCI_MANIFEST,
        MT_DOCKER_LIST,
        MT_DOCKER_MANIFEST,
    ]
    .join(", ")
}

fn is_index_type(mt: &str) -> bool {
    mt == MT_OCI_INDEX || mt == MT_DOCKER_LIST
}

/// An authenticated client for one registry.
pub struct Registry {
    http: reqwest::Client,
    creds: Option<Credentials>,
    token: Option<String>,
}

/// A manifest as served: raw bytes (so a retag is byte-identical), media type and digest.
#[derive(Debug, Clone)]
pub struct RawManifest {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub digest: String,
}

impl Registry {
    pub fn new(creds: Option<Credentials>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("fluxvm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
            .build()
            .context("building the registry HTTP client")?;
        Ok(Self {
            http,
            creds,
            token: None,
        })
    }

    /// Sends one request, answering a 401 challenge once with a bearer token. Any other status is returned as is.
    async fn send(
        &mut self,
        method: Method,
        url: &str,
        accept: Option<&str>,
        body: Option<(&str, &[u8])>,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let mut req = self.http.request(method.clone(), url);
            if let Some(a) = accept {
                req = req.header("Accept", a);
            }
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            if let Some((ct, b)) = body {
                req = req.header("Content-Type", ct).body(b.to_vec());
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("{method} {url}"))?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let challenge = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                self.token = Some(self.fetch_token(&challenge).await?);
                continue;
            }
            return Ok(resp);
        }
        bail!(
            "{method} {url}: still unauthorized after a token (check the credentials and that they may do this)"
        )
    }

    async fn fetch_token(&self, challenge: &str) -> Result<String> {
        let params = parse_bearer_challenge(challenge)
            .with_context(|| format!("registry asked for unsupported auth: {challenge:?}"))?;
        let realm = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.clone())
            .context("auth challenge has no realm")?;
        let query: Vec<(String, String)> =
            params.into_iter().filter(|(k, _)| k != "realm").collect();
        let mut req = self.http.get(&realm).query(&query);
        if let Some(c) = &self.creds {
            req = req.basic_auth(&c.username, Some(&c.password));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("token request to {realm}"))?;
        if !resp.status().is_success() {
            bail!("token request to {realm}: HTTP {}", resp.status());
        }
        #[derive(Deserialize)]
        struct Tok {
            token: Option<String>,
            access_token: Option<String>,
        }
        let body = read_capped(resp, MAX_JSON).await?;
        let t: Tok = serde_json::from_slice(&body).context("parsing token response")?;
        t.token
            .or(t.access_token)
            .context("token response has no token")
    }

    pub async fn get_manifest(&mut self, r: &ImageRef, reference: &str) -> Result<RawManifest> {
        let url = format!("{}/manifests/{reference}", r.base_url());
        let resp = self
            .send(Method::GET, &url, Some(&accept_all()), None)
            .await?;
        if !resp.status().is_success() {
            return Err(http_error("GET", &url, resp).await);
        }
        let header_ct = content_type(&resp);
        let bytes = read_capped(resp, MAX_JSON).await?;
        let digest = sha256_digest(&bytes);
        if reference.starts_with("sha256:") && digest != reference {
            bail!("manifest digest mismatch: asked for {reference}, got {digest}");
        }
        let media_type = match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v
                .get("mediaType")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(header_ct),
            Err(_) => header_ct,
        }
        .unwrap_or_else(|| MT_OCI_MANIFEST.to_string());
        Ok(RawManifest {
            bytes,
            media_type,
            digest,
        })
    }

    /// PUTs a manifest under a tag or digest; returns the digest of what was stored and the `OCI-Subject` header.
    async fn put_manifest(
        &mut self,
        r: &ImageRef,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<(String, Option<String>)> {
        let url = format!("{}/manifests/{reference}", r.base_url());
        let resp = self
            .send(Method::PUT, &url, None, Some((media_type, bytes)))
            .await?;
        if !resp.status().is_success() {
            return Err(http_error("PUT", &url, resp).await);
        }
        let subject = resp
            .headers()
            .get("oci-subject")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok((sha256_digest(bytes), subject))
    }

    async fn get_blob(&mut self, r: &ImageRef, d: &str, size: u64) -> Result<Vec<u8>> {
        if size > MAX_JSON {
            bail!("blob {d} is {size} bytes, too large to read as JSON");
        }
        let url = format!("{}/blobs/{d}", r.base_url());
        let resp = self.send(Method::GET, &url, Some("*/*"), None).await?;
        if !resp.status().is_success() {
            return Err(http_error("GET", &url, resp).await);
        }
        let bytes = read_capped(resp, MAX_JSON).await?;
        if sha256_digest(&bytes) != d {
            bail!("blob digest mismatch for {d}");
        }
        Ok(bytes)
    }

    /// Monolithic blob upload (`POST` then `PUT ?digest=`). Skips the upload when the blob already exists.
    async fn put_blob(&mut self, r: &ImageRef, bytes: &[u8]) -> Result<String> {
        let digest = sha256_digest(bytes);
        let head = format!("{}/blobs/{digest}", r.base_url());
        if self
            .send(Method::HEAD, &head, None, None)
            .await?
            .status()
            .is_success()
        {
            return Ok(digest);
        }
        let start = format!("{}/blobs/uploads/", r.base_url());
        let resp = self.send(Method::POST, &start, None, None).await?;
        if !resp.status().is_success() {
            return Err(http_error("POST", &start, resp).await);
        }
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .context("registry gave no upload Location")?
            .to_string();
        let mut url = resolve_url(r, &loc);
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("digest=");
        url.push_str(&digest);
        let resp = self
            .send(
                Method::PUT,
                &url,
                None,
                Some(("application/octet-stream", bytes)),
            )
            .await?;
        if !resp.status().is_success() {
            return Err(http_error("PUT", &url, resp).await);
        }
        Ok(digest)
    }
}

fn content_type(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// An absolute URL for a `Location`/`Link` value, which registries may send relative.
fn resolve_url(r: &ImageRef, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let origin = r
        .base_url()
        .split("/v2/")
        .next()
        .unwrap_or_default()
        .to_string();
    if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else {
        format!("{origin}/{loc}")
    }
}

/// Turns a failed response into an error, including the registry's own message when it sent one.
async fn http_error(method: &str, url: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = read_capped(resp, 16 * 1024).await.unwrap_or_default();
    let msg = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/errors/0").map(|e| {
                format!(
                    "{} {}",
                    e["code"].as_str().unwrap_or(""),
                    e["message"].as_str().unwrap_or("")
                )
            })
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match msg {
        Some(m) => anyhow::anyhow!("{method} {url}: HTTP {status}: {m}"),
        None => anyhow::anyhow!("{method} {url}: HTTP {status}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// inspect

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlatformInfo {
    pub architecture: String,
    pub os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(rename = "os_version", skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
}

impl PlatformInfo {
    fn from_value(v: &Value) -> Option<Self> {
        let arch = v.get("architecture")?.as_str()?;
        let os = v.get("os")?.as_str()?;
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            architecture: arch.into(),
            os: os.into(),
            variant: s("variant"),
            os_version: s("os.version"),
        })
    }

    pub fn label(&self) -> String {
        let mut s = format!("{}/{}", self.os, self.architecture);
        if let Some(v) = &self.variant {
            s.push('/');
            s.push_str(v);
        }
        s
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentInfo {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformInfo>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl ContentInfo {
    fn from_value(v: &Value) -> Self {
        Self {
            media_type: v["mediaType"].as_str().unwrap_or_default().into(),
            digest: v["digest"].as_str().unwrap_or_default().into(),
            size: v["size"].as_u64().unwrap_or(0),
            artifact_type: v["artifactType"].as_str().map(str::to_owned),
            platform: v.get("platform").and_then(PlatformInfo::from_value),
            annotations: string_map(v.get("annotations")),
        }
    }
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ConfigInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    pub diff_ids: usize,
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_config(v: &Value) -> ConfigInfo {
    let c = v.get("config");
    let opt = |k: &str| {
        c.and_then(|c| c.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    ConfigInfo {
        platform: PlatformInfo::from_value(v),
        created: v.get("created").and_then(Value::as_str).map(str::to_owned),
        entrypoint: str_list(c.and_then(|c| c.get("Entrypoint"))),
        cmd: str_list(c.and_then(|c| c.get("Cmd"))),
        env: str_list(c.and_then(|c| c.get("Env"))),
        user: opt("User"),
        working_dir: opt("WorkingDir"),
        labels: string_map(c.and_then(|c| c.get("Labels"))),
        diff_ids: v
            .pointer("/rootfs/diff_ids")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectReport {
    pub reference: String,
    pub digest: String,
    pub media_type: String,
    /// `image` for a manifest, `index` for a multi-platform index.
    pub kind: String,
    /// Size of the manifest document itself.
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<ContentInfo>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ContentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_detail: Option<ConfigInfo>,
    pub layers: Vec<ContentInfo>,
    /// Sum of the layer sizes (compressed).
    pub layers_size: u64,
    pub manifests: Vec<ContentInfo>,
}

pub async fn inspect(reference: &str, creds: Option<Credentials>) -> Result<InspectReport> {
    let r = ImageRef::parse(reference)?;
    let mut reg = Registry::new(creds)?;
    let m = reg.get_manifest(&r, &r.reference).await?;
    let v: Value = serde_json::from_slice(&m.bytes).context("parsing manifest")?;
    let index = is_index_type(&m.media_type) || v.get("manifests").is_some();
    let layers: Vec<ContentInfo> = v["layers"]
        .as_array()
        .map(|a| a.iter().map(ContentInfo::from_value).collect())
        .unwrap_or_default();
    let manifests: Vec<ContentInfo> = v["manifests"]
        .as_array()
        .map(|a| a.iter().map(ContentInfo::from_value).collect())
        .unwrap_or_default();
    let config = v.get("config").map(ContentInfo::from_value);
    let mut config_detail = None;
    if let Some(c) = &config
        && c.media_type.contains("config")
        && c.size <= MAX_JSON
        && validate_digest(&c.digest).is_ok()
        && let Ok(bytes) = reg.get_blob(&r, &c.digest, c.size).await
        && let Ok(cv) = serde_json::from_slice::<Value>(&bytes)
    {
        config_detail = Some(parse_config(&cv));
    }
    Ok(InspectReport {
        reference: r.to_string(),
        digest: m.digest,
        media_type: m.media_type,
        kind: if index { "index" } else { "image" }.into(),
        size: m.bytes.len() as u64,
        artifact_type: v["artifactType"].as_str().map(str::to_owned),
        subject: v.get("subject").map(ContentInfo::from_value),
        annotations: string_map(v.get("annotations")),
        layers_size: layers.iter().map(|l| l.size).sum(),
        config,
        config_detail,
        layers,
        manifests,
    })
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", U[i])
    }
}

/// A readable rendering of an [`InspectReport`].
pub fn render_inspect(r: &InspectReport) -> String {
    let mut o = String::new();
    let mut line = |k: &str, v: &str| o.push_str(&format!("{k:<14}{v}\n"));
    line("Reference", &r.reference);
    line("Digest", &r.digest);
    line("Type", &format!("{} ({})", r.kind, r.media_type));
    line("Manifest size", &human_size(r.size));
    if let Some(a) = &r.artifact_type {
        line("Artifact type", a);
    }
    if let Some(s) = &r.subject {
        line("Subject", &s.digest);
    }
    if let Some(cd) = &r.config_detail {
        if let Some(p) = &cd.platform {
            line("Platform", &p.label());
        }
        if let Some(c) = &cd.created {
            line("Created", c);
        }
        if !cd.entrypoint.is_empty() {
            line("Entrypoint", &cd.entrypoint.join(" "));
        }
        if !cd.cmd.is_empty() {
            line("Cmd", &cd.cmd.join(" "));
        }
        if let Some(u) = &cd.user {
            line("User", u);
        }
        if let Some(w) = &cd.working_dir {
            line("WorkingDir", w);
        }
        for e in &cd.env {
            line("Env", e);
        }
        for (k, v) in &cd.labels {
            line("Label", &format!("{k}={v}"));
        }
    }
    if let Some(c) = &r.config {
        line(
            "Config",
            &format!("{} {} ({})", c.digest, c.media_type, human_size(c.size)),
        );
    }
    for (k, v) in &r.annotations {
        line("Annotation", &format!("{k}={v}"));
    }
    if !r.layers.is_empty() {
        o.push_str(&format!(
            "Layers ({}, {} compressed):\n",
            r.layers.len(),
            human_size(r.layers_size)
        ));
        for l in &r.layers {
            o.push_str(&format!(
                "  {}  {:>10}  {}\n",
                l.digest,
                human_size(l.size),
                l.media_type
            ));
        }
    }
    if !r.manifests.is_empty() {
        o.push_str(&format!("Manifests ({}):\n", r.manifests.len()));
        for m in &r.manifests {
            let p = m
                .platform
                .as_ref()
                .map_or_else(|| "-".to_string(), PlatformInfo::label);
            o.push_str(&format!(
                "  {}  {:>10}  {:<16}  {}\n",
                m.digest,
                human_size(m.size),
                p,
                m.artifact_type.as_deref().unwrap_or(&m.media_type)
            ));
        }
    }
    o
}

// ---------------------------------------------------------------------------------------------------------------
// tags

/// Every tag of the repository, following `Link: <…>; rel="next"` pages. `page_size` becomes `?n=`.
pub async fn list_tags(
    reference: &str,
    creds: Option<Credentials>,
    page_size: Option<usize>,
) -> Result<Vec<String>> {
    let r = ImageRef::parse(reference)?;
    let mut reg = Registry::new(creds)?;
    let mut url = format!("{}/tags/list", r.base_url());
    if let Some(n) = page_size {
        url.push_str(&format!("?n={n}"));
    }
    let mut tags = Vec::new();
    for _ in 0..MAX_TAG_PAGES {
        let resp = reg
            .send(Method::GET, &url, Some("application/json"), None)
            .await?;
        if !resp.status().is_success() {
            return Err(http_error("GET", &url, resp).await);
        }
        let next = resp
            .headers()
            .get_all("link")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(parse_link_next);
        let body = read_capped(resp, MAX_JSON).await?;
        let v: Value = serde_json::from_slice(&body).context("parsing tag list")?;
        tags.extend(str_list(v.get("tags")));
        match next {
            Some(n) => url = resolve_url(&r, &n),
            None => return Ok(tags),
        }
    }
    bail!("tag list for {r} has more than {MAX_TAG_PAGES} pages")
}

/// The target of `<url>; rel="next"` in a Link header value.
fn parse_link_next(h: &str) -> Option<String> {
    h.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        let is_next = params.split(';').any(|p| {
            p.trim().replace('\'', "\"").replace(' ', "") == "rel=\"next\""
                || p.trim() == "rel=next"
        });
        let t = target.trim();
        (is_next && t.starts_with('<') && t.ends_with('>')).then(|| t[1..t.len() - 1].to_string())
    })
}

// ---------------------------------------------------------------------------------------------------------------
// tag / delete

#[derive(Debug, Clone, Serialize)]
pub struct TagResult {
    pub repository: String,
    pub digest: String,
    pub source: String,
    pub tag: String,
}

fn validate_tag(t: &str) -> Result<()> {
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        bail!(
            "invalid tag {t:?} (letters, digits, '.', '_' and '-', at most 128, no leading '.' or '-')"
        );
    }
    Ok(())
}

/// Points `new_tag` at the manifest `reference` names, by downloading it and uploading the identical bytes.
pub async fn retag(
    reference: &str,
    new_tag: &str,
    creds: Option<Credentials>,
) -> Result<TagResult> {
    let r = ImageRef::parse(reference)?;
    validate_tag(new_tag)?;
    let mut reg = Registry::new(creds)?;
    let m = reg.get_manifest(&r, &r.reference).await?;
    let (digest, _) = reg
        .put_manifest(&r, new_tag, &m.media_type, &m.bytes)
        .await?;
    Ok(TagResult {
        repository: format!("{}/{}", r.registry, r.repository),
        digest,
        source: r.reference,
        tag: new_tag.into(),
    })
}

/// Deletes a manifest by digest. Every tag pointing at it goes too, so a tag reference is refused.
pub async fn delete_manifest(reference: &str, creds: Option<Credentials>) -> Result<String> {
    let r = ImageRef::parse(reference)?;
    if !r.reference.starts_with("sha256:") {
        bail!(
            "refusing to delete {r}: give a digest (repo@sha256:…); deleting a manifest removes every tag that points at it. \
             `fluxctl image inspect {r}` shows the digest"
        );
    }
    let mut reg = Registry::new(creds)?;
    let url = format!("{}/manifests/{}", r.base_url(), r.reference);
    let resp = reg.send(Method::DELETE, &url, None, None).await?;
    match resp.status() {
        s if s.is_success() => Ok(r.reference),
        StatusCode::NOT_FOUND => bail!("{r}: manifest not found (already deleted?)"),
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => bail!(
            "{} refused to delete {r}: HTTP {}. Deletion is disabled on this registry, or these credentials may not delete \
             (registry:2 needs REGISTRY_STORAGE_DELETE_ENABLED=true; GHCR and Docker Hub use their own package/tag deletion APIs)",
            r.registry,
            resp.status()
        ),
        _ => Err(http_error("DELETE", &url, resp).await),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// referrers

fn fallback_tag(digest: &str) -> Result<String> {
    Ok(format!("sha256-{}", validate_digest(digest)?))
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferrersReport {
    pub subject: String,
    /// `api` (OCI 1.1 referrers endpoint) or `tag-schema` (the `sha256-<hex>` index tag).
    pub via: String,
    pub referrers: Vec<ContentInfo>,
}

async fn resolve_digest(reg: &mut Registry, r: &ImageRef) -> Result<String> {
    if r.reference.starts_with("sha256:") {
        return Ok(r.reference.clone());
    }
    Ok(reg.get_manifest(r, &r.reference).await?.digest)
}

fn index_entries(bytes: &[u8]) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_slice(bytes).context("parsing referrers index")?;
    Ok(v["manifests"].as_array().cloned().unwrap_or_default())
}

pub async fn referrers(
    reference: &str,
    artifact_type: Option<&str>,
    creds: Option<Credentials>,
) -> Result<ReferrersReport> {
    let r = ImageRef::parse(reference)?;
    let mut reg = Registry::new(creds)?;
    let subject = resolve_digest(&mut reg, &r).await?;
    let (entries, via) = fetch_referrers(&mut reg, &r, &subject).await?;
    let referrers = entries
        .iter()
        .map(ContentInfo::from_value)
        .filter(|c| artifact_type.is_none_or(|t| c.artifact_type.as_deref() == Some(t)))
        .collect();
    Ok(ReferrersReport {
        subject,
        via: via.into(),
        referrers,
    })
}

async fn fetch_referrers(
    reg: &mut Registry,
    r: &ImageRef,
    subject: &str,
) -> Result<(Vec<Value>, &'static str)> {
    let url = format!("{}/referrers/{subject}", r.base_url());
    let resp = reg
        .send(Method::GET, &url, Some(MT_OCI_INDEX), None)
        .await?;
    if resp.status().is_success() {
        let bytes = read_capped(resp, MAX_JSON).await?;
        return Ok((index_entries(&bytes)?, "api"));
    }
    if resp.status() != StatusCode::NOT_FOUND {
        return Err(http_error("GET", &url, resp).await);
    }
    // No referrers API: fall back to the index at tag `sha256-<hex>`.
    let tag = fallback_tag(subject)?;
    let furl = format!("{}/manifests/{tag}", r.base_url());
    let resp = reg
        .send(Method::GET, &furl, Some(MT_OCI_INDEX), None)
        .await?;
    match resp.status() {
        s if s.is_success() => {
            let bytes = read_capped(resp, MAX_JSON).await?;
            Ok((index_entries(&bytes)?, "tag-schema"))
        }
        StatusCode::NOT_FOUND => Ok((Vec::new(), "tag-schema")),
        _ => Err(http_error("GET", &furl, resp).await),
    }
}

#[derive(Debug, Clone)]
pub struct AttachOptions {
    pub artifact_type: String,
    /// Media type of the file layer; defaults to the artifact type.
    pub layer_media_type: Option<String>,
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachResult {
    pub subject: String,
    pub artifact_digest: String,
    pub artifact_type: String,
    pub blob_digest: String,
    pub size: u64,
    /// `api` when the registry linked the referrer itself (`OCI-Subject`), else `tag-schema` (index updated here).
    pub via: String,
}

/// Publishes `file` as an artifact whose manifest has `subject` set to the manifest `reference` names.
pub async fn attach(
    reference: &str,
    file: &Path,
    opts: &AttachOptions,
    creds: Option<Credentials>,
) -> Result<AttachResult> {
    let r = ImageRef::parse(reference)?;
    if opts.artifact_type.is_empty() || opts.artifact_type.chars().any(char::is_whitespace) {
        bail!("artifact type must be a media type such as application/spdx+json");
    }
    let meta = std::fs::metadata(file).with_context(|| format!("reading {}", file.display()))?;
    if !meta.is_file() || meta.len() > MAX_ATTACH_BYTES {
        bail!(
            "{} must be a regular file of at most {MAX_ATTACH_BYTES} bytes",
            file.display()
        );
    }
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let mut reg = Registry::new(creds)?;
    let target = reg.get_manifest(&r, &r.reference).await?;

    let empty = b"{}";
    reg.put_blob(&r, empty).await?;
    let blob_digest = reg.put_blob(&r, &data).await?;

    let mut layer_ann = BTreeMap::new();
    if let Some(n) = file.file_name().and_then(|n| n.to_str()) {
        layer_ann.insert(ANN_TITLE.to_string(), n.to_string());
    }
    let mut ann = opts.annotations.clone();
    ann.entry(ANN_CREATED.to_string())
        .or_insert_with(rfc3339_now);
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MT_OCI_MANIFEST,
        "artifactType": opts.artifact_type,
        "config": {"mediaType": MT_EMPTY, "digest": sha256_digest(empty), "size": empty.len()},
        "layers": [{
            "mediaType": opts.layer_media_type.as_deref().unwrap_or(&opts.artifact_type),
            "digest": blob_digest,
            "size": data.len(),
            "annotations": layer_ann,
        }],
        "subject": {"mediaType": target.media_type, "digest": target.digest, "size": target.bytes.len()},
        "annotations": ann,
    });
    let bytes = serde_json::to_vec(&manifest)?;
    let artifact_digest = sha256_digest(&bytes);
    let (_, oci_subject) = reg
        .put_manifest(&r, &artifact_digest, MT_OCI_MANIFEST, &bytes)
        .await?;
    let via = if oci_subject.as_deref() == Some(target.digest.as_str()) {
        "api"
    } else {
        // The registry did not link the referrer: maintain the `sha256-<hex>` index ourselves. This is a
        // read-modify-write, so two concurrent attaches to one subject can lose an entry.
        let mut entry = json!({
            "mediaType": MT_OCI_MANIFEST,
            "digest": artifact_digest,
            "size": bytes.len(),
            "artifactType": opts.artifact_type,
        });
        if !ann.is_empty() {
            entry["annotations"] = json!(ann);
        }
        update_fallback_index(&mut reg, &r, &target.digest, entry).await?;
        "tag-schema"
    };
    Ok(AttachResult {
        subject: target.digest,
        artifact_digest,
        artifact_type: opts.artifact_type.clone(),
        blob_digest,
        size: data.len() as u64,
        via: via.into(),
    })
}

async fn update_fallback_index(
    reg: &mut Registry,
    r: &ImageRef,
    subject: &str,
    entry: Value,
) -> Result<()> {
    let tag = fallback_tag(subject)?;
    let url = format!("{}/manifests/{tag}", r.base_url());
    let resp = reg
        .send(Method::GET, &url, Some(MT_OCI_INDEX), None)
        .await?;
    let mut entries = match resp.status() {
        s if s.is_success() => index_entries(&read_capped(resp, MAX_JSON).await?)?,
        StatusCode::NOT_FOUND => Vec::new(),
        _ => return Err(http_error("GET", &url, resp).await),
    };
    let digest = entry["digest"].clone();
    entries.retain(|e| e["digest"] != digest);
    entries.push(entry);
    let index = json!({"schemaVersion": 2, "mediaType": MT_OCI_INDEX, "manifests": entries});
    reg.put_manifest(r, &tag, MT_OCI_INDEX, &serde_json::to_vec(&index)?)
        .await?;
    Ok(())
}

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86400, secs % 86400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------------------------------------------
// index create

#[derive(Debug, Clone, Serialize)]
pub struct IndexResult {
    pub reference: String,
    pub digest: String,
    pub manifests: Vec<ContentInfo>,
}

/// Builds an OCI image index at `target` (a tag) from manifests in the same repository, taking each platform from
/// that manifest's config. Indexes cannot be nested here, and two manifests may not claim the same platform.
pub async fn create_index(
    target: &str,
    manifest_refs: &[String],
    annotations: &BTreeMap<String, String>,
    creds: Option<Credentials>,
) -> Result<IndexResult> {
    let t = ImageRef::parse(target)?;
    if t.reference.starts_with("sha256:") {
        bail!("the index target must be a tag, not a digest");
    }
    validate_tag(&t.reference)?;
    if manifest_refs.is_empty() {
        bail!("give at least one manifest reference");
    }
    let mut reg = Registry::new(creds)?;
    let mut entries: Vec<ContentInfo> = Vec::new();
    for s in manifest_refs {
        let mr = ImageRef::parse(s)?;
        if mr.registry != t.registry || mr.repository != t.repository {
            bail!(
                "{s} is in {}/{}, not {}/{}: an index can only list manifests of its own repository (copy it first)",
                mr.registry,
                mr.repository,
                t.registry,
                t.repository
            );
        }
        let m = reg.get_manifest(&mr, &mr.reference).await?;
        if is_index_type(&m.media_type) {
            bail!("{s} is itself an index; list its platform manifests instead");
        }
        let v: Value = serde_json::from_slice(&m.bytes).context("parsing manifest")?;
        let cfg = ContentInfo::from_value(&v["config"]);
        validate_digest(&cfg.digest).with_context(|| format!("{s}: manifest has no config"))?;
        let cv: Value = serde_json::from_slice(&reg.get_blob(&mr, &cfg.digest, cfg.size).await?)
            .with_context(|| format!("{s}: parsing the image config"))?;
        let platform = PlatformInfo::from_value(&cv).with_context(|| {
            format!("{s}: the image config has no architecture/os, so it has no platform")
        })?;
        if entries
            .iter()
            .any(|e| e.platform.as_ref() == Some(&platform))
        {
            bail!(
                "two manifests are {}; an index needs one per platform",
                platform.label()
            );
        }
        entries.push(ContentInfo {
            media_type: m.media_type,
            digest: m.digest,
            size: m.bytes.len() as u64,
            artifact_type: None,
            platform: Some(platform),
            annotations: BTreeMap::new(),
        });
    }
    let descriptors: Vec<Value> = entries
        .iter()
        .map(|e| {
            let p = e.platform.as_ref().expect("set above");
            let mut platform = json!({"architecture": p.architecture, "os": p.os});
            if let Some(v) = &p.variant {
                platform["variant"] = json!(v);
            }
            if let Some(v) = &p.os_version {
                platform["os.version"] = json!(v);
            }
            json!({"mediaType": e.media_type, "digest": e.digest, "size": e.size, "platform": platform})
        })
        .collect();
    let mut index =
        json!({"schemaVersion": 2, "mediaType": MT_OCI_INDEX, "manifests": descriptors});
    if !annotations.is_empty() {
        index["annotations"] = json!(annotations);
    }
    let bytes = serde_json::to_vec(&index)?;
    let (digest, _) = reg
        .put_manifest(&t, &t.reference, MT_OCI_INDEX, &bytes)
        .await?;
    Ok(IndexResult {
        reference: t.to_string(),
        digest,
        manifests: entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, HeaderValue, Uri},
        response::{IntoResponse, Response},
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct St {
        /// By tag and by digest.
        manifests: HashMap<String, (String, Vec<u8>)>,
        blobs: HashMap<String, Vec<u8>>,
        tags: Vec<String>,
        referrers_api: bool,
        allow_delete: bool,
        log: Vec<String>,
    }

    type Shared = Arc<Mutex<St>>;

    fn resp(code: u16, headers: &[(&str, String)], body: Vec<u8>) -> Response {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        (axum::http::StatusCode::from_u16(code).unwrap(), h, body).into_response()
    }

    async fn handler(
        State(st): State<Shared>,
        method: axum::http::Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let host = headers["host"].to_str().unwrap().to_string();
        let path = uri.path().to_string();
        let query = uri.query().unwrap_or("").to_string();
        let mut st = st.lock().unwrap();
        st.log.push(format!("{method} {path}?{query}"));
        if path == "/token" {
            return resp(200, &[], br#"{"token":"good"}"#.to_vec());
        }
        let authed = headers
            .get("authorization")
            .is_some_and(|v| v == "Bearer good");
        if !authed {
            return resp(
                401,
                &[(
                    "www-authenticate",
                    format!("Bearer realm=\"http://{host}/token\",service=\"mock\""),
                )],
                vec![],
            );
        }
        let Some(rest) = path.strip_prefix("/v2/app/") else {
            return resp(404, &[], vec![]);
        };
        let digest_hdr = |b: &[u8]| ("docker-content-digest", sha256_digest(b));
        match (method.as_str(), rest) {
            ("GET", "tags/list") => {
                let last = query
                    .split('&')
                    .find_map(|p| p.strip_prefix("last="))
                    .map(str::to_owned);
                let n: usize = query
                    .split('&')
                    .find_map(|p| p.strip_prefix("n="))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(usize::MAX);
                let start = last
                    .and_then(|l| st.tags.iter().position(|t| *t == l).map(|i| i + 1))
                    .unwrap_or(0);
                let page: Vec<_> = st.tags.iter().skip(start).take(n).cloned().collect();
                let more = start + page.len() < st.tags.len();
                let mut h = vec![];
                if more {
                    h.push((
                        "link",
                        format!(
                            "</v2/app/tags/list?last={}&n={n}>; rel=\"next\"",
                            page.last().unwrap()
                        ),
                    ));
                }
                resp(
                    200,
                    &h,
                    serde_json::to_vec(&json!({"name": "app", "tags": page})).unwrap(),
                )
            }
            ("POST", "blobs/uploads/") => resp(
                202,
                &[("location", "/v2/app/blobs/uploads/u1".into())],
                vec![],
            ),
            ("PUT", "blobs/uploads/u1") => {
                let d = query
                    .strip_prefix("digest=")
                    .unwrap_or_default()
                    .to_string();
                if sha256_digest(&body) != d {
                    return resp(400, &[], vec![]);
                }
                st.blobs.insert(d, body.to_vec());
                resp(201, &[], vec![])
            }
            (m, r) if r.starts_with("blobs/") => {
                let d = &r["blobs/".len()..];
                match (m, st.blobs.get(d)) {
                    ("HEAD", Some(_)) => resp(200, &[], vec![]),
                    ("GET", Some(b)) => resp(200, &[], b.clone()),
                    _ => resp(404, &[], vec![]),
                }
            }
            ("GET", r) if r.starts_with("referrers/") => {
                if !st.referrers_api {
                    return resp(404, &[], vec![]);
                }
                let d = &r["referrers/".len()..];
                let mut entries = vec![];
                for (k, (_, b)) in &st.manifests {
                    let v: Value = serde_json::from_slice(b).unwrap_or_default();
                    if k.starts_with("sha256:") && v["subject"]["digest"] == d {
                        entries.push(json!({
                            "mediaType": MT_OCI_MANIFEST, "digest": k, "size": b.len(),
                            "artifactType": v["artifactType"],
                        }));
                    }
                }
                resp(
                    200,
                    &[("content-type", MT_OCI_INDEX.into())],
                    serde_json::to_vec(
                        &json!({"schemaVersion": 2, "mediaType": MT_OCI_INDEX, "manifests": entries}),
                    )
                    .unwrap(),
                )
            }
            ("GET", r) if r.starts_with("manifests/") => {
                match st.manifests.get(&r["manifests/".len()..]) {
                    Some((ct, b)) => resp(
                        200,
                        &[("content-type", ct.clone()), digest_hdr(b)],
                        b.clone(),
                    ),
                    None => resp(
                        404,
                        &[],
                        br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"nope"}]}"#.to_vec(),
                    ),
                }
            }
            ("PUT", r) if r.starts_with("manifests/") => {
                let key = r["manifests/".len()..].to_string();
                let ct = headers["content-type"].to_str().unwrap().to_string();
                let d = sha256_digest(&body);
                let v: Value = serde_json::from_slice(&body).unwrap();
                let mut h = vec![("docker-content-digest", d.clone())];
                if st.referrers_api
                    && let Some(s) = v["subject"]["digest"].as_str()
                {
                    h.push(("oci-subject", s.to_string()));
                }
                st.manifests.insert(d.clone(), (ct.clone(), body.to_vec()));
                st.manifests.insert(key, (ct, body.to_vec()));
                resp(201, &h, vec![])
            }
            ("DELETE", r) if r.starts_with("manifests/") => {
                if !st.allow_delete {
                    return resp(
                        405,
                        &[],
                        br#"{"errors":[{"code":"UNSUPPORTED","message":"delete disabled"}]}"#
                            .to_vec(),
                    );
                }
                match st.manifests.remove(&r["manifests/".len()..]) {
                    Some(_) => resp(202, &[], vec![]),
                    None => resp(404, &[], vec![]),
                }
            }
            _ => resp(404, &[], vec![]),
        }
    }

    async fn serve(st: St) -> (String, Shared) {
        let st = Arc::new(Mutex::new(st));
        let app = Router::new().fallback(handler).with_state(st.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (addr, st)
    }

    /// Stores an image manifest (and its config) for `arch`, tagged `tag`; returns its digest.
    fn put_image(st: &Shared, arch: &str, tag: &str) -> String {
        let mut s = st.lock().unwrap();
        let cfg = serde_json::to_vec(&json!({
            "architecture": arch, "os": "linux", "created": "2026-01-02T03:04:05Z",
            "config": {"Cmd": ["/bin/app"], "Env": ["A=1"], "Labels": {"k": "v"}},
            "rootfs": {"type": "layers", "diff_ids": [sha256_digest(b"tar")]}
        }))
        .unwrap();
        let cd = sha256_digest(&cfg);
        s.blobs.insert(cd.clone(), cfg.clone());
        let layer = format!("layer-{arch}").into_bytes();
        let m = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": MT_OCI_MANIFEST,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": cfg.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": sha256_digest(&layer), "size": layer.len()}],
            "annotations": {"org.opencontainers.image.version": "1"}
        }))
        .unwrap();
        let d = sha256_digest(&m);
        s.manifests
            .insert(d.clone(), (MT_OCI_MANIFEST.into(), m.clone()));
        s.manifests.insert(tag.into(), (MT_OCI_MANIFEST.into(), m));
        d
    }

    #[tokio::test]
    async fn tags_follow_link_pagination() {
        let tags: Vec<String> = ["a", "b", "c", "d", "e"].map(String::from).to_vec();
        let (addr, st) = serve(St {
            tags: tags.clone(),
            ..Default::default()
        })
        .await;
        let got = list_tags(&format!("{addr}/app"), None, Some(2))
            .await
            .unwrap();
        assert_eq!(got, tags);
        let pages = st
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.starts_with("GET /v2/app/tags/list"))
            .count();
        // 3 pages, plus the unauthenticated first attempt.
        assert_eq!(pages, 4);
    }

    #[test]
    fn link_header_parsing() {
        assert_eq!(
            parse_link_next("</v2/x/tags/list?last=b&n=2>; rel=\"next\"").as_deref(),
            Some("/v2/x/tags/list?last=b&n=2")
        );
        assert_eq!(parse_link_next("<http://a/b>; rel=\"prev\""), None);
        assert_eq!(parse_link_next(""), None);
    }

    #[tokio::test]
    async fn inspect_image_and_index() {
        let (addr, st) = serve(St::default()).await;
        let d = put_image(&st, "arm64", "v1");
        let r = inspect(&format!("{addr}/app:v1"), None).await.unwrap();
        assert_eq!((r.kind.as_str(), r.digest.as_str()), ("image", d.as_str()));
        assert_eq!(r.layers.len(), 1);
        assert_eq!(r.layers_size, "layer-arm64".len() as u64);
        let cd = r.config_detail.as_ref().unwrap();
        assert_eq!(cd.platform.as_ref().unwrap().label(), "linux/arm64");
        assert_eq!(cd.cmd, ["/bin/app"]);
        assert_eq!(cd.labels["k"], "v");
        assert_eq!(r.annotations["org.opencontainers.image.version"], "1");
        let text = render_inspect(&r);
        assert!(text.contains("linux/arm64") && text.contains(&d));

        let d2 = put_image(&st, "amd64", "v1-amd64");
        create_index(
            &format!("{addr}/app:multi"),
            &[format!("{addr}/app:v1"), format!("{addr}/app@{d2}")],
            &BTreeMap::new(),
            None,
        )
        .await
        .unwrap();
        let r = inspect(&format!("{addr}/app:multi"), None).await.unwrap();
        assert_eq!(r.kind, "index");
        assert_eq!(r.manifests.len(), 2);
        assert!(render_inspect(&r).contains("linux/amd64"));
    }

    #[tokio::test]
    async fn retag_copies_identical_bytes() {
        let (addr, st) = serve(St::default()).await;
        let d = put_image(&st, "arm64", "v1");
        let out = retag(&format!("{addr}/app:v1"), "stable", None)
            .await
            .unwrap();
        assert_eq!(out.digest, d);
        let s = st.lock().unwrap();
        assert_eq!(s.manifests["stable"].1, s.manifests["v1"].1);
        drop(s);
        assert!(
            retag(&format!("{addr}/app:v1"), "bad tag", None)
                .await
                .is_err()
        );
        assert!(retag(&format!("{addr}/app:nope"), "x", None).await.is_err());
    }

    #[tokio::test]
    async fn delete_by_digest_and_forbidden() {
        let (addr, st) = serve(St::default()).await;
        let d = put_image(&st, "arm64", "v1");
        // A tag is refused client-side without any DELETE request.
        let e = delete_manifest(&format!("{addr}/app:v1"), None)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("give a digest"), "{e:#}");
        assert!(
            !st.lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l.starts_with("DELETE"))
        );
        // Registry forbids.
        let e = delete_manifest(&format!("{addr}/app@{d}"), None)
            .await
            .unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("refused to delete") && msg.contains("DELETE_ENABLED"),
            "{msg}"
        );
        // Allowed.
        st.lock().unwrap().allow_delete = true;
        assert_eq!(
            delete_manifest(&format!("{addr}/app@{d}"), None)
                .await
                .unwrap(),
            d
        );
        assert!(!st.lock().unwrap().manifests.contains_key(&d));
        let e = delete_manifest(&format!("{addr}/app@{d}"), None)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("not found"));
    }

    #[tokio::test]
    async fn attach_and_list_via_referrers_api() {
        let (addr, st) = serve(St {
            referrers_api: true,
            ..Default::default()
        })
        .await;
        let subject = put_image(&st, "arm64", "v1");
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sbom.spdx.json");
        std::fs::write(&f, br#"{"spdxVersion":"SPDX-2.3"}"#).unwrap();
        let opts = AttachOptions {
            artifact_type: "application/spdx+json".into(),
            layer_media_type: None,
            annotations: BTreeMap::from([("k".to_string(), "v".to_string())]),
        };
        let out = attach(&format!("{addr}/app:v1"), &f, &opts, None)
            .await
            .unwrap();
        assert_eq!(out.via, "api");
        assert_eq!(out.subject, subject);
        // The stored artifact manifest has the subject and the file as its layer.
        let am: Value =
            serde_json::from_slice(&st.lock().unwrap().manifests[&out.artifact_digest].1).unwrap();
        assert_eq!(am["subject"]["digest"], subject);
        assert_eq!(am["artifactType"], "application/spdx+json");
        assert_eq!(am["layers"][0]["annotations"][ANN_TITLE], "sbom.spdx.json");
        assert_eq!(am["annotations"]["k"], "v");
        assert!(st.lock().unwrap().blobs.contains_key(&out.blob_digest));
        // No fallback tag was written.
        assert!(
            !st.lock()
                .unwrap()
                .manifests
                .keys()
                .any(|k| k.starts_with("sha256-"))
        );

        let rep = referrers(&format!("{addr}/app:v1"), None, None)
            .await
            .unwrap();
        assert_eq!(rep.via, "api");
        assert_eq!(rep.subject, subject);
        assert_eq!(rep.referrers.len(), 1);
        assert_eq!(rep.referrers[0].digest, out.artifact_digest);
        assert_eq!(
            rep.referrers[0].artifact_type.as_deref(),
            Some("application/spdx+json")
        );
        let none = referrers(&format!("{addr}/app:v1"), Some("application/vnd.x"), None)
            .await
            .unwrap();
        assert!(none.referrers.is_empty());
    }

    #[tokio::test]
    async fn referrers_fall_back_to_tag_schema() {
        let (addr, st) = serve(St::default()).await;
        let subject = put_image(&st, "arm64", "v1");
        // Nothing attached: empty, via the fallback.
        let rep = referrers(&format!("{addr}/app@{subject}"), None, None)
            .await
            .unwrap();
        assert_eq!((rep.via.as_str(), rep.referrers.len()), ("tag-schema", 0));

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("att.json");
        std::fs::write(&f, b"{\"a\":1}").unwrap();
        let mk = |t: &str| AttachOptions {
            artifact_type: t.into(),
            layer_media_type: Some("application/json".into()),
            annotations: BTreeMap::new(),
        };
        let a1 = attach(
            &format!("{addr}/app:v1"),
            &f,
            &mk("application/spdx+json"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(a1.via, "tag-schema");
        std::fs::write(&f, b"{\"b\":2}").unwrap();
        let a2 = attach(
            &format!("{addr}/app:v1"),
            &f,
            &mk("application/vnd.in-toto+json"),
            None,
        )
        .await
        .unwrap();
        // Re-attaching the same content does not duplicate the entry.
        attach(
            &format!("{addr}/app:v1"),
            &f,
            &mk("application/vnd.in-toto+json"),
            None,
        )
        .await
        .unwrap();

        let tag = format!("sha256-{}", &subject["sha256:".len()..]);
        let idx: Value = serde_json::from_slice(&st.lock().unwrap().manifests[&tag].1).unwrap();
        assert_eq!(idx["manifests"].as_array().unwrap().len(), 2);

        let rep = referrers(&format!("{addr}/app:v1"), None, None)
            .await
            .unwrap();
        assert_eq!(rep.via, "tag-schema");
        let digests: Vec<_> = rep.referrers.iter().map(|r| r.digest.clone()).collect();
        assert!(digests.contains(&a1.artifact_digest) && digests.contains(&a2.artifact_digest));
        let only = referrers(
            &format!("{addr}/app:v1"),
            Some("application/spdx+json"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(only.referrers.len(), 1);
        assert_eq!(only.referrers[0].digest, a1.artifact_digest);
    }

    #[tokio::test]
    async fn attach_rejects_bad_input() {
        let (addr, st) = serve(St::default()).await;
        put_image(&st, "arm64", "v1");
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        std::fs::write(&f, b"x").unwrap();
        let opts = |t: &str| AttachOptions {
            artifact_type: t.into(),
            layer_media_type: None,
            annotations: BTreeMap::new(),
        };
        assert!(
            attach(&format!("{addr}/app:v1"), &f, &opts(""), None)
                .await
                .is_err()
        );
        assert!(
            attach(&format!("{addr}/app:v1"), dir.path(), &opts("a/b"), None)
                .await
                .is_err()
        );
        assert!(
            attach(&format!("{addr}/app:missing"), &f, &opts("a/b"), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn index_create_builds_platform_descriptors() {
        let (addr, st) = serve(St::default()).await;
        let arm = put_image(&st, "arm64", "arm");
        let amd = put_image(&st, "amd64", "amd");
        let ann = BTreeMap::from([("note".to_string(), "x".to_string())]);
        let out = create_index(
            &format!("{addr}/app:1.0"),
            &[format!("{addr}/app:arm"), format!("{addr}/app:amd")],
            &ann,
            None,
        )
        .await
        .unwrap();
        let stored = st.lock().unwrap().manifests["1.0"].clone();
        assert_eq!(stored.0, MT_OCI_INDEX);
        assert_eq!(sha256_digest(&stored.1), out.digest);
        let v: Value = serde_json::from_slice(&stored.1).unwrap();
        assert_eq!(v["manifests"][0]["digest"], arm);
        assert_eq!(
            v["manifests"][0]["platform"],
            json!({"architecture": "arm64", "os": "linux"})
        );
        assert_eq!(v["manifests"][1]["digest"], amd);
        assert_eq!(v["annotations"]["note"], "x");

        // Duplicate platform, nested index, other repo, digest target and empty list are all refused.
        let dup = create_index(
            &format!("{addr}/app:2"),
            &[format!("{addr}/app:arm"), format!("{addr}/app@{arm}")],
            &ann,
            None,
        )
        .await;
        assert!(format!("{:#}", dup.unwrap_err()).contains("one per platform"));
        let nested = create_index(
            &format!("{addr}/app:2"),
            &[format!("{addr}/app:1.0")],
            &ann,
            None,
        )
        .await;
        assert!(format!("{:#}", nested.unwrap_err()).contains("itself an index"));
        let other = create_index(
            &format!("{addr}/app:2"),
            &[format!("{addr}/other:arm")],
            &ann,
            None,
        )
        .await;
        assert!(format!("{:#}", other.unwrap_err()).contains("own repository"));
        assert!(
            create_index(
                &format!("{addr}/app@{arm}"),
                &[format!("{addr}/app:arm")],
                &ann,
                None
            )
            .await
            .is_err()
        );
        assert!(
            create_index(&format!("{addr}/app:2"), &[], &ann, None)
                .await
                .is_err()
        );
        assert!(!st.lock().unwrap().manifests.contains_key("2"));
    }

    #[test]
    fn timestamp_is_rfc3339() {
        let t = rfc3339_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }
}
