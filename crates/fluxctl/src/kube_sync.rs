//! `fluxctl kube sync`: copy a directory into a running pod every time a file changes, so an interpreted app (Node, Python,
//! a static site) reloads in about a second with no image build. It is what a bind mount gives a Linux developer; the
//! files travel as a tar stream through the VM into `kubectl exec`.

use crate::{
    kube_dev::{POLL, SKIP, fingerprint, shell_quote},
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::{Context, Result, bail};
use std::{path::Path, process::Command, time::Instant};

/// Largest tree sent at once; a bigger one is almost certainly the wrong directory.
pub(crate) const MAX_BYTES: usize = 64 << 20;

pub struct Sync<'a> {
    pub target: &'a str,
    pub namespace: &'a str,
    pub container: Option<&'a str>,
    pub dir: &'a Path,
    pub to: &'a str,
    pub then: Option<&'a str>,
    pub once: bool,
}

/// `web` -> `deploy/web`; `pod/x`, `deploy/x`, `statefulset/x` and `daemonset/x` are kept.
pub fn resolve_target(t: &str) -> Result<String> {
    let (kind, name) = match t.split_once('/') {
        Some((k, n)) => (k, n),
        None => ("deploy", t),
    };
    let kind = match kind {
        "deploy" | "deployment" | "deployments" => "deploy",
        "pod" | "pods" | "po" => "pod",
        "statefulset" | "sts" => "statefulset",
        "daemonset" | "ds" => "daemonset",
        other => bail!(
            "cannot sync into a {other:?}: use deploy/NAME, pod/NAME, statefulset/NAME or daemonset/NAME"
        ),
    };
    if !kube_host::resource_name_ok(name) {
        bail!("{name:?} is not a Kubernetes name");
    }
    Ok(format!("{kind}/{name}"))
}

/// Paths and names that go inside a remote command are limited to plain characters, so nothing needs escaping.
pub(crate) fn path_ok(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 200
        && !p.contains("..")
        && p.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'_' | b'.' | b'-'))
}

pub fn validate(s: &Sync<'_>) -> Result<String> {
    let target = resolve_target(s.target)?;
    if !kube_host::resource_name_ok(s.namespace) {
        bail!("namespace {:?} is not a Kubernetes name", s.namespace);
    }
    if let Some(c) = s.container
        && !kube_host::resource_name_ok(c)
    {
        bail!("container {c:?} is not a Kubernetes name");
    }
    if !path_ok(s.to) || s.to == "/" {
        bail!(
            "--to {:?} must be an absolute path of plain characters, not /",
            s.to
        );
    }
    Ok(target)
}

/// The command run in the VM: unpack the tar stream on stdin into the pod.
pub fn extract_script(
    target: &str,
    namespace: &str,
    container: Option<&str>,
    to: &str,
    then: Option<&str>,
) -> String {
    let c = container.map(|c| format!(" -c {c}")).unwrap_or_default();
    let after = then.map(|t| format!(" && {t}")).unwrap_or_default();
    let inner = format!("mkdir -p {to} && tar xf - -C {to}{after}");
    format!(
        "sudo k3s kubectl exec -i -n {namespace} {target}{c} -- sh -c {}",
        shell_quote(&inner)
    )
}

/// The Mac's `tar` arguments that pack `dir`, leaving out the directories the watcher ignores.
pub fn tar_args(dir: &Path) -> Vec<String> {
    let mut a = vec![
        "-cf".into(),
        "-".into(),
        "-C".into(),
        dir.display().to_string(),
    ];
    for s in SKIP {
        a.extend(["--exclude".into(), s.into()]);
    }
    a.extend(["--exclude".into(), ".DS_Store".into(), ".".into()]);
    a
}

