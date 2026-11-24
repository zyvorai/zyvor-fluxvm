// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `docker build` on the FluxVM Docker socket, forwarded to the FluxVM image builder.
//!
//! The Docker CLI sends `POST /build?t=NAME:TAG&dockerfile=…&buildargs={…}…` with the build context as a tar body. This module is
//! the part that needs no socket: reading those parameters ([`parse_query`]), refusing the ones FluxVM cannot honour in words that
//! name the `fluxctl` command to use instead, unpacking the tar context safely ([`extract_context`]), applying `.dockerignore`
//! ([`Ignore`]), the Docker JSON progress lines ([`stream_line`] and friends), the [`DockerBuilder`] trait the build runs behind
//! (so tests use a fake), the real builder that runs `fluxctl mac build` ([`FluxctlBuilder`]), and the build history Studio shows
//! ([`History`]). The HTTP handler is `docker_engine/build.rs`.
//!
//! Only the classic builder (`DOCKER_BUILDKIT=0`, which the engine's `/_ping` asks for with `Builder-Version: 1`) is served.
//! BuildKit's `/grpc` and `/session` are answered with a plain-English refusal.
//!
//! The history keeps tags, flag names and the outcome. It never keeps a `--build-arg` value (they are often secrets).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
};

/// Largest build context accepted (bytes of tar as sent).
pub const MAX_CONTEXT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// How many builds the history keeps.
pub const HISTORY_LIMIT: usize = 100;
/// The file name of the history, next to the engine socket.
pub const HISTORY_FILE: &str = "docker-builds.json";

/// What the Docker CLI asked for, after the query string was read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildRequest {
    pub tags: Vec<String>,
    pub dockerfile: String,
    pub build_args: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub target: Option<String>,
    pub platform: Option<String>,
    pub no_cache: bool,
    pub quiet: bool,
}

/// A request FluxVM will not build, with the HTTP status and the sentence the Docker CLI prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

fn refuse(status: u16, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        message: message.into(),
    }
}

/// `(query parameter, CLI flag, what to do instead)` for the flags FluxVM does not honour.
const UNSUPPORTED: &[(&str, &str, &str)] = &[
    (
        "memory",
        "--memory",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "memswap",
        "--memory-swap",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cpushares",
        "--cpu-shares",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cpusetcpus",
        "--cpuset-cpus",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cpusetmems",
        "--cpuset-mems",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cpuperiod",
        "--cpu-period",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cpuquota",
        "--cpu-quota",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "shmsize",
        "--shm-size",
        "The build runs in a macOS VM sized by FluxVM, so per-build limits do not apply. Remove the flag.",
    ),
    (
        "cgroupparent",
        "--cgroup-parent",
        "A macOS build VM has no cgroups. Remove the flag.",
    ),
    (
        "ulimits",
        "--ulimit",
        "Set the limit inside the Macfile instead, for example `RUN ulimit -n 4096 && make`.",
    ),
    (
        "extrahosts",
        "--add-host",
        "Put the entry in the Macfile: `RUN echo '10.0.0.5 db.local' | sudo tee -a /etc/hosts`.",
    ),
    (
        "squash",
        "--squash",
        "A FluxVM image is one macOS disk, so there are no layers to squash. Remove the flag.",
    ),
    (
        "outputs",
        "--output",
        "Built images go to FluxVM's image store. Export one as a tar with `fluxctl mac save NAME:TAG --output image.tar`.",
    ),
    (
        "remote",
        "a git or URL build context",
        "Clone the repository and run `docker build` inside its folder, or run `fluxctl mac build -t NAME:TAG .` there.",
    ),
];

fn is_default_network(v: &str) -> bool {
    matches!(v, "" | "default" | "bridge")
}

fn truthy(v: &str) -> bool {
    matches!(v, "1" | "true" | "True" | "TRUE")
}

