//! Incident bundles: one job collects a VM's runtime state (QMP, cgroup and
//! PSI, console log, network and eBPF state, an optional packet capture) into
//! `<diagnostics_dir>/<id>/<token>/bundle.tar.gz`, and optionally dumps guest
//! memory next to it. See docs/diagnostics-bundle.md.

use crate::{VmManager, audit_event};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fluxvm_core::config::Config;
use fluxvm_core::model::{
    BackendKind, DiagnosticsBundle, DiagnosticsRequest, DiagnosticsSection, MemoryDumpInfo,
    VmRecord, VmStatus,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const BUNDLE_FILE: &str = "bundle.tar.gz";
pub const DUMP_FILE: &str = "memory.elf";
const JOB_FILE: &str = "job.json";
pub const STATE_RUNNING: &str = "running";
pub const STATE_DONE: &str = "done";
pub const STATE_FAILED: &str = "failed";

/// Bundles kept per VM; starting another removes the oldest finished one.
const KEEP_PER_VM: usize = 5;
const MAX_PCAP_SECONDS: u32 = 30;
const DEFAULT_LOG_LINES: usize = 2000;
const MAX_LOG_LINES: usize = 20_000;
/// The console log is read from its last this-many bytes.
const LOG_TAIL_BYTES: u64 = 8 << 20;
const FLOW_LIMIT: usize = 1000;
const DUMP_DEADLINE: Duration = Duration::from_secs(30 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Free space a memory dump must leave in the diagnostics directory, on top
/// of the guest's memory size.
const DUMP_HEADROOM: u64 = 1 << 30;

/// cgroup v2 files copied into `vm/cgroup/`.
const CGROUP_FILES: &[&str] = &[
    "cgroup.events",
    "cgroup.procs",
    "cpu.max",
    "cpu.stat",
    "cpu.pressure",
    "cpuset.cpus.effective",
    "cpuset.mems.effective",
    "io.stat",
    "io.pressure",
    "memory.current",
    "memory.max",
    "memory.events",
    "memory.stat",
    "memory.pressure",
    "pids.current",
];

/// Keys whose values a redacted bundle replaces with a fingerprint. Matched
/// as substrings of the lower-cased key.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "private_key",
    "api_key",
    "apikey",
    "credential",
    "user_data",
    "userdata",
    "content",
    "psk",
];

/// Words after which a redacted bundle hides the value in text files.
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "private_key",
    "bearer",
];

/// A diagnostics job is still running for the VM: a state conflict.
#[derive(Debug)]
pub struct DiagnosticsBusy(pub String);

impl std::fmt::Display for DiagnosticsBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DiagnosticsBusy {}

pub(crate) fn vm_dir(cfg: &Config, id: Uuid) -> PathBuf {
    cfg.diagnostics_dir().join(id.to_string())
}

/// Removes every bundle and memory dump of a deleted VM.
pub(crate) fn remove_vm_dir(cfg: &Config, id: Uuid) {
    let dir = vm_dir(cfg, id);
    if let Err(e) = fs::remove_dir_all(&dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "removing diagnostics");
    }
}

fn valid_token(token: &str) -> Result<()> {
    if token.is_empty()
        || token.len() > 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("invalid diagnostics token");
    }
    Ok(())
}

/// The job record on disk, with the FluxVM process that ran it so a job left
/// `running` by a restart reads as failed.
#[derive(serde::Deserialize, Serialize)]
struct JobFile {
    #[serde(flatten)]
    bundle: DiagnosticsBundle,
    #[serde(default)]
    runner_pid: u32,
}

fn read_job(dir: &Path) -> Result<DiagnosticsBundle> {
    let raw = fs::read(dir.join(JOB_FILE)).with_context(|| format!("reading {}", dir.display()))?;
    let job: JobFile = serde_json::from_slice(&raw).context("parsing diagnostics job")?;
    let mut b = job.bundle;
    if b.state == STATE_RUNNING && job.runner_pid != std::process::id() {
        b.state = STATE_FAILED.into();
        b.error = Some("interrupted: FluxVM restarted while the bundle was collected".into());
    }
    Ok(b)
}

