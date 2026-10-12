// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Ephemeral GitHub Actions runners on FluxVM VMs: `fluxvm-agent gha-runners --config gha.json`.
//!
//! Every poll it looks for queued workflow jobs in the configured repositories. For each new job whose labels match a
//! pool, it asks GitHub for a just-in-time (JIT) runner configuration, creates a VM through `fluxvm-agent central`
//! (`POST /fleet/vms`, so tenancy, leases and quotas apply), and starts the runner in the guest with that config. A JIT
//! runner takes exactly one job and then unregisters itself; once GitHub no longer knows the runner, its VM is deleted.
//! A runner older than `job_timeout_secs` is removed from GitHub and its VM deleted regardless.
//!
//! The runner must already be installed in the image (`runner_dir`, default `/opt/actions-runner`). Two ways to start
//! it:
//!
//! - `cloud-init` (Linux guests): the start command is appended to the VM's `cloud_init.runcmd`.
//! - `exec` (macOS guests, or images without cloud-init): after the create, the command is run through the guest agent
//!   (`POST /fleet/vms/{node}/{id}/exec`), retried until the agent answers or `boot_timeout_secs` passes.
//!
//! The JIT config is a credential for one runner registration. It travels in the create body (cloud-init) or the exec
//! call; it is never logged, and VMs are created without `"reschedulable"`, so central never stores it.
//!
//! A pool can take its VMs from a central warm pool (`/fleet/pools`) by naming it in its `vm` body: `"vm": {"pool":
//! "macos-ci"}` with `"launch": "exec"`. Each job's create then gets an already-booted VM in seconds, renamed to the
//! runner's name, and the runner starts through the guest agent; central boots a replacement in the background. A warm
//! VM has already booted, so it cannot take cloud-init: such a pool must use `exec` and carry no `cloud_init`.
//!
//! On start the scaler adopts what a previous run left: a fleet VM named `fluxvm-gha-*` whose GitHub runner still
//! exists is tracked again; one without a runner is deleted, as is a `fluxvm-gha-*` runner registration without a VM.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Every runner and VM this scaler creates is named with this prefix.
pub const NAME_PREFIX: &str = "fluxvm-gha-";
const GITHUB_API_VERSION: &str = "2022-11-28";
/// How long a job id is remembered as served (a job queued longer than this gets another runner).
const SERVED_TTL: Duration = Duration::from_secs(6 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerScope {
    /// Runners register on each repository whose job they serve.
    #[default]
    Repo,
    /// Runners register on the organization (`owner`), in `runner_group_id`.
    Org,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Launch {
    #[default]
    CloudInit,
    Exec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubConfig {
    #[serde(default = "default_api")]
    pub api_url: String,
    pub owner: String,
    /// Repositories (names without the owner) whose queued jobs are watched.
    pub repos: Vec<String>,
    #[serde(default)]
    pub scope: RunnerScope,
    /// Environment variable holding a token that can read Actions and administer self-hosted runners
    /// (fine-grained: "Administration: write" on the repos, or "Self-hosted runners: write" on the org).
    #[serde(default = "default_github_token_env")]
    pub token_env: String,
    #[serde(default = "default_group")]
    pub runner_group_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetConfig {
    pub url: String,
    /// Environment variable holding the fleet bearer token (a tenant token, so runner VMs land on the tenant's
    /// leased Macs and count against its quota).
    #[serde(default = "default_fleet_token_env")]
    pub token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    /// A job is served by the first pool whose labels include all of the job's labels (case-insensitive). Include
    /// `self-hosted`.
    pub labels: Vec<String>,
    #[serde(default = "default_max_runners")]
    pub max_runners: usize,
    /// A `POST /fleet/vms` body; `name` is set per runner.
    pub vm: Value,
    #[serde(default)]
    pub launch: Launch,
    #[serde(default = "default_runner_dir")]
    pub runner_dir: String,
    /// Run the runner as this user (recommended); unset runs it as root with `RUNNER_ALLOW_RUNASROOT=1`.
    #[serde(default)]
    pub runner_user: Option<String>,
    #[serde(default = "default_job_timeout")]
    pub job_timeout_secs: u64,
    #[serde(default = "default_boot_timeout")]
    pub boot_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhaConfig {
    pub github: GithubConfig,
    pub fleet: FleetConfig,
    pub pools: Vec<Pool>,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Optional `workflow_job` webhook listener: queued jobs start a runner at once instead of on the next poll.
    /// Polling keeps running as a safety net (missed deliveries, restarts).
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Address for `POST /github/webhook`, e.g. `0.0.0.0:8790` (put TLS in front of it).
    pub listen: String,
    /// Environment variable holding the webhook secret; deliveries without a valid `X-Hub-Signature-256` get a 401.
    #[serde(default = "default_webhook_secret_env")]
    pub secret_env: String,
}

fn default_webhook_secret_env() -> String {
    "GITHUB_WEBHOOK_SECRET".into()
}

/// Most pages read from one paginated GitHub list (100 items each).
const MAX_PAGES: usize = 20;

/// The `rel="next"` URL of a GitHub `Link` header.
pub fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == "rel=\"next\"")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

/// HMAC-SHA256 (RFC 2104) over `data`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// Checks GitHub's `X-Hub-Signature-256: sha256=<hex>` in constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let want: String = hmac_sha256(secret, body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex.len() == want.len()
        && hex
            .bytes()
            .zip(want.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a.to_ascii_lowercase() ^ b))
            == 0
}

/// A queued job: repository (without owner), job id, labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub repo: String,
    pub id: u64,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookEvent {
    Queued(QueuedJob),
    /// A job finished somewhere: worth retiring runners now rather than at the next poll.
    Completed,
}

/// Reads a `workflow_job` delivery for one of the watched repositories; anything else is `None`.
pub fn parse_webhook(
    event: &str,
    body: &[u8],
    owner: &str,
    repos: &[String],
) -> Option<WebhookEvent> {
    if event != "workflow_job" {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let full = v["repository"]["full_name"].as_str()?;
    let (o, repo) = full.split_once('/')?;
    if !o.eq_ignore_ascii_case(owner) || !repos.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
        return None;
    }
    match v["action"].as_str()? {
        "queued" => {
            let job = &v["workflow_job"];
            Some(WebhookEvent::Queued(QueuedJob {
                repo: repo.to_owned(),
                id: job["id"].as_u64()?,
                labels: job["labels"]
                    .as_array()?
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect(),
            }))
        }
        "completed" => Some(WebhookEvent::Completed),
        _ => None,
    }
}

/// `POST /github/webhook`: verifies the signature and forwards `workflow_job` events to the scaler.
pub fn webhook_router(
    secret: Vec<u8>,
    owner: String,
    repos: Vec<String>,
    tx: tokio::sync::mpsc::UnboundedSender<WebhookEvent>,
) -> axum::Router {
    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    let secret = Arc::new(secret);
    axum::Router::new().route(
        "/github/webhook",
        post(move |headers: HeaderMap, body: Bytes| {
            let (secret, owner, repos, tx) =
                (secret.clone(), owner.clone(), repos.clone(), tx.clone());
            async move {
                let sig = headers
                    .get("x-hub-signature-256")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                if !verify_signature(&secret, &body, sig) {
                    return StatusCode::UNAUTHORIZED;
                }
                let event = headers
                    .get("x-github-event")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                match parse_webhook(event, &body, &owner, &repos) {
                    Some(ev) => {
                        let _ = tx.send(ev);
                        StatusCode::ACCEPTED
                    }
                    None => StatusCode::NO_CONTENT,
                }
            }
        }),
    )
}

fn default_api() -> String {
    "https://api.github.com".into()
}
fn default_github_token_env() -> String {
    "GITHUB_TOKEN".into()
}
fn default_fleet_token_env() -> String {
    "FLUXVM_AGENT_TOKEN".into()
}
fn default_group() -> u64 {
    1
}
fn default_max_runners() -> usize {
    2
}
fn default_runner_dir() -> String {
    "/opt/actions-runner".into()
}
fn default_job_timeout() -> u64 {
    6 * 3600
}
fn default_boot_timeout() -> u64 {
    600
}
fn default_poll() -> u64 {
    15
}

/// Characters allowed in values spliced into the guest's shell command.
fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
}

impl GhaConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.pools.is_empty(), "no pools configured");
        ensure!(!self.github.repos.is_empty(), "github.repos is empty");
        let mut names = HashSet::new();
        for p in &self.pools {
            ensure!(
                p.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !p.name.is_empty()
                    && p.name.len() <= 20,
                "pool name {:?}: use up to 20 lowercase letters, digits and '-'",
                p.name
            );
            ensure!(names.insert(&p.name), "pool {:?} defined twice", p.name);
            ensure!(!p.labels.is_empty(), "pool {:?} has no labels", p.name);
            ensure!(p.max_runners > 0, "pool {:?}: max_runners is 0", p.name);
            ensure!(
                p.vm.is_object(),
                "pool {:?}: vm must be a JSON object",
                p.name
            );
            // ---- warmpool ----
            if p.vm.get("pool").is_some_and(|v| !v.is_null()) {
                ensure!(
                    p.launch == Launch::Exec && p.vm.get("cloud_init").is_none_or(Value::is_null),
                    "pool {:?}: VMs from a warm pool are already booted, so cloud-init cannot start the runner: use \"launch\": \"exec\" and no cloud_init",
                    p.name
                );
            }
            ensure!(
                shell_safe(&p.runner_dir),
                "pool {:?}: runner_dir {:?} has unsupported characters",
                p.name,
                p.runner_dir
            );
            if let Some(u) = &p.runner_user {
                ensure!(
                    shell_safe(u),
                    "pool {:?}: runner_user {u:?} has unsupported characters",
                    p.name
                );
            }
        }
        Ok(())
    }
}

