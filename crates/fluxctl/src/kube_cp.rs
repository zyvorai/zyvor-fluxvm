//! `fluxctl kube cp`: copy a file or directory between this Mac and a pod, like `docker cp`. Both directions are a tar
//! stream through the VM into `kubectl exec`, the same path `kube sync` uses, so it needs no `tar` quirks on either side.

use crate::{
    kube_host::{self, KubeOptions},
    kube_sync::{MAX_BYTES, extract_script, path_ok, resolve_target},
    run::VmApi,
};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// One side of the copy.
#[derive(Debug, PartialEq)]
pub enum Side {
    Local(PathBuf),
    /// `deploy/web`-style target and an absolute path inside its pod.
    Pod { target: String, path: String },
}

/// `web:/app/log`, `pod/web-1:/tmp` are pod paths; anything else, including `./a:b` and `/x:y`, is a Mac path.
pub fn parse_side(s: &str) -> Result<Side> {
    let local = s.starts_with(['/', '.', '~']) || !s.contains(':');
    if local {
        return Ok(Side::Local(PathBuf::from(s)));
    }
    let (t, p) = s.rsplit_once(':').context("no path after the colon")?;
    let target = resolve_target(t)?;
    if !path_ok(p) || p == "/" {
        bail!("{p:?} must be an absolute path of plain characters, not /");
    }
    Ok(Side::Pod {
        target,
        path: p.trim_end_matches('/').to_owned(),
    })
}

pub enum Plan {
    /// Mac file or directory into a pod directory.
    Up { src: PathBuf, target: String, to: String },
    /// Pod file or directory into a Mac directory.
    Down { target: String, from: String, dir: PathBuf },
}

pub fn plan(src: &str, dst: &str) -> Result<Plan> {
    match (parse_side(src)?, parse_side(dst)?) {
        (Side::Local(src), Side::Pod { target, path }) => Ok(Plan::Up { src, target, to: path }),
        (Side::Pod { target, path }, Side::Local(dir)) => Ok(Plan::Down { target, from: path, dir }),
        (Side::Local(_), Side::Local(_)) => bail!("one side must be a pod path like web:/app"),
        (Side::Pod { .. }, Side::Pod { .. }) => bail!("copying between two pods is not supported; copy to the Mac first"),
    }
}

fn check_ns(ns: &str, container: Option<&str>) -> Result<()> {
    if !kube_host::resource_name_ok(ns) {
        bail!("namespace {ns:?} is not a Kubernetes name");
    }
    if let Some(c) = container
        && !kube_host::resource_name_ok(c)
    {
        bail!("container {c:?} is not a Kubernetes name");
    }
    Ok(())
}

fn exec_prefix(target: &str, ns: &str, container: Option<&str>) -> String {
    let c = container.map(|c| format!(" -c {c}")).unwrap_or_default();
    format!("sudo k3s kubectl exec -n {ns} {target}{c}")
}

/// The VM command that exists-checks a pod path, so a typo is a clear error and not an empty archive.
pub fn exists_script(target: &str, ns: &str, container: Option<&str>, path: &str) -> String {
    format!("{} -- test -e {path}", exec_prefix(target, ns, container))
}

/// The VM command that packs `path` (a file or a directory) in the pod, capped one byte past the limit.
pub fn pack_script(target: &str, ns: &str, container: Option<&str>, path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let dir = if dir.is_empty() { "/" } else { dir };
    format!(
        "{} -- tar cf - -C {dir} {name} | head -c {}",
        exec_prefix(target, ns, container),
        MAX_BYTES + 1
    )
}

/// The Mac's `tar` arguments that pack one file or directory by its own name.
pub fn local_pack_args(src: &Path) -> Result<Vec<String>> {
    let name = src
        .file_name()
        .with_context(|| format!("{} has no name to copy", src.display()))?
        .to_string_lossy()
        .into_owned();
    let parent = match src.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.display().to_string(),
        _ => ".".to_owned(),
    };
    Ok(vec!["-cf".into(), "-".into(), "-C".into(), parent, name])
}