fn write_job(dir: &Path, bundle: &DiagnosticsBundle) -> Result<()> {
    let job = JobFile {
        bundle: bundle.clone(),
        runner_pid: std::process::id(),
    };
    let tmp = dir.join(format!("{JOB_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(&job)?)?;
    fs::rename(&tmp, dir.join(JOB_FILE))?;
    Ok(())
}

fn list_jobs(cfg: &Config, id: Uuid) -> Vec<DiagnosticsBundle> {
    let mut out: Vec<DiagnosticsBundle> = fs::read_dir(vm_dir(cfg, id))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_job(&e.path()).ok())
        .collect();
    out.sort_by_key(|b| b.created_at);
    out
}

/// Why `vm`'s memory cannot be dumped; `None` when it can.
pub fn memory_dump_unsupported(cfg: &Config, vm: &VmRecord) -> Option<String> {
    if vm.backend != BackendKind::Qemu {
        return Some(format!(
            "memory dumps need the qemu backend (backend={:?})",
            vm.backend
        ));
    }
    if !matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
        return Some(format!(
            "VM is {:?}; memory dumps need a live QEMU",
            vm.status
        ));
    }
    if vm.request.security_profile.is_confidential() {
        return Some("a confidential VM's memory cannot be dumped".into());
    }
    let cap = cfg.diagnostics_max_memory_dump_mib();
    if cap == 0 {
        return Some("memory dumps are disabled (diagnostics_max_memory_dump_mib = 0)".into());
    }
    if vm.request.memory_mib > cap {
        return Some(format!(
            "guest has {} MiB of memory, above diagnostics_max_memory_dump_mib ({cap} MiB)",
            vm.request.memory_mib
        ));
    }
    None
}

fn free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a zeroed statvfs is a valid out parameter and `c` outlives the call.
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    Some((vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64))
}

/// Creates `dir` readable by FluxVM only (bundles and memory dumps hold guest
/// secrets) and tightens it if it already exists.
fn private_dir(dir: &Path, recursive: bool) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Refuses a second running job for the VM, prunes old bundles and records
/// `bundle` as running in `dir`.
fn start_job(
    cfg: &Config,
    id: Uuid,
    base: &Path,
    dir: &Path,
    bundle: &DiagnosticsBundle,
) -> Result<()> {
    let jobs = list_jobs(cfg, id);
    if jobs.iter().any(|j| j.state == STATE_RUNNING) {
        return Err(DiagnosticsBusy(
            "a diagnostics bundle is already being collected for this VM".into(),
        )
        .into());
    }
    for old in jobs
        .iter()
        .take((jobs.len() + 1).saturating_sub(KEEP_PER_VM))
    {
        let _ = fs::remove_dir_all(base.join(&old.token));
    }
    private_dir(dir, false)?;
    write_job(dir, bundle)
}

