//! `fluxctl kube git install`: the cluster follows your git branch. A `post-checkout` hook saves the cluster as a checkpoint
//! named after the branch you leave and puts back the one saved for the branch you enter, so each branch has its own
//! databases, migrations and deployed state. A branch with no checkpoint yet starts as a copy of where you were, as a new
//! git branch starts from the commit you were on. Only a running cluster is touched, and the hook never makes `git
//! checkout` fail.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::kube_checkpoint;
use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

/// Marks a hook this command wrote, so it is never mistaken for one the user wrote.
const MARKER: &str = "# fluxvm-kube-git";

/// The longest slug before its hash: `git-`, the slug, a dash and five hex digits must fit a checkpoint label (40).
const SLUG_MAX: usize = 28;

/// A checkpoint-safe name for a branch: lowercase letters, digits and single dashes. A branch whose name had to change (a
/// slash, capitals, a long name) gets a short hash of the real name, so `feature/x` and `feature-x` stay apart.
pub fn slug(branch: &str) -> String {
    let mut out = String::new();
    for c in branch.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    let lossless = out == branch && out.len() <= SLUG_MAX;
    if lossless {
        return out;
    }
    out.truncate(SLUG_MAX);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push('x');
    }
    format!("{out}-{}", short_hash(branch))
}

/// Five hex digits of the branch name's SHA-256.
fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(3)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..5]
        .to_owned()
}

/// The checkpoint that holds a branch's cluster state.
pub fn label(branch: &str) -> String {
    format!("git-{}", slug(branch))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Switch {
    /// The branch whose state the cluster holds now, to be saved before leaving it.
    pub save: Option<String>,
    /// The branch whose saved state to put back.
    pub restore: Option<String>,
}

/// What a checkout to `to` does, given the branch the cluster's state belongs to (`recorded`) and which checkpoints exist.
pub fn plan(recorded: Option<&str>, to: &str, has: impl Fn(&str) -> bool) -> Switch {
    if recorded == Some(to) {
        return Switch {
            save: None,
            restore: None,
        };
    }
    Switch {
        save: recorded.map(str::to_owned),
        restore: has(&label(to)).then(|| to.to_owned()),
    }
}

/// Which branch each repository's cluster state belongs to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct State(pub BTreeMap<String, String>);

impl State {
    pub fn parse(text: &str) -> State {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        State(
            v["repos"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, b)| Some((k.clone(), b.as_str()?.to_owned())))
                .collect(),
        )
    }

    pub fn render(&self) -> String {
        json!({ "repos": self.0 }).to_string()
    }
}

fn state_file(cluster: &str) -> PathBuf {
    kube_host::run_dir().join(format!("kube-{cluster}.git.json"))
}

fn read_state(cluster: &str) -> State {
    State::parse(&std::fs::read_to_string(state_file(cluster)).unwrap_or_default())
}