fn pack(dir: &Path) -> Result<Vec<u8>> {
    let out = Command::new("tar")
        .args(tar_args(dir))
        .env("COPYFILE_DISABLE", "1") // no ._ files next to every file
        .output()
        .context("running tar")?;
    if !out.status.success() {
        bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.len() > MAX_BYTES {
        bail!(
            "{} holds more than {} MiB; sync a smaller directory",
            dir.display(),
            MAX_BYTES >> 20
        );
    }
    Ok(out.stdout)
}

async fn once<A: VmApi>(api: &A, o: &KubeOptions, s: &Sync<'_>, target: &str) -> Result<String> {
    let started = Instant::now();
    let tar = pack(s.dir)?;
    let script = extract_script(target, s.namespace, s.container, s.to, s.then);
    kube_host::guest_run_bytes(api, o, &script, Some(&tar)).await?;
    Ok(format!(
        "synced {} KiB into {target}:{} ({:.1}s)",
        tar.len().div_ceil(1024),
        s.to,
        started.elapsed().as_secs_f32()
    ))
}

pub async fn run<A: VmApi>(api: &A, o: &KubeOptions, s: &Sync<'_>) -> Result<()> {
    let target = validate(s)?;
    if !s.dir.is_dir() {
        bail!("--dir {}: not a directory", s.dir.display());
    }
    eprintln!("{}", once(api, o, s, &target).await?);
    if s.once {
        return Ok(());
    }
    eprintln!("watching {} (Ctrl-C to stop)", s.dir.display());
    let mut seen = fingerprint(s.dir);
    loop {
        tokio::time::sleep(POLL).await;
        let now = fingerprint(s.dir);
        if now == seen {
            continue;
        }
        // Let a burst of saves settle first.
        let mut settled = now;
        loop {
            tokio::time::sleep(POLL / 2).await;
            let again = fingerprint(s.dir);
            if again == settled {
                break;
            }
            settled = again;
        }
        match once(api, o, s, &target).await {
            Ok(m) => eprintln!("{m}"),
            Err(e) => eprintln!("{e:#}"),
        }
        seen = fingerprint(s.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_resolved_and_checked() {
        assert_eq!(resolve_target("web").unwrap(), "deploy/web");
        assert_eq!(resolve_target("deployment/web").unwrap(), "deploy/web");
        assert_eq!(resolve_target("pod/web-1").unwrap(), "pod/web-1");
        assert_eq!(resolve_target("sts/db").unwrap(), "statefulset/db");
        assert!(resolve_target("job/x").is_err());
        assert!(resolve_target("deploy/Web;id").is_err());
        assert!(resolve_target("").is_err());
    }

    #[test]
    fn destinations_are_plain_absolute_paths() {
        for ok in ["/app", "/usr/share/nginx/html", "/srv/my-app_1.2"] {
            assert!(path_ok(ok), "{ok}");
        }
        for bad in ["app", "/", "/a b", "/a;rm", "/a/../b", "/a'b", "/a$b", ""] {
            let s = Sync {
                target: "web",
                namespace: "default",
                container: None,
                dir: Path::new("."),
                to: bad,
                then: None,
                once: true,
            };
            assert!(bad == "/app" || validate(&s).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_script_unpacks_into_the_pod_and_can_run_a_command_after() {
        let s = extract_script(
            "deploy/web",
            "shop",
            Some("app"),
            "/app",
            Some("kill -HUP 1"),
        );
        assert_eq!(
            s,
            "sudo k3s kubectl exec -i -n shop deploy/web -c app -- sh -c 'mkdir -p /app && tar xf - -C /app && kill -HUP 1'"
        );
        let plain = extract_script("pod/x", "default", None, "/www", None);
        assert!(plain.ends_with("sh -c 'mkdir -p /www && tar xf - -C /www'"));
    }

    #[test]
    fn tar_leaves_out_what_the_watcher_ignores() {
        let a = tar_args(Path::new("/src"));
        assert_eq!(&a[..4], ["-cf", "-", "-C", "/src"]);
        assert!(a.windows(2).any(|w| w == ["--exclude", ".git"]));
        assert!(a.windows(2).any(|w| w == ["--exclude", "node_modules"]));
        assert_eq!(a.last().unwrap(), ".");
    }

    #[test]
    fn a_directory_packs_into_a_tar_without_mac_metadata() {
        let dir = std::env::temp_dir().join(format!("fluxctl-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("index.html"), "hi").unwrap();
        std::fs::write(dir.join("node_modules/x.js"), "x").unwrap();
        let tar = pack(&dir).unwrap();
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("index.html"));
        assert!(!text.contains("x.js"), "ignored directories are not sent");
        assert!(!text.contains("._index.html"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
