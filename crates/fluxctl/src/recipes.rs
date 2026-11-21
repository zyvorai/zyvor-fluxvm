// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl recipe list | show | run | credentials | uninstall`: a short catalog of things people actually want ("a dev
//! database", "a website", "a CI runner", "a private registry", "monitoring"), layered over what already exists
//! (`fluxctl kube add`, `fluxctl mac runner`, and the Docker engine) instead of next to it.
//!
//! A recipe is *data*: [`render`] turns a name and parameters into a [`Plan`] (the commands, the URL, where the
//! credentials will be, a backup schedule, an uninstall). `show` prints the plan, `run` executes its steps, and Studio's
//! Recipes page shows the same plan with a "Copy as fluxctl" line. Rendering is pure, so each recipe has a golden file.
//!
//! Secrets: a generated password is written to a 0600 file in a 0700 directory before anything runs, reaches commands
//! only through that file or a pipe (never argv), and is never printed by `run`, `show` or the logs; `credentials` is the
//! one command that prints it, on request.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
pub enum RecipeCommand {
    /// The recipes, one line each.
    List,
    /// What a recipe does, without doing it: the commands, the URL, where credentials go, backup and uninstall.
    Show {
        name: String,
        /// Parameter as key=value (see `show` for the list), repeatable.
        #[arg(long = "set", value_parser = parse_kv)]
        set: Vec<(String, String)>,
        #[arg(long)]
        json: bool,
    },
    /// Run it. `--dry-run` prints the commands and runs none.
    Run {
        name: String,
        #[arg(long = "set", value_parser = parse_kv)]
        set: Vec<(String, String)>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the credentials a recipe stored (the only command that does).
    Credentials { name: String },
    /// Undo a recipe: the uninstall commands from `show`. `--keep-data` leaves data volumes in place.
    Uninstall {
        name: String,
        #[arg(long = "set", value_parser = parse_kv)]
        set: Vec<(String, String)>,
        #[arg(long)]
        keep_data: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("expected key=value, got {s:?}")),
    }
}

pub struct Info {
    pub name: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// (key, default, help); an empty default means the parameter is required.
    pub params: &'static [(&'static str, &'static str, &'static str)],
}

pub const CATALOG: &[Info] = &[
    Info {
        name: "dev-database",
        title: "Dev database",
        summary: "PostgreSQL, MySQL, MongoDB or Redis in the local cluster, on its usual port on 127.0.0.1.",
        params: &[("engine", "postgres", "postgres, mysql, mongodb or redis")],
    },
    Info {
        name: "website",
        title: "Website",
        summary: "Serve a folder of static files with nginx, restarted automatically.",
        params: &[
            ("dir", "", "the folder with index.html"),
            ("port", "8080", "port on this machine"),
        ],
    },
    Info {
        name: "ci-runner",
        title: "CI runner (GitHub Actions)",
        summary: "A self-hosted runner: one fresh macOS VM per job on a Mac, or an ephemeral container on Linux.",
        params: &[
            ("os", "linux", "macos or linux"),
            ("repo", "", "OWNER/REPO"),
            ("template", "", "macOS only: the guest image with the Actions runner in it"),
        ],
    },
    Info {
        name: "registry",
        title: "Private registry",
        summary: "A Docker/OCI registry with a user name and a generated password.",
        params: &[("port", "5000", "port on this machine")],
    },
    Info {
        name: "monitoring",
        title: "Monitoring",
        summary: "Grafana in the local cluster for dashboards over your fleet's metrics.",
        params: &[],
    },
];

pub fn info(name: &str) -> Option<&'static Info> {
    CATALOG.iter().find(|i| i.name == name)
}