/// `'text'` for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `post-checkout` hook. Git passes the old HEAD, the new HEAD and 1 for a branch checkout (0 for a file checkout).
pub fn hook_script(fluxctl: &Path, cluster: &str) -> String {
    format!(
        "#!/bin/sh\n{MARKER}\n# Written by `fluxctl kube git install`: the cluster follows the branch. Remove it with `fluxctl kube git uninstall`.\n\
         [ \"$3\" = \"1\" ] || exit 0\n\
         {} kube git hook --name {} >&2 || true\n\
         exit 0\n",
        sh_quote(&fluxctl.display().to_string()),
        sh_quote(cluster)
    )
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn hook_path() -> Result<PathBuf> {
    let p = git(&["rev-parse", "--git-path", "hooks/post-checkout"])
        .context("this is not a git repository")?;
    Ok(PathBuf::from(p))
}

pub fn install(fluxctl: &Path, cluster: &str, force: bool) -> Result<String> {
    if !kube_host::name_ok(cluster) {
        bail!("cluster name {cluster:?} is not a Kubernetes name");
    }
    let path = hook_path()?;
    if let Ok(old) = std::fs::read_to_string(&path)
        && !old.contains(MARKER)
        && !force
    {
        bail!(
            "{} already has a post-checkout hook that fluxctl did not write. Add this line to it, or use --force to replace it:\n  {} kube git hook --name {} >&2 || true",
            path.display(),
            sh_quote(&fluxctl.display().to_string()),
            sh_quote(cluster)
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, hook_script(fluxctl, cluster))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(format!(
        "installed {}\nFrom now on `git checkout BRANCH` saves this cluster's state for the branch you leave and restores the one for the branch you enter (about 10 s when it has one).",
        path.display()
    ))
}

pub fn uninstall() -> Result<String> {
    let path = hook_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) if t.contains(MARKER) => {
            std::fs::remove_file(&path)?;
            Ok(format!("removed {}", path.display()))
        }
        Ok(_) => bail!(
            "{} was not written by `kube git install`; leaving it alone",
            path.display()
        ),
        Err(_) => bail!("no post-checkout hook is installed"),
    }
}

/// The hook's work. Everything it prints goes to the terminal that ran `git checkout`; it returns Ok whatever happens, so
/// a problem with the cluster never makes a checkout fail.
pub async fn hook<A: VmApi>(api: &A, o: &KubeOptions) -> Result<()> {
    if let Err(e) = switch(api, o).await {
        eprintln!("fluxctl kube git: {e:#}");
    }
    Ok(())
}

