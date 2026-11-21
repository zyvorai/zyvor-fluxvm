// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! GitOps bootstrap: installs Flux or Argo CD from a pinned, checksum-verified manifest ([`super::pins`]) and points it at one
//! Git repository. This is not `flux bootstrap`: it does not create the repository or push to it.
//!
//! The deploy key is read from a file on this Mac (which must be private: mode 600 or stricter), put in a Secret that is
//! sent on stdin to `kubectl apply -f -`, and never appears in an argument, a log line or the report. A Flux Secret needs the Git
//! server's host keys; they are taken from a file you provide, never fetched on first use. Not run against real hardware.

use super::pins;
use super::platform::Remote;
use super::recipes::{PinSrc, apply_pin, dns_label_ok};
use super::script::K3S_BIN;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_yaml::{Mapping, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Flux,
    Argo,
}

pub struct GitopsReq {
    pub tool: Tool,
    pub repo: String,
    pub branch: String,
    pub path: String,
    /// Name of the Flux GitRepository/Kustomization, or the Argo CD Application.
    pub name: String,
    /// Contents of the deploy key file (never logged).
    pub key: Option<String>,
    pub known_hosts: Option<String>,
    /// Argo CD: sync automatically (prune and self-heal). Flux always syncs.
    pub auto_sync: bool,
    pub dest_namespace: String,
    pub src: PinSrc,
}

#[derive(Debug, Serialize, Default)]
pub struct GitopsReport {
    pub tool: String,
    pub version: String,
    pub applied: Vec<String>,
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------- input checks

fn plain(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.len() <= 300 && s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b))
}

#[derive(Debug, PartialEq, Eq)]
pub enum RepoKind {
    /// `git@host:org/repo.git` or `ssh://git@host/org/repo.git`
    Ssh,
    Https,
}

pub fn repo_kind(url: &str) -> Result<RepoKind> {
    if let Some(rest) = url.strip_prefix("https://") {
        if plain(rest, "@:/._~-%") && !rest.contains('@') {
            return Ok(RepoKind::Https);
        }
        bail!("repository URL {url:?} has characters or credentials in it that are not allowed (use a deploy key, not a password in the URL)");
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        if plain(rest, "@:/._~-") {
            return Ok(RepoKind::Ssh);
        }
    } else if let Some((user_host, p)) = url.split_once(':')
        && user_host.contains('@')
        && plain(user_host, "@._-")
        && plain(p, "/._~-")
        && !p.starts_with("//")
    {
        return Ok(RepoKind::Ssh);
    }
    bail!("repository {url:?} must be git@host:org/repo.git, ssh://git@host/org/repo.git or https://host/org/repo.git")
}

/// Flux wants the `ssh://` form; `git@host:org/repo` becomes `ssh://git@host/org/repo`.
pub fn flux_url(url: &str) -> String {
    match url.split_once(':') {
        Some((uh, p)) if !url.contains("://") => format!("ssh://{uh}/{p}"),
        _ => url.to_owned(),
    }
}

pub fn branch_ok(b: &str) -> bool {
    plain(b, "._/-") && !b.contains("..") && !b.starts_with('-') && !b.starts_with('/') && !b.ends_with('/')
}

/// A path inside the repository, without `..`; returns it without a leading `./`.
pub fn repo_path(p: &str) -> Result<String> {
    let t = p.strip_prefix("./").unwrap_or(p).trim_matches('/');
    if t.is_empty() || t == "." {
        return Ok(String::new());
    }
    if !plain(t, "._/-") || t.split('/').any(|c| c == ".." || c.is_empty()) {
        bail!("--path {p:?} must be a relative path inside the repository");
    }
    Ok(t.to_owned())
}

/// Reads a deploy key. It must be private to this user and look like a private key; its content is returned, not shown.
pub fn read_key_file(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.mode() & 0o077 != 0 {
        bail!("{} is readable by other users (mode {:o}); chmod 600 it", path.display(), meta.mode() & 0o777);
    }
    if meta.len() > 16 * 1024 {
        bail!("{} is larger than 16 KiB; it is not a deploy key", path.display());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    check_key(&text)?;
    Ok(text)
}

pub fn check_key(text: &str) -> Result<()> {
    if !text.contains("PRIVATE KEY-----") {
        bail!("the deploy key file does not contain a private key");
    }
    if text.contains("ENCRYPTED") {
        bail!("the deploy key is passphrase-protected; a controller cannot enter a passphrase, use a key without one");
    }
    Ok(())
}

/// Reads a known_hosts file (at most 64 KiB).
pub fn read_known_hosts(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > 64 * 1024 {
        bail!("{} is larger than 64 KiB", path.display());
    }
    let t = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if !t.lines().any(|l| !l.trim().is_empty() && !l.starts_with('#') && l.split_whitespace().count() >= 3) {
        bail!("{} has no `host keytype key` line", path.display());
    }
    Ok(t)
}

// ---------------------------------------------------------------- generators

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(s(k), v);
    }
    Value::Mapping(m)
}