impl VmManager {
    /// Starts a diagnostics bundle job for `id` and returns it `running`.
    pub async fn start_diagnostics(
        self: &Arc<Self>,
        id: Uuid,
        req: DiagnosticsRequest,
    ) -> Result<DiagnosticsBundle> {
        let vm = self.get(id).await?;
        if req.pcap_seconds > MAX_PCAP_SECONDS {
            bail!("pcap_seconds must be 0-{MAX_PCAP_SECONDS}");
        }
        if req.log_lines.is_some_and(|n| n > MAX_LOG_LINES) {
            bail!("log_lines must be at most {MAX_LOG_LINES}");
        }
        fluxvm_network::edge_capture::validate("diag", &req.pcap_filter)?;
        let base = vm_dir(&self.cfg, id);
        private_dir(&self.cfg.diagnostics_dir(), true)?;
        private_dir(&base, true)?;
        if req.memory_dump {
            if let Some(why) = memory_dump_unsupported(&self.cfg, &vm) {
                bail!("{why}");
            }
            let need = (vm.request.memory_mib << 20) + DUMP_HEADROOM;
            if let Some(free) = free_bytes(&base)
                && free < need
            {
                bail!(
                    "{} has {} MiB free; a memory dump of this guest needs {} MiB",
                    base.display(),
                    free >> 20,
                    need >> 20
                );
            }
        }
        let token = Uuid::new_v4().simple().to_string()[..16].to_string();
        let dir = base.join(&token);
        let bundle = DiagnosticsBundle {
            token: token.clone(),
            vm_id: id,
            state: STATE_RUNNING.into(),
            created_at: Utc::now(),
            finished_at: None,
            request: req,
            error: None,
            sections: Vec::new(),
            size_bytes: 0,
            sha256: String::new(),
            memory_dump: None,
        };
        {
            // Checking for a running job and recording the new one must not
            // interleave with another start.
            let _start = STARTING.lock().unwrap_or_else(|e| e.into_inner());
            start_job(&self.cfg, id, &base, &dir, &bundle)?;
        }
        audit_event(
            "vm.diagnostics.start",
            &[
                ("vm_id", &id.to_string()),
                ("token", &token),
                ("memory_dump", &bundle.request.memory_dump.to_string()),
                ("redact", &bundle.request.redact.to_string()),
            ],
        );
        let mgr = Arc::clone(self);
        let job = bundle.clone();
        tokio::spawn(async move {
            let mut job = job;
            let outcome = mgr.collect(&vm, &mut job, &dir).await;
            job.finished_at = Some(Utc::now());
            match outcome {
                Ok(()) => job.state = STATE_DONE.into(),
                Err(e) => {
                    job.state = STATE_FAILED.into();
                    job.error = Some(format!("{e:#}"));
                }
            }
            if let Err(e) = write_job(&dir, &job) {
                tracing::warn!(vm = %job.vm_id, token = %job.token, error = %e, "recording diagnostics outcome");
            }
            audit_event(
                "vm.diagnostics.finish",
                &[
                    ("vm_id", &job.vm_id.to_string()),
                    ("token", &job.token),
                    ("state", &job.state),
                ],
            );
        });
        Ok(bundle)
    }

    pub async fn list_diagnostics(&self, id: Uuid) -> Result<Vec<DiagnosticsBundle>> {
        self.get(id).await?;
        Ok(list_jobs(&self.cfg, id))
    }

    pub async fn get_diagnostics(&self, id: Uuid, token: &str) -> Result<DiagnosticsBundle> {
        self.get(id).await?;
        valid_token(token)?;
        read_job(&vm_dir(&self.cfg, id).join(token))
    }

    /// The finished bundle's path, or an error naming its state.
    pub async fn diagnostics_file(&self, id: Uuid, token: &str, file: &str) -> Result<PathBuf> {
        let job = self.get_diagnostics(id, token).await?;
        if job.state != STATE_DONE {
            bail!("diagnostics bundle {token} is {}", job.state);
        }
        let path = vm_dir(&self.cfg, id).join(token).join(file);
        if !path.is_file() {
            bail!("diagnostics bundle {token} has no {file}");
        }
        Ok(path)
    }

    pub async fn delete_diagnostics(&self, id: Uuid, token: &str) -> Result<()> {
        let job = self.get_diagnostics(id, token).await?;
        if job.state == STATE_RUNNING {
            return Err(DiagnosticsBusy(format!(
                "diagnostics bundle {token} is still being collected"
            ))
            .into());
        }
        fs::remove_dir_all(vm_dir(&self.cfg, id).join(token))?;
        audit_event(
            "vm.diagnostics.delete",
            &[("vm_id", &id.to_string()), ("token", token)],
        );
        Ok(())
    }