fn unpack_local(tar: &[u8], dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut child = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(dir)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running tar")?;
    let mut stdin = child.stdin.take().context("tar has no input")?;
    // tar reads as it goes; a feeder thread keeps a full stderr pipe from stalling the write.
    let out = std::thread::scope(|s| {
        let w = s.spawn(move || stdin.write_all(tar));
        let out = child.wait_with_output();
        let _ = w.join();
        out
    })?;
    if !out.status.success() {
        bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    src: &str,
    dst: &str,
    namespace: &str,
    container: Option<&str>,
) -> Result<String> {
    check_ns(namespace, container)?;
    match plan(src, dst)? {
        Plan::Up { src, target, to } => {
            if !src.exists() {
                bail!("{}: no such file or directory", src.display());
            }
            let out = Command::new("tar")
                .args(local_pack_args(&src)?)
                .env("COPYFILE_DISABLE", "1")
                .output()
                .context("running tar")?;
            if !out.status.success() {
                bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            if out.stdout.len() > MAX_BYTES {
                bail!("{} is larger than {} MiB", src.display(), MAX_BYTES >> 20);
            }
            let script = extract_script(&target, namespace, container, &to, None);
            kube_host::guest_run_bytes(api, o, &script, Some(&out.stdout)).await?;
            Ok(format!("copied {} into {target}:{to}", src.display()))
        }
        Plan::Down { target, from, dir } => {
            kube_host::guest_sh(api, o, &exists_script(&target, namespace, container, &from))
                .await
                .with_context(|| format!("{target}:{from} does not exist (or the pod is not running)"))?;
            let tar =
                kube_host::guest_output_bytes(api, o, &pack_script(&target, namespace, container, &from), None).await?;
            if tar.len() > MAX_BYTES {
                bail!("{target}:{from} is larger than {} MiB", MAX_BYTES >> 20);
            }
            unpack_local(&tar, &dir)?;
            Ok(format!("copied {target}:{from} into {}", dir.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_paths_and_mac_paths_are_told_apart() {
        assert_eq!(
            parse_side("web:/app/log").unwrap(),
            Side::Pod { target: "deploy/web".into(), path: "/app/log".into() }
        );
        assert_eq!(
            parse_side("pod/web-1:/tmp/").unwrap(),
            Side::Pod { target: "pod/web-1".into(), path: "/tmp".into() }
        );
        for local in ["./a:b", "/x:y", "~/f", "plain.txt", "../up"] {
            assert!(matches!(parse_side(local).unwrap(), Side::Local(_)), "{local}");
        }
    }

    #[test]
    fn bad_pod_paths_are_refused() {
        for bad in ["web:rel", "web:/", "web:/a b", "web:/a;rm", "web:/a/../b", "job/x:/a", "Web;id:/a"] {
            assert!(parse_side(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn exactly_one_side_is_a_pod() {
        assert!(matches!(plan("./f", "web:/app").unwrap(), Plan::Up { .. }));
        assert!(matches!(plan("web:/app/f", "./out").unwrap(), Plan::Down { .. }));
        assert!(plan("./a", "./b").is_err());
        assert!(plan("web:/a", "db:/b").is_err());
    }

    #[test]
    fn scripts_name_the_pod_and_cap_the_download() {
        let s = pack_script("deploy/web", "shop", Some("app"), "/var/log/app.log");
        assert!(s.starts_with("sudo k3s kubectl exec -n shop deploy/web -c app -- tar cf - -C /var/log app.log | head -c "));
        assert!(s.ends_with(&(MAX_BYTES + 1).to_string()));
        assert!(pack_script("pod/x", "default", None, "/etc").contains("-C / etc"));
        assert_eq!(
            exists_script("pod/x", "default", None, "/etc"),
            "sudo k3s kubectl exec -n default pod/x -- test -e /etc"
        );
    }

    #[test]
    fn local_packing_uses_the_name_not_the_whole_path() {
        assert_eq!(
            local_pack_args(Path::new("/Users/me/site/index.html")).unwrap(),
            ["-cf", "-", "-C", "/Users/me/site", "index.html"]
        );
        assert_eq!(local_pack_args(Path::new("f.txt")).unwrap()[3], ".");
        assert!(local_pack_args(Path::new("/")).is_err());
    }

    #[test]
    fn a_roundtrip_through_tar_restores_files() {
        let base = std::env::temp_dir().join(format!("fluxctl-cp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/d")).unwrap();
        std::fs::write(base.join("src/d/a.txt"), "hello").unwrap();
        let out = Command::new("tar").args(local_pack_args(&base.join("src/d")).unwrap()).output().unwrap();
        assert!(out.status.success());
        unpack_local(&out.stdout, &base.join("out")).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("out/d/a.txt")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&base);
    }
}
