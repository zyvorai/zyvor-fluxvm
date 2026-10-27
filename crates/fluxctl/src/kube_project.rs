//! `fluxctl kube start`: a project describes what it needs in `fluxvm-kube.yaml`, and one command builds that
//! environment from nothing (and, run again, changes nothing): the cluster, the images, the apps, your manifests and
//! the forwards. It adds no new mechanism; it runs the commands that already exist, in the order that works.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

use crate::kube_apps::App;
use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

#[derive(Debug, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The cluster's name (default: the one on the command line).
    pub name: Option<String>,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    /// Keep k3s's Traefik, for Ingress hosts.
    pub traefik: Option<bool>,
    /// Images on the Mac to copy into the cluster first (`docker save`), such as `app:dev`.
    #[serde(default)]
    pub images: Vec<String>,
    /// Services to run with `kube add`: postgres, redis, ...
    #[serde(default)]
    pub apps: Vec<String>,
    /// YAML files, or folders of them, applied in this order. Paths are relative to the project file.
    #[serde(default)]
    pub manifests: Vec<PathBuf>,
    /// Services to give a port on this Mac, as `kube forward` does.
    #[serde(default)]
    pub forwards: Vec<Forward>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    pub service: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub service_port: Option<u16>,
    pub local: Option<u16>,
}

fn default_namespace() -> String {
    "default".into()
}

/// Reads and checks a project file's text.
pub fn parse(text: &str) -> Result<Project> {
    let p: Project = serde_yaml::from_str(text).context("reading the project file")?;
    if let Some(n) = &p.name
        && !kube_host::name_ok(n)
    {
        bail!("name {n:?} is not a Kubernetes name (lowercase letters, digits, dashes)");
    }
    for a in &p.apps {
        if App::from_str(a, true).is_err() {
            bail!(
                "app {a:?} is not one of: {}",
                crate::kube_apps::ALL
                    .iter()
                    .map(|a| a.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    if let Some(dup) = p
        .apps
        .iter()
        .enumerate()
        .find(|(i, a)| p.apps[..*i].contains(a))
    {
        bail!("app {:?} is listed twice", dup.1);
    }
    if let Some(bad) = p.images.iter().find(|i| !kube_host::image_ok(i)) {
        bail!("image {bad:?} is not a plain image reference");
    }
    for m in &p.manifests {
        let escapes = m.is_absolute() || m.components().any(|c| matches!(c, Component::ParentDir));
        if escapes {
            bail!(
                "manifest path {} must stay inside the project folder",
                m.display()
            );
        }
    }
    for f in &p.forwards {
        if !crate::kube_forward::name_ok(&f.service) || !kube_host::resource_name_ok(&f.namespace) {
            bail!(
                "forward {:?} in {:?} is not a Kubernetes name",
                f.service,
                f.namespace
            );
        }
    }
    Ok(p)
}

/// The project's wishes laid over the options from the command line.
pub fn apply_to(p: &Project, mut o: KubeOptions) -> KubeOptions {
    if let Some(n) = &p.name {
        o.name = n.clone();
    }
    if let Some(v) = p.cpus {
        o.cpus = v;
    }
    if let Some(v) = p.memory_mib {
        o.memory_mib = v;
    }
    if let Some(v) = p.disk_gib {
        o.disk_gib = v;
    }
    if let Some(v) = p.traefik {
        o.traefik = v;
    }
    o
}

/// The YAML files to apply: a file is itself; a folder is its `.yaml` and `.yml` files, by name.
pub fn manifest_files(base: &Path, entries: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = vec![];
    for e in entries {
        let path = base.join(e);
        if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .flatten()
                .map(|d| d.path())
                .filter(|p| {
                    p.is_file()
                        && matches!(p.extension().and_then(|x| x.to_str()), Some("yaml" | "yml"))
                })
                .collect();
            files.sort();
            if files.is_empty() {
                bail!("{} holds no .yaml or .yml files", path.display());
            }
            out.extend(files);
        } else if path.is_file() {
            out.push(path);
        } else {
            bail!("manifest {} does not exist", path.display());
        }
    }
    Ok(out)
}

/// A project file given as an address, not a path.
pub fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// Most a fetched project file or manifest may weigh, and how many manifests a remote project may list.
const MAX_REMOTE_BYTES: usize = 1 << 20;
const MAX_REMOTE_MANIFESTS: usize = 30;

/// `manifest` (a relative path from a project file) resolved against the project's own address. Only `https`, only
/// plain `.yaml`/`.yml` files: a remote project cannot list a folder, and nothing it names may leave its directory.
pub fn join_url(project_url: &str, manifest: &Path) -> Result<String> {
    if !project_url.starts_with("https://") {
        bail!("a project is fetched over https only, not {project_url:?}");
    }
    let m = manifest.to_string_lossy();
    let yaml = m.ends_with(".yaml") || m.ends_with(".yml");
    if !yaml {
        bail!(
            "{m:?}: a project fetched from an address lists each manifest as a .yaml or .yml file, not a folder"
        );
    }
    if m.contains(['\\', '?', '#', '%'])
        || m.starts_with('/')
        || m.split('/').any(|c| c == ".." || c.is_empty())
    {
        bail!("manifest {m:?} must be a plain relative path inside the project");
    }
    let base = project_url.split(['?', '#']).next().unwrap_or(project_url);
    let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
    Ok(format!("{dir}/{m}"))
}

async fn fetch_text(url: &str) -> Result<String> {
    // Redirects are followed only to https, so a fetch cannot be bounced onto plain http.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.url().scheme() == "https" && a.previous().len() < 4 {
                a.follow()
            } else {
                a.stop()
            }
        }))
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        bail!("{url}: {}", resp.status());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    if bytes.len() > MAX_REMOTE_BYTES {
        bail!("{url} is larger than {} KiB", MAX_REMOTE_BYTES >> 10);
    }
    String::from_utf8(bytes.to_vec()).with_context(|| format!("{url} is not text"))
}