    async fn collect(&self, vm: &VmRecord, job: &mut DiagnosticsBundle, dir: &Path) -> Result<()> {
        let id = vm.id;
        let req = job.request.clone();
        let mut b = Bundle::new(req.redact);

        // Started first so it runs while everything else is read.
        let capture = if req.pcap_seconds > 0 {
            Some(self.start_bundle_capture(vm, &job.token, &req))
        } else {
            None
        };

        let mut record = serde_json::to_value(vm)?;
        if req.redact {
            redact_json(&mut record);
        }
        b.json_value("vm/record.json", Ok(record));
        b.json("vm/metrics.json", self.metrics(id).await);
        b.json("vm/pressure.json", self.pressure(id).await);
        b.json("vm/cpuset.json", self.get_cpuset(id).await);
        b.json("vm/frozen.json", self.is_frozen(id).await);
        b.json("vm/numa.json", self.numa_status(id).await);
        self.add_cgroup(vm, &mut b);
        b.text(
            "logs/console.log",
            tail_lines(&vm.log_path, req.log_lines.unwrap_or(DEFAULT_LOG_LINES)),
        );

        let qmp = vm.workspace.join("qmp.sock");
        let live_qemu = vm.backend == BackendKind::Qemu
            && matches!(vm.status, VmStatus::Running | VmStatus::Paused)
            && qmp.exists();
        if live_qemu {
            for (name, v) in fluxvm_qemu::diagnostics::query_state(&qmp).await {
                b.json_value(&format!("qmp/{name}.json"), v);
            }
        } else {
            b.skip("qmp", "no live QEMU monitor for this VM");
        }

        let status = self.network_status(id).await;
        let iface = status
            .as_ref()
            .ok()
            .and_then(|s| s.interface.clone())
            .or_else(|| vm.tap_name.clone());
        b.json("network/status.json", status);
        b.json("network/stats.json", self.network_stats(id).await);
        b.json(
            "network/flows.json",
            self.network_flows(id, FLOW_LIMIT).await,
        );
        let cfg = self.cfg.clone();
        b.json(
            "network/drops.json",
            tokio::task::spawn_blocking(move || {
                fluxvm_network::edge_contract::attributed_drops(&cfg, id, FLOW_LIMIT)
            })
            .await
            .context("drop reader panicked")
            .and_then(|r| r),
        );
        b.text(
            "network/datapath.txt",
            Ok(datapath_commands(iface.as_deref(), vm.netns.as_deref()).await),
        );

        b.json(
            "host/memory.json",
            Ok(
                tokio::task::spawn_blocking(fluxvm_core::pressure_admission::sample_host)
                    .await
                    .unwrap_or_default(),
            ),
        );
        b.json("host/info.json", Ok(host_info()));

        if let Some(capture) = capture {
            match capture {
                Ok(token) => b.bytes(
                    "network/capture.pcap",
                    self.finish_bundle_capture(id, &token, req.pcap_seconds)
                        .await,
                ),
                Err(e) => b.bytes("network/capture.pcap", Err(e)),
            }
        }

        if req.memory_dump {
            let path = dir.join(DUMP_FILE);
            match self.dump_memory(vm, &path).await {
                Ok(info) => {
                    b.note("memory-dump", Ok(0));
                    job.memory_dump = Some(info);
                }
                Err(e) => b.note("memory-dump", Err(e)),
            }
        }

        job.sections = b.sections.clone();
        let mut manifest = job.clone();
        manifest.state = STATE_DONE.into();
        manifest.finished_at = Some(Utc::now());
        b.files.push((
            "manifest.json".into(),
            serde_json::to_vec_pretty(&manifest)?,
        ));

        let path = dir.join(BUNDLE_FILE);
        let files = std::mem::take(&mut b.files);
        let written = path.clone();
        tokio::task::spawn_blocking(move || write_tar_gz(&written, &files))
            .await
            .context("bundle writer panicked")??;
        job.size_bytes = fs::metadata(&path)?.len();
        job.sha256 = sha256_file(&path)?;
        Ok(())
    }

