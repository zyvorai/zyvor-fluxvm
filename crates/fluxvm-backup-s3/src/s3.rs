// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A small S3 client: SigV4-signed PUT (single or multipart), GET and
//! DELETE against AWS S3, MinIO, Ceph RGW or Atlas S3.
//!
//! Every upload carries `Content-MD5`, which S3 requires for writes with
//! Object Lock retention and which lets the server reject a corrupted body.

use crate::{DeleteOutcome, ObjectStore, PutOptions};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Files above this go up in parts of this size.
const PART_SIZE: usize = 64 << 20;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3Target {
    /// `https://s3.eu-central-1.amazonaws.com`, `http://minio:9000`, ...
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// `endpoint/bucket/key` (MinIO, RGW, most self-hosted) rather than
    /// `bucket.endpoint/key`.
    #[serde(default = "default_true")]
    pub path_style: bool,
}

impl std::fmt::Debug for S3Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Target")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("path_style", &self.path_style)
            .finish_non_exhaustive()
    }
}

fn default_region() -> String {
    "us-east-1".into()
}

fn default_true() -> bool {
    true
}

pub struct S3Store {
    t: S3Target,
    http: reqwest::Client,
    scheme: String,
    host: String,
    base_path: String,
}

/// SigV4's URI encoding: everything but unreserved characters, and `/` too
/// unless `keep_slash`.
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// One request to sign.
pub(crate) struct SignInput<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Already URI-encoded path.
    pub path: &'a str,
    pub query: &'a [(String, String)],
    /// Extra headers to sign, lower-case names.
    pub headers: &'a [(String, String)],
    pub payload_sha256: &'a str,
    pub now: DateTime<Utc>,
}

/// The SigV4 `Authorization` header value and the `x-amz-date` it signed.
pub(crate) fn sign(
    access_key: &str,
    secret: &str,
    region: &str,
    input: &SignInput<'_>,
) -> (String, String) {
    let amz_date = input.now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = input.now.format("%Y%m%d").to_string();
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), input.host.into()),
        ("x-amz-content-sha256".into(), input.payload_sha256.into()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    headers.extend(input.headers.iter().cloned());
    headers.sort();
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let mut q: Vec<(String, String)> = input
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    q.sort();
    let canonical_query: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.path,
        canonical_query.join("&"),
        canonical_headers,
        signed,
        input.payload_sha256
    );
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
    (
        format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"
        ),
        amz_date,
    )
}

/// The first `<tag>` element's text in an S3 XML body.
fn xml_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{tag}>"))? + start;
    Some(&body[start..end])
}

