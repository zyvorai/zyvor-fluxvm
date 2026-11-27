// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl templates ls | show | run | down`: one-command stacks that ship inside the binary (Postgres with Adminer, Redis,
//! Mailpit as Compose files; nginx and whoami as k3s manifests). The data is `fluxvm_core::stack_templates`, which
//! Studio's Templates page lists too.
//!
//! `run NAME` writes the file to `~/.local/share/fluxvm/templates/NAME/` (or `--dir`) so you can see and edit it, says so, and
//! then starts it: Compose files with `docker compose -f FILE up -d` (on a Mac start the engine first with `fluxctl docker up`),
//! k3s manifests with `kubectl apply -f -` against a FluxVM k3s cluster (`--cluster NAME`, else kubectl's current context).
//! `down NAME` stops it again (Compose keeps its volumes; pass `--volumes` to delete them).
//!
//! VM environments (`debian`, `build`, ...: `vm_env`) are listed and started by the same subcommands, but make a VM
//! through the FluxVM daemon or state directory instead (`vm_env_up`, docs/vm-templates.md).

use crate::cli_errors::coded;
use crate::gateway::kube::{KubeRun, Selection, SystemKubectl};
use crate::vm_env;
use crate::vm_env_up::{Job, VmFlags};
use crate::vm_net::Backend;
use anyhow::{Context, Result, bail};
use fluxvm_core::stack_templates::{self as st, Kind, Template};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum TemplatesCommand {
    /// The templates, one line each.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// The file a template runs, printed (redirect it to keep your own copy).
    Show { name: String },
    /// Start a template with one command. A Compose or k3s template writes its file first and says where; a VM environment
    /// (debian, ubuntu, build, amd64, kvm, or your own .toml) creates, starts or just checks its VM (docs/vm-templates.md).
    Run(RunArgs),
    /// Stop what `run` started.
    Down(RunArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    pub name: String,
    /// Where to keep the file (default: ~/.local/share/fluxvm/templates/NAME).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// k3s templates: the FluxVM cluster by name (`~/.kube/fluxvm-NAME.yaml`).
    #[arg(long)]
    pub cluster: Option<String>,
    /// k3s templates: a kubectl context instead.
    #[arg(long)]
    pub kube_context: Option<String>,
    /// Do not ask before overwriting a file you edited (VM environments: before deleting or rewinding a VM).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Compose `down`: also delete the volumes (the data).
    #[arg(long)]
    pub volumes: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub vm: VmFlags,
}

fn lookup(name: &str) -> Result<&'static Template> {
    st::find(name).ok_or_else(|| {
        let mut names: Vec<_> = st::TEMPLATES.iter().map(|t| t.name).collect();
        names.extend(vm_env::names());
        coded(
            format!(
                "there is no template called {name:?}. Available: {}",
                names.join(", ")
            ),
            "TEMPLATE_UNKNOWN",
        )
    })
}

pub fn ls_text() -> String {
    let w = st::TEMPLATES
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0);
    let mut s = String::new();
    for t in st::TEMPLATES {
        s.push_str(&format!(
            "{:<w$}  {:<7}  {}\n",
            t.name,
            t.kind.word(),
            t.summary
        ));
    }
    s.push_str(
        "\nStart one: fluxctl templates run NAME      See its file: fluxctl templates show NAME\n",
    );
    s
}

pub(crate) fn data_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("fluxvm/templates")
}

pub fn file_for(t: &Template, dir: &Path) -> PathBuf {
    dir.join(match t.kind {
        Kind::Compose => "compose.yaml",
        Kind::K3s => "manifests.yaml",
    })
}