    fn start_bundle_capture(
        &self,
        vm: &VmRecord,
        token: &str,
        req: &DiagnosticsRequest,
    ) -> Result<String> {
        let now = Utc::now().timestamp().max(0) as u64;
        let session = fluxvm_network::edge_contract::CaptureSession {
            token: format!("diag-{token}"),
            namespace: "fluxvm-diagnostics".into(),
            machine: vm.name.clone(),
            seconds: req.pcap_seconds,
            filter: req.pcap_filter.clone(),
            expires_at: fluxvm_network::edge_contract::rfc3339(
                now + u64::from(req.pcap_seconds) + 120,
            ),
            state: String::new(),
            started_unix: 0,
            packets: 0,
            error: String::new(),
        };
        let started = fluxvm_network::edge_contract::start_capture(&self.cfg, vm.id, session)?;
        Ok(started.token)
    }

    async fn finish_bundle_capture(&self, id: Uuid, token: &str, seconds: u32) -> Result<Vec<u8>> {
        use fluxvm_network::edge_contract::CaptureFile;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(seconds) + 30);
        loop {
            match fluxvm_network::edge_contract::capture_file(&self.cfg, id, token)? {
                CaptureFile::Ready(path) => {
                    return fs::read(&path).with_context(|| format!("reading {}", path.display()));
                }
                CaptureFile::Missing => bail!("capture {token} produced no pcap"),
                CaptureFile::Running if tokio::time::Instant::now() > deadline => {
                    bail!("capture {token} still running after {}s", seconds + 30)
                }
                CaptureFile::Running => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    async fn dump_memory(&self, vm: &VmRecord, path: &Path) -> Result<MemoryDumpInfo> {
        let current = self.get(vm.id).await?;
        if let Some(why) = memory_dump_unsupported(&self.cfg, &current) {
            bail!("{why}");
        }
        let took = fluxvm_qemu::diagnostics::dump_memory(
            &current.workspace.join("qmp.sock"),
            path,
            DUMP_DEADLINE,
        )
        .await?;
        let p = path.to_path_buf();
        let (size_bytes, sha256) = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
            Ok((fs::metadata(&p)?.len(), sha256_file(&p)?))
        })
        .await
        .context("hashing memory dump panicked")??;
        audit_event(
            "vm.diagnostics.memory_dump",
            &[
                ("vm_id", &vm.id.to_string()),
                ("bytes", &size_bytes.to_string()),
            ],
        );
        Ok(MemoryDumpInfo {
            path: path.to_path_buf(),
            format: "elf".into(),
            size_bytes,
            sha256,
            paused_ms: took.as_millis() as u64,
        })
    }

    fn add_cgroup(&self, vm: &VmRecord, b: &mut Bundle) {
        let Some(path) = &vm.cgroup_path else {
            b.skip("vm/cgroup", "VM has no cgroup");
            return;
        };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            Path::new("/sys/fs/cgroup").join(path)
        };
        for file in CGROUP_FILES {
            if let Ok(raw) = fs::read(path.join(file)) {
                b.add(format!("vm/cgroup/{file}"), raw);
            }
        }
    }
}

struct Bundle {
    redact: bool,
    files: Vec<(String, Vec<u8>)>,
    sections: Vec<DiagnosticsSection>,
}

impl Bundle {
    fn new(redact: bool) -> Self {
        Self {
            redact,
            files: Vec::new(),
            sections: Vec::new(),
        }
    }

    fn add(&mut self, name: String, data: Vec<u8>) {
        self.sections.push(DiagnosticsSection {
            name: name.clone(),
            ok: true,
            error: None,
            bytes: data.len() as u64,
        });
        self.files.push((name, data));
    }

    fn fail(&mut self, name: &str, e: anyhow::Error) {
        self.sections.push(DiagnosticsSection {
            name: name.into(),
            ok: false,
            error: Some(format!("{e:#}")),
            bytes: 0,
        });
    }