impl S3Store {
    pub fn new(t: S3Target) -> Result<Self> {
        let (scheme, rest) = t
            .endpoint
            .split_once("://")
            .context("S3 endpoint needs a scheme, e.g. https://")?;
        if !matches!(scheme, "http" | "https") {
            bail!("S3 endpoint scheme must be http or https");
        }
        let rest = rest.trim_end_matches('/');
        let (host, base_path) = match rest.split_once('/') {
            Some((h, p)) => (h.to_string(), format!("/{p}")),
            None => (rest.to_string(), String::new()),
        };
        if host.is_empty() || t.bucket.is_empty() {
            bail!("S3 target needs an endpoint host and a bucket");
        }
        let host = if t.path_style {
            host
        } else {
            format!("{}.{host}", t.bucket)
        };
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15 * 60))
            .build()?;
        Ok(Self {
            scheme: scheme.into(),
            host,
            base_path,
            http,
            t,
        })
    }

    fn path(&self, key: &str) -> String {
        let key = uri_encode(key, true);
        if self.t.path_style {
            format!(
                "{}/{}/{key}",
                self.base_path,
                uri_encode(&self.t.bucket, false)
            )
        } else {
            format!("{}/{key}", self.base_path)
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(String, String)],
        mut headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            sha256_hex(&body)
        };
        if let Some(tok) = &self.t.session_token {
            headers.push(("x-amz-security-token".into(), tok.clone()));
        }
        let path = self.path(key);
        let (auth, amz_date) = sign(
            &self.t.access_key_id,
            &self.t.secret_access_key,
            &self.t.region,
            &SignInput {
                method: method.as_str(),
                host: &self.host,
                path: &path,
                query,
                headers: &headers,
                payload_sha256: &payload,
                now: Utc::now(),
            },
        );
        let mut url = format!("{}://{}{path}", self.scheme, self.host);
        if !query.is_empty() {
            let q: Vec<String> = query
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        uri_encode(k, false)
                    } else {
                        format!("{}={}", uri_encode(k, false), uri_encode(v, false))
                    }
                })
                .collect();
            url.push('?');
            url.push_str(&q.join("&"));
        }
        let mut req = self
            .http
            .request(method, url)
            .header("authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.body(body).send().await.context("S3 request")
    }

    async fn check(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let code = xml_text(&body, "Code").unwrap_or("");
        let msg = xml_text(&body, "Message").unwrap_or(body.trim());
        bail!("S3 {what}: {status} {code} {msg}");
    }

    fn put_headers(data: &[u8], opts: &PutOptions) -> Vec<(String, String)> {
        let mut h = vec![(
            "content-md5".to_string(),
            base64::engine::general_purpose::STANDARD.encode(Md5::digest(data)),
        )];
        h.extend(lock_headers(opts));
        h
    }

    async fn put_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u32,
        data: Vec<u8>,
    ) -> Result<String> {
        let md5 = base64::engine::general_purpose::STANDARD.encode(Md5::digest(&data));
        let resp = self
            .send(
                reqwest::Method::PUT,
                key,
                &[
                    ("partNumber".into(), number.to_string()),
                    ("uploadId".into(), upload_id.into()),
                ],
                vec![("content-md5".into(), md5)],
                data,
            )
            .await?;
        let resp = Self::check(resp, &format!("upload part {number} of {key}")).await?;
        resp.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .context("S3 upload part returned no ETag")
    }

    async fn multipart(
        &self,
        key: &str,
        file: &mut tokio::fs::File,
        first: Vec<u8>,
        opts: &PutOptions,
    ) -> Result<()> {
        let resp = self
            .send(
                reqwest::Method::POST,
                key,
                &[("uploads".into(), String::new())],
                lock_headers(opts),
                Vec::new(),
            )
            .await?;
        let body = Self::check(resp, &format!("start multipart upload of {key}"))
            .await?
            .text()
            .await?;
        let upload_id = xml_text(&body, "UploadId")
            .context("S3 multipart upload returned no UploadId")?
            .to_string();
        let result: Result<Vec<(u32, String)>> = async {
            let mut parts = Vec::new();
            let mut chunk = first;
            let mut number = 1u32;
            loop {
                let etag = self.put_part(key, &upload_id, number, chunk).await?;
                parts.push((number, etag));
                let mut next = vec![0u8; PART_SIZE];
                let n = read_full(file, &mut next).await?;
                if n == 0 {
                    break;
                }
                next.truncate(n);
                chunk = next;
                number += 1;
            }
            Ok(parts)
        }
        .await;
        let parts = match result {
            Ok(p) => p,
            Err(e) => {
                let _ = self
                    .send(
                        reqwest::Method::DELETE,
                        key,
                        &[("uploadId".into(), upload_id.clone())],
                        vec![],
                        Vec::new(),
                    )
                    .await;
                return Err(e);
            }
        };
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (n, etag) in &parts {
            xml.push_str(&format!(
                "<Part><PartNumber>{n}</PartNumber><ETag>{etag}</ETag></Part>"
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let resp = self
            .send(
                reqwest::Method::POST,
                key,
                &[("uploadId".into(), upload_id)],
                vec![],
                xml.into_bytes(),
            )
            .await?;
        let body = Self::check(resp, &format!("complete multipart upload of {key}"))
            .await?
            .text()
            .await?;
        // S3 can report a failed completion inside a 200 response.
        if body.contains("<Error>") {
            bail!(
                "S3 complete multipart upload of {key}: {} {}",
                xml_text(&body, "Code").unwrap_or(""),
                xml_text(&body, "Message").unwrap_or("")
            );
        }
        Ok(())
    }
}

fn lock_headers(opts: &PutOptions) -> Vec<(String, String)> {
    match opts.retain_until {
        Some(until) => vec![
            ("x-amz-object-lock-mode".into(), "COMPLIANCE".into()),
            (
                "x-amz-object-lock-retain-until-date".into(),
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        ],
        None => vec![],
    }
}

async fn read_full(f: &mut tokio::fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]).await? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put_file(&self, key: &str, path: &Path, opts: &PutOptions) -> Result<()> {
        let mut f = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let mut first = vec![0u8; PART_SIZE];
        let n = read_full(&mut f, &mut first).await?;
        first.truncate(n);
        if n < PART_SIZE {
            return self.put_bytes(key, first, opts).await;
        }
        self.multipart(key, &mut f, first, opts).await
    }

    async fn put_bytes(&self, key: &str, data: Vec<u8>, opts: &PutOptions) -> Result<()> {
        let headers = Self::put_headers(&data, opts);
        let resp = self
            .send(reqwest::Method::PUT, key, &[], headers, data)
            .await?;
        Self::check(resp, &format!("put {key}")).await?;
        Ok(())
    }

    async fn get_file(&self, key: &str, dest: &Path) -> Result<()> {
        let resp = self
            .send(reqwest::Method::GET, key, &[], vec![], Vec::new())
            .await?;
        let mut resp = Self::check(resp, &format!("get {key}")).await?;
        let mut out = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        while let Some(chunk) = resp.chunk().await.context("reading S3 object")? {
            out.write_all(&chunk).await?;
        }
        out.sync_all().await?;
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(reqwest::Method::GET, key, &[], vec![], Vec::new())
            .await?;
        let resp = Self::check(resp, &format!("get {key}")).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    async fn delete(&self, key: &str) -> Result<DeleteOutcome> {
        let resp = self
            .send(reqwest::Method::DELETE, key, &[], vec![], Vec::new())
            .await?;
        match resp.status().as_u16() {
            200 | 204 => Ok(DeleteOutcome::Deleted),
            404 => Ok(DeleteOutcome::NotFound),
            403 => {
                let body = resp.text().await.unwrap_or_default();
                if body.to_ascii_lowercase().contains("retention") || body.contains("ObjectLocked")
                {
                    Ok(DeleteOutcome::RetentionHeld)
                } else {
                    bail!(
                        "S3 delete {key}: 403 {}",
                        xml_text(&body, "Message").unwrap_or(body.trim())
                    )
                }
            }
            _ => {
                Self::check(resp, &format!("delete {key}")).await?;
                Ok(DeleteOutcome::Deleted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// AWS's own worked example ("Example: GET Object" in the SigV4 docs
    /// for S3): GET /test.txt with a Range header.
    #[test]
    fn matches_the_aws_sigv4_get_object_example() {
        let (auth, date) = sign(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            &SignInput {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
                headers: &[("range".into(), "bytes=0-9".into())],
                payload_sha256: EMPTY_SHA256,
                now: Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap(),
            },
        );
        assert_eq!(date, "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn builds_path_and_virtual_host_urls() {
        let mut t = S3Target {
            endpoint: "http://minio:9000/".into(),
            bucket: "backups".into(),
            region: "us-east-1".into(),
            access_key_id: "a".into(),
            secret_access_key: "s".into(),
            session_token: None,
            path_style: true,
        };
        let s = S3Store::new(t.clone()).unwrap();
        assert_eq!(s.host, "minio:9000");
        assert_eq!(
            s.path("ns/db/c1/00000001-b 1/root.qcow2"),
            "/backups/ns/db/c1/00000001-b%201/root.qcow2"
        );
        t.endpoint = "https://s3.eu-central-1.amazonaws.com".into();
        t.path_style = false;
        let s = S3Store::new(t).unwrap();
        assert_eq!(s.host, "backups.s3.eu-central-1.amazonaws.com");
        assert_eq!(s.path("a/b"), "/a/b");
    }

    #[test]
    fn debug_hides_the_secret() {
        let t = S3Target {
            endpoint: "http://m".into(),
            bucket: "b".into(),
            region: "r".into(),
            access_key_id: "a".into(),
            secret_access_key: "TOPSECRET".into(),
            session_token: Some("TOKEN".into()),
            path_style: true,
        };
        let d = format!("{t:?}");
        assert!(!d.contains("TOPSECRET") && !d.contains("TOKEN"), "{d}");
    }

    #[test]
    fn lock_headers_use_compliance_mode() {
        let until = Utc.with_ymd_and_hms(2027, 1, 2, 3, 4, 5).unwrap();
        let h = lock_headers(&PutOptions {
            retain_until: Some(until),
        });
        assert_eq!(h[0], ("x-amz-object-lock-mode".into(), "COMPLIANCE".into()));
        assert_eq!(h[1].1, "2027-01-02T03:04:05Z");
        assert!(lock_headers(&PutOptions::default()).is_empty());
    }
}