/// `job_labels ⊆ pool_labels`, ignoring case.
pub fn labels_match(job_labels: &[String], pool_labels: &[String]) -> bool {
    let pool: HashSet<String> = pool_labels.iter().map(|l| l.to_lowercase()).collect();
    !job_labels.is_empty() && job_labels.iter().all(|l| pool.contains(&l.to_lowercase()))
}

pub fn pick_pool<'a>(pools: &'a [Pool], job_labels: &[String]) -> Option<&'a Pool> {
    pools.iter().find(|p| labels_match(job_labels, &p.labels))
}

/// The guest command that starts a JIT runner in the background.
pub fn launch_command(pool: &Pool, jit_config: &str) -> Result<String> {
    ensure!(
        !jit_config.is_empty()
            && jit_config
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
        "GitHub returned a JIT config that is not base64"
    );
    let run = format!("{}/run.sh --jitconfig {jit_config}", pool.runner_dir);
    let inner = match &pool.runner_user {
        Some(u) => format!("su -l {u} -c '{run}'"),
        None => format!("RUNNER_ALLOW_RUNASROOT=1 {run}"),
    };
    Ok(format!(
        "nohup sh -c \"{inner}\" >/tmp/fluxvm-actions-runner.log 2>&1 &"
    ))
}

/// The `POST /fleet/vms` body for one runner VM.
pub fn vm_body(pool: &Pool, name: &str, jit_config: &str) -> Result<Value> {
    let mut body = pool.vm.clone();
    let obj = body.as_object_mut().context("pool vm must be an object")?;
    obj.insert("name".into(), json!(name));
    obj.remove("reschedulable");
    // A safety net: the VM goes away even if this scaler dies.
    obj.entry("ttl_seconds")
        .or_insert_with(|| json!(pool.job_timeout_secs + pool.boot_timeout_secs));
    if pool.launch == Launch::CloudInit {
        let cmd = launch_command(pool, jit_config)?;
        let ci = obj
            .entry("cloud_init")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("vm.cloud_init must be an object")?;
        let runcmd = ci
            .entry("runcmd")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("vm.cloud_init.runcmd must be an array")?;
        runcmd.push(json!(cmd));
    }
    Ok(body)
}

