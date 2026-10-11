// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `apple.extra_disks` of kind `overlay`: a copy-on-write overlay on a base image, served over NBD by a supervised
//! `fluxvm-blockd serve` on 127.0.0.1 (ephemeral port) and attached to the guest as an ordinary `nbd` disk.
//!
//! One server per VM carries every overlay disk as a separate export. It is started before the runner, the runner
//! config gets the disks rewritten to `nbd://127.0.0.1:<port>/<disk name>`, and it is stopped when the runner exits
//! (also by itself via `--watch-pid`, should the daemon be gone). A server that dies while the runner lives is restarted
//! on the same port.
//!
//! UNVERIFIED: whether Virtualization.framework's NBD client works against `fluxvm-blockd` has not been run on hardware.

use anyhow::{Context, Result, bail};
use fluxvm_core::model::{AppleDisk, AppleDiskKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `--export` argument for overlay disk `name`.
pub fn export_spec(name: &str, d: &AppleDisk) -> Option<String> {
    let overlay = d.overlay.as_ref()?;
    let mut s = format!("{name}={},overlay={}", d.path.display(), overlay.display());
    if d.read_only {
        s.push_str(",ro");
    }
    if let Some(i) = d.iops {
        s.push_str(&format!(",iops={i}"));
    }
    if let Some(b) = d.bps {
        s.push_str(&format!(",bps={b}"));
    }
    Some(s)
}

/// The overlay disks of a VM as `(export name, --export spec)`, in disk order.
pub fn exports(disks: &[AppleDisk]) -> Vec<(String, String)> {
    disks
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == AppleDiskKind::Overlay)
        .filter_map(|(i, d)| {
            let name = d.name_at(i);
            export_spec(&name, d).map(|spec| (name, spec))
        })
        .collect()
}

/// The disk list the runner gets: overlay disks become `nbd` disks on `port`; everything else is unchanged.
pub fn runner_disks(disks: &[AppleDisk], port: u16) -> Vec<AppleDisk> {
    disks
        .iter()
        .enumerate()
        .map(|(i, d)| {
            if d.kind != AppleDiskKind::Overlay {
                return d.clone();
            }
            AppleDisk {
                name: Some(d.name_at(i)),
                path: PathBuf::new(),
                kind: AppleDiskKind::Nbd,
                url: Some(format!("nbd://127.0.0.1:{port}/{}", d.name_at(i))),
                overlay: None,
                iops: None,
                bps: None,
                ..d.clone()
            }
        })
        .collect()
}

pub fn find_blockd() -> Result<PathBuf> {
    let mut tried = Vec::new();
    if let Some(p) = std::env::var_os("FLUXVM_BLOCKD") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for d in [Some(dir), dir.parent()].into_iter().flatten() {
            let p = d.join("fluxvm-blockd");
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p);
        }
    }
    bail!(
        "fluxvm-blockd not found (tried {}); install it next to fluxctl or set FLUXVM_BLOCKD",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `fluxvm-blockd: listening on 127.0.0.1:34567` -> 34567.
pub fn parse_listening(line: &str) -> Option<u16> {
    let addr = line.split("listening on ").nth(1)?.trim();
    addr.rsplit(':').next()?.parse().ok()
}

fn args_for(specs: &[(String, String)], listen: &str, pid_file: &Path) -> Vec<String> {
    let mut a = vec!["serve".to_string()];
    for (_, spec) in specs {
        a.push("--export".into());
        a.push(spec.clone());
    }
    a.push("--listen".into());
    a.push(listen.into());
    a.push("--watch-pid-file".into());
    a.push(pid_file.display().to_string());
    a
}

/// A running overlay server for one VM.
pub struct Blockd {
    child: tokio::process::Child,
    bin: PathBuf,
    specs: Vec<(String, String)>,
    pub port: u16,
    log: PathBuf,
    pid_file: PathBuf,
}

impl Blockd {
    /// Starts the server and waits for its listening address. The runner's pid is not known yet, so the server watches
    /// `<workspace>/runner.pid` (written by [`Blockd::supervise`]) and exits by itself once that process is gone.
    pub async fn start(specs: Vec<(String, String)>, workspace: &Path) -> Result<Self> {
        if specs.is_empty() {
            bail!("no overlay disks to serve");
        }
        let bin = find_blockd()?;
        let log = workspace.join("blockd.log");
        let pid_file = workspace.join("runner.pid");
        let _ = std::fs::remove_file(&pid_file);
        let (child, port) = Self::spawn(&bin, &specs, "127.0.0.1:0", &pid_file, &log).await?;
        Ok(Self {
            child,
            bin,
            specs,
            port,
            log,
            pid_file,
        })
    }

    async fn spawn(
        bin: &Path,
        specs: &[(String, String)],
        listen: &str,
        pid_file: &Path,
        log: &Path,
    ) -> Result<(tokio::process::Child, u16)> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut child = tokio::process::Command::new(bin)
            .args(args_for(specs, listen, pid_file))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let stderr = child.stderr.take().context("blockd stderr")?;
        let mut lines = BufReader::new(stderr).lines();
        let mut logf = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .await
            .context("opening blockd.log")?;
        let port = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                let _ = logf.write_all(format!("{line}\n").as_bytes()).await;
                if let Some(p) = parse_listening(&line) {
                    return Ok::<_, std::io::Error>(Some(p));
                }
            }
            Ok(None)
        })
        .await;
        let port = match port {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) | Ok(Err(_)) => {
                let status = child.wait().await.ok();
                bail!(
                    "fluxvm-blockd exited before listening ({status:?}): see {}",
                    log.display()
                );
            }
            Err(_) => {
                let _ = child.start_kill();
                bail!("fluxvm-blockd did not start listening within 10s");
            }
        };
        // Keep draining its stderr into the log so it never blocks on a full pipe.
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = logf.write_all(format!("{line}\n").as_bytes()).await;
            }
        });
        Ok((child, port))
    }

    /// Stops the server when `stop` changes or its sender is dropped (the VM's runner exited, or launch failed), and
    /// restarts it on the same port if it dies first.
    pub fn supervise(mut self, runner_pid: u32, mut stop: tokio::sync::watch::Receiver<bool>) {
        let _ = std::fs::write(&self.pid_file, runner_pid.to_string());
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let started = tokio::time::Instant::now();
                let exited = tokio::select! {
                    r = self.child.wait() => Some(r),
                    _ = stop.changed() => None,
                };
                let Some(r) = exited else {
                    graceful_stop(&mut self.child).await;
                    return;
                };
                if *stop.borrow() || !crate::usernet::pid_alive(runner_pid) {
                    return;
                }
                tracing::warn!(status = ?r.ok(), "fluxvm-blockd exited; restarting on port {}", self.port);
                if started.elapsed() < Duration::from_secs(5) {
                    failures += 1;
                    if failures >= 3 {
                        tracing::error!(
                            "fluxvm-blockd keeps failing (see {}); the overlay disks are unavailable",
                            self.log.display()
                        );
                        return;
                    }
                } else {
                    failures = 0;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                let listen = format!("127.0.0.1:{}", self.port);
                match Self::spawn(&self.bin, &self.specs, &listen, &self.pid_file, &self.log).await
                {
                    Ok((c, _)) => self.child = c,
                    Err(e) => {
                        tracing::error!(error = %e, "restarting fluxvm-blockd failed");
                        return;
                    }
                }
            }
        });
    }
}