    fn skip(&mut self, name: &str, why: &str) {
        self.fail(name, anyhow::anyhow!("{why}"));
    }

    fn note(&mut self, name: &str, r: Result<u64>) {
        match r {
            Ok(bytes) => self.sections.push(DiagnosticsSection {
                name: name.into(),
                ok: true,
                error: None,
                bytes,
            }),
            Err(e) => self.fail(name, e),
        }
    }

    fn json<T: Serialize>(&mut self, name: &str, r: Result<T>) {
        self.json_value(name, r.and_then(|v| Ok(serde_json::to_value(v)?)));
    }

    fn json_value(&mut self, name: &str, r: Result<Value>) {
        match r.and_then(|v| Ok(serde_json::to_vec_pretty(&v)?)) {
            Ok(data) => self.add(name.into(), data),
            Err(e) => self.fail(name, e),
        }
    }

    fn text(&mut self, name: &str, r: Result<String>) {
        match r {
            Ok(s) => {
                let s = if self.redact { redact_text(&s) } else { s };
                self.add(name.into(), s.into_bytes());
            }
            Err(e) => self.fail(name, e),
        }
    }

    fn bytes(&mut self, name: &str, r: Result<Vec<u8>>) {
        match r {
            Ok(data) => self.add(name.into(), data),
            Err(e) => self.fail(name, e),
        }
    }
}

fn write_tar_gz(path: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let file = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mtime = Utc::now().timestamp().max(0) as u64;
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        tar.append_data(&mut header, name, data.as_slice())
            .with_context(|| format!("adding {name}"))?;
    }
    let mut gz = tar.into_inner()?;
    gz.flush()?;
    gz.finish()?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
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
    Ok(format!("{:x}", h.finalize()))
}

/// The last `lines` lines of `path`, read from at most its last
/// `LOG_TAIL_BYTES`.
fn tail_lines(path: &Path, lines: usize) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        // The first line was cut by the seek.
        all.remove(0);
    }
    let from = all.len().saturating_sub(lines);
    let mut out = all[from..].join("\n");
    out.push('\n');
    Ok(out)
}

/// Link, tc and BPF state of the VM's dataplane interface, as the host tools
/// print it. A missing tool shows as its error.
async fn datapath_commands(iface: Option<&str>, netns: Option<&str>) -> String {
    let Some(iface) = iface else {
        return "VM has no host network interface\n".into();
    };
    let cmds: [&[&str]; 5] = [
        &["ip", "-d", "-s", "link", "show", "dev", iface],
        &["tc", "-s", "qdisc", "show", "dev", iface],
        &["tc", "-s", "filter", "show", "dev", iface, "ingress"],
        &["tc", "-s", "filter", "show", "dev", iface, "egress"],
        &["bpftool", "net", "show", "dev", iface],
    ];
    let mut out = String::new();
    for argv in cmds {
        let mut full: Vec<&str> = Vec::new();
        if let Some(ns) = netns {
            full.extend(["ip", "netns", "exec", ns]);
        }
        full.extend_from_slice(argv);
        out.push_str(&format!("$ {}\n", full.join(" ")));
        let mut cmd = tokio::process::Command::new(full[0]);
        cmd.args(&full[1..]).kill_on_drop(true);
        match tokio::time::timeout(COMMAND_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => {
                out.push_str(&String::from_utf8_lossy(&o.stdout));
                out.push_str(&String::from_utf8_lossy(&o.stderr));
                if !o.status.success() {
                    out.push_str(&format!("[exit {}]\n", o.status));
                }
            }
            Ok(Err(e)) => out.push_str(&format!("[{e}]\n")),
            Err(_) => out.push_str(&format!("[timed out after {COMMAND_TIMEOUT:?}]\n")),
        }
        out.push('\n');
    }
    out
}

#[derive(Serialize)]
struct HostInfo {
    hostname: String,
    kernel: String,
    fluxvm_version: &'static str,
    uptime_secs: Option<f64>,
    loadavg: String,
    collected_at: chrono::DateTime<Utc>,
}