/// The commands `run` will execute, as argv lists (the file is passed by path for Compose, on stdin for kubectl).
pub fn run_argv(
    t: &Template,
    file: &Path,
    sel: &Selection,
    home: &Path,
    down: bool,
    volumes: bool,
) -> Result<Vec<String>> {
    match t.kind {
        Kind::Compose => {
            let mut v: Vec<String> = ["docker", "compose", "-f"].map(String::from).into();
            v.push(file.display().to_string());
            if down {
                v.push("down".into());
                if volumes {
                    v.push("--volumes".into());
                }
            } else {
                v.extend(["up", "-d", "--wait"].map(String::from));
            }
            Ok(v)
        }
        Kind::K3s => {
            let mut v = sel.prefix(Some(home))?;
            v.extend([if down { "delete" } else { "apply" }, "-f", "-"].map(String::from));
            if down {
                v.push("--ignore-not-found".into());
            }
            Ok(v)
        }
    }
}

fn needs(t: &Template, why: String) -> anyhow::Error {
    match t.kind {
        Kind::Compose => coded(
            format!(
                "{why}\n  (on a Mac: `fluxctl docker up` then `eval \"$(fluxctl docker env)\"`)"
            ),
            "TEMPLATE_NEEDS_DOCKER",
        ),
        Kind::K3s => coded(
            format!("{why}\n  (no cluster yet? `fluxctl kube up`)"),
            "TEMPLATE_NEEDS_KUBECTL",
        ),
    }
}

/// `run` or `down` of a VM environment, which needs the VM manager or a daemon. `None` for everything else, which
/// [`run`] handles without either.
pub fn env_job(cmd: &TemplatesCommand) -> Result<Option<Job>> {
    let (a, down) = match cmd {
        TemplatesCommand::Run(a) => (a, false),
        TemplatesCommand::Down(a) => (a, true),
        _ => return Ok(None),
    };
    let stack = st::find(&a.name).is_some();
    let Some(template) = (!stack)
        .then(|| vm_env::load(&a.name))
        .transpose()?
        .flatten()
    else {
        if stack && a.vm != VmFlags::default() {
            bail!(
                "--vm, --cpus, --dry-run and the other VM environment options do not apply to {}, which is not a VM",
                a.name
            );
        }
        return Ok(None);
    };
    if a.dir.is_some() || a.cluster.is_some() || a.kube_context.is_some() || a.volumes {
        bail!(
            "--dir, --cluster, --kube-context and --volumes are for Compose and k3s templates; {} is a VM (`down --delete` removes it)",
            a.name
        );
    }
    Ok(Some(Job {
        template,
        reference: a.name.clone(),
        flags: a.vm.clone(),
        down,
        yes: a.yes,
        json: a.json,
    }))
}

