//! `fluxctl kube dev`: save a file, and the image is rebuilt, copied into the cluster and the deployment restarted. The
//! watcher polls modification times so it needs no extra dependency; the decisions are pure and tested.

use crate::{
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant, SystemTime},
};

const SKIP: [&str; 5] = [".git", "target", "node_modules", ".venv", "__pycache__"];
const POLL: Duration = Duration::from_millis(700);

/// What changes when a source file is added, removed or saved: the file count and the newest modification time.
pub fn fingerprint(dir: &Path) -> (usize, u128) {
    fn walk(dir: &Path, count: &mut usize, newest: &mut u128) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            if SKIP.iter().any(|s| name.to_str() == Some(s)) {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                walk(&e.path(), count, newest);
            } else {
                *count += 1;
                if let Ok(t) = meta.modified().and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH)
                        .map_err(std::io::Error::other)
                }) {
                    *newest = (*newest).max(t.as_nanos());
                }
            }
        }
    }
    let (mut count, mut newest) = (0, 0);
    walk(dir, &mut count, &mut newest);
    (count, newest)
}

/// The build command when none is given.
pub fn default_build(image: &str, dir: &Path) -> String {
    format!(
        "docker build -t {image} {}",
        shell_quote(&dir.display().to_string())
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The in-VM command that restarts the deployment and waits for the new pods.
pub fn restart_script(namespace: &str, deployment: &str) -> String {
    format!(
        "sudo k3s kubectl -n {namespace} rollout restart deploy/{deployment} && sudo k3s kubectl -n {namespace} rollout status deploy/{deployment} --timeout=180s"
    )
}

pub fn validate(image: &str, deployment: &str, namespace: &str) -> Result<()> {
    if !kube_host::image_ok(image) {
        bail!("image {image:?} is not a plain image reference");
    }
    if !kube_host::name_ok(deployment) {
        bail!("deployment {deployment:?} is not a Kubernetes name");
    }
    if !kube_host::name_ok(namespace) {
        bail!("namespace {namespace:?} is not a Kubernetes name");
    }
    Ok(())
}

pub struct Dev<'a> {
    pub image: &'a str,
    pub deployment: &'a str,
    pub namespace: &'a str,
    pub dir: &'a Path,
    pub build: Option<&'a str>,
    pub once: bool,
}

/// Build, load, restart. A failed build is reported and returns `Ok(false)` so the watch keeps going.
async fn cycle<A: VmApi>(api: &A, o: &KubeOptions, d: &Dev<'_>) -> Result<bool> {
    let started = Instant::now();
    let build = d
        .build
        .map(str::to_owned)
        .unwrap_or_else(|| default_build(d.image, d.dir));
    eprintln!("building: {build}");
    let status = Command::new("sh")
        .args(["-c", &build])
        .current_dir(d.dir)
        .status()
        .context("running the build command")?;
    if !status.success() {
        eprintln!("build failed ({status}); fix it and save again");
        return Ok(false);
    }
    for line in kube_host::load(api, o, &[d.image.to_owned()], None).await? {
        eprintln!("{line}");
    }
    let out = kube_host::guest_sh(api, o, &restart_script(d.namespace, d.deployment)).await?;
    eprintln!("{out}");
    eprintln!(
        "{}/{} is running the new image ({:.1}s)",
        d.namespace,
        d.deployment,
        started.elapsed().as_secs_f32()
    );
    Ok(true)
}

pub async fn run<A: VmApi>(api: &A, o: &KubeOptions, d: &Dev<'_>) -> Result<()> {
    validate(d.image, d.deployment, d.namespace)?;
    if !d.dir.is_dir() {
        bail!("--dir {}: not a directory", d.dir.display());
    }
    // Start from one cycle, so the cluster runs what is on disk now.
    let ok = cycle(api, o, d).await?;
    if d.once {
        if ok {
            return Ok(());
        }
        bail!("the build failed");
    }
    eprintln!("watching {} (Ctrl-C to stop)", d.dir.display());
    let mut seen = fingerprint(d.dir);
    loop {
        tokio::time::sleep(POLL).await;
        let now = fingerprint(d.dir);
        if now == seen {
            continue;
        }
        // Let a burst of saves settle before building.
        let mut settled = now;
        loop {
            tokio::time::sleep(POLL / 2).await;
            let again = fingerprint(d.dir);
            if again == settled {
                break;
            }
            settled = again;
        }
        if let Err(e) = cycle(api, o, d).await {
            eprintln!("{e:#}");
        }
        // Files the build itself writes (outside the skipped dirs) must not retrigger it.
        seen = fingerprint(d.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert!(validate("app:dev", "web", "default").is_ok());
        assert!(validate("app dev", "web", "default").is_err());
        assert!(validate("app:dev", "Web;rm", "default").is_err());
        assert!(validate("app:dev", "web", "$x").is_err());
    }

    #[test]
    fn the_default_build_quotes_the_directory() {
        assert_eq!(
            default_build("app:dev", Path::new("my app")),
            "docker build -t app:dev 'my app'"
        );
        assert!(default_build("a:b", Path::new("it's")).contains("'it'\\''s'"));
    }

    #[test]
    fn the_restart_waits_for_the_rollout() {
        let s = restart_script("shop", "web");
        assert!(s.contains("-n shop rollout restart deploy/web"));
        assert!(s.contains("rollout status deploy/web"));
    }

    #[test]
    fn saving_adding_and_removing_files_change_the_fingerprint() {
        let dir = std::env::temp_dir().join(format!("fluxctl-dev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        let first = fingerprint(&dir);
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        assert_eq!(fingerprint(&dir), first, "skipped directories are ignored");
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        let added = fingerprint(&dir);
        assert_ne!(added, first);
        std::fs::remove_file(dir.join("b.txt")).unwrap();
        assert_eq!(fingerprint(&dir).0, first.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