fn host_info() -> HostInfo {
    let read = |p: &str| {
        fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    HostInfo {
        hostname: read("/proc/sys/kernel/hostname"),
        kernel: read("/proc/sys/kernel/osrelease"),
        fluxvm_version: env!("CARGO_PKG_VERSION"),
        uptime_secs: read("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok()),
        loadavg: read("/proc/loadavg"),
        collected_at: Utc::now(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| k.contains(s))
}

fn fingerprint(v: &Value) -> Value {
    let raw = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
    Value::String(format!("redacted:sha256:{}", &digest[..16]))
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Replaces the value of every secret-looking key (see `SECRET_KEYS`) with
/// a sha256 fingerprint, so two bundles can still be compared, and scrubs
/// other strings like text files (`runcmd: "... password=x"`).
pub fn redact_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = redact_text(s),
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_secret_key(k) && !is_empty_value(val) && !val.is_boolean() {
                    *val = fingerprint(val);
                } else {
                    redact_json(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

/// Hides the value after `password=`, `token: ` or `Bearer ` (see
/// `SECRET_WORDS`) on every line.
pub fn redact_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line));
    }
    out
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let found = SECRET_WORDS
            .iter()
            .filter_map(|w| lower[i..].find(w).map(|p| (i + p, *w)))
            .min_by_key(|(p, _)| *p);
        let Some((pos, word)) = found else { break };
        let mut j = pos + word.len();
        while j < bytes.len()
            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
        {
            j += 1;
        }
        while j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
            j += 1;
        }
        let separated = if word == "bearer" {
            j < bytes.len() && bytes[j] == b' '
        } else {
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            j < bytes.len() && (bytes[j] == b'=' || bytes[j] == b':')
        };
        if separated {
            j += 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'"' || bytes[j] == b'\'') {
                j += 1;
            }
            let start = j;
            while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && !matches!(bytes[j], b'"' | b'\'' | b',' | b';' | b'&')
            {
                j += 1;
            }
            if j > start {
                out.push_str(&line[i..start]);
                out.push_str("[redacted]");
                i = j;
                continue;
            }
        }
        out.push_str(&line[i..pos + word.len()]);
        i = pos + word.len();
    }
    out.push_str(&line[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_json_fingerprints_secret_keys() {
        let mut v = json!({
            "name": "web",
            "request": {
                "cloud_init": {"write_files": [{"path": "/etc/app.conf", "content": "pw=hunter2"}], "user": "ubuntu"},
                "secret_env": {"DB": {"name": "db"}},
                "agent": {"token": "abc", "enabled": true},
                "created_by_token": null,
                "memory_mib": 1024,
                "runcmd": ["echo password=hunter2 > /dev/ttyS0"]
            }
        });
        redact_json(&mut v);
        let req = &v["request"];
        assert_eq!(v["name"], "web");
        assert_eq!(req["cloud_init"]["user"], "ubuntu");
        assert_eq!(req["cloud_init"]["write_files"][0]["path"], "/etc/app.conf");
        let content = req["cloud_init"]["write_files"][0]["content"]
            .as_str()
            .unwrap();
        assert!(content.starts_with("redacted:sha256:"), "{content}");
        assert!(req["secret_env"].as_str().unwrap().starts_with("redacted:"));
        assert!(
            req["agent"]["token"]
                .as_str()
                .unwrap()
                .starts_with("redacted:")
        );
        assert_eq!(req["agent"]["enabled"], true);
        assert!(req["created_by_token"].is_null());
        assert_eq!(req["memory_mib"], 1024);
        assert_eq!(req["runcmd"][0], "echo password=[redacted] > /dev/ttyS0");
    }

    #[test]
    fn redact_json_is_stable() {
        let mut a = json!({"token": "same"});
        let mut b = json!({"token": "same"});
        redact_json(&mut a);
        redact_json(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn redact_text_hides_values() {
        let s = "login ok\npassword=hunter2 user=bob\nAuthorization: Bearer eyJhbGc.x\n\"api_key\": \"k-123\", next\nno secrets here";
        let r = redact_text(s);
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("eyJhbGc"), "{r}");
        assert!(!r.contains("k-123"), "{r}");
        assert!(r.contains("user=bob"), "{r}");
        assert!(r.contains("login ok"));
        assert!(r.contains("no secrets here"));
        assert!(r.contains(", next"), "{r}");
    }

    #[test]
    fn redact_text_keeps_bare_words_and_utf8() {
        let s = "the token expired — retry\nsecret: ü-value";
        let r = redact_text(s);
        assert!(r.starts_with("the token expired — retry"), "{r}");
        assert!(!r.contains("ü-value"), "{r}");
    }

    #[test]
    fn tail_lines_returns_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("console.log");
        fs::write(&p, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_lines(&p, 2).unwrap(), "c\nd\n");
        assert_eq!(tail_lines(&p, 10).unwrap(), "a\nb\nc\nd\n");
    }

    #[test]
    fn tar_gz_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(BUNDLE_FILE);
        write_tar_gz(
            &p,
            &[
                ("manifest.json".into(), b"{}".to_vec()),
                (
                    "qmp/query-status.json".into(),
                    b"{\"status\":\"running\"}".to_vec(),
                ),
            ],
        )
        .unwrap();
        let gz = flate2::read::GzDecoder::new(fs::File::open(&p).unwrap());
        let mut names = Vec::new();
        for e in tar::Archive::new(gz).entries().unwrap() {
            names.push(e.unwrap().path().unwrap().display().to_string());
        }
        assert_eq!(names, ["manifest.json", "qmp/query-status.json"]);
        assert_eq!(sha256_file(&p).unwrap().len(), 64);
    }

    #[test]
    fn job_left_running_by_another_process_reads_failed() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = DiagnosticsBundle {
            token: "abc".into(),
            vm_id: Uuid::nil(),
            state: STATE_RUNNING.into(),
            created_at: Utc::now(),
            finished_at: None,
            request: DiagnosticsRequest::default(),
            error: None,
            sections: Vec::new(),
            size_bytes: 0,
            sha256: String::new(),
            memory_dump: None,
        };
        write_job(dir.path(), &bundle).unwrap();
        assert_eq!(read_job(dir.path()).unwrap().state, STATE_RUNNING);
        let job = JobFile {
            bundle,
            runner_pid: std::process::id() + 1,
        };
        fs::write(dir.path().join(JOB_FILE), serde_json::to_vec(&job).unwrap()).unwrap();
        let b = read_job(dir.path()).unwrap();
        assert_eq!(b.state, STATE_FAILED);
        assert!(b.error.unwrap().contains("restarted"));
    }

    #[test]
    fn second_running_job_is_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            diagnostics_dir: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };
        let id = Uuid::new_v4();
        let base = vm_dir(&cfg, id);
        fs::create_dir_all(&base).unwrap();
        let job = |token: &str| DiagnosticsBundle {
            token: token.into(),
            vm_id: id,
            state: STATE_RUNNING.into(),
            created_at: Utc::now(),
            finished_at: None,
            request: DiagnosticsRequest::default(),
            error: None,
            sections: Vec::new(),
            size_bytes: 0,
            sha256: String::new(),
            memory_dump: None,
        };
        start_job(&cfg, id, &base, &base.join("aaaa"), &job("aaaa")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(base.join("aaaa"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let err = start_job(&cfg, id, &base, &base.join("bbbb"), &job("bbbb")).unwrap_err();
        assert!(err.downcast_ref::<DiagnosticsBusy>().is_some());
        assert!(!base.join("bbbb").exists());
    }

    #[test]
    fn tokens_are_checked() {
        assert!(valid_token("0123abcd-ef").is_ok());
        assert!(valid_token("../x").is_err());
        assert!(valid_token("").is_err());
    }
}