/// What a project will do, in words, for the confirmation a remote project needs.
pub fn summary(p: &Project, manifests: &[String]) -> String {
    let mut lines = vec![format!(
        "cluster {}{}",
        p.name.as_deref().unwrap_or("(from --name)"),
        if p.traefik == Some(true) {
            " with Traefik"
        } else {
            ""
        }
    )];
    if !p.images.is_empty() {
        lines.push(format!(
            "copy images from this Mac's Docker: {}",
            p.images.join(", ")
        ));
    }
    if !p.apps.is_empty() {
        lines.push(format!("add apps: {}", p.apps.join(", ")));
    }
    for m in manifests {
        lines.push(format!("apply {m}"));
    }
    for f in &p.forwards {
        lines.push(format!(
            "open a port on this Mac for {}/{}",
            f.namespace, f.service
        ));
    }
    lines.join("\n  ")
}

/// Everything `start` will apply, read before the first slow step: (label, YAML).
async fn load_project(file: &str, yes: bool) -> Result<(Project, Vec<(String, String)>)> {
    if is_url(file) {
        if !file.starts_with("https://") {
            bail!("a project is fetched over https only, not {file:?}");
        }
        let project = parse(&fetch_text(file).await?)?;
        if project.manifests.len() > MAX_REMOTE_MANIFESTS {
            bail!("a remote project may list at most {MAX_REMOTE_MANIFESTS} manifests");
        }
        let urls = project
            .manifests
            .iter()
            .map(|m| join_url(file, m))
            .collect::<Result<Vec<_>>>()?;
        if !yes {
            bail!(
                "{file} would do this on your Mac and in the cluster:\n  {}\nOnly run projects you trust. Add --yes to go ahead.",
                summary(&project, &urls)
            );
        }
        let mut docs = vec![];
        for u in urls {
            let yaml = fetch_text(&u).await?;
            docs.push((u, yaml));
        }
        return Ok((project, docs));
    }
    let path = Path::new(file);
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {file} (create it, or pass --file)"))?;
    let project = parse(&text)?;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut docs = vec![];
    for m in manifest_files(dir, &project.manifests)? {
        let yaml =
            std::fs::read_to_string(&m).with_context(|| format!("reading {}", m.display()))?;
        docs.push((m.display().to_string(), yaml));
    }
    Ok((project, docs))
}