/// Read a JSON object of strings (`buildargs`, `labels`). `null` and an empty value mean none.
fn json_map(name: &str, v: &str) -> Result<BTreeMap<String, String>, Refusal> {
    if v.trim().is_empty() || v.trim() == "null" {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str::<BTreeMap<String, Option<String>>>(v)
        .map(|m| m.into_iter().map(|(k, v)| (k, v.unwrap_or_default())).collect())
        .map_err(|_| {
            refuse(
                400,
                format!(
                    "The `{name}` parameter of this build is not a JSON object of strings. Update the docker CLI, or build with `fluxctl mac build -t NAME:TAG .`."
                ),
            )
        })
}

/// A tag must be one word. The builder checks the rest (registry, characters) and says what is wrong.
fn tag_problem(t: &str) -> Option<String> {
    if t.is_empty() || t.chars().any(|c| c.is_whitespace()) {
        return Some(format!(
            "The image name {t:?} is not valid. Use `-t name:tag` with no spaces."
        ));
    }
    None
}

/// The `fluxctl` command that does the same build, for messages and for the history. Build-arg values are never shown.
pub fn fluxctl_equivalent(r: &BuildRequest) -> String {
    let mut s = String::from("fluxctl mac build");
    if let Some(t) = r.tags.first() {
        s.push_str(&format!(" -t {t}"));
    }
    if !r.dockerfile.is_empty() && r.dockerfile != "Dockerfile" {
        s.push_str(&format!(" -f {}", r.dockerfile));
    }
    for k in r.build_args.keys() {
        s.push_str(&format!(" --build-arg {k}=VALUE"));
    }
    for k in r.labels.keys() {
        s.push_str(&format!(" --label {k}=VALUE"));
    }
    if let Some(p) = &r.platform {
        s.push_str(&format!(" --platform {p}"));
    }
    if let Some(t) = &r.target {
        s.push_str(&format!(" --target {t}"));
    }
    if r.no_cache {
        s.push_str(" --no-cache");
    }
    s.push_str(" .");
    s
}

/// Read the query of `POST /build`. Anything FluxVM cannot honour is refused here, before the context is uploaded to disk.
pub fn parse_query(pairs: &[(String, String)]) -> Result<BuildRequest, Refusal> {
    let mut r = BuildRequest {
        dockerfile: "Dockerfile".into(),
        ..Default::default()
    };
    let get = |k: &str| {
        pairs
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    // BuildKit asks for version=2; the classic builder omits it or sends 1.
    if get("version") == Some("2") {
        return Err(refuse(
            501,
            "This docker is asking for a BuildKit build (`docker buildx build`, or `docker build` with BuildKit on), which FluxVM does not serve. \
             Run `DOCKER_BUILDKIT=0 docker build -t NAME:TAG .`, or use FluxVM directly: `fluxctl mac build -t NAME:TAG .` \
             (it also has `--secret` and `--ssh default`).",
        ));
    }
    for (param, flag, advice) in UNSUPPORTED {
        if let Some(v) = get(param)
            && !v.is_empty()
            && v != "0"
            && v != "false"
            && v != "[]"
            && v != "null"
        {
            return Err(refuse(
                501,
                format!(
                    "`docker build` with {flag} is not supported when FluxVM builds the image. {advice} The FluxVM build command is `fluxctl mac build -t NAME:TAG .` (see `fluxctl mac build --help`)."
                ),
            ));
        }
    }
    if let Some(n) = get("networkmode")
        && !is_default_network(n)
    {
        return Err(refuse(
            501,
            format!(
                "`docker build --network {n}` is not supported when FluxVM builds the image: the build VM always uses its own NAT network. Remove the flag."
            ),
        ));
    }
    if let Some(c) = get("cachefrom")
        && !matches!(c, "" | "[]" | "null")
    {
        return Err(refuse(
            501,
            "`docker build --cache-from` is not supported when FluxVM builds the image: FluxVM caches build steps itself. \
             Run the build again to reuse the cache, add `fluxctl mac build --no-cache` to skip it, and see it with `fluxctl mac cache`.",
        ));
    }
    for (k, v) in pairs {
        if k == "t" && !v.is_empty() {
            if let Some(p) = tag_problem(v) {
                return Err(refuse(400, p));
            }
            if !r.tags.contains(v) {
                r.tags.push(v.clone());
            }
        }
    }
    if let Some(f) = get("dockerfile").filter(|f| !f.is_empty()) {
        r.dockerfile = f.to_owned();
    }
    r.build_args = json_map("buildargs", get("buildargs").unwrap_or(""))?;
    r.labels = json_map("labels", get("labels").unwrap_or(""))?;
    r.target = get("target").filter(|t| !t.is_empty()).map(str::to_owned);
    r.platform = get("platform").filter(|t| !t.is_empty()).map(str::to_owned);
    r.no_cache = get("nocache").is_some_and(truthy);
    r.quiet = get("q").is_some_and(truthy);
    Ok(r)
}

// ---------------------------------------------------------------------------------------------------------------------------
// Docker's JSON progress stream
// ---------------------------------------------------------------------------------------------------------------------------

/// One line of the build stream: `{"stream":"text\n"}`.
pub fn stream_line(text: &str) -> String {
    let mut t = text.trim_end_matches('\n').to_owned();
    t.push('\n');
    format!("{}\n", json!({ "stream": t }))
}

/// The line the Docker CLI reads the new image's id from (`--iidfile`).
pub fn aux_line(image_id: &str) -> String {
    format!("{}\n", json!({ "aux": { "ID": image_id } }))
}

/// A failure: the CLI prints `message` and exits 1.
pub fn error_line(message: &str) -> String {
    format!(
        "{}\n",
        json!({ "errorDetail": { "message": message }, "error": message })
    )
}

/// `sha256:<64 hex>` as Docker prints it; the short form is 12 characters.
pub fn short_id(id: &str) -> &str {
    let h = id.strip_prefix("sha256:").unwrap_or(id);
    &h[..h.len().min(12)]
}

// ---------------------------------------------------------------------------------------------------------------------------
// The build context
// ---------------------------------------------------------------------------------------------------------------------------

/// What unpacking the context found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextInfo {
    pub files: u64,
    pub bytes: u64,
    /// Entries skipped because they were not plain files, folders or links (devices and the like).
    pub skipped: u64,
}

/// Unpack a (plain or gzip) tar into `dest`. A path that would leave `dest` is an error, not skipped.
pub fn extract_context(
    tar_path: &Path,
    dest: &Path,
    max_bytes: u64,
) -> Result<ContextInfo, String> {
    use std::io::Read;
    let open = |e: std::io::Error| {
        format!("Could not read the uploaded build context: {e}. Run the build again.")
    };
    let mut f = std::fs::File::open(tar_path).map_err(open)?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic).map_err(open)?;
    drop(f);
    let f = std::fs::File::open(tar_path).map_err(open)?;
    let reader: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(flate2::read::GzDecoder::new(f))
    } else {
        Box::new(f)
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_unpack_xattrs(false);
    let bad = |e: std::io::Error| {
        format!(
            "The build context is not a readable tar archive ({e}). Run `docker build` from the folder that holds the Dockerfile, or use `fluxctl mac build -t NAME:TAG .`."
        )
    };
    let mut info = ContextInfo::default();
    for entry in ar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let kind = entry.header().entry_type();
        let size = entry.header().size().unwrap_or(0);
        info.bytes += size;
        if info.bytes > max_bytes {
            return Err(format!(
                "The build context is larger than {} GiB. Add a `.dockerignore` for the big folders (build output, .git), then run the build again.",
                max_bytes >> 30
            ));
        }
        let path = entry.path().map_err(bad)?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "The build context has a path that leaves the context ({}). Rebuild the context from the project folder.",
                path.display()
            ));
        }
        match kind {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                info.files += 1;
            }
            tar::EntryType::Directory | tar::EntryType::Symlink | tar::EntryType::Link => {}
            // PAX headers are consumed by the tar crate; everything else (devices, fifos) is not a build input.
            _ => {
                info.skipped += 1;
                continue;
            }
        }
        if matches!(kind, tar::EntryType::Symlink | tar::EntryType::Link) {
            // A link may not point out of the context: a COPY of it would read the host.
            let target = entry
                .link_name()
                .map_err(bad)?
                .map(|l| l.into_owned())
                .unwrap_or_default();
            if !link_stays_inside(&path, &target, kind == tar::EntryType::Link) {
                info.skipped += 1;
                continue;
            }
        }
        entry.unpack_in(dest).map_err(bad)?;
    }
    Ok(info)
}