/// One thing `run` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a command. `stdin` is a template (`{{password}}` is filled from the stored credentials) piped to it;
    /// `stdout_to` saves its output to a 0600 file.
    Run {
        what: String,
        argv: Vec<String>,
        stdin: Option<String>,
        stdout_to: Option<String>,
    },
    /// Write a 0600 file from a template (the `{{...}}` come from the stored credentials).
    WriteFile {
        what: String,
        path: String,
        template: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    pub title: String,
    pub steps: Vec<Step>,
    pub url: String,
    /// Credentials that are generated at run time (names); never shown, stored in the credentials file.
    pub generated: Vec<String>,
    /// Credentials that are fixed defaults of the thing being run (label, value); safe to show, and not secret.
    pub defaults: Vec<(String, String)>,
    pub backup_schedule: String,
    pub backup_command: String,
    pub uninstall: Vec<Vec<String>>,
    pub notes: Vec<String>,
}

/// Where a recipe keeps its files: `<home>/.config/fluxvm/recipes/<name>` (mode 0700).
pub fn dir_for(home: &Path, name: &str) -> PathBuf {
    home.join(".config/fluxvm/recipes").join(name)
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

fn param(p: &BTreeMap<String, String>, key: &str) -> Result<String> {
    let i = CATALOG
        .iter()
        .flat_map(|i| i.params)
        .find(|(k, _, _)| *k == key)
        .with_context(|| format!("unknown parameter {key}"))?;
    let v = p.get(key).cloned().unwrap_or_else(|| i.1.to_owned());
    if v.is_empty() {
        bail!("this recipe needs --set {key}=...  ({})", i.2);
    }
    Ok(v)
}

fn port(p: &BTreeMap<String, String>) -> Result<u16> {
    let v = param(p, "port")?;
    v.parse::<u16>()
        .ok()
        .filter(|p| *p >= 1)
        .with_context(|| format!("port must be 1-65535, got {v}"))
}

/// A value that is safe to put in a container name, a path segment or a `docker -v` argument.
fn plain(v: &str, what: &str) -> Result<String> {
    if v.is_empty() || v.chars().any(|c| c.is_control() || c == ':' || c == ',') {
        bail!("{what} must not be empty or contain ':' or ','");
    }
    Ok(v.to_owned())
}

/// Turns a recipe name and parameters into a plan. Pure: nothing is read or run.
pub fn render(name: &str, set: &BTreeMap<String, String>, home: &Path) -> Result<Plan> {
    let info = info(name).with_context(|| {
        format!(
            "no recipe named {name}; the recipes are: {}",
            CATALOG.iter().map(|i| i.name).collect::<Vec<_>>().join(", ")
        )
    })?;
    for k in set.keys() {
        if !info.params.iter().any(|(p, _, _)| *p == k.as_str()) {
            bail!(
                "recipe {name} has no parameter {k}; it takes: {}",
                info.params
                    .iter()
                    .map(|p| p.0)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let rdir = dir_for(home, name);
    let rd = rdir.display().to_string();
    let mut plan = Plan {
        name: name.to_owned(),
        title: info.title.to_owned(),
        steps: vec![],
        url: String::new(),
        generated: vec![],
        defaults: vec![],
        backup_schedule: String::new(),
        backup_command: String::new(),
        uninstall: vec![],
        notes: vec![],
    };
    match name {
        "dev-database" => {
            let engine = param(set, "engine")?;
            let app = match engine.as_str() {
                "postgres" => crate::kube_apps::App::Postgres,
                "mysql" => crate::kube_apps::App::Mysql,
                "mongodb" => crate::kube_apps::App::Mongodb,
                "redis" => crate::kube_apps::App::Redis,
                other => bail!("engine must be postgres, mysql, mongodb or redis, got {other}"),
            };
            plan.steps.push(Step::Run {
                what: format!("start {engine} in the local cluster"),
                argv: argv(&["fluxctl", "kube", "add", &engine]),
                stdin: None,
                stdout_to: None,
            });
            for (k, v) in app.connect() {
                if k == "url" {
                    plan.url = v.clone();
                }
                plan.defaults.push((k.to_owned(), v));
            }
            let dumpable = engine != "redis";
            plan.backup_schedule = "daily at 03:00".into();
            plan.backup_command = if dumpable {
                format!("fluxctl kube db-backup {engine} \"$HOME/db-backups/{engine}-$(date +%F).sql\"")
            } else {
                "redis persists to its volume; copy the volume with `fluxctl kube` snapshots".into()
            };
            plan.uninstall = vec![argv(&["fluxctl", "kube", "remove", &engine, "--delete-data"])];
            plan.notes.push(
                "The password is the development default shown in the connection string; the port is open on 127.0.0.1 only."
                    .into(),
            );
            plan.notes
                .push("Needs the local cluster: `fluxctl kube up` once.".into());
        }
        "website" => {
            let dir = plain(&param(set, "dir")?, "dir")?;
            let port = port(set)?;
            plan.steps.push(Step::Run {
                what: "serve the folder with nginx".into(),
                argv: argv(&[
                    "docker",
                    "run",
                    "-d",
                    "--name",
                    "fluxvm-website",
                    "--restart",
                    "unless-stopped",
                    "-p",
                    &format!("{port}:80"),
                    "-v",
                    &format!("{dir}:/usr/share/nginx/html:ro"),
                    "nginx:alpine",
                ]),
                stdin: None,
                stdout_to: None,
            });
            plan.url = format!("http://localhost:{port}");
            plan.backup_schedule = "the folder is the source of truth; back it up with the rest of your files".into();
            plan.backup_command = format!("tar czf \"$HOME/site-$(date +%F).tgz\" -C \"{dir}\" .");
            plan.uninstall = vec![argv(&["docker", "rm", "-f", "fluxvm-website"])];
            plan.notes.push("Needs a Docker engine: `fluxctl docker up` on a Mac, or Docker on Linux.".into());
        }
        "ci-runner" => {
            let os = param(set, "os")?;
            let repo = plain(&param(set, "repo")?, "repo")?;
            if repo.split('/').count() != 2 {
                bail!("repo must look like OWNER/REPO");
            }
            let token = format!("{rd}/github.token");
            match os.as_str() {
                "macos" => {
                    let template = param(set, "template")?;
                    plan.steps.push(Step::Run {
                        what: "serve GitHub Actions jobs, one fresh macOS VM each (stays running; Ctrl-C to stop)".into(),
                        argv: argv(&["fluxctl", "mac", "runner", "--repo", &repo, "--token-file", &token, "--template", &template, "--label", "fluxvm"]),
                        stdin: None,
                        stdout_to: None,
                    });
                    plan.url = format!("https://github.com/{repo}/settings/actions/runners");
                    plan.uninstall = vec![argv(&["fluxctl", "mac", "ps"])];
                    plan.notes.push("Workflow: `runs-on: [self-hosted, macOS, ARM64, fluxvm]`.".into());
                }
                "linux" => {
                    let env = format!("{rd}/runner.env");
                    plan.steps.push(Step::WriteFile {
                        what: "write the runner's settings (0600)".into(),
                        path: env.clone(),
                        template: format!("ACCESS_TOKEN={{{{github_token}}}}\nREPO_URL=https://github.com/{repo}\nEPHEMERAL=true\nLABELS=fluxvm,linux\nRUNNER_NAME_PREFIX=fluxvm\n"),
                    });
                    plan.steps.push(Step::Run {
                        what: "start the runner container".into(),
                        argv: argv(&["docker", "run", "-d", "--name", "fluxvm-ci-runner", "--restart", "unless-stopped", "--env-file", &env, "myoung34/github-runner:latest"]),
                        stdin: None,
                        stdout_to: None,
                    });
                    plan.url = format!("https://github.com/{repo}/settings/actions/runners");
                    plan.uninstall = vec![argv(&["docker", "rm", "-f", "fluxvm-ci-runner"])];
                    plan.notes.push("Workflow: `runs-on: [self-hosted, linux, fluxvm]`. The image is the community `myoung34/github-runner`; review it before trusting it with a private repository.".into());
                }
                other => bail!("os must be macos or linux, got {other}"),
            }
            plan.generated.push("github_token".into());
            plan.notes.push(format!(
                "Save a GitHub token that may create runners in {token} (mode 0600). `recipe run` stops if it is missing or readable by others; it is never passed on a command line."
            ));
            plan.backup_schedule = "nothing to back up: runners are disposable".into();
            plan.backup_command = "none".into();
        }
        "registry" => {
            let port = port(set)?;
            let auth = format!("{rd}/htpasswd");
            plan.generated.push("password".into());
            plan.steps.push(Step::Run {
                what: "hash the generated password for the registry (the password goes in through stdin)".into(),
                argv: argv(&["docker", "run", "--rm", "-i", "--entrypoint", "htpasswd", "httpd:2", "-Bin", "fluxvm"]),
                stdin: Some("{{password}}".into()),
                stdout_to: Some(auth.clone()),
            });
            plan.steps.push(Step::Run {
                what: "start the registry".into(),
                argv: argv(&[
                    "docker", "run", "-d", "--name", "fluxvm-registry", "--restart", "unless-stopped",
                    "-p", &format!("{port}:5000"),
                    "-v", "fluxvm-registry-data:/var/lib/registry",
                    "-v", &format!("{rd}:/auth:ro"),
                    "-e", "REGISTRY_AUTH=htpasswd",
                    "-e", "REGISTRY_AUTH_HTPASSWD_REALM=FluxVM",
                    "-e", "REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd",
                    "registry:2",
                ]),
                stdin: None,
                stdout_to: None,
            });
            plan.url = format!("localhost:{port}");
            plan.defaults.push(("user".into(), "fluxvm".into()));
            plan.defaults.push(("login".into(), format!("fluxctl recipe credentials registry | docker login localhost:{port} -u fluxvm --password-stdin")));
            plan.backup_schedule = "weekly, Sunday 03:00".into();
            plan.backup_command = "docker run --rm -v fluxvm-registry-data:/data -v \"$HOME/backups\":/out alpine tar czf /out/registry-$(date +%F).tgz -C /data .".into();
            plan.uninstall = vec![
                argv(&["docker", "rm", "-f", "fluxvm-registry"]),
                argv(&["docker", "volume", "rm", "fluxvm-registry-data"]),
            ];
            plan.notes.push("Plain HTTP on this machine: Docker allows that for localhost. Put TLS in front (docs/recipes.md) before using it from other machines.".into());
        }
        "monitoring" => {
            plan.steps.push(Step::Run {
                what: "start Grafana in the local cluster".into(),
                argv: argv(&["fluxctl", "kube", "add", "grafana"]),
                stdin: None,
                stdout_to: None,
            });
            plan.url = "http://localhost:3000".into();
            plan.defaults.push(("user".into(), "admin".into()));
            plan.defaults.push(("password".into(), "admin (the development default: change it at first sign-in)".into()));
            plan.backup_schedule = "weekly, Sunday 03:00".into();
            plan.backup_command = "export dashboards from Grafana (Share, Export); the volume is kept on `kube remove` without --delete-data".into();
            plan.uninstall = vec![argv(&["fluxctl", "kube", "remove", "grafana", "--delete-data"])];
            plan.notes.push("Fleet metrics to scrape: docs/fleet-metrics.md. Needs the local cluster: `fluxctl kube up` once.".into());
        }
        _ => unreachable!("every catalog entry has a renderer"),
    }
    Ok(plan)
}

impl Plan {
    /// The plan as text for `show`, Studio and the golden files. Contains no secret: generated ones appear by name only.
    pub fn text(&self, home: &Path) -> String {
        let mut s = format!("{}\n", self.title);
        s.push_str("\nSteps\n");
        for (i, st) in self.steps.iter().enumerate() {
            match st {
                Step::Run { what, argv, stdin, stdout_to } => {
                    s.push_str(&format!("  {}. {what}\n       {}", i + 1, shell_line(argv)));
                    if stdin.is_some() {
                        s.push_str("   (secret on stdin)");
                    }
                    if let Some(p) = stdout_to {
                        s.push_str(&format!("   > {p}"));
                    }
                    s.push('\n');
                }
                Step::WriteFile { what, path, .. } => {
                    s.push_str(&format!("  {}. {what}\n       write {path} (mode 0600)\n", i + 1));
                }
            }
        }
        s.push_str(&format!("\nOpen it      {}\n", self.url));
        s.push_str("Credentials\n");
        let cf = dir_for(home, &self.name).join("credentials");
        if self.generated.iter().all(|g| g == "github_token") && self.defaults.is_empty() {
            s.push_str("  none\n");
        }
        for (k, v) in &self.defaults {
            s.push_str(&format!("  {k}: {v}\n"));
        }
        let gen_: Vec<&str> = self.generated.iter().map(String::as_str).filter(|g| *g != "github_token").collect();
        if !gen_.is_empty() {
            s.push_str(&format!(
                "  generated ({}), kept in {} (mode 0600); show with `fluxctl recipe credentials {}`\n",
                gen_.join(", "),
                cf.display(),
                self.name
            ));
        }
        s.push_str(&format!("Backup       {}\n             {}\n", self.backup_schedule, self.backup_command));
        s.push_str("Uninstall\n");
        for u in &self.uninstall {
            s.push_str(&format!("  {}\n", shell_line(u)));
        }
        for n in &self.notes {
            s.push_str(&format!("Note         {n}\n"));
        }
        s
    }

    /// The commands as one pasteable block ("Copy as fluxctl").
    pub fn as_commands(&self) -> String {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Run { argv, stdin: None, stdout_to: None, .. } => Some(shell_line(argv)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// An argv as a shell line, quoting what needs it.
pub fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn random_secret() -> String {
    // 24 characters from 32 symbols: 120 bits.
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let b = {
        use std::io::Read;
        let mut b = [0u8; 24];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut b))
            .expect("/dev/urandom");
        b
    };
    b.iter().map(|x| A[(*x & 31) as usize] as char).collect()
}

fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(d) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_private(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).with_context(|| format!("{} is missing", path.display()))?;
    if m.mode() & 0o077 != 0 {
        bail!("{} is readable by group or others; run chmod 600 on it", path.display());
    }
    Ok(std::fs::read_to_string(path)?)
}

/// Parses the credentials file (`key=value` lines).
pub fn parse_credentials(raw: &str) -> BTreeMap<String, String> {
    raw.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn fill(template: &str, creds: &BTreeMap<String, String>) -> Result<String> {
    let mut out = template.to_owned();
    while let Some(i) = out.find("{{") {
        let j = out[i..].find("}}").context("unterminated {{")? + i;
        let key = out[i + 2..j].to_owned();
        let v = creds.get(&key).with_context(|| format!("no stored credential {key}"))?;
        out.replace_range(i..j + 2, v);
    }
    Ok(out)
}

/// What executes a [`Step::Run`]. Tests use a recorder; the CLI uses the real thing.
pub trait Runner {
    fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<Vec<u8>>;
}

pub struct Process;
impl Runner for Process {
    fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<Vec<u8>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let exe = if argv[0] == "fluxctl" {
            std::env::current_exe().context("finding fluxctl")?
        } else {
            PathBuf::from(&argv[0])
        };
        let mut c = Command::new(exe);
        c.args(&argv[1..]);
        c.stderr(Stdio::inherit());
        let capture = stdin.is_some();
        c.stdin(if capture { Stdio::piped() } else { Stdio::inherit() });
        c.stdout(Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("starting {}", argv[0]))?;
        if let Some(s) = stdin {
            child.stdin.take().context("stdin")?.write_all(s.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("{} failed ({})", shell_line(argv), out.status);
        }
        Ok(out.stdout)
    }
}

/// Runs the plan: credentials first (0600), then each step. Returns what to tell the person; never a secret.
pub fn execute(plan: &Plan, home: &Path, runner: &mut dyn Runner) -> Result<String> {
    let dir = dir_for(home, &plan.name);
    let cred_path = dir.join("credentials");
    let mut creds = if cred_path.exists() {
        parse_credentials(&read_private(&cred_path)?)
    } else {
        BTreeMap::new()
    };
    // A GitHub token is the person's: it must already be there. Everything else is generated once and kept.
    for g in &plan.generated {
        if g == "github_token" {
            let tf = dir.join("github.token");
            let t = read_private(&tf).with_context(|| {
                format!("save a GitHub token that may create runners in {} (chmod 600) first", tf.display())
            })?;
            creds.insert(g.clone(), t.trim().to_owned());
        } else if !creds.contains_key(g) {
            creds.insert(g.clone(), random_secret());
        }
    }
    let keep: BTreeMap<_, _> = creds
        .iter()
        .filter(|(k, _)| k.as_str() != "github_token")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !keep.is_empty() || !plan.defaults.is_empty() {
        let mut body = String::new();
        for (k, v) in &plan.defaults {
            body.push_str(&format!("{k}={v}\n"));
        }
        for (k, v) in &keep {
            body.push_str(&format!("{k}={v}\n"));
        }
        write_private(&cred_path, body.as_bytes())?;
    }
    for st in &plan.steps {
        match st {
            Step::WriteFile { path, template, .. } => {
                write_private(Path::new(path), fill(template, &creds)?.as_bytes())?;
            }
            Step::Run { argv, stdin, stdout_to, .. } => {
                let stdin = stdin.as_deref().map(|t| fill(t, &creds)).transpose()?;
                let out = runner.run(argv, stdin.as_deref())?;
                if let Some(p) = stdout_to {
                    write_private(Path::new(p), &out)?;
                }
            }
        }
    }
    let mut msg = format!("{} is ready.\n  open: {}\n", plan.title, plan.url);
    if !plan.generated.iter().all(|g| g == "github_token") || !plan.defaults.is_empty() {
        msg.push_str(&format!(
            "  credentials: {} (mode 0600); `fluxctl recipe credentials {}` prints them\n",
            cred_path.display(),
            plan.name
        ));
    }
    msg.push_str(&format!("  backup: {}\n    {}\n", plan.backup_schedule, plan.backup_command));
    msg.push_str(&format!("  undo: fluxctl recipe uninstall {}\n", plan.name));
    Ok(msg)
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

pub fn run(cmd: RecipeCommand) -> Result<()> {
    let home = home_dir()?;
    let map = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeMap<_, _>>();
    match cmd {
        RecipeCommand::List => {
            for i in CATALOG {
                println!("{:<14} {:<28} {}", i.name, i.title, i.summary);
            }
            println!("\nfluxctl recipe show NAME     what it would do\nfluxctl recipe run NAME      do it");
        }
        RecipeCommand::Show { name, set, json } => {
            let i = info(&name);
            let p = render(&name, &map(set), &home);
            if json {
                let i = i.with_context(|| format!("no recipe named {name}"))?;
                let p = p.ok();
                println!("{}", serde_json::to_string_pretty(&plan_json(i, p.as_ref(), &home))?);
            } else {
                match p {
                    Ok(p) => print!("{}", p.text(&home)),
                    Err(e) => {
                        if let Some(i) = i {
                            println!("{}: {}\n", i.title, i.summary);
                            for (k, d, h) in i.params {
                                println!("  --set {k}={}   {h}", if d.is_empty() { "<required>" } else { d });
                            }
                            println!();
                        }
                        return Err(e);
                    }
                }
            }
        }
        RecipeCommand::Run { name, set, dry_run } => {
            let p = render(&name, &map(set), &home)?;
            if dry_run {
                print!("{}", p.text(&home));
                return Ok(());
            }
            print!("{}", execute(&p, &home, &mut Process)?);
        }
        RecipeCommand::Credentials { name } => {
            info(&name).with_context(|| format!("no recipe named {name}"))?;
            let path = dir_for(&home, &name).join("credentials");
            // The registry login line pipes this, so a bare password is printed when that is all there is.
            let creds = parse_credentials(&read_private(&path).with_context(|| {
                format!("{name} has no stored credentials; run it first")
            })?);
            if creds.len() == 1 {
                println!("{}", creds.values().next().expect("one"));
            } else if let Some(pw) = creds.get("password").filter(|_| name == "registry") {
                println!("{pw}");
            } else {
                for (k, v) in creds {
                    println!("{k}={v}");
                }
            }
        }
        RecipeCommand::Uninstall { name, set, keep_data, dry_run } => {
            let p = render(&name, &map(set), &home)?;
            for u in &p.uninstall {
                let mut u = u.clone();
                if keep_data {
                    u.retain(|a| a != "--delete-data");
                    if u.get(1).map(String::as_str) == Some("volume") {
                        continue;
                    }
                }
                println!("$ {}", shell_line(&u));
                if !dry_run {
                    Process.run(&u, None)?;
                }
            }
        }
    }
    Ok(())
}

/// The plan as JSON for Studio. Secrets are not in a plan, so none are here.
pub fn plan_json(i: &Info, p: Option<&Plan>, home: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": i.name, "title": i.title, "summary": i.summary,
        "params": i.params.iter().map(|(k, d, h)| serde_json::json!({"key": k, "default": d, "help": h})).collect::<Vec<_>>(),
        "plan": p.map(|p| serde_json::json!({
            "text": p.text(home), "commands": p.as_commands(), "url": p.url,
            "backup_schedule": p.backup_schedule, "backup_command": p.backup_command,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    const HOME: &str = "/home/demo";

    fn golden(name: &str, got: &str) {
        let path = format!("{}/src/recipes_golden/{name}.txt", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, got).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(got, want, "golden file {path} differs; rerun with UPDATE_GOLDEN=1 to accept");
    }

    #[test]
    fn every_recipe_renders_and_matches_its_golden_file() {
        let cases: &[(&str, &str, &[(&str, &str)])] = &[
            ("dev-database", "dev-database", &[]),
            ("dev-database", "dev-database-mysql", &[("engine", "mysql")]),
            ("website", "website", &[("dir", "/srv/site")]),
            ("ci-runner", "ci-runner-linux", &[("repo", "acme/app")]),
            ("ci-runner", "ci-runner-macos", &[("repo", "acme/app"), ("os", "macos"), ("template", "registry.example/ci-mac:1")]),
            ("registry", "registry", &[]),
            ("monitoring", "monitoring", &[]),
        ];
        for (name, file, kv) in cases {
            let p = render(name, &set(kv), Path::new(HOME)).unwrap();
            golden(file, &p.text(Path::new(HOME)));
        }
        assert_eq!(CATALOG.len(), 5);
    }

    #[test]
    fn required_parameters_and_typos_are_explained() {
        let e = render("website", &set(&[]), Path::new(HOME)).unwrap_err().to_string();
        assert!(e.contains("--set dir="), "{e}");
        let e = render("website", &set(&[("dri", "x")]), Path::new(HOME)).unwrap_err().to_string();
        assert!(e.contains("no parameter dri") && e.contains("dir, port"), "{e}");
        let e = render("nope", &set(&[]), Path::new(HOME)).unwrap_err().to_string();
        assert!(e.contains("dev-database, website"), "{e}");
        assert!(render("dev-database", &set(&[("engine", "oracle")]), Path::new(HOME)).is_err());
        assert!(render("website", &set(&[("dir", "a:b")]), Path::new(HOME)).is_err());
        assert!(render("registry", &set(&[("port", "99999")]), Path::new(HOME)).is_err());
        assert!(render("ci-runner", &set(&[("repo", "acme")]), Path::new(HOME)).is_err());
    }

    #[derive(Default)]
    struct Rec(Vec<(Vec<String>, Option<String>)>);
    impl Runner for Rec {
        fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<Vec<u8>> {
            self.0.push((argv.to_vec(), stdin.map(str::to_owned)));
            Ok(b"fluxvm:$2y$hash\n".to_vec())
        }
    }

    #[test]
    fn a_generated_secret_is_stored_private_and_never_on_a_command_line_or_in_the_output() {
        use std::os::unix::fs::MetadataExt;
        let home = tempfile::tempdir().unwrap();
        let p = render("registry", &set(&[]), home.path()).unwrap();
        let mut r = Rec::default();
        let msg = execute(&p, home.path(), &mut r).unwrap();
        let creds = dir_for(home.path(), "registry").join("credentials");
        let raw = std::fs::read_to_string(&creds).unwrap();
        let pw = parse_credentials(&raw)["password"].clone();
        assert_eq!(pw.len(), 24);
        assert_eq!(std::fs::metadata(&creds).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(creds.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
        // The password reached the hasher through stdin only.
        assert_eq!(r.0[0].1.as_deref(), Some(pw.as_str()));
        assert!(r.0.iter().all(|(argv, _)| argv.iter().all(|a| !a.contains(&pw))));
        // The hashed result was saved private, and the report never shows the secret.
        let htpasswd = dir_for(home.path(), "registry").join("htpasswd");
        assert_eq!(std::fs::metadata(&htpasswd).unwrap().mode() & 0o777, 0o600);
        assert!(!msg.contains(&pw) && !p.text(home.path()).contains(&pw));
        // Running again keeps the same password (idempotent, so a restart does not lock clients out).
        execute(&p, home.path(), &mut Rec::default()).unwrap();
        assert_eq!(parse_credentials(&std::fs::read_to_string(&creds).unwrap())["password"], pw);
    }

    #[test]
    fn the_ci_runner_needs_the_persons_token_file_and_keeps_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let p = render("ci-runner", &set(&[("repo", "acme/app")]), home.path()).unwrap();
        let e = execute(&p, home.path(), &mut Rec::default()).unwrap_err();
        assert!(format!("{e:#}").contains("github.token"), "{e:#}");
        let tf = dir_for(home.path(), "ci-runner").join("github.token");
        std::fs::create_dir_all(tf.parent().unwrap()).unwrap();
        std::fs::write(&tf, "ghp_SECRET\n").unwrap();
        std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(execute(&p, home.path(), &mut Rec::default()).is_err(), "a world-readable token is refused");
        std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut r = Rec::default();
        let msg = execute(&p, home.path(), &mut r).unwrap();
        let env = std::fs::read_to_string(dir_for(home.path(), "ci-runner").join("runner.env")).unwrap();
        assert!(env.contains("ACCESS_TOKEN=ghp_SECRET"));
        assert!(!msg.contains("ghp_SECRET"));
        assert!(r.0.iter().all(|(a, _)| a.iter().all(|x| !x.contains("ghp_SECRET"))));
        // The token is not copied into the credentials file either.
        assert!(!dir_for(home.path(), "ci-runner").join("credentials").exists());
    }

    #[test]
    fn shell_lines_are_quoted() {
        assert_eq!(shell_line(&argv(&["a", "b c", "it's", ""])), "a 'b c' 'it'\\''s' ''");
    }

    #[test]
    fn copy_as_fluxctl_lists_only_commands_without_secrets() {
        let p = render("registry", &set(&[]), Path::new(HOME)).unwrap();
        let c = p.as_commands();
        assert!(c.contains("fluxvm-registry") && !c.contains("htpasswd"), "{c}");
        let p = render("monitoring", &set(&[]), Path::new(HOME)).unwrap();
        assert_eq!(p.as_commands(), "fluxctl kube add grafana");
    }
}