pub async fn start<A: VmApi>(api: &A, base: KubeOptions, file: &str, yes: bool) -> Result<()> {
    // Check and read everything that can be before the first slow step.
    let (project, manifests) = load_project(file, yes).await?;
    let o = apply_to(&project, base);

    let kubeconfig = kube_host::up(api, &o).await?;
    eprintln!("cluster {} is up ({})", o.name, kubeconfig.display());
    if !project.images.is_empty() {
        for line in kube_host::load(api, &o, &project.images, None).await? {
            eprintln!("{line}");
        }
    }
    for name in &project.apps {
        let app = App::from_str(name, true).expect("checked in parse");
        eprintln!("{}", crate::kube_apps::add(api, &o, app).await?);
    }
    for (label, yaml) in &manifests {
        kube_host::guest_apply(api, &o, yaml)
            .await
            .with_context(|| format!("applying {label}"))?;
        eprintln!("applied {label}");
    }
    for f in &project.forwards {
        eprintln!(
            "{}",
            crate::kube_forward::forward(
                api,
                &o,
                &f.service,
                &f.namespace,
                f.service_port,
                f.local
            )
            .await?
        );
    }
    print!(
        "{}",
        crate::kube_ps::render(&crate::kube_ps::gather(api, &o).await?)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
name: shop
cpus: 4
memory_mib: 8192
traefik: true
images: [app:dev]
apps: [postgres, Redis]
manifests: [k8s/]
forwards:
  - service: web
    local: 8080
  - {service: api, namespace: shop, service_port: 9090}
"#;

    #[test]
    fn a_full_project_is_read() {
        let p = parse(FULL).unwrap();
        assert_eq!(p.name.as_deref(), Some("shop"));
        assert_eq!(p.apps, ["postgres", "Redis"]);
        assert_eq!(p.forwards[0].namespace, "default");
        assert_eq!(p.forwards[1].service_port, Some(9090));
        let o = apply_to(&p, KubeOptions::default());
        assert_eq!(
            (o.name.as_str(), o.cpus, o.memory_mib, o.traefik),
            ("shop", 4, 8192, true)
        );
        assert_eq!(
            o.disk_gib,
            KubeOptions::default().disk_gib,
            "unset fields keep the defaults"
        );
    }

    #[test]
    fn an_empty_project_is_valid_and_changes_nothing() {
        let p = parse("{}").unwrap();
        assert_eq!(p, Project::default());
        assert_eq!(
            apply_to(&p, KubeOptions::default()).name,
            KubeOptions::default().name
        );
    }

    #[test]
    fn mistakes_are_named() {
        let e = |t: &str| parse(t).unwrap_err().to_string();
        assert!(e("appz: [redis]").contains("reading the project file"));
        assert!(e("apps: [oracle]").contains("is not one of: postgres"));
        assert!(e("apps: [redis, redis]").contains("listed twice"));
        assert!(e("images: ['a b']").contains("plain image"));
        assert!(e("name: Bad_Name").contains("not a Kubernetes name"));
        assert!(e("manifests: [../x.yaml]").contains("inside the project"));
        assert!(e("manifests: [/etc/x.yaml]").contains("inside the project"));
        assert!(e("forwards: [{service: 'A;b'}]").contains("not a Kubernetes name"));
    }

    #[test]
    fn manifests_are_files_or_sorted_folders() {
        let dir = std::env::temp_dir().join(format!("fluxctl-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("k8s")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("k8s/20-svc.yaml"), "x").unwrap();
        std::fs::write(dir.join("k8s/10-deploy.yml"), "x").unwrap();
        std::fs::write(dir.join("k8s/notes.txt"), "x").unwrap();
        std::fs::write(dir.join("one.yaml"), "x").unwrap();
        let got = manifest_files(&dir, &["k8s".into(), "one.yaml".into()]).unwrap();
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["10-deploy.yml", "20-svc.yaml", "one.yaml"]);
        assert!(
            manifest_files(&dir, &["empty".into()])
                .unwrap_err()
                .to_string()
                .contains("no .yaml")
        );
        assert!(
            manifest_files(&dir, &["missing.yaml".into()])
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plain_http_is_refused_before_anything_is_fetched() {
        let e = load_project("http://127.0.0.1:9/p.yaml", true)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("https only"), "{e}");
    }

    #[test]
    fn a_project_address_is_told_from_a_path() {
        assert!(is_url("https://example.com/p.yaml"));
        assert!(is_url("http://example.com/p.yaml"));
        assert!(!is_url("fluxvm-kube.yaml") && !is_url("./https://x"));
    }

    #[test]
    fn remote_manifests_resolve_beside_the_project_and_stay_inside_it() {
        let base = "https://raw.example.com/acme/shop/main/fluxvm-kube.yaml?token=1";
        assert_eq!(
            join_url(base, Path::new("k8s/web.yaml")).unwrap(),
            "https://raw.example.com/acme/shop/main/k8s/web.yaml"
        );
        assert!(
            join_url("http://x/p.yaml", Path::new("a.yaml")).is_err(),
            "plain http"
        );
        for bad in [
            "k8s/",
            "k8s",
            "../a.yaml",
            "/a.yaml",
            "a//b.yaml",
            "a.yaml?x=1",
            "a%2e.yaml",
            "a#b.yaml",
            "a\\b.yaml",
            "a.txt",
        ] {
            assert!(
                join_url("https://x.dev/p.yaml", Path::new(bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_confirmation_names_every_effect() {
        let p = parse("name: shop\ntraefik: true\nimages: [app:dev]\napps: [postgres]\nforwards: [{service: web}]\n").unwrap();
        let s = summary(&p, &["https://x.dev/k8s/web.yaml".into()]);
        for want in [
            "shop with Traefik",
            "app:dev",
            "add apps: postgres",
            "apply https://x.dev/k8s/web.yaml",
            "default/web",
        ] {
            assert!(s.contains(want), "{want} in {s}");
        }
    }
}