/// Whether a link at `path` (relative to the context root) pointing at `target` stays inside the context.
fn link_stays_inside(path: &Path, target: &Path, hard: bool) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = if hard {
        0
    } else {
        // A symlink resolves against its own folder.
        path.components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .count() as i64
            - 1
    };
    for c in target.components() {
        match c {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

/// `.dockerignore`: Docker's patterns, matched against paths relative to the context root.
#[derive(Debug, Clone, Default)]
pub struct Ignore {
    rules: Vec<(Vec<String>, bool)>,
}

impl Ignore {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (neg, pat) = match line.strip_prefix('!') {
                Some(p) => (true, p.trim()),
                None => (false, line),
            };
            let pat = pat.trim_start_matches("./").trim_start_matches('/');
            let pat = pat.trim_end_matches('/');
            if pat.is_empty() {
                continue;
            }
            let segs: Vec<String> = pat
                .split('/')
                .filter(|s| !s.is_empty() && *s != ".")
                .map(str::to_owned)
                .collect();
            rules.push((segs, neg));
        }
        Self { rules }
    }

    /// Whether `rel` (a `/`-separated path under the root) is left out of the build. The last rule that matches the path or one of
    /// its parent folders decides.
    pub fn ignored(&self, rel: &str) -> bool {
        let segs: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let mut out = false;
        for (pat, neg) in &self.rules {
            let pat: Vec<&str> = pat.iter().map(String::as_str).collect();
            let hit = (1..=segs.len()).any(|n| match_segments(&pat, &segs[..n]));
            if hit {
                out = !neg;
            }
        }
        out
    }
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| match_segments(rest, &path[i..])),
        Some((p, rest)) => match path.split_first() {
            Some((s, tail)) => glob(p, s) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// One path segment against one pattern segment: `*`, `?`, `[a-z]`, `[^a]`, `\x`.
fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| go(&p[1..], &s[i..])),
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && go(&p[2..], &s[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) else {
                    return s.first() == Some(&'[') && go(&p[1..], &s[1..]);
                };
                let Some(&c) = s.first() else { return false };
                let mut set = &p[1..close];
                let neg = matches!(set.first(), Some('^' | '!'));
                if neg {
                    set = &set[1..];
                }
                let mut found = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        found |= set[i] <= c && c <= set[i + 2];
                        i += 3;
                    } else {
                        found |= set[i] == c;
                        i += 1;
                    }
                }
                found != neg && go(&p[close + 1..], &s[1..])
            }
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    go(&p, &s)
}

/// Remove what `.dockerignore` leaves out. The Dockerfile and `.dockerignore` itself always stay (Docker's rule). Returns how many
/// files were removed. The docker CLI already filters on its side; this covers other clients.
pub fn apply_dockerignore(root: &Path, dockerfile: &str) -> std::io::Result<u64> {
    let Ok(text) = std::fs::read_to_string(root.join(".dockerignore")) else {
        return Ok(0);
    };
    let ig = Ignore::parse(&text);
    if ig.rules.is_empty() {
        return Ok(0);
    }
    let keep = [dockerfile.trim_start_matches("./"), ".dockerignore"];
    fn walk(
        dir: &Path,
        rel: &str,
        ig: &Ignore,
        keep: &[&str],
        removed: &mut u64,
    ) -> std::io::Result<bool> {
        let mut empty = true;
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ft = e.file_type()?;
            if ft.is_dir() {
                if walk(&e.path(), &r, ig, keep, removed)? {
                    std::fs::remove_dir(e.path())?;
                } else {
                    empty = false;
                }
            } else if ig.ignored(&r) && !keep.contains(&r.as_str()) {
                std::fs::remove_file(e.path())?;
                *removed += 1;
            } else {
                empty = false;
            }
        }
        Ok(empty)
    }
    let mut removed = 0;
    walk(root, "", &ig, &keep, &mut removed)?;
    Ok(removed)
}