async fn switch<A: VmApi>(api: &A, o: &KubeOptions) -> Result<()> {
    let Some(repo) = git(&["rev-parse", "--show-toplevel"]) else {
        return Ok(());
    };
    // Detached HEAD (a tag, a rebase in progress) is not a branch: leave the cluster alone.
    let Some(to) = git(&["symbolic-ref", "--short", "-q", "HEAD"]) else {
        return Ok(());
    };
    let (_, status) = match kube_checkpoint::server(api, o).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fluxctl kube git: the cluster stays as it is ({e:#})");
            return Ok(());
        }
    };
    if status != "running" || kube_checkpoint::is_asleep(api, o).await {
        eprintln!(
            "fluxctl kube git: {} is not running, so its state stays as it is (start it with `fluxctl kube up --name {}`)",
            o.name, o.name
        );
        return Ok(());
    }
    let mut state = read_state(&o.name);
    let existing: Vec<String> = kube_checkpoint::list(api, o).await?["checkpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["label"].as_str().map(str::to_owned))
        .collect();
    let recorded = state.0.get(&repo).cloned();
    let step = plan(recorded.as_deref(), &to, |l| {
        existing.iter().any(|e| e == l)
    });
    if let Some(from) = &step.save {
        let l = label(from);
        if existing.iter().any(|e| *e == l) {
            kube_checkpoint::remove(api, o, &l).await?;
        }
        eprintln!("fluxctl kube git: saving the cluster for {from}");
        kube_checkpoint::checkpoint(api, o, &l)
            .await
            .context("could not save the cluster, so it is left as it is")?;
    }
    if let Some(back) = &step.restore {
        eprintln!("fluxctl kube git: putting back the cluster for {back}");
        kube_checkpoint::restore(api, o, &label(back)).await?;
    } else if recorded.as_deref() != Some(&to) {
        eprintln!(
            "fluxctl kube git: {to} has no saved cluster yet; it starts from the current one"
        );
    }
    state.0.insert(repo, to);
    kube_host::write_private(&state_file(&o.name), &state.render())?;
    Ok(())
}

/// `kube git status`
pub async fn status<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    let repo = git(&["rev-parse", "--show-toplevel"]).context("this is not a git repository")?;
    let hook = hook_path()?;
    let installed = std::fs::read_to_string(&hook)
        .map(|t| t.contains(MARKER))
        .unwrap_or(false);
    let state = read_state(&o.name);
    let saved: Vec<String> = kube_checkpoint::list(api, o).await?["checkpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["label"].as_str())
        .filter(|l| l.starts_with("git-"))
        .map(str::to_owned)
        .collect();
    Ok(format!(
        "hook: {}\ncluster {} holds the state of: {}\nsaved branches: {}",
        if installed {
            "installed"
        } else {
            "not installed (`fluxctl kube git install`)"
        },
        o.name,
        state
            .0
            .get(&repo)
            .map(String::as_str)
            .unwrap_or("(nothing recorded yet)"),
        if saved.is_empty() {
            "none".to_owned()
        } else {
            saved.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_branch_names_keep_their_name() {
        assert_eq!(slug("main"), "main");
        assert_eq!(slug("fix-123"), "fix-123");
        assert_eq!(label("main"), "git-main");
    }

    #[test]
    fn changed_names_get_a_hash_so_they_cannot_collide() {
        let a = slug("feature/login");
        let b = slug("feature-login");
        assert_ne!(a, b);
        assert!(
            a.starts_with("feature-login-") && a.len() == "feature-login-".len() + 5,
            "{a}"
        );
        // The same name always gives the same slug.
        assert_eq!(slug("feature/login"), a);
        assert_ne!(slug("Main"), slug("main"));
    }

    #[test]
    fn every_label_is_a_valid_checkpoint_label() {
        for b in [
            "main",
            "feature/login",
            "Feature_X",
            "release/2026.10",
            "a".repeat(200).as_str(),
            "///",
            "ünï/cödé",
            "-lead",
            "trail-",
        ] {
            let l = label(b);
            assert!(kube_checkpoint::label_ok(&l), "{b:?} -> {l:?}");
        }
    }

    #[test]
    fn a_checkout_saves_the_branch_left_and_restores_the_one_entered_if_saved() {
        let saved = |l: &str| l == "git-b";
        // main -> b: save main, restore b.
        assert_eq!(
            plan(Some("main"), "b", saved),
            Switch {
                save: Some("main".into()),
                restore: Some("b".into())
            }
        );
        // main -> new branch: save main, nothing to restore, the new branch starts from here.
        assert_eq!(
            plan(Some("main"), "new", saved),
            Switch {
                save: Some("main".into()),
                restore: None
            }
        );
        // Same branch again (a file checkout or `git checkout main` on main): nothing.
        assert_eq!(
            plan(Some("main"), "main", saved),
            Switch {
                save: None,
                restore: None
            }
        );
        // First ever checkout: nothing to save.
        assert_eq!(
            plan(None, "b", saved),
            Switch {
                save: None,
                restore: Some("b".into())
            }
        );
    }

    #[test]
    fn the_state_file_round_trips_and_survives_garbage() {
        let mut s = State::default();
        s.0.insert("/work/app".into(), "feature/x".into());
        assert_eq!(State::parse(&s.render()), s);
        assert_eq!(State::parse(""), State::default());
        assert_eq!(State::parse("not json"), State::default());
        assert_eq!(State::parse(r#"{"repos":{"/a":1}}"#), State::default());
    }

    #[test]
    fn the_hook_only_acts_on_branch_checkouts_and_never_fails_git() {
        let h = hook_script(Path::new("/opt/homebrew/bin/fluxctl"), "dev");
        assert!(h.starts_with("#!/bin/sh\n"));
        assert!(h.contains(MARKER));
        assert!(h.contains("[ \"$3\" = \"1\" ] || exit 0"));
        assert!(h.contains("'/opt/homebrew/bin/fluxctl' kube git hook --name 'dev' >&2 || true"));
        assert!(h.trim_end().ends_with("exit 0"));
        // A path with a quote cannot break out of the script.
        let h = hook_script(Path::new("/tmp/it's/fluxctl"), "dev");
        assert!(h.contains(r"'/tmp/it'\''s/fluxctl'"), "{h}");
    }
}