/// A Secret with `stringData`. Built with serde_yaml so multi-line keys are quoted correctly.
pub fn secret_yaml(name: &str, ns: &str, labels: &[(&str, &str)], data: &[(&str, &str)]) -> String {
    let mut meta = vec![("name", s(name)), ("namespace", s(ns))];
    if !labels.is_empty() {
        meta.push(("labels", map(labels.iter().map(|(k, v)| (*k, s(v))).collect())));
    }
    let v = map(vec![
        ("apiVersion", s("v1")),
        ("kind", s("Secret")),
        ("metadata", map(meta)),
        ("type", s("Opaque")),
        ("stringData", map(data.iter().map(|(k, v)| (*k, s(v))).collect())),
    ]);
    serde_yaml::to_string(&v).expect("a mapping serialises")
}

/// Flux: the `GitRepository` and the `Kustomization` that applies `path` from it.
pub fn flux_sync_yaml(name: &str, url: &str, branch: &str, path: &str, with_secret: bool) -> String {
    let secret = if with_secret { format!("  secretRef:\n    name: {name}\n") } else { String::new() };
    format!(
        "apiVersion: source.toolkit.fluxcd.io/v1\nkind: GitRepository\nmetadata:\n  name: {name}\n  namespace: flux-system\n\
         spec:\n  interval: 1m0s\n  ref:\n    branch: {branch}\n{secret}  url: {url}\n---\n\
         apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: {name}\n  namespace: flux-system\n\
         spec:\n  interval: 10m0s\n  path: ./{path}\n  prune: true\n  sourceRef:\n    kind: GitRepository\n    name: {name}\n"
    )
}

/// Argo CD: an `Application` for `path` of the repository, deployed into this cluster.
pub fn argo_app_yaml(name: &str, url: &str, branch: &str, path: &str, dest_ns: &str, auto_sync: bool) -> String {
    let sync = if auto_sync { "  syncPolicy:\n    automated:\n      prune: true\n      selfHeal: true\n" } else { "" };
    format!(
        "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: {name}\n  namespace: argocd\n\
         spec:\n  project: default\n  source:\n    repoURL: {url}\n    targetRevision: {branch}\n    path: {}\n  \
         destination:\n    server: https://kubernetes.default.svc\n    namespace: {dest_ns}\n{sync}",
        if path.is_empty() { "." } else { path }
    )
}

// ---------------------------------------------------------------- the flow

fn kubectl(op: &str, args: &str) -> String {
    format!(": fluxvm-op {op}; set -eu; sudo -n {K3S_BIN} kubectl {args}")
}