/// The build file to use inside `root`: the requested one, or for the default name a `Macfile` when there is no `Dockerfile`.
pub fn find_build_file(root: &Path, requested: &str) -> Result<String, String> {
    let rel = requested.trim_start_matches("./");
    let bad = rel.is_empty()
        || Path::new(rel).is_absolute()
        || Path::new(rel)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if bad {
        return Err(format!(
            "The build file {requested:?} must be a path inside the build context. Use `-f path/in/context`."
        ));
    }
    if root.join(rel).is_file() {
        return Ok(rel.to_owned());
    }
    if rel == "Dockerfile" && root.join("Macfile").is_file() {
        return Ok("Macfile".into());
    }
    Err(format!(
        "Cannot locate the build file {rel:?} in the build context. FluxVM builds macOS images from a Macfile (or a Dockerfile written in Macfile syntax): \
         put one in the folder you build, or point at it with `-f`. `fluxctl mac build --help` shows the syntax."
    ))
}

// ---------------------------------------------------------------------------------------------------------------------------
// The builder behind the socket
// ---------------------------------------------------------------------------------------------------------------------------

/// One build, ready to run: the request and the unpacked context.
#[derive(Debug, Clone)]
pub struct BuildJob {
    pub request: BuildRequest,
    pub context_dir: PathBuf,
    /// The build file, relative to `context_dir`.
    pub build_file: String,
}

/// What a finished build made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltImage {
    /// `sha256:<64 hex>`.
    pub id: String,
}

pub type BuildFuture<'a> = Pin<Box<dyn Future<Output = Result<BuiltImage, String>> + Send + 'a>>;

/// Where a build really happens. Behind a trait so tests run with a fake and the Docker socket does not care what builds.
/// `Err` is the sentence the Docker CLI prints.
pub trait DockerBuilder: Send + Sync {
    /// A short name for the history ("fluxctl mac build").
    fn name(&self) -> &'static str;
    /// `Some(why)` when this host cannot build at all, in words that say what to do. Checked before the context is uploaded.
    fn unavailable(&self) -> Option<String>;
    /// Run the build, sending each output line to `progress`. Dropping the future cancels the build.
    fn build<'a>(
        &'a self,
        job: &'a BuildJob,
        progress: &'a (dyn Fn(String) + Send + Sync),
    ) -> BuildFuture<'a>;
}

/// `sha256:<hex>` from whatever digest text the store reported.
pub fn image_id_from_digest(digest: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = digest.strip_prefix("sha256:").unwrap_or(digest);
    if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
        return format!("sha256:{}", h.to_ascii_lowercase());
    }
    let d = Sha256::digest(digest.as_bytes());
    format!(
        "sha256:{}",
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// The digest of `tag` in the output of `fluxctl mac images --json`.
pub fn digest_in_images(json: &str, tag: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let want = if tag.rsplit('/').next().is_some_and(|l| l.contains(':')) {
        tag.to_owned()
    } else {
        format!("{tag}:latest")
    };
    v.as_array()?
        .iter()
        .find(|i| {
            i["reference"]
                .as_str()
                .is_some_and(|r| r == want || r == tag)
        })
        .and_then(|i| i["digest"].as_str())
        .map(str::to_owned)
}

/// The arguments of `fluxctl mac build` for a job.
pub fn mac_build_args(job: &BuildJob) -> Vec<String> {
    let r = &job.request;
    let mut a: Vec<String> = vec!["mac".into(), "build".into()];
    a.push("-t".into());
    a.push(
        r.tags
            .first()
            .cloned()
            .unwrap_or_else(|| "docker-build:latest".into()),
    );
    a.push("-f".into());
    a.push(job.context_dir.join(&job.build_file).display().to_string());
    for (k, v) in &r.build_args {
        a.push("--build-arg".into());
        a.push(format!("{k}={v}"));
    }
    for (k, v) in &r.labels {
        a.push("--label".into());
        a.push(format!("{k}={v}"));
    }
    if let Some(p) = &r.platform {
        a.push("--platform".into());
        a.push(p.clone());
    }
    if let Some(t) = &r.target {
        a.push("--target".into());
        a.push(t.clone());
    }
    if r.no_cache {
        a.push("--no-cache".into());
    }
    a.push(job.context_dir.display().to_string());
    a
}

/// The real builder: runs `fluxctl mac build` against the daemon (the build is the Mac builder's, unchanged).
pub struct FluxctlBuilder {
    bin: Option<PathBuf>,
    url: Option<String>,
    token: Option<String>,
    is_mac: bool,
}

impl FluxctlBuilder {
    /// `url` and `token` reach the daemon from the child; both go in its environment, never on its command line.
    pub fn new(url: Option<String>, token: Option<String>) -> Self {
        Self {
            bin: find_fluxctl(),
            url,
            token,
            is_mac: cfg!(target_os = "macos"),
        }
    }
}

fn find_fluxctl() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("fluxctl")))
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("fluxctl"))
            .find(|p| p.is_file())
    })
}