/// Runs the template through `runner`. Returns the commands it ran, for `--json`.
pub fn execute(
    a: &RunArgs,
    down: bool,
    home: &Path,
    runner: &dyn KubeRun,
    ask_overwrite: &dyn Fn(&Path) -> bool,
) -> Result<serde_json::Value> {
    let t = lookup(&a.name)?;
    let dir = a.dir.clone().unwrap_or_else(|| data_dir(home).join(t.name));
    let file = file_for(t, &dir);
    let sel = Selection {
        kube_context: a.kube_context.clone(),
        cluster: a.cluster.clone(),
    };
    let argv = run_argv(t, &file, &sel, home, down, a.volumes)?;
    let progress = |s: String| {
        if !a.json {
            println!("{s}")
        }
    };
    if !down {
        let edited = std::fs::read_to_string(&file).is_ok_and(|old| old != t.body);
        if edited && !a.yes && !ask_overwrite(&file) {
            bail!(
                "{} was edited and not overwritten; run with --yes to replace it, or use another --dir",
                file.display()
            );
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&file, t.body).with_context(|| format!("writing {}", file.display()))?;
        progress(format!("wrote {}", file.display()));
    } else if !file.exists() && t.kind == Kind::Compose {
        bail!(
            "{} is not there: nothing was started with `fluxctl templates run {}` from this folder (use --dir if you kept it elsewhere)",
            file.display(),
            t.name
        );
    }
    progress(format!(
        "running: {}",
        crate::gateway::kube::shell_join(&argv)
    ));
    let stdin = (t.kind == Kind::K3s).then_some(t.body.as_bytes());
    let out = runner
        .run(&argv, stdin, Duration::from_secs(900))
        .map_err(|e| needs(t, format!("could not run {}: {e}", argv[0])))?;
    if out.code != 0 {
        let msg = [out.stderr.trim(), out.stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("it exited with an error");
        return Err(needs(t, format!("{} failed: {msg}", t.name)));
    }
    if !down {
        progress(format!("\n{} is up.\n  {}", t.title, t.after));
        progress(format!("stop it: fluxctl templates down {}", t.name));
    } else {
        progress(format!("{} is stopped.", t.title));
    }
    Ok(
        json!({"name": t.name, "kind": t.kind, "file": file, "ran": argv, "action": if down {"down"} else {"run"}, "after": t.after}),
    )
}

pub fn run(cmd: &TemplatesCommand) -> Result<()> {
    match cmd {
        TemplatesCommand::Ls { json } => {
            let (backend, arch) = (Backend::host(), std::env::consts::ARCH);
            if *json {
                let mut v = st::gallery_json();
                v["environments"] = vm_env::gallery_json(backend, arch);
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}{}", ls_text(), vm_env::ls_text(backend, arch));
            }
        }
        TemplatesCommand::Show { name } => match vm_env::body(name) {
            Some(body) => print!("{body}"),
            None => print!("{}", lookup(name)?.body),
        },
        TemplatesCommand::Run(a) | TemplatesCommand::Down(a) => {
            let down = matches!(cmd, TemplatesCommand::Down(_));
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")?;
            let ask = |f: &Path| {
                use std::io::{BufRead, IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    return false;
                }
                eprint!("{} was edited. Overwrite it? [y/N] ", f.display());
                let _ = std::io::stderr().flush();
                let mut l = String::new();
                std::io::stdin().lock().read_line(&mut l).is_ok()
                    && matches!(l.trim(), "y" | "Y" | "yes")
            };
            let v = execute(a, down, &home, &SystemKubectl, &ask)?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::kube::fake::FakeKubectl;

    fn args(name: &str, dir: &Path) -> RunArgs {
        RunArgs {
            name: name.into(),
            dir: Some(dir.into()),
            cluster: None,
            kube_context: None,
            yes: false,
            volumes: false,
            json: true,
            vm: VmFlags::default(),
        }
    }

    #[test]
    fn vm_environments_become_jobs_and_the_two_kinds_of_flags_do_not_mix() {
        let d = tempfile::tempdir().unwrap();
        let mut a = args("debian", d.path());
        a.dir = None;
        a.vm.dry_run = true;
        let job = env_job(&TemplatesCommand::Run(a.clone())).unwrap().unwrap();
        assert_eq!(job.template.name, "debian");
        assert!(!job.down && job.flags.dry_run);
        assert!(
            env_job(&TemplatesCommand::Down(a.clone()))
                .unwrap()
                .unwrap()
                .down
        );
        a.volumes = true;
        let e = env_job(&TemplatesCommand::Down(a)).unwrap_err();
        assert!(e.to_string().contains("down --delete"), "{e}");
        // A stack template stays on the old path, and VM flags are refused for it instead of being ignored.
        let mut s = args("redis", d.path());
        assert!(
            env_job(&TemplatesCommand::Run(s.clone()))
                .unwrap()
                .is_none()
        );
        s.vm.dry_run = true;
        assert!(env_job(&TemplatesCommand::Run(s)).is_err());
        assert!(
            env_job(&TemplatesCommand::Run(args("nope", d.path())))
                .unwrap()
                .is_none()
        );
        assert!(
            env_job(&TemplatesCommand::Show {
                name: "debian".into()
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn every_template_body_parses_and_compose_ones_have_services() {
        for t in st::TEMPLATES {
            let docs: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(t.body)
                .map(|d| serde_yaml::Value::deserialize(d).unwrap())
                .collect();
            assert!(!docs.is_empty(), "{}", t.name);
            match t.kind {
                Kind::Compose => assert!(docs[0].get("services").is_some(), "{}", t.name),
                Kind::K3s => assert!(docs.iter().all(|d| d.get("kind").is_some()), "{}", t.name),
            }
        }
    }
    use serde::Deserialize;

    #[test]
    fn a_compose_template_is_written_then_started_with_docker_compose() {
        let d = tempfile::tempdir().unwrap();
        let f = FakeKubectl::default().text(0, "", "");
        let v = execute(
            &args("redis", d.path()),
            false,
            Path::new("/h"),
            &f,
            &|_| false,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(d.path().join("compose.yaml"))
                .unwrap()
                .contains("redis:7")
        );
        let call = &f.argvs()[0];
        assert!(
            call.starts_with("docker compose -f ") && call.ends_with("up -d --wait"),
            "{call}"
        );
        assert_eq!(v["action"], "run");
    }

    #[test]
    fn a_k3s_template_is_applied_on_stdin_to_the_chosen_cluster() {
        let d = tempfile::tempdir().unwrap();
        let f = FakeKubectl::default().text(0, "deployment created", "");
        let mut a = args("whoami", d.path());
        a.cluster = Some("lab".into());
        execute(&a, false, Path::new("/h"), &f, &|_| false).unwrap();
        let calls = f.calls.borrow();
        assert_eq!(
            calls[0].0.join(" "),
            "kubectl --kubeconfig /h/.kube/fluxvm-lab.yaml apply -f -"
        );
        assert!(calls[0].1.as_deref().unwrap().contains("traefik/whoami"));
    }

    #[test]
    fn missing_docker_and_failures_are_explained() {
        let d = tempfile::tempdir().unwrap();
        let f = FakeKubectl::default().fail("no such file");
        let e = execute(
            &args("redis", d.path()),
            false,
            Path::new("/h"),
            &f,
            &|_| false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("TEMPLATE_NEEDS_DOCKER") && e.contains("fluxctl docker up"),
            "{e}"
        );
        let f = FakeKubectl::default().text(1, "", "connection refused");
        let e = execute(
            &args("whoami", d.path()),
            false,
            Path::new("/h"),
            &f,
            &|_| false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("TEMPLATE_NEEDS_KUBECTL") && e.contains("connection refused"),
            "{e}"
        );
    }

    #[test]
    fn an_unknown_name_lists_the_choices() {
        let d = tempfile::tempdir().unwrap();
        let f = FakeKubectl::default();
        let e = execute(&args("nope", d.path()), false, Path::new("/h"), &f, &|_| {
            false
        })
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("TEMPLATE_UNKNOWN") && e.contains("postgres-adminer"),
            "{e}"
        );
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn an_edited_file_is_not_overwritten_without_a_yes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("compose.yaml"), "mine").unwrap();
        let f = FakeKubectl::default().text(0, "", "");
        assert!(
            execute(
                &args("redis", d.path()),
                false,
                Path::new("/h"),
                &f,
                &|_| false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("compose.yaml")).unwrap(),
            "mine"
        );
        let mut a = args("redis", d.path());
        a.yes = true;
        execute(&a, false, Path::new("/h"), &f, &|_| false).unwrap();
        assert!(
            std::fs::read_to_string(d.path().join("compose.yaml"))
                .unwrap()
                .contains("redis:7")
        );
    }

    #[test]
    fn down_stops_and_volumes_are_only_deleted_when_asked() {
        let d = tempfile::tempdir().unwrap();
        let t = lookup("redis").unwrap();
        let file = file_for(t, d.path());
        let plain = run_argv(
            t,
            &file,
            &Selection::default(),
            Path::new("/h"),
            true,
            false,
        )
        .unwrap();
        assert!(plain.ends_with(&["down".to_owned()]));
        let vol = run_argv(t, &file, &Selection::default(), Path::new("/h"), true, true).unwrap();
        assert!(vol.contains(&"--volumes".to_owned()));
    }

    #[test]
    fn the_listing_names_every_template() {
        let s = ls_text();
        for t in st::TEMPLATES {
            assert!(s.contains(t.name));
        }
    }
}