pub async fn bootstrap<R: Remote>(r: &R, req: GitopsReq) -> Result<GitopsReport> {
    if !dns_label_ok(&req.name) {
        bail!("--name {:?} must be a DNS label", req.name);
    }
    if !branch_ok(&req.branch) {
        bail!("--branch {:?} is not a branch name", req.branch);
    }
    if !dns_label_ok(&req.dest_namespace) {
        bail!("--dest-namespace {:?} must be a DNS label", req.dest_namespace);
    }
    let path = repo_path(&req.path)?;
    let kind = repo_kind(&req.repo)?;
    if let Some(k) = &req.key {
        check_key(k)?;
        if kind == RepoKind::Https {
            bail!("a deploy key is for ssh repositories; this URL is https");
        }
    }
    if kind == RepoKind::Ssh && req.key.is_none() {
        bail!("an ssh repository needs --deploy-key-file");
    }
    if req.tool == Tool::Flux && kind == RepoKind::Ssh && req.known_hosts.is_none() {
        bail!("Flux needs the Git server's host keys: pass --known-hosts-file (lines from `ssh-keyscan HOST` that you have checked against the provider's published fingerprints)");
    }
    let (pin, name) = match req.tool {
        Tool::Flux => (&pins::FLUX, "flux"),
        Tool::Argo => (&pins::ARGOCD, "argocd"),
    };
    let mut rep = GitopsReport { tool: name.into(), version: pin.version.into(), ..Default::default() };
    match req.tool {
        Tool::Flux => {
            apply_pin(r, &pins::FLUX, req.src).await?;
            rep.applied.push(format!("flux {}", pins::FLUX.version));
            for crd in ["gitrepositories.source.toolkit.fluxcd.io", "kustomizations.kustomize.toolkit.fluxcd.io"] {
                r.run_server(&kubectl("gitops-wait-crd", &format!("wait --for=condition=Established crd/{crd} --timeout=90s")), None).await?;
            }
            for d in ["source-controller", "kustomize-controller"] {
                r.run_server(&kubectl("gitops-rollout", &format!("-n flux-system rollout status deployment/{d} --timeout=180s")), None).await?;
            }
            let url = if kind == RepoKind::Ssh { flux_url(&req.repo) } else { req.repo.clone() };
            if let Some(key) = &req.key {
                let sec = secret_yaml(&req.name, "flux-system", &[], &[("identity", key), ("known_hosts", req.known_hosts.as_deref().unwrap_or_default())]);
                r.run_server(&kubectl("gitops-secret", "apply -f -"), Some(sec.as_bytes())).await?;
                rep.applied.push(format!("Secret {} (deploy key) in flux-system", req.name));
            }
            let sync = flux_sync_yaml(&req.name, &url, &req.branch, &path, req.key.is_some());
            r.run_server(&kubectl("gitops-sync", "apply -f -"), Some(sync.as_bytes())).await?;
            rep.applied.push(format!("GitRepository and Kustomization {} ({} @ {}, path ./{path})", req.name, req.repo, req.branch));
            rep.notes.push("Flux syncs every minute and prunes what the repository no longer lists. Add the deploy key's public half to the repository (read-only is enough)".into());
        }
        Tool::Argo => {
            r.run_server(&kubectl("gitops-namespace", "create namespace argocd --dry-run=client -o yaml | sudo -n /usr/local/bin/k3s kubectl apply -f -"), None).await?;
            apply_pin(r, &pins::ARGOCD, req.src).await?;
            rep.applied.push(format!("argocd {}", pins::ARGOCD.version));
            r.run_server(&kubectl("gitops-wait-crd", "wait --for=condition=Established crd/applications.argoproj.io --timeout=90s"), None).await?;
            r.run_server(&kubectl("gitops-rollout", "-n argocd rollout status statefulset/argocd-application-controller --timeout=240s"), None).await?;
            r.run_server(&kubectl("gitops-rollout", "-n argocd rollout status deployment/argocd-repo-server --timeout=240s"), None).await?;
            if let Some(kh) = &req.known_hosts {
                let cm = serde_yaml::to_string(&map(vec![
                    ("apiVersion", s("v1")),
                    ("kind", s("ConfigMap")),
                    ("metadata", map(vec![
                        ("name", s("argocd-ssh-known-hosts-cm")),
                        ("namespace", s("argocd")),
                        ("labels", map(vec![("app.kubernetes.io/name", s("argocd-ssh-known-hosts-cm")), ("app.kubernetes.io/part-of", s("argocd"))])),
                    ])),
                    ("data", map(vec![("ssh_known_hosts", s(kh))])),
                ]))?;
                r.run_server(&kubectl("gitops-known-hosts", "apply --server-side --force-conflicts -f -"), Some(cm.as_bytes())).await?;
                rep.applied.push("argocd-ssh-known-hosts-cm replaced with your known_hosts (Argo CD's built-in list is gone)".into());
            }
            if let Some(key) = &req.key {
                let sec = secret_yaml(
                    &format!("repo-{}", req.name),
                    "argocd",
                    &[("argocd.argoproj.io/secret-type", "repository")],
                    &[("type", "git"), ("url", &req.repo), ("sshPrivateKey", key)],
                );
                r.run_server(&kubectl("gitops-secret", "apply -f -"), Some(sec.as_bytes())).await?;
                rep.applied.push(format!("repository Secret repo-{} (deploy key) in argocd", req.name));
            }
            let app = argo_app_yaml(&req.name, &req.repo, &req.branch, &path, &req.dest_namespace, req.auto_sync);
            r.run_server(&kubectl("gitops-sync", "apply -f -"), Some(app.as_bytes())).await?;
            rep.applied.push(format!("Application {} ({} @ {}, path {})", req.name, req.repo, req.branch, if path.is_empty() { "." } else { &path }));
            if !req.auto_sync {
                rep.notes.push("the Application does not sync by itself: sync it in the UI or with `argocd app sync`, or repeat with --auto-sync".into());
            }
            rep.notes.push("the admin password is in the argocd-initial-admin-secret Secret; read it with kubectl on a server and change it, then delete that Secret".into());
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAFAKEKEYMATERIAL\n-----END OPENSSH PRIVATE KEY-----\n";
    const KH: &str = "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n";

    fn req(tool: Tool) -> GitopsReq {
        GitopsReq {
            tool,
            repo: "git@github.com:acme/fleet.git".into(),
            branch: "main".into(),
            path: "./clusters/dev".into(),
            name: "fleet".into(),
            key: Some(KEY.into()),
            known_hosts: Some(KH.into()),
            auto_sync: false,
            dest_namespace: "default".into(),
            src: PinSrc::Download,
        }
    }

    #[test]
    fn repo_urls() {
        assert_eq!(repo_kind("git@github.com:acme/fleet.git").unwrap(), RepoKind::Ssh);
        assert_eq!(repo_kind("ssh://git@host.example:2222/acme/fleet.git").unwrap(), RepoKind::Ssh);
        assert_eq!(repo_kind("https://github.com/acme/fleet.git").unwrap(), RepoKind::Https);
        for bad in ["https://user:pw@github.com/a/b", "git@host:a b", "http://x/y", "git@h:a;rm", "https://x/y\nz", "file:///etc", "", "ssh://"] {
            assert!(repo_kind(bad).is_err(), "{bad:?}");
        }
        assert_eq!(flux_url("git@github.com:acme/fleet.git"), "ssh://git@github.com/acme/fleet.git");
        assert_eq!(flux_url("ssh://git@h:22/a/b"), "ssh://git@h:22/a/b");
    }

    #[test]
    fn paths_and_branches() {
        assert_eq!(repo_path("./clusters/dev").unwrap(), "clusters/dev");
        assert_eq!(repo_path("clusters/dev/").unwrap(), "clusters/dev");
        assert_eq!(repo_path(".").unwrap(), "");
        assert!(repo_path("../x").is_err() && repo_path("a/../b").is_err() && repo_path("a b").is_err() && repo_path("a//b").is_err());
        assert!(branch_ok("main") && branch_ok("release/1.2") && !branch_ok("a b") && !branch_ok("-x") && !branch_ok("a..b"));
    }

    #[test]
    fn key_checks() {
        assert!(check_key(KEY).is_ok());
        assert!(check_key("hello").is_err());
        assert!(check_key("-----BEGIN ENCRYPTED PRIVATE KEY-----\nx\n-----END ENCRYPTED PRIVATE KEY-----").is_err());
    }

    #[test]
    fn key_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fluxvm-gitops-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("key");
        std::fs::write(&p, KEY).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key_file(&p).unwrap_err().to_string().contains("readable by other users"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key_file(&p).unwrap(), KEY);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flux_sync_golden() {
        assert_eq!(
            flux_sync_yaml("fleet", "ssh://git@github.com/acme/fleet.git", "main", "clusters/dev", true),
            "apiVersion: source.toolkit.fluxcd.io/v1\nkind: GitRepository\nmetadata:\n  name: fleet\n  namespace: flux-system\n\
spec:\n  interval: 1m0s\n  ref:\n    branch: main\n  secretRef:\n    name: fleet\n  url: ssh://git@github.com/acme/fleet.git\n---\n\
apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: fleet\n  namespace: flux-system\n\
spec:\n  interval: 10m0s\n  path: ./clusters/dev\n  prune: true\n  sourceRef:\n    kind: GitRepository\n    name: fleet\n"
        );
        assert!(!flux_sync_yaml("f", "https://h/a/b", "main", "", false).contains("secretRef"));
    }

    #[test]
    fn argo_app_golden() {
        assert_eq!(
            argo_app_yaml("fleet", "git@github.com:acme/fleet.git", "main", "clusters/dev", "default", true),
            "apiVersion: argoproj.io/v1alpha1\nkind: Application\nmetadata:\n  name: fleet\n  namespace: argocd\n\
spec:\n  project: default\n  source:\n    repoURL: git@github.com:acme/fleet.git\n    targetRevision: main\n    path: clusters/dev\n  \
destination:\n    server: https://kubernetes.default.svc\n    namespace: default\n  syncPolicy:\n    automated:\n      prune: true\n      selfHeal: true\n"
        );
        assert!(!argo_app_yaml("f", "u", "main", "", "default", false).contains("syncPolicy"));
    }

    #[test]
    fn secret_round_trips_a_multiline_key() {
        let y = secret_yaml("fleet", "flux-system", &[], &[("identity", KEY), ("known_hosts", KH)]);
        let v: Value = serde_yaml::from_str(&y).unwrap();
        assert_eq!(v["stringData"]["identity"].as_str().unwrap(), KEY);
        assert_eq!(v["kind"], "Secret");
        assert_eq!(v["metadata"]["namespace"], "flux-system");
    }

    /// The key may be in the Secret that goes on stdin and nowhere else: not in a script (argv), not in the report.
    fn assert_key_only_on_stdin(r: &FakeRemote, rep: &GitopsReport) {
        let log = r.log.borrow();
        assert!(log.iter().all(|(_, script, _)| !script.contains("FAKEKEYMATERIAL")));
        assert!(!serde_json::to_string(rep).unwrap().contains("FAKEKEYMATERIAL"));
        assert_eq!(log.iter().filter(|(_, _, i)| i.as_deref().is_some_and(|x| x.contains("FAKEKEYMATERIAL"))).count(), 1);
    }

    #[tokio::test]
    async fn flux_installs_waits_then_secret_then_sync() {
        let r = FakeRemote::cluster(1, 0);
        let rep = bootstrap(&r, req(Tool::Flux)).await.unwrap();
        assert_eq!(
            r.ops(),
            ["c-s1:pin-apply", "c-s1:gitops-wait-crd", "c-s1:gitops-wait-crd", "c-s1:gitops-rollout", "c-s1:gitops-rollout", "c-s1:gitops-secret", "c-s1:gitops-sync"]
        );
        assert!(r.log.borrow()[0].1.contains(pins::FLUX.sha256));
        assert!(r.log.borrow()[6].2.as_deref().unwrap().contains("url: ssh://git@github.com/acme/fleet.git"));
        assert_key_only_on_stdin(&r, &rep);
    }

    #[tokio::test]
    async fn argo_installs_into_its_namespace_then_repo_secret_then_application() {
        let r = FakeRemote::cluster(1, 0);
        let rep = bootstrap(&r, req(Tool::Argo)).await.unwrap();
        let ops = r.ops();
        assert_eq!(ops[0], "c-s1:gitops-namespace");
        assert_eq!(ops[1], "c-s1:pin-apply");
        assert!(r.log.borrow()[1].1.contains("-n argocd") && r.log.borrow()[1].1.contains(pins::ARGOCD.sha256));
        assert!(ops.ends_with(&["c-s1:gitops-known-hosts".into(), "c-s1:gitops-secret".into(), "c-s1:gitops-sync".into()]));
        let secret = r.log.borrow().iter().find(|(_, s, _)| s.contains("gitops-secret")).unwrap().2.clone().unwrap();
        assert!(secret.contains("argocd.argoproj.io/secret-type: repository") && secret.contains("sshPrivateKey"));
        assert!(rep.notes.iter().any(|n| n.contains("does not sync by itself")));
        assert_key_only_on_stdin(&r, &rep);
    }

    #[tokio::test]
    async fn bad_requests_install_nothing() {
        let cases: Vec<Box<dyn Fn(&mut GitopsReq)>> = vec![
            Box::new(|q| q.key = None),                                    // ssh repo without a key
            Box::new(|q| q.known_hosts = None),                            // Flux without host keys
            Box::new(|q| q.repo = "https://github.com/a/b".into()),        // key with an https repo
            Box::new(|q| q.key = Some("not a key".into())),
            Box::new(|q| q.path = "../etc".into()),
            Box::new(|q| q.branch = "a b".into()),
            Box::new(|q| q.name = "Bad Name".into()),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut q = req(Tool::Flux);
            f(&mut q);
            let r = FakeRemote::cluster(1, 0);
            assert!(bootstrap(&r, q).await.is_err(), "case {i}");
            assert!(r.log.borrow().is_empty(), "case {i} sent something");
        }
        // an https repository needs neither key nor host keys
        let mut q = req(Tool::Flux);
        q.repo = "https://github.com/acme/fleet.git".into();
        q.key = None;
        q.known_hosts = None;
        let r = FakeRemote::cluster(1, 0);
        bootstrap(&r, q).await.unwrap();
        assert!(!r.ops().iter().any(|o| o.ends_with("gitops-secret")));
        // Argo CD does not need host keys (its built-in list covers the common providers)
        let mut q = req(Tool::Argo);
        q.known_hosts = None;
        bootstrap(&FakeRemote::cluster(1, 0), q).await.unwrap();
    }
}