impl DockerBuilder for FluxctlBuilder {
    fn name(&self) -> &'static str {
        "fluxctl mac build"
    }

    fn unavailable(&self) -> Option<String> {
        if !self.is_mac {
            return Some(
                "`docker build` on this socket is forwarded to the FluxVM Mac builder, and this host is not a Mac. \
                 For Linux images run `fluxctl docker up`, then `eval \"$(fluxctl docker env)\"` and `docker build` again."
                    .into(),
            );
        }
        if self.bin.is_none() {
            return Some(
                "`docker build` is forwarded to `fluxctl mac build`, but fluxctl was not found next to the daemon or on its PATH. \
                 Install fluxctl next to fluxvmd (or add it to the daemon's PATH) and restart the daemon, or build in a terminal with `fluxctl mac build -t NAME:TAG .`."
                    .into(),
            );
        }
        None
    }

    fn build<'a>(
        &'a self,
        job: &'a BuildJob,
        progress: &'a (dyn Fn(String) + Send + Sync),
    ) -> BuildFuture<'a> {
        Box::pin(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            use tokio::process::Command;
            let bin = self.bin.clone().ok_or("fluxctl was not found")?;
            let mk = |args: Vec<String>| {
                let mut c = Command::new(&bin);
                if let Some(u) = &self.url {
                    c.env("FLUXVM_URL", u);
                }
                if let Some(t) = &self.token {
                    c.env("FLUXVM_TOKEN", t);
                }
                c.args(args)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true);
                c
            };
            let mut child = mk(mac_build_args(job))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| {
                    format!("Could not start fluxctl ({e}). Check that {} can run, or build in a terminal with `fluxctl mac build`.", bin.display())
                })?;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            for out in [
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            ]
            .into_iter()
            .flatten()
            {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(out).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut tail: Vec<String> = Vec::new();
            while let Some(l) = rx.recv().await {
                progress(l.clone());
                tail.push(l);
                if tail.len() > 12 {
                    tail.remove(0);
                }
            }
            let status = child
                .wait()
                .await
                .map_err(|e| format!("The build did not finish: {e}"))?;
            if !status.success() {
                let why = tail
                    .iter()
                    .filter(|l| !l.trim().is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(if why.is_empty() {
                    "`fluxctl mac build` failed without a message. Run the same build in a terminal: `fluxctl mac build -t NAME:TAG .`".into()
                } else {
                    why
                });
            }
            let first = job.request.tags.first().cloned().unwrap_or_default();
            for extra in job.request.tags.iter().skip(1) {
                let o = mk(vec![
                    "mac".into(),
                    "tag".into(),
                    first.clone(),
                    extra.clone(),
                ])
                .output()
                .await;
                if !o.map(|o| o.status.success()).unwrap_or(false) {
                    return Err(format!(
                        "Built {first} but could not add the name {extra}. Add it yourself: `fluxctl mac tag {first} {extra}`."
                    ));
                }
            }
            let listing = mk(vec!["mac".into(), "images".into(), "--json".into()])
                .output()
                .await;
            let digest = listing
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| digest_in_images(&String::from_utf8_lossy(&o.stdout), &first));
            let digest = digest.ok_or_else(|| {
                format!("Built {first}, but it is not in the image list. Check with `fluxctl mac images`.")
            })?;
            Ok(BuiltImage {
                id: image_id_from_digest(&digest),
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRecord {
    pub id: String,
    pub started_unix: u64,
    pub finished_unix: Option<u64>,
    pub duration_ms: Option<u64>,
    /// `running`, `succeeded`, `failed` or `cancelled`.
    pub state: String,
    pub tags: Vec<String>,
    pub dockerfile: String,
    /// Names only: a value is often a secret.
    pub build_arg_names: Vec<String>,
    pub target: Option<String>,
    pub platform: Option<String>,
    pub no_cache: bool,
    pub context_bytes: u64,
    pub image_id: Option<String>,
    pub error: Option<String>,
    pub builder: String,
    /// The `fluxctl` command that does the same build.
    pub fluxctl: String,
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The most recent builds, newest last, kept in memory and in a file so a daemon restart keeps them.
pub struct History {
    path: Option<PathBuf>,
    items: Mutex<Vec<BuildRecord>>,
}

impl History {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut items: Vec<BuildRecord> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // A build that was running when the daemon stopped did not finish.
        for r in items.iter_mut().filter(|r| r.state == "running") {
            r.state = "failed".into();
            r.error = Some(
                "The daemon stopped while this build was running. Run the build again.".into(),
            );
        }
        Self {
            path,
            items: Mutex::new(items),
        }
    }

    fn save(&self, items: &[BuildRecord]) {
        let Some(p) = &self.path else { return };
        let tmp = p.with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec_pretty(items)
            && std::fs::write(&tmp, b).is_ok()
        {
            let _ = std::fs::rename(&tmp, p);
        }
    }

    pub fn start(&self, rec: BuildRecord) {
        let mut g = self.items.lock().unwrap();
        g.push(rec);
        let over = g.len().saturating_sub(HISTORY_LIMIT);
        g.drain(..over);
        self.save(&g);
    }

    pub fn finish(&self, id: &str, f: impl FnOnce(&mut BuildRecord)) {
        let mut g = self.items.lock().unwrap();
        if let Some(r) = g.iter_mut().find(|r| r.id == id) {
            f(r);
        }
        self.save(&g);
    }

    /// Newest first.
    pub fn list(&self) -> Vec<BuildRecord> {
        let mut v = self.items.lock().unwrap().clone();
        v.reverse();
        v
    }

    /// The id of a succeeded build that has this name (a missing tag means `:latest`).
    pub fn image_for(&self, reference: &str) -> Option<String> {
        let want = with_latest(reference);
        self.items
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|r| r.state == "succeeded")
            .find(|r| r.tags.iter().any(|t| with_latest(t) == want))
            .and_then(|r| r.image_id.clone())
    }

    /// `(id, tags, created)` of each distinct image built, newest first, for `docker images`.
    pub fn images(&self) -> Vec<(String, Vec<String>, u64)> {
        let mut out: Vec<(String, Vec<String>, u64)> = Vec::new();
        for r in self
            .items
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|r| r.state == "succeeded")
        {
            let Some(id) = &r.image_id else { continue };
            for t in &r.tags {
                // A newer build owns a name; an older one no longer shows it.
                if out
                    .iter()
                    .any(|(_, tags, _)| tags.iter().any(|x| with_latest(x) == with_latest(t)))
                {
                    continue;
                }
                match out.iter_mut().find(|(i, _, _)| i == id) {
                    Some((_, tags, _)) => tags.push(with_latest(t)),
                    None => out.push((
                        id.clone(),
                        vec![with_latest(t)],
                        r.finished_unix.unwrap_or(r.started_unix),
                    )),
                }
            }
        }
        out
    }
}

fn with_latest(r: &str) -> String {
    if r.rsplit('/')
        .next()
        .is_some_and(|l| l.contains(':') || l.contains('@'))
    {
        r.to_owned()
    } else {
        format!("{r}:latest")
    }
}

/// The builder and the history of one Docker socket.
pub struct BuildEngine {
    pub builder: Arc<dyn DockerBuilder>,
    pub history: Arc<History>,
    /// Where the socket lives, for Studio.
    pub socket: Option<PathBuf>,
}

impl BuildEngine {
    pub fn new(builder: Arc<dyn DockerBuilder>, history: Arc<History>) -> Self {
        Self {
            builder,
            history,
            socket: None,
        }
    }
}

static GLOBAL: OnceLock<Arc<BuildEngine>> = OnceLock::new();

/// Remember the engine the daemon's Docker socket uses, for `GET /v1/docker/builds`. The first one wins.
pub fn set_global(e: Arc<BuildEngine>) {
    let _ = GLOBAL.set(e);
}

/// `GET /v1/docker/builds`: the history and whether this host can build, in words.
pub fn view(engine: Option<&BuildEngine>) -> Value {
    let try_cmd = "docker build -t NAME:TAG .";
    match engine {
        None => json!({
            "enabled": false,
            "available": false,
            "reason": "The built-in Docker socket is off on this host, so `docker build` is not forwarded. Set `docker_sock` in the daemon config, or build with `fluxctl mac build -t NAME:TAG .`.",
            "builds": [],
            "try": try_cmd,
            "fluxctl": "fluxctl mac build -t NAME:TAG .",
        }),
        Some(e) => {
            let why = e.builder.unavailable();
            json!({
                "enabled": true,
                "available": why.is_none(),
                "reason": why,
                "builder": e.builder.name(),
                "socket": e.socket.as_ref().map(|s| s.display().to_string()),
                "builds": e.history.list(),
                "try": try_cmd,
                "fluxctl": "fluxctl mac build -t NAME:TAG .",
                "context": "fluxctl docker context install --builtin --use",
            })
        }
    }
}

/// The engine registered by [`set_global`].
pub fn global() -> Option<Arc<BuildEngine>> {
    GLOBAL.get().cloned()
}

/// A scratch folder for one build that is removed when dropped.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn create() -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("fluxvm-docker-build-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &[(&str, &str)]) -> Vec<(String, String)> {
        s.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn reads_the_flags_docker_sends() {
        let r = parse_query(&q(&[
            ("t", "app:1"),
            ("t", "app:latest"),
            ("dockerfile", "ci/Macfile"),
            ("buildargs", r#"{"VER":"3","X":null}"#),
            ("labels", r#"{"a":"b"}"#),
            ("target", "release"),
            ("platform", "darwin/arm64"),
            ("nocache", "1"),
            ("q", "0"),
            ("rm", "1"),
            ("pull", "1"),
            ("version", "1"),
            ("networkmode", "default"),
            ("memory", "0"),
            ("cachefrom", "[]"),
        ]))
        .unwrap();
        assert_eq!(r.tags, ["app:1", "app:latest"]);
        assert_eq!(r.dockerfile, "ci/Macfile");
        assert_eq!(r.build_args["VER"], "3");
        assert_eq!(r.build_args["X"], "");
        assert_eq!(r.labels["a"], "b");
        assert_eq!(r.target.as_deref(), Some("release"));
        assert_eq!(r.platform.as_deref(), Some("darwin/arm64"));
        assert!(r.no_cache && !r.quiet);
    }

    #[test]
    fn defaults_to_dockerfile_and_no_tag() {
        let r = parse_query(&[]).unwrap();
        assert_eq!(r.dockerfile, "Dockerfile");
        assert!(r.tags.is_empty());
    }

    #[test]
    fn refusals_name_the_flag_and_the_fluxctl_way() {
        for (p, v, flag, hint) in [
            ("memory", "1073741824", "--memory", "fluxctl mac build"),
            ("cpusetcpus", "0-1", "--cpuset-cpus", "fluxctl mac build"),
            ("squash", "1", "--squash", "no layers"),
            ("extrahosts", r#"["a:1.2.3.4"]"#, "--add-host", "/etc/hosts"),
            ("outputs", "type=tar", "--output", "fluxctl mac save"),
            (
                "remote",
                "https://x/y.git",
                "git or URL",
                "fluxctl mac build",
            ),
            ("networkmode", "host", "--network host", "NAT"),
            ("cachefrom", r#"["x"]"#, "--cache-from", "fluxctl mac cache"),
            ("version", "2", "BuildKit", "DOCKER_BUILDKIT=0"),
        ] {
            let e = parse_query(&q(&[(p, v)])).unwrap_err();
            assert_eq!(e.status, 501, "{p}");
            assert!(e.message.contains(flag), "{p}: {}", e.message);
            assert!(e.message.contains(hint), "{p}: {}", e.message);
        }
    }

    #[test]
    fn bad_json_and_bad_tags_are_400() {
        assert_eq!(
            parse_query(&q(&[("buildargs", "{nope")]))
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(parse_query(&q(&[("t", "a b")])).unwrap_err().status, 400);
    }

    #[test]
    fn equivalent_command_hides_build_arg_values() {
        let r = parse_query(&q(&[
            ("t", "a:1"),
            ("buildargs", r#"{"TOKEN":"hunter2"}"#),
            ("nocache", "1"),
            ("dockerfile", "M"),
        ]))
        .unwrap();
        let c = fluxctl_equivalent(&r);
        assert_eq!(
            c,
            "fluxctl mac build -t a:1 -f M --build-arg TOKEN=VALUE --no-cache ."
        );
        assert!(!c.contains("hunter2"));
    }

    #[test]
    fn stream_lines_are_docker_json() {
        assert_eq!(
            stream_line("Step 1/1 : FROM x"),
            "{\"stream\":\"Step 1/1 : FROM x\\n\"}\n"
        );
        assert_eq!(aux_line("sha256:ab"), "{\"aux\":{\"ID\":\"sha256:ab\"}}\n");
        let e: Value = serde_json::from_str(error_line("boom").trim()).unwrap();
        assert_eq!(e["errorDetail"]["message"], "boom");
        assert_eq!(e["error"], "boom");
        assert_eq!(short_id("sha256:0123456789abcdef"), "0123456789ab");
    }

    #[test]
    fn dockerignore_follows_dockers_rules() {
        let ig = Ignore::parse(
            "# c\n*.md\n**/*.log\nnode_modules\n!node_modules/keep\n/tmp/\nbuild/**\n!build/out/ok.txt\nf?o.[ab]\n",
        );
        assert!(ig.ignored("README.md"));
        assert!(!ig.ignored("docs/README.md"), "*.md is root-anchored");
        assert!(ig.ignored("a/b/c.log") && ig.ignored("c.log"));
        assert!(ig.ignored("node_modules/x/y.js"));
        assert!(
            !ig.ignored("node_modules/keep/a.js"),
            "an exception wins when it comes later"
        );
        assert!(ig.ignored("tmp/x") && ig.ignored("tmp"));
        assert!(ig.ignored("build/a") && !ig.ignored("build/out/ok.txt"));
        assert!(ig.ignored("foo.a") && ig.ignored("fxo.b") && !ig.ignored("foo.c"));
        assert!(!ig.ignored("src/main.rs"));
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (p, d) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, p, *d).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn unpacks_plain_and_gzip_contexts_and_applies_dockerignore() {
        let d = tempfile::tempdir().unwrap();
        let t = tar_of(&[
            ("Dockerfile", b"FROM base:15\n"),
            (".dockerignore", b"*.log\nsecret/\n"),
            ("src/a.rs", b"x"),
            ("x.log", b"y"),
            ("secret/key", b"k"),
        ]);
        for gz in [false, true] {
            let tp = d.path().join("ctx.tar");
            if gz {
                use std::io::Write;
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(&t).unwrap();
                std::fs::write(&tp, e.finish().unwrap()).unwrap();
            } else {
                std::fs::write(&tp, &t).unwrap();
            }
            let out = d.path().join(if gz { "gz" } else { "plain" });
            std::fs::create_dir_all(&out).unwrap();
            let info = extract_context(&tp, &out, MAX_CONTEXT_BYTES).unwrap();
            assert_eq!(info.files, 5);
            assert_eq!(apply_dockerignore(&out, "Dockerfile").unwrap(), 2);
            assert!(out.join("Dockerfile").is_file() && out.join("src/a.rs").is_file());
            assert!(!out.join("x.log").exists() && !out.join("secret").exists());
        }
    }

    #[test]
    fn the_dockerfile_survives_its_own_ignore_rule() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Dockerfile"), "FROM x").unwrap();
        std::fs::write(
            d.path().join(".dockerignore"),
            "Dockerfile\n.dockerignore\n",
        )
        .unwrap();
        assert_eq!(apply_dockerignore(d.path(), "Dockerfile").unwrap(), 0);
        assert!(d.path().join("Dockerfile").is_file());
    }

    #[test]
    fn a_path_out_of_the_context_is_refused() {
        let d = tempfile::tempdir().unwrap();
        // Written by hand: the tar builder refuses `..` itself.
        let mut t = tar_of(&[("a", b"x")]);
        let name = b"../evil";
        t[..name.len()].copy_from_slice(name);
        t[name.len()..100].fill(0);
        let mut sum = 0u32;
        t[148..156].fill(b' ');
        for b in &t[..512] {
            sum += *b as u32;
        }
        t[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let tp = d.path().join("c.tar");
        std::fs::write(&tp, &t).unwrap();
        let out = d.path().join("o");
        std::fs::create_dir_all(&out).unwrap();
        let e = extract_context(&tp, &out, MAX_CONTEXT_BYTES).unwrap_err();
        assert!(e.contains("leaves the context"), "{e}");
        assert!(!d.path().join("evil").exists());
    }

    #[test]
    fn a_huge_context_and_garbage_are_explained() {
        let d = tempfile::tempdir().unwrap();
        let tp = d.path().join("c.tar");
        std::fs::write(&tp, tar_of(&[("a", &[0u8; 2048])])).unwrap();
        let e = extract_context(&tp, d.path(), 1024).unwrap_err();
        assert!(e.contains(".dockerignore"), "{e}");
        std::fs::write(&tp, b"not a tar at all, just text that goes on for a while").unwrap();
        // Short garbage reads as an empty archive or an error; either way nothing is built from it.
        let r = extract_context(&tp, d.path(), 1024);
        assert!(r.is_err() || r.unwrap().files == 0);
    }

    #[test]
    fn links_may_not_point_out() {
        assert!(link_stays_inside(
            Path::new("a/b/l"),
            Path::new("../c"),
            false
        ));
        assert!(!link_stays_inside(
            Path::new("a/l"),
            Path::new("../../etc"),
            false
        ));
        assert!(!link_stays_inside(
            Path::new("l"),
            Path::new("/etc/passwd"),
            false
        ));
        assert!(link_stays_inside(Path::new("l"), Path::new("a/b"), true));
    }

    #[test]
    fn finds_the_build_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Macfile"), "FROM x").unwrap();
        assert_eq!(find_build_file(d.path(), "Dockerfile").unwrap(), "Macfile");
        assert!(
            find_build_file(d.path(), "Other")
                .unwrap_err()
                .contains("Macfile")
        );
        assert!(
            find_build_file(d.path(), "../x")
                .unwrap_err()
                .contains("inside the build context")
        );
        std::fs::write(d.path().join("Dockerfile"), "FROM x").unwrap();
        assert_eq!(
            find_build_file(d.path(), "./Dockerfile").unwrap(),
            "Dockerfile"
        );
    }

    #[test]
    fn the_mac_build_command_carries_every_flag() {
        let r = parse_query(&q(&[
            ("t", "a:1"),
            ("buildargs", r#"{"V":"2"}"#),
            ("labels", r#"{"k":"v"}"#),
            ("platform", "darwin/arm64"),
            ("target", "t"),
            ("nocache", "1"),
        ]))
        .unwrap();
        let a = mac_build_args(&BuildJob {
            request: r,
            context_dir: "/c".into(),
            build_file: "Macfile".into(),
        });
        assert_eq!(
            a,
            [
                "mac",
                "build",
                "-t",
                "a:1",
                "-f",
                "/c/Macfile",
                "--build-arg",
                "V=2",
                "--label",
                "k=v",
                "--platform",
                "darwin/arm64",
                "--target",
                "t",
                "--no-cache",
                "/c"
            ]
        );
    }

    #[test]
    fn image_ids_come_from_the_store_digest() {
        let h = "a".repeat(64);
        assert_eq!(
            image_id_from_digest(&format!("sha256:{h}")),
            format!("sha256:{h}")
        );
        let odd = image_id_from_digest("weird");
        assert!(odd.starts_with("sha256:") && odd.len() == 71);
        assert_eq!(image_id_from_digest("weird"), odd);
        let j = r#"[{"reference":"app:1","digest":"sha256:x"},{"reference":"web:latest","digest":"d2"}]"#;
        assert_eq!(digest_in_images(j, "app:1").as_deref(), Some("sha256:x"));
        assert_eq!(digest_in_images(j, "web").as_deref(), Some("d2"));
        assert_eq!(digest_in_images(j, "none"), None);
    }

    fn rec(id: &str, state: &str, tags: &[&str], image: Option<&str>) -> BuildRecord {
        BuildRecord {
            id: id.into(),
            started_unix: 1,
            finished_unix: Some(2),
            duration_ms: Some(1000),
            state: state.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            dockerfile: "Dockerfile".into(),
            build_arg_names: vec![],
            target: None,
            platform: None,
            no_cache: false,
            context_bytes: 0,
            image_id: image.map(str::to_owned),
            error: None,
            builder: "fake".into(),
            fluxctl: "fluxctl mac build".into(),
        }
    }

    #[test]
    fn history_persists_caps_and_tracks_names() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(HISTORY_FILE);
        let h = History::new(Some(p.clone()));
        h.start(rec("1", "succeeded", &["app"], Some("sha256:one")));
        h.start(rec("2", "succeeded", &["app:2", "app"], Some("sha256:two")));
        h.start(rec("3", "failed", &["bad"], None));
        h.start(rec("4", "running", &["live"], None));
        assert_eq!(
            h.image_for("app").as_deref(),
            Some("sha256:two"),
            "a newer build owns the name"
        );
        assert_eq!(h.image_for("app:2").as_deref(), Some("sha256:two"));
        assert_eq!(h.image_for("bad"), None);
        let imgs = h.images();
        assert_eq!(
            imgs.len(),
            1,
            "the older build lost its only name: {imgs:?}"
        );
        assert_eq!(imgs[0].0, "sha256:two");
        // A reload keeps it, and a build that was running is marked failed.
        let h2 = History::new(Some(p));
        let l = h2.list();
        assert_eq!(l[0].id, "4");
        assert_eq!(l[0].state, "failed");
        assert!(l[0].error.as_ref().unwrap().contains("Run the build again"));
        for i in 0..(HISTORY_LIMIT + 5) {
            h2.start(rec(&format!("n{i}"), "failed", &[], None));
        }
        assert_eq!(h2.list().len(), HISTORY_LIMIT);
    }

    #[test]
    fn the_view_says_why_it_cannot_build() {
        let v = view(None);
        assert_eq!(v["enabled"], false);
        assert!(v["reason"].as_str().unwrap().contains("fluxctl mac build"));
        let b = FluxctlBuilder {
            bin: None,
            url: None,
            token: None,
            is_mac: true,
        };
        assert!(b.unavailable().unwrap().contains("fluxctl was not found"));
        let b = FluxctlBuilder {
            bin: Some("/x".into()),
            url: None,
            token: None,
            is_mac: false,
        };
        assert!(b.unavailable().unwrap().contains("fluxctl docker up"));
        let b = FluxctlBuilder {
            bin: Some("/x".into()),
            url: None,
            token: None,
            is_mac: true,
        };
        assert!(b.unavailable().is_none());
        let e = BuildEngine::new(Arc::new(b), Arc::new(History::new(None)));
        let v = view(Some(&e));
        assert_eq!(v["available"], true);
        assert_eq!(v["builder"], "fluxctl mac build");
    }
}