/// One runner this scaler started.
#[derive(Debug, Clone)]
pub struct Runner {
    pub name: String,
    pub pool: String,
    pub runner_id: u64,
    /// `orgs/{owner}` or `repos/{owner}/{repo}`: where the runner is registered.
    pub scope_path: String,
    pub node: String,
    pub vm_id: String,
    pub started: Instant,
    pub job_timeout: Duration,
}

struct Ctx {
    cfg: GhaConfig,
    http: reqwest::Client,
    github_token: String,
    fleet_token: Option<String>,
}

impl Ctx {
    fn gh(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/{path}", self.cfg.github.api_url))
            .bearer_auth(&self.github_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
    }

    fn fleet(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self
            .http
            .request(method, format!("{}{path}", self.cfg.fleet.url));
        match &self.fleet_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    fn scope_path(&self, repo: &str) -> String {
        match self.cfg.github.scope {
            RunnerScope::Org => format!("orgs/{}", self.cfg.github.owner),
            RunnerScope::Repo => format!("repos/{}/{repo}", self.cfg.github.owner),
        }
    }

    async fn gh_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self.gh(method.clone(), path);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GitHub {method} {path}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        ensure!(
            status.is_success(),
            "GitHub {method} {path}: HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        );
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Every item under `key` of a paginated GitHub list, following `Link: rel="next"` (same API host only, so the
    /// token never leaves it) for up to [`MAX_PAGES`] pages.
    async fn gh_paged(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        let base = self.cfg.github.api_url.trim_end_matches('/').to_owned();
        let mut url = format!("{base}/{path}");
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.github_token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .send()
                .await
                .with_context(|| format!("GitHub GET {path}"))?;
            let status = resp.status();
            let next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(next_link);
            let text = resp.text().await.unwrap_or_default();
            ensure!(
                status.is_success(),
                "GitHub GET {path}: HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            out.extend(v[key].as_array().cloned().unwrap_or_default());
            match next {
                Some(n) if n.starts_with(&format!("{base}/")) => url = n,
                Some(n) => bail!("GitHub pagination pointed off the API host: {n}"),
                None => return Ok(out),
            }
        }
        tracing::warn!(
            path,
            pages = MAX_PAGES,
            "stopped following GitHub pagination"
        );
        Ok(out)
    }

    /// Queued jobs of `repo` as `(job id, labels)`.
    async fn queued_jobs(&self, repo: &str) -> Result<Vec<(u64, Vec<String>)>> {
        let owner = &self.cfg.github.owner;
        let runs = self
            .gh_paged(
                &format!("repos/{owner}/{repo}/actions/runs?status=queued&per_page=100"),
                "workflow_runs",
            )
            .await?;
        let mut out = Vec::new();
        for run in &runs {
            let Some(run_id) = run["id"].as_u64() else {
                continue;
            };
            let jobs = self
                .gh_paged(
                    &format!(
                        "repos/{owner}/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"
                    ),
                    "jobs",
                )
                .await?;
            for job in &jobs {
                if job["status"].as_str() != Some("queued") {
                    continue;
                }
                let (Some(id), Some(labels)) = (job["id"].as_u64(), job["labels"].as_array())
                else {
                    continue;
                };
                let labels = labels
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect();
                out.push((id, labels));
            }
        }
        Ok(out)
    }

    async fn runner_exists(&self, scope_path: &str, id: u64) -> Result<bool> {
        let resp = self
            .gh(
                reqwest::Method::GET,
                &format!("{scope_path}/actions/runners/{id}"),
            )
            .send()
            .await
            .context("GitHub runner lookup")?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            s => bail!("GitHub runner {id} lookup: HTTP {s}"),
        }
    }

    async fn delete_runner(&self, scope_path: &str, id: u64) -> Result<()> {
        let resp = self
            .gh(
                reqwest::Method::DELETE,
                &format!("{scope_path}/actions/runners/{id}"),
            )
            .send()
            .await
            .context("GitHub runner delete")?;
        let s = resp.status().as_u16();
        ensure!(s == 204 || s == 404, "GitHub runner {id} delete: HTTP {s}");
        Ok(())
    }

    /// Runners named with [`NAME_PREFIX`] in one scope: `(id, name)`.
    async fn our_runners(&self, scope_path: &str) -> Result<Vec<(u64, String)>> {
        let runners = self
            .gh_paged(
                &format!("{scope_path}/actions/runners?per_page=100"),
                "runners",
            )
            .await?;
        Ok(runners
            .iter()
            .filter_map(|r| {
                let name = r["name"].as_str()?;
                name.starts_with(NAME_PREFIX)
                    .then(|| (r["id"].as_u64(), name.to_owned()))
                    .and_then(|(id, n)| id.map(|id| (id, n)))
            })
            .collect())
    }

    async fn delete_vm(&self, node: &str, vm_id: &str) -> Result<()> {
        let resp = self
            .fleet(
                reqwest::Method::DELETE,
                &format!("/fleet/vms/{node}/{vm_id}"),
            )
            .send()
            .await
            .context("fleet delete")?;
        let s = resp.status();
        ensure!(
            s.is_success() || s.as_u16() == 404,
            "fleet delete of {vm_id} on {node}: HTTP {s}"
        );
        Ok(())
    }

    /// Registers a JIT runner, creates its VM and starts it. On any failure after registration, the registration
    /// (and VM, if created) is removed again.
    async fn provision(&self, pool: &Pool, repo: &str) -> Result<Runner> {
        let name = format!(
            "{NAME_PREFIX}{}-{}",
            pool.name,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let scope_path = self.scope_path(repo);
        let mut jit_body = json!({
            "name": name,
            "labels": pool.labels,
            "work_folder": "_work",
        });
        jit_body["runner_group_id"] = json!(self.cfg.github.runner_group_id);
        let jit = self
            .gh_json(
                reqwest::Method::POST,
                &format!("{scope_path}/actions/runners/generate-jitconfig"),
                Some(jit_body),
            )
            .await?;
        let runner_id = jit["runner"]["id"]
            .as_u64()
            .context("generate-jitconfig returned no runner id")?;
        let config = jit["encoded_jit_config"]
            .as_str()
            .context("generate-jitconfig returned no encoded_jit_config")?
            .to_owned();

        let created = async {
            let body = vm_body(pool, &name, &config)?;
            let resp = self
                .fleet(reqwest::Method::POST, "/fleet/vms")
                .json(&body)
                .send()
                .await
                .context("fleet create")?;
            let status = resp.status();
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            ensure!(status.is_success(), "fleet create: HTTP {status}: {v}");
            let node = v["node"]
                .as_str()
                .context("fleet create: no node")?
                .to_owned();
            let vm_id = v["vm"]["id"]
                .as_str()
                .context("fleet create: no vm id")?
                .to_owned();
            anyhow::Ok((node, vm_id))
        }
        .await;
        let (node, vm_id) = match created {
            Ok(x) => x,
            Err(e) => {
                let _ = self.delete_runner(&scope_path, runner_id).await;
                return Err(e);
            }
        };
        if pool.launch == Launch::Exec
            && let Err(e) = self.exec_launch(pool, &node, &vm_id, &config).await
        {
            let _ = self.delete_vm(&node, &vm_id).await;
            let _ = self.delete_runner(&scope_path, runner_id).await;
            return Err(e);
        }
        Ok(Runner {
            name,
            pool: pool.name.clone(),
            runner_id,
            scope_path,
            node,
            vm_id,
            started: Instant::now(),
            job_timeout: Duration::from_secs(pool.job_timeout_secs),
        })
    }

    async fn exec_launch(&self, pool: &Pool, node: &str, vm_id: &str, config: &str) -> Result<()> {
        let cmd = launch_command(pool, config)?;
        let deadline = Instant::now() + Duration::from_secs(pool.boot_timeout_secs);
        let mut last = String::new();
        while Instant::now() < deadline {
            let resp = self
                .fleet(
                    reqwest::Method::POST,
                    &format!("/fleet/vms/{node}/{vm_id}/exec"),
                )
                .json(&json!({"command": cmd, "timeout_seconds": 30}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    let v: Value = r.json().await.unwrap_or(Value::Null);
                    let code = v["exit_code"].as_i64().unwrap_or(0);
                    ensure!(code == 0, "runner start exited {code}: {}", v["stderr"]);
                    return Ok(());
                }
                Ok(r) => last = format!("HTTP {}", r.status()),
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        bail!(
            "guest agent did not answer within {}s ({last})",
            pool.boot_timeout_secs
        )
    }
}

/// The scaler's state between polls.
pub struct Scaler {
    ctx: Arc<Ctx>,
    pub runners: Vec<Runner>,
    served: HashMap<u64, Instant>,
}

impl Scaler {
    pub fn new(cfg: GhaConfig) -> Result<Self> {
        cfg.validate()?;
        let github_token = std::env::var(&cfg.github.token_env)
            .with_context(|| format!("{} is not set", cfg.github.token_env))?;
        let fleet_token = std::env::var(&cfg.fleet.token_env).ok();
        Self::with_tokens(cfg, github_token, fleet_token)
    }

    pub fn with_tokens(
        cfg: GhaConfig,
        github_token: String,
        fleet_token: Option<String>,
    ) -> Result<Self> {
        cfg.validate()?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("fluxvm-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            ctx: Arc::new(Ctx {
                cfg,
                http,
                github_token,
                fleet_token,
            }),
            runners: Vec::new(),
            served: HashMap::new(),
        })
    }

    fn scope_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .ctx
            .cfg
            .github
            .repos
            .iter()
            .map(|r| self.ctx.scope_path(r))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Adopts runners a previous run left behind and removes orphans (see the module docs).
    pub async fn adopt(&mut self) -> Result<Value> {
        let mut registered: HashMap<String, (u64, String)> = HashMap::new();
        for sp in self.scope_paths() {
            for (id, name) in self.ctx.our_runners(&sp).await? {
                registered.insert(name, (id, sp.clone()));
            }
        }
        let resp = self
            .ctx
            .fleet(reqwest::Method::GET, "/fleet/vms")
            .send()
            .await
            .context("fleet list")?;
        ensure!(
            resp.status().is_success(),
            "fleet list: HTTP {}",
            resp.status()
        );
        let list: Value = resp.json().await?;
        let (mut adopted, mut deleted_vms, mut deleted_runners) = (0, 0, 0);
        let mut with_vm = HashSet::new();
        for vm in list["items"].as_array().into_iter().flatten() {
            let name = vm["name"].as_str().unwrap_or_default();
            if !name.starts_with(NAME_PREFIX) {
                continue;
            }
            let (Some(node), Some(id)) = (vm["node"].as_str(), vm["id"].as_str()) else {
                continue;
            };
            match registered.get(name) {
                Some((runner_id, sp)) => {
                    with_vm.insert(name.to_owned());
                    let pool = name
                        .strip_prefix(NAME_PREFIX)
                        .and_then(|r| r.rsplit_once('-'))
                        .map(|(p, _)| p.to_owned())
                        .unwrap_or_default();
                    let timeout = self
                        .ctx
                        .cfg
                        .pools
                        .iter()
                        .find(|p| p.name == pool)
                        .map_or(default_job_timeout(), |p| p.job_timeout_secs);
                    self.runners.push(Runner {
                        name: name.to_owned(),
                        pool,
                        runner_id: *runner_id,
                        scope_path: sp.clone(),
                        node: node.to_owned(),
                        vm_id: id.to_owned(),
                        started: Instant::now(),
                        job_timeout: Duration::from_secs(timeout),
                    });
                    adopted += 1;
                }
                None => {
                    if self.ctx.delete_vm(node, id).await.is_ok() {
                        deleted_vms += 1;
                    }
                }
            }
        }
        for (name, (id, sp)) in &registered {
            if !with_vm.contains(name) && self.ctx.delete_runner(sp, *id).await.is_ok() {
                deleted_runners += 1;
            }
        }
        Ok(json!({
            "adopted": adopted,
            "deleted_orphan_vms": deleted_vms,
            "deleted_orphan_runners": deleted_runners,
        }))
    }

    fn active_in(&self, pool: &str) -> usize {
        self.runners.iter().filter(|r| r.pool == pool).count()
    }

    /// One poll: retire finished or timed-out runners, then start runners for new queued jobs.
    pub async fn tick(&mut self) -> Result<Value> {
        let retired = self.retire().await;
        let mut failed = Vec::new();
        let mut jobs = Vec::new();
        for repo in self.ctx.cfg.github.repos.clone() {
            match self.ctx.queued_jobs(&repo).await {
                Ok(j) => jobs.extend(j.into_iter().map(|(id, labels)| QueuedJob {
                    repo: repo.clone(),
                    id,
                    labels,
                })),
                Err(e) => failed.push(json!({"repo": repo, "error": e.to_string()})),
            }
        }
        let (started, start_failed) = self.start_for_jobs(jobs).await;
        failed.extend(start_failed);
        Ok(json!({
            "active": self.runners.len(),
            "started": started,
            "retired": retired,
            "failed": failed,
        }))
    }

    /// Deletes the VMs of runners whose job is done (GitHub no longer knows the runner) or that timed out.
    pub async fn retire(&mut self) -> Vec<Value> {
        let now = Instant::now();
        self.served
            .retain(|_, t| now.duration_since(*t) < SERVED_TTL);
        let mut retired = Vec::new();
        let mut keep = Vec::new();
        for r in std::mem::take(&mut self.runners) {
            let timed_out = now.duration_since(r.started) > r.job_timeout;
            let gone = match self.ctx.runner_exists(&r.scope_path, r.runner_id).await {
                Ok(exists) => !exists,
                Err(e) => {
                    tracing::warn!(runner = %r.name, error = %e, "runner lookup failed; keeping it");
                    false
                }
            };
            if !(gone || timed_out) {
                keep.push(r);
                continue;
            }
            if timed_out && !gone {
                let _ = self.ctx.delete_runner(&r.scope_path, r.runner_id).await;
            }
            match self.ctx.delete_vm(&r.node, &r.vm_id).await {
                Ok(()) => retired.push(json!({
                    "runner": r.name,
                    "reason": if gone { "job done" } else { "timed out" },
                })),
                Err(e) => {
                    tracing::warn!(runner = %r.name, error = %e, "deleting runner VM failed; retrying next poll");
                    keep.push(r);
                }
            }
        }
        self.runners = keep;
        retired
    }

    /// Starts runners for the jobs not served yet, as far as each pool's `max_runners` allows. Returns
    /// `(started, failed)`; a failed job is forgotten so the next poll (or delivery) tries it again.
    pub async fn start_for_jobs(&mut self, jobs: Vec<QueuedJob>) -> (Vec<Value>, Vec<Value>) {
        let now = Instant::now();
        let mut started = Vec::new();
        let mut failed = Vec::new();
        let mut set = tokio::task::JoinSet::new();
        let mut planned: HashMap<String, usize> = HashMap::new();
        for QueuedJob {
            repo,
            id: job_id,
            labels,
        } in jobs
        {
            if self.served.contains_key(&job_id) {
                continue;
            }
            let Some(pool) = pick_pool(&self.ctx.cfg.pools, &labels) else {
                tracing::debug!(job_id, ?labels, "no pool matches this job");
                continue;
            };
            let busy = self.active_in(&pool.name) + planned.get(&pool.name).copied().unwrap_or(0);
            if busy >= pool.max_runners {
                continue;
            }
            *planned.entry(pool.name.clone()).or_default() += 1;
            self.served.insert(job_id, now);
            let ctx = self.ctx.clone();
            let pool = pool.clone();
            set.spawn(async move { (job_id, ctx.provision(&pool, &repo).await) });
        }
        while let Some(res) = set.join_next().await {
            match res {
                Ok((job_id, Ok(r))) => {
                    tracing::info!(runner = %r.name, node = %r.node, vm = %r.vm_id, job_id, "runner started");
                    started.push(
                        json!({"runner": r.name, "node": r.node, "vm": r.vm_id, "job": job_id}),
                    );
                    self.runners.push(r);
                }
                Ok((job_id, Err(e))) => {
                    tracing::warn!(job_id, error = %e, "starting a runner failed");
                    // Let the next poll try this job again.
                    self.served.remove(&job_id);
                    failed.push(json!({"job": job_id, "error": format!("{e:#}")}));
                }
                Err(e) => failed.push(json!({"error": e.to_string()})),
            }
        }
        (started, failed)
    }
}

/// `fluxvm-agent gha-runners`: adopt, then poll forever.
pub async fn run(cfg: GhaConfig) -> Result<()> {
    let every = Duration::from_secs(cfg.poll_interval_secs.max(5));
    let mut scaler = Scaler::new(cfg)?;
    match scaler.adopt().await {
        Ok(report) => tracing::info!(%report, "adopted runners from a previous run"),
        Err(e) => tracing::warn!(error = %e, "adopting previous runners failed"),
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    if let Some(w) = &scaler.ctx.cfg.webhook {
        let secret = std::env::var(&w.secret_env)
            .with_context(|| format!("{} is not set (webhook secret)", w.secret_env))?;
        ensure!(!secret.is_empty(), "{} is empty", w.secret_env);
        let app = webhook_router(
            secret.into_bytes(),
            scaler.ctx.cfg.github.owner.clone(),
            scaler.ctx.cfg.github.repos.clone(),
            tx,
        );
        let listener = tokio::net::TcpListener::bind(&w.listen)
            .await
            .with_context(|| format!("binding webhook listener {}", w.listen))?;
        tracing::info!(listen = %w.listen, "GitHub webhook listener up");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "webhook listener stopped");
            }
        });
    } else {
        // Keep the sender alive so `rx` never reports closed.
        std::mem::forget(tx);
    }
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => match scaler.tick().await {
                Ok(report) => tracing::debug!(%report, "gha poll"),
                Err(e) => tracing::warn!(error = %e, "gha poll failed"),
            },
            Some(ev) = rx.recv() => match ev {
                WebhookEvent::Queued(job) => {
                    let (started, failed) = scaler.start_for_jobs(vec![job]).await;
                    tracing::debug!(started = started.len(), failed = failed.len(), "webhook job");
                }
                WebhookEvent::Completed => {
                    let retired = scaler.retire().await;
                    tracing::debug!(retired = retired.len(), "webhook completion");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode,
        routing::{delete, get, post},
    };
    use std::sync::Mutex;

    fn pool(launch: Launch) -> Pool {
        Pool {
            name: "linux".into(),
            labels: vec!["self-hosted".into(), "Linux".into(), "fluxvm".into()],
            max_runners: 1,
            vm: json!({"backend": "vz", "image": "/images/debian-13.raw", "cloud_init": {"runcmd": ["echo hi"]}}),
            launch,
            runner_dir: "/opt/actions-runner".into(),
            runner_user: Some("runner".into()),
            job_timeout_secs: 3600,
            boot_timeout_secs: 30,
        }
    }

    #[test]
    fn labels_match_as_a_case_insensitive_subset() {
        let p = pool(Launch::CloudInit).labels;
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(labels_match(&s(&["self-hosted", "linux"]), &p));
        assert!(labels_match(&s(&["FLUXVM"]), &p));
        assert!(!labels_match(&s(&["self-hosted", "macos"]), &p));
        assert!(!labels_match(&s(&[]), &p));
    }

    #[test]
    fn cloud_init_launch_appends_the_runner_and_a_ttl() {
        let p = pool(Launch::CloudInit);
        let b = vm_body(&p, "fluxvm-gha-linux-abcd1234", "QUJD+/=").unwrap();
        assert_eq!(b["name"], "fluxvm-gha-linux-abcd1234");
        assert_eq!(b["ttl_seconds"], 3630);
        let runcmd = b["cloud_init"]["runcmd"].as_array().unwrap();
        assert_eq!(runcmd[0], "echo hi");
        assert_eq!(
            runcmd[1],
            "nohup sh -c \"su -l runner -c '/opt/actions-runner/run.sh --jitconfig QUJD+/='\" >/tmp/fluxvm-actions-runner.log 2>&1 &"
        );
        let exec = vm_body(&pool(Launch::Exec), "n", "QUJD").unwrap();
        assert_eq!(exec["cloud_init"]["runcmd"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hostile_values_never_reach_the_shell() {
        assert!(launch_command(&pool(Launch::Exec), "abc; rm -rf /").is_err());
        let mut p = pool(Launch::Exec);
        p.runner_user = None;
        assert!(
            launch_command(&p, "QUJD")
                .unwrap()
                .contains("RUNNER_ALLOW_RUNASROOT=1 /opt/actions-runner/run.sh")
        );
        let cfg = GhaConfig {
            github: GithubConfig {
                api_url: default_api(),
                owner: "zyvorai".into(),
                repos: vec!["app".into()],
                scope: RunnerScope::Repo,
                token_env: default_github_token_env(),
                runner_group_id: 1,
            },
            fleet: FleetConfig {
                url: "http://x".into(),
                token_env: default_fleet_token_env(),
            },
            pools: vec![Pool {
                runner_user: Some("bob'; reboot".into()),
                ..pool(Launch::Exec)
            }],
            poll_interval_secs: 15,
            webhook: None,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn warm_pool_runners_must_start_through_the_guest_agent() {
        let mut cfg: GhaConfig = serde_json::from_value(json!({
            "github": {"owner": "zyvorai", "repos": ["app"]},
            "fleet": {"url": "http://fleet:7799"},
            "pools": [{"name": "macos", "labels": ["self-hosted", "macOS"], "launch": "exec",
                       "vm": {"pool": "macos-ci"}}]
        }))
        .unwrap();
        cfg.validate().unwrap();
        let b = vm_body(&cfg.pools[0], "fluxvm-gha-macos-1", "QUJD").unwrap();
        assert_eq!(b["pool"], "macos-ci");
        assert!(b.get("cloud_init").is_none());
        cfg.pools[0].launch = Launch::CloudInit;
        assert!(cfg.validate().is_err());
        cfg.pools[0].launch = Launch::Exec;
        cfg.pools[0].vm["cloud_init"] = json!({"runcmd": []});
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_parses_with_defaults() {
        let cfg: GhaConfig = serde_json::from_value(json!({
            "github": {"owner": "zyvorai", "repos": ["zyvor-fluxvm"], "scope": "org"},
            "fleet": {"url": "http://fleet:7799"},
            "pools": [{"name": "macos", "labels": ["self-hosted", "macOS"], "launch": "exec",
                       "vm": {"backend": "vz", "apple": {"guest_os": "macos"}}}]
        }))
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.github.scope, RunnerScope::Org);
        assert_eq!(cfg.pools[0].launch, Launch::Exec);
        assert_eq!(cfg.pools[0].max_runners, 2);
        assert_eq!(cfg.poll_interval_secs, 15);
    }

    /// A fake GitHub and a fake fleet in one server.
    #[derive(Default)]
    struct Fake {
        queued: Vec<(u64, Vec<String>)>,
        runners: HashMap<u64, String>,
        next_runner: u64,
        jit_bodies: Vec<Value>,
        vms: HashMap<String, (String, Value)>,
        deleted_vms: Vec<String>,
        deleted_runners: Vec<u64>,
    }

    type S = Arc<Mutex<Fake>>;

    async fn serve() -> (String, S) {
        let s: S = Arc::new(Mutex::new(Fake {
            next_runner: 100,
            ..Default::default()
        }));
        let app = Router::new()
            .route(
                "/repos/{o}/{r}/actions/runs",
                get(|State(s): State<S>| async move {
                    let f = s.lock().unwrap();
                    let runs: Vec<Value> = if f.queued.is_empty() {
                        vec![]
                    } else {
                        vec![json!({"id": 1})]
                    };
                    Json(json!({"workflow_runs": runs}))
                }),
            )
            .route(
                "/repos/{o}/{r}/actions/runs/{id}/jobs",
                get(|State(s): State<S>| async move {
                    let f = s.lock().unwrap();
                    let jobs: Vec<Value> = f
                        .queued
                        .iter()
                        .map(|(id, l)| json!({"id": id, "status": "queued", "labels": l}))
                        .collect();
                    Json(json!({"jobs": jobs}))
                }),
            )
            .route(
                "/repos/{o}/{r}/actions/runners/generate-jitconfig",
                post(|State(s): State<S>, Json(b): Json<Value>| async move {
                    let mut f = s.lock().unwrap();
                    f.next_runner += 1;
                    let id = f.next_runner;
                    f.runners.insert(id, b["name"].as_str().unwrap().to_owned());
                    f.jit_bodies.push(b);
                    (
                        StatusCode::CREATED,
                        Json(json!({"runner": {"id": id}, "encoded_jit_config": "SklUQ09ORklH"})),
                    )
                }),
            )
            .route(
                "/repos/{o}/{r}/actions/runners",
                get(|State(s): State<S>| async move {
                    let f = s.lock().unwrap();
                    let runners: Vec<Value> = f
                        .runners
                        .iter()
                        .map(|(id, n)| json!({"id": id, "name": n}))
                        .collect();
                    Json(json!({"runners": runners}))
                }),
            )
            .route(
                "/repos/{o}/{r}/actions/runners/{id}",
                get(
                    |State(s): State<S>, Path((_, _, id)): Path<(String, String, u64)>| async move {
                        if s.lock().unwrap().runners.contains_key(&id) {
                            StatusCode::OK
                        } else {
                            StatusCode::NOT_FOUND
                        }
                    },
                )
                .delete(
                    |State(s): State<S>, Path((_, _, id)): Path<(String, String, u64)>| async move {
                        let mut f = s.lock().unwrap();
                        f.runners.remove(&id);
                        f.deleted_runners.push(id);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/fleet/vms",
                post(|State(s): State<S>, Json(b): Json<Value>| async move {
                    let mut f = s.lock().unwrap();
                    let id = format!("vm-{}", f.vms.len() + 1);
                    f.vms.insert(id.clone(), ("mac-a".into(), b.clone()));
                    Json(json!({"node": "mac-a", "vm": {"id": id, "name": b["name"]}}))
                })
                .get(|State(s): State<S>| async move {
                    let f = s.lock().unwrap();
                    let items: Vec<Value> = f
                        .vms
                        .iter()
                        .map(|(id, (node, b))| json!({"id": id, "node": node, "name": b["name"]}))
                        .collect();
                    Json(json!({"items": items}))
                }),
            )
            .route(
                "/fleet/vms/{node}/{id}",
                delete(
                    |State(s): State<S>, Path((_, id)): Path<(String, String)>| async move {
                        let mut f = s.lock().unwrap();
                        f.vms.remove(&id);
                        f.deleted_vms.push(id);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(s.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), s)
    }

    fn cfg(url: &str) -> GhaConfig {
        GhaConfig {
            github: GithubConfig {
                api_url: url.into(),
                owner: "zyvorai".into(),
                repos: vec!["app".into()],
                scope: RunnerScope::Repo,
                token_env: default_github_token_env(),
                runner_group_id: 1,
            },
            fleet: FleetConfig {
                url: url.into(),
                token_env: default_fleet_token_env(),
            },
            pools: vec![pool(Launch::CloudInit)],
            poll_interval_secs: 15,
            webhook: None,
        }
    }

    #[tokio::test]
    async fn a_queued_job_gets_a_runner_vm_that_goes_away_after_the_job() {
        let (url, fake) = serve().await;
        fake.lock().unwrap().queued = vec![
            (7, vec!["self-hosted".into(), "linux".into()]),
            (8, vec!["self-hosted".into(), "linux".into()]),
            (9, vec!["windows".into()]),
        ];
        let mut s = Scaler::with_tokens(cfg(&url), "gh".into(), Some("tenant".into())).unwrap();

        // max_runners = 1: one runner for job 7, none for 8 yet, none for the unmatched job 9.
        let r = s.tick().await.unwrap();
        assert_eq!(r["started"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(s.runners.len(), 1);
        {
            let f = fake.lock().unwrap();
            assert_eq!(f.vms.len(), 1);
            let (_, body) = f.vms.values().next().unwrap();
            assert!(
                body["name"]
                    .as_str()
                    .unwrap()
                    .starts_with("fluxvm-gha-linux-")
            );
            let runcmd = body["cloud_init"]["runcmd"].as_array().unwrap();
            assert!(
                runcmd[1]
                    .as_str()
                    .unwrap()
                    .contains("--jitconfig SklUQ09ORklH")
            );
            assert_eq!(f.jit_bodies[0]["runner_group_id"], 1);
        }

        // Still running: nothing changes; job 8 still waits for capacity.
        let r = s.tick().await.unwrap();
        assert_eq!(r["started"].as_array().unwrap().len(), 0, "{r}");

        // The job finishes: GitHub removes the ephemeral runner, the scaler deletes its VM and serves job 8.
        {
            let mut f = fake.lock().unwrap();
            f.runners.clear();
            f.queued.retain(|(id, _)| *id != 7);
        }
        let r = s.tick().await.unwrap();
        assert_eq!(r["retired"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(r["started"].as_array().unwrap().len(), 1, "{r}");
        let f = fake.lock().unwrap();
        assert_eq!(f.deleted_vms, vec!["vm-1".to_string()]);
        assert_eq!(f.vms.len(), 1);
    }

    #[test]
    fn next_links_are_found() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(
            next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#),
            None
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let hex: String = hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (RFC 4231 test case 6).
        let hex: String = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        )
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let hex: String = hmac_sha256(secret, body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256={hex}")
    }

    #[test]
    fn signatures_are_checked() {
        let sig = sign(b"s3cret", b"{}");
        assert!(verify_signature(b"s3cret", b"{}", &sig));
        assert!(verify_signature(
            b"s3cret",
            b"{}",
            &sig.to_uppercase().replacen("SHA256", "sha256", 1)
        ));
        assert!(!verify_signature(b"other", b"{}", &sig));
        assert!(!verify_signature(b"s3cret", b"{ }", &sig));
        assert!(!verify_signature(b"s3cret", b"{}", "sha1=abc"));
        assert!(!verify_signature(b"s3cret", b"{}", ""));
    }

    #[test]
    fn webhooks_are_parsed_for_watched_repos_only() {
        let body = |action: &str, repo: &str| {
            json!({
                "action": action,
                "repository": {"full_name": repo},
                "workflow_job": {"id": 42, "labels": ["self-hosted", "macOS"]},
            })
            .to_string()
        };
        let repos = vec!["App".to_string()];
        assert_eq!(
            parse_webhook(
                "workflow_job",
                body("queued", "zyvorai/app").as_bytes(),
                "zyvorai",
                &repos
            ),
            Some(WebhookEvent::Queued(QueuedJob {
                repo: "app".into(),
                id: 42,
                labels: vec!["self-hosted".into(), "macOS".into()],
            }))
        );
        assert_eq!(
            parse_webhook(
                "workflow_job",
                body("completed", "zyvorai/app").as_bytes(),
                "zyvorai",
                &repos
            ),
            Some(WebhookEvent::Completed)
        );
        assert_eq!(
            parse_webhook(
                "workflow_job",
                body("in_progress", "zyvorai/app").as_bytes(),
                "zyvorai",
                &repos
            ),
            None
        );
        assert_eq!(
            parse_webhook(
                "workflow_job",
                body("queued", "other/app").as_bytes(),
                "zyvorai",
                &repos
            ),
            None
        );
        assert_eq!(
            parse_webhook(
                "push",
                body("queued", "zyvorai/app").as_bytes(),
                "zyvorai",
                &repos
            ),
            None
        );
    }

    #[tokio::test]
    async fn the_webhook_endpoint_rejects_bad_signatures_and_forwards_jobs() {
        use tower::ServiceExt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app = webhook_router(b"s3cret".to_vec(), "zyvorai".into(), vec!["app".into()], tx);
        let body = json!({
            "action": "queued",
            "repository": {"full_name": "zyvorai/app"},
            "workflow_job": {"id": 7, "labels": ["self-hosted"]},
        })
        .to_string();
        let req = |sig: &str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/github/webhook")
                .header("x-github-event", "workflow_job")
                .header("x-hub-signature-256", sig)
                .body(axum::body::Body::from(body.clone()))
                .unwrap()
        };
        let resp = app.clone().oneshot(req("sha256=00")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
        let resp = app
            .clone()
            .oneshot(req(&sign(b"s3cret", body.as_bytes())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(matches!(rx.try_recv(), Ok(WebhookEvent::Queued(j)) if j.id == 7));
    }

    #[tokio::test]
    async fn paginated_lists_are_followed_on_the_api_host_only() {
        use axum::extract::Query;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let b2 = base.clone();
        let app = Router::new()
            .route(
                "/orgs/zyvorai/actions/runners",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let base = b2.clone();
                    async move {
                        let page: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                        let mut headers = axum::http::HeaderMap::new();
                        if page < 3 {
                            let next = format!(
                                "<{base}/orgs/zyvorai/actions/runners?per_page=100&page={}>; rel=\"next\"",
                                page + 1
                            );
                            headers.insert("link", next.parse().unwrap());
                        }
                        let runners = vec![json!({"id": page, "name": format!("{NAME_PREFIX}linux-p{page}")})];
                        (headers, Json(json!({"runners": runners})))
                    }
                }),
            )
            .route(
                "/orgs/evil/actions/runners",
                get(|| async {
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        "link",
                        "<https://attacker.example/steal>; rel=\"next\"".parse().unwrap(),
                    );
                    (headers, Json(json!({"runners": []})))
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut c = cfg(&base);
        c.github.scope = RunnerScope::Org;
        let s = Scaler::with_tokens(c, "gh".into(), None).unwrap();
        let mine = s.ctx.our_runners("orgs/zyvorai").await.unwrap();
        assert_eq!(mine.len(), 3);
        let err = s.ctx.our_runners("orgs/evil").await.unwrap_err();
        assert!(err.to_string().contains("off the API host"), "{err}");
    }

    #[tokio::test]
    async fn restarts_adopt_live_runners_and_clean_orphans() {
        let (url, fake) = serve().await;
        {
            let mut f = fake.lock().unwrap();
            // A live pair, a VM without a runner, a runner without a VM, and an unrelated VM.
            f.runners.insert(1, "fluxvm-gha-linux-aaaa1111".into());
            f.runners.insert(2, "fluxvm-gha-linux-bbbb2222".into());
            f.vms.insert(
                "vm-a".into(),
                ("mac-a".into(), json!({"name": "fluxvm-gha-linux-aaaa1111"})),
            );
            f.vms.insert(
                "vm-b".into(),
                ("mac-a".into(), json!({"name": "fluxvm-gha-linux-cccc3333"})),
            );
            f.vms
                .insert("vm-c".into(), ("mac-a".into(), json!({"name": "postgres"})));
        }
        let mut s = Scaler::with_tokens(cfg(&url), "gh".into(), None).unwrap();
        let r = s.adopt().await.unwrap();
        assert_eq!(r["adopted"], 1, "{r}");
        assert_eq!(r["deleted_orphan_vms"], 1, "{r}");
        assert_eq!(r["deleted_orphan_runners"], 1, "{r}");
        assert_eq!(s.runners[0].vm_id, "vm-a");
        assert_eq!(s.runners[0].pool, "linux");
        let f = fake.lock().unwrap();
        assert!(f.vms.contains_key("vm-c"));
        assert_eq!(f.deleted_runners, vec![2]);
    }
}