/// SIGINT (blockd's clean shutdown), then SIGKILL after 3 s.
async fn graceful_stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain signal delivery to our own child.
        unsafe {
            libc::kill(pid as i32, libc::SIGINT);
        }
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(name: &str) -> AppleDisk {
        AppleDisk {
            name: Some(name.into()),
            path: "/img/base.raw".into(),
            overlay: Some("/img/vm.ovl".into()),
            kind: AppleDiskKind::Overlay,
            ..Default::default()
        }
    }

    #[test]
    fn export_spec_carries_limits_and_ro() {
        let mut d = overlay("scratch");
        d.read_only = true;
        d.iops = Some(200);
        d.bps = Some(1_000_000);
        assert_eq!(
            export_spec("scratch", &d).unwrap(),
            "scratch=/img/base.raw,overlay=/img/vm.ovl,ro,iops=200,bps=1000000"
        );
        assert_eq!(
            export_spec("x", &overlay("x")).unwrap(),
            "x=/img/base.raw,overlay=/img/vm.ovl"
        );
    }

    #[test]
    fn overlay_disks_become_nbd_disks_on_the_served_port() {
        let img = AppleDisk {
            path: "/img/plain.raw".into(),
            ..Default::default()
        };
        let disks = vec![img.clone(), overlay("data")];
        let out = runner_disks(&disks, 4242);
        assert_eq!(out[0], img);
        assert_eq!(out[1].kind, AppleDiskKind::Nbd);
        assert_eq!(out[1].url.as_deref(), Some("nbd://127.0.0.1:4242/data"));
        assert!(out[1].path.as_os_str().is_empty() && out[1].overlay.is_none());
        let ex = exports(&disks);
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].0, "data");
        // An unnamed overlay disk is exported under its positional name, matching the NBD url.
        let unnamed = AppleDisk {
            name: None,
            ..overlay("")
        };
        let both = vec![img, unnamed];
        assert_eq!(exports(&both)[0].0, "disk1");
        assert_eq!(
            runner_disks(&both, 1)[1].url.as_deref(),
            Some("nbd://127.0.0.1:1/disk1")
        );
    }

    #[test]
    fn listening_line_is_parsed() {
        assert_eq!(
            parse_listening("fluxvm-blockd: listening on 127.0.0.1:34567"),
            Some(34567)
        );
        assert_eq!(parse_listening("something else"), None);
    }

    #[test]
    fn serve_args_use_an_ephemeral_loopback_port() {
        let a = args_for(
            &[("d".into(), "d=/b,overlay=/o".into())],
            "127.0.0.1:0",
            Path::new("/ws/runner.pid"),
        );
        assert_eq!(
            a,
            [
                "serve",
                "--export",
                "d=/b,overlay=/o",
                "--listen",
                "127.0.0.1:0",
                "--watch-pid-file",
                "/ws/runner.pid"
            ]
        );
    }
}
