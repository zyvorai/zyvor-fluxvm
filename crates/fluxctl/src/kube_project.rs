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

pub async fn start<A: VmApi>(api: &A, base: KubeOptions, file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading {} (create it, or pass --file)", file.display()))?;
    let project = parse(&text)?;
    let dir = file
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Check everything that can be checked before the first slow step.
    let manifests = manifest_files(dir, &project.manifests)?;
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
    for m in &manifests {
        let yaml =
            std::fs::read_to_string(m).with_context(|| format!("reading {}", m.display()))?;
        kube_host::guest_apply(api, &o, &yaml)
            .await
            .with_context(|| format!("applying {}", m.display()))?;
        eprintln!("applied {}", m.display());
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
}
