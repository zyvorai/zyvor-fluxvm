// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Build secrets and SSH agent forwarding for `fluxctl mac build` (`--secret`, `--ssh`, `RUN --mount=...`).
//!
//! A secret reaches the guest only for the one `RUN` that mounts it: the file is copied in with mode 600, the command runs,
//! and the file is removed before the step ends (and before the build cache snapshots the disk). The value is never part of a
//! command line, a plan, a cache key, an image history line or a message of ours. `--ssh default` forwards the host's agent
//! with `ssh -A` for the mounting step only, so no private key is copied anywhere.
//!
//! Not verified against a real macOS guest: tested with fakes and temporary files.

use crate::mac_build::{Mount, Step};
use crate::mac_errors::coded;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where secrets without a `target=` land in the guest. Removed after each step.
pub const SECRET_DIR: &str = "/private/tmp/fluxvm-secrets";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Env(String),
}

/// The `--secret` values of one build: names to local files. Environment sources are written to private temporary files, which
/// are deleted when this is dropped.
#[derive(Debug, Default)]
pub struct Secrets {
    files: BTreeMap<String, PathBuf>,
    temp: Vec<PathBuf>,
}

impl Drop for Secrets {
    fn drop(&mut self) {
        for t in &self.temp {
            std::fs::remove_file(t).ok();
        }
    }
}

/// `id=NAME,src=PATH` or `id=NAME,env=VAR` (also `source=`), as Docker spells it.
pub fn parse_flag(spec: &str) -> Result<(String, Source)> {
    let bad = |why: &str| {
        coded(
            format!(
                "--secret {spec:?}: {why}. Write it as --secret id=NAME,src=/path/to/file (or id=NAME,env=VARIABLE)"
            ),
            "MAC_BUILD_FLAG_INVALID",
        )
    };
    let (mut id, mut src, mut env) = (None, None, None);
    for kv in spec.split(',') {
        match kv.split_once('=') {
            Some(("id", v)) => id = Some(v),
            Some(("src" | "source", v)) => src = Some(v),
            Some(("env", v)) => env = Some(v),
            _ => return Err(bad(&format!("'{kv}' is not understood"))),
        }
    }
    let id = id
        .filter(|i| {
            !i.is_empty()
                && i.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        })
        .ok_or_else(|| bad("id=NAME is missing (letters, digits, _ . -)"))?;
    match (src, env) {
        (Some(p), None) if !p.is_empty() => Ok((id.to_string(), Source::File(p.into()))),
        (None, Some(v)) if !v.is_empty() => Ok((id.to_string(), Source::Env(v.to_string()))),
        (Some(_), Some(_)) => Err(bad("give src= or env=, not both")),
        _ => Err(bad("src=PATH or env=VARIABLE is missing")),
    }
}

impl Secrets {
    /// Read every `--secret` flag, checking now (before any VM is created) that each source exists. `env` looks variables up.
    pub fn from_flags(flags: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Secrets> {
        let mut s = Secrets::default();
        for f in flags {
            let (id, src) = parse_flag(f)?;
            if s.files.contains_key(&id) {
                return Err(coded(
                    format!("--secret id={id} was given twice. Keep one of them"),
                    "MAC_BUILD_FLAG_INVALID",
                ));
            }
            let path = match src {
                Source::File(p) => {
                    if !p.is_file() {
                        return Err(coded(
                            format!(
                                "--secret id={id}: {} is not a readable file. Check the path, or use env=VARIABLE to read it from the environment",
                                p.display()
                            ),
                            "MAC_SECRET_MISSING",
                        ));
                    }
                    p
                }
                Source::Env(var) => {
                    let Some(v) = env(&var).filter(|v| !v.is_empty()) else {
                        return Err(coded(
                            format!(
                                "--secret id={id}: the environment variable {var} is not set or is empty. Export it in this shell and run the build again"
                            ),
                            "MAC_SECRET_MISSING",
                        ));
                    };
                    let p = s.write_temp(&id, &v)?;
                    s.temp.push(p.clone());
                    p
                }
            };
            s.files.insert(id, path);
        }
        Ok(s)
    }

    #[cfg(unix)]
    fn write_temp(&self, id: &str, value: &str) -> Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut n = 0u32;
        loop {
            let p = std::env::temp_dir().join(format!(
                "fluxvm-secret-{}-{}-{n}",
                std::process::id(),
                id.replace(['/', '.'], "_")
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&p)
            {
                Ok(mut f) => {
                    f.write_all(value.as_bytes())?;
                    return Ok(p);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e).context("writing a private temporary file for --secret"),
            }
        }
    }

    #[cfg(not(unix))]
    fn write_temp(&self, _id: &str, _value: &str) -> Result<PathBuf> {
        bail!("--secret env= needs a Unix host")
    }

    pub fn path(&self, id: &str) -> Option<&Path> {
        self.files.get(id).map(PathBuf::as_path)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// `text` with every secret value replaced by `***`. For our own error text, as a last guard; values of 4 bytes or fewer
    /// are not masked because they would mangle ordinary words.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for p in self.files.values() {
            if let Ok(v) = std::fs::read_to_string(p) {
                let v = v.trim_end_matches(['\n', '\r']);
                if v.len() > 4 {
                    out = out.replace(v, "***");
                }
            }
        }
        out
    }
}

/// `--ssh default`: the only form that makes sense for a guest that reaches the network through this Mac.
pub fn parse_ssh(flags: &[String]) -> Result<bool> {
    for f in flags {
        if f != "default" {
            return Err(coded(
                format!(
                    "--ssh {f:?}: only --ssh default is supported. It forwards the SSH agent this shell is using (SSH_AUTH_SOCK)"
                ),
                "MAC_BUILD_FLAG_INVALID",
            ));
        }
    }
    Ok(!flags.is_empty())
}

/// Before anything is created: every mount in the plan has what it needs, and flags nothing uses are called out.
/// Returns warnings (unused `--secret`/`--ssh`). `auth_sock` is `SSH_AUTH_SOCK`.
pub fn preflight(
    steps: &[Step],
    secrets: &Secrets,
    ssh: bool,
    auth_sock: Option<&str>,
) -> Result<Vec<String>> {
    let mut used_secret = Vec::new();
    let mut used_ssh = false;
    for s in steps {
        let Step::Run { line, mounts, .. } = s else {
            continue;
        };
        for m in mounts {
            match m {
                Mount::Secret { id, .. } => {
                    if secrets.path(id).is_none() {
                        return Err(coded(
                            format!(
                                "line {line}: RUN --mount=type=secret,id={id} needs a secret, but none named {id} was given. Add --secret id={id},src=FILE (or env=VARIABLE) to the build command"
                            ),
                            "MAC_SECRET_MISSING",
                        ));
                    }
                    used_secret.push(id.as_str());
                }
                Mount::Ssh => {
                    used_ssh = true;
                    if !ssh {
                        return Err(coded(
                            format!(
                                "line {line}: RUN --mount=type=ssh needs the SSH agent. Add --ssh default to the build command"
                            ),
                            "MAC_SSH_AGENT_MISSING",
                        ));
                    }
                }
            }
        }
    }
    if used_ssh && !auth_sock.is_some_and(|s| !s.is_empty() && Path::new(s).exists()) {
        return Err(coded(
            "--ssh default: no SSH agent is running in this shell (SSH_AUTH_SOCK is unset or stale). Start one with `eval $(ssh-agent)` and `ssh-add`, then run the build again",
            "MAC_SSH_AGENT_MISSING",
        ));
    }
    let mut warns = Vec::new();
    for id in secrets.ids() {
        if !used_secret.contains(&id) {
            warns.push(format!(
                "--secret id={id} is not used: no RUN has --mount=type=secret,id={id}. It was not sent to the guest"
            ));
        }
    }
    if ssh && !used_ssh {
        warns.push(
            "--ssh default is not used: no RUN has --mount=type=ssh. The agent was not forwarded"
                .into(),
        );
    }
    Ok(warns)
}

/// The guest side of a mounted step: copy a file in, run a command (optionally with the SSH agent forwarded).
pub trait MountIo {
    async fn put(&mut self, user: &str, local: &Path, dst: &str) -> Result<()>;
    async fn run(&mut self, user: &str, command: &str, forward_agent: bool) -> Result<()>;
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run `command` with `mounts`: put each secret in the guest (mode 600), run, then always remove them. A secret that cannot be
/// removed fails the build, because the disk would be stored with it.
pub async fn run_mounted<H: MountIo>(
    h: &mut H,
    secrets: &Secrets,
    user: &str,
    command: &str,
    mounts: &[Mount],
) -> Result<()> {
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    let mut agent = false;
    for m in mounts {
        match m {
            Mount::Secret { id, target } => {
                let t = target
                    .clone()
                    .unwrap_or_else(|| format!("{SECRET_DIR}/{id}"));
                let Some(local) = secrets.path(id) else {
                    bail!("no --secret id={id} was given");
                };
                targets.push((t, local.to_path_buf()));
            }
            Mount::Ssh => agent = true,
        }
    }
    let mut exports = String::new();
    for (m, (t, _)) in mounts
        .iter()
        .filter(|m| matches!(m, Mount::Secret { .. }))
        .zip(&targets)
    {
        if let Mount::Secret { id, .. } = m {
            // The variable names the file, never holds the value.
            let var: String = id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            exports.push_str(&format!("export FLUXVM_SECRET_{var}={}; ", sq(t)));
        }
    }
    let cleanup_cmd = (!targets.is_empty()).then(|| {
        let files: Vec<String> = targets.iter().map(|(t, _)| sq(t)).collect();
        format!(
            "rm -f -- {}; rmdir {} 2>/dev/null; true",
            files.join(" "),
            sq(SECRET_DIR)
        )
    });
    let mut result: Result<()> = Ok(());
    if !targets.is_empty() {
        let mut dirs: Vec<String> = targets
            .iter()
            .filter_map(|(t, _)| Path::new(t).parent().map(|p| sq(&p.to_string_lossy())))
            .collect();
        dirs.sort();
        dirs.dedup();
        result = h
            .run(
                user,
                &format!("umask 077; mkdir -p -- {}", dirs.join(" ")),
                false,
            )
            .await;
        for (t, local) in &targets {
            if result.is_err() {
                break;
            }
            result = h.put(user, local, t).await;
            if result.is_ok() {
                result = h.run(user, &format!("chmod 600 -- {}", sq(t)), false).await;
            }
        }
    }
    if result.is_ok() {
        result = h.run(user, &format!("{exports}{command}"), agent).await;
    }
    if let Some(c) = cleanup_cmd {
        let cleaned = h.run(user, &c, false).await;
        if let Err(e) = cleaned {
            if result.is_ok() {
                return Err(coded(
                    format!(
                        "the secret files could not be removed from the build instance ({e:#}). The image was not stored. Run the build again; if it keeps failing, run `fluxctl mac doctor`"
                    ),
                    "MAC_SECRET_CLEANUP",
                ));
            }
            eprintln!(
                "warning: the secret files may still be in the build instance, which is being deleted: {e:#}"
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        fail_on: Option<&'static str>,
        agent: Vec<bool>,
    }

    impl MountIo for Fake {
        async fn put(&mut self, _u: &str, local: &Path, dst: &str) -> Result<()> {
            self.log.push(format!("put {} -> {dst}", local.display()));
            Ok(())
        }
        async fn run(&mut self, _u: &str, command: &str, fwd: bool) -> Result<()> {
            self.log.push(format!("run {command}"));
            self.agent.push(fwd);
            if self.fail_on.is_some_and(|f| command.contains(f)) {
                bail!("boom");
            }
            Ok(())
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A value shaped like nothing real, built at run time.
    fn value() -> String {
        ["zq", "unit", "value", "7"].join("-")
    }

    fn secrets_with(id: &str) -> (Secrets, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("s.txt");
        std::fs::write(&f, format!("{}\n", value())).unwrap();
        let flag = format!("id={id},src={}", f.display());
        (Secrets::from_flags(&[flag], &|_| None).unwrap(), d)
    }

    fn secret(id: &str) -> Mount {
        Mount::Secret {
            id: id.into(),
            target: None,
        }
    }

    #[test]
    fn flags_parse_and_reject_plainly() {
        assert_eq!(
            parse_flag("id=npm,src=/x/y").unwrap(),
            ("npm".into(), Source::File("/x/y".into()))
        );
        assert_eq!(
            parse_flag("id=a.b,env=TOK").unwrap().1,
            Source::Env("TOK".into())
        );
        for bad in [
            "src=/x",
            "id=a",
            "id=a,src=/x,env=Y",
            "id=a/b,src=/x",
            "id=a,zz=1",
        ] {
            let e = format!("{:#}", parse_flag(bad).unwrap_err());
            assert!(
                e.contains("fluxctl explain MAC_BUILD_FLAG_INVALID"),
                "{bad}: {e}"
            );
        }
        assert!(parse_ssh(&["default".into()]).unwrap());
        assert!(!parse_ssh(&[]).unwrap());
        assert!(parse_ssh(&["other".into()]).is_err());
    }

    #[test]
    fn missing_sources_fail_before_a_build_and_never_print_values() {
        let e = Secrets::from_flags(&["id=a,src=/no/such/file".into()], &|_| None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_SECRET_MISSING") && e.contains("/no/such/file"),
            "{e}"
        );
        let e = Secrets::from_flags(&["id=a,env=NOPE".into()], &|_| None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("NOPE") && e.to_lowercase().contains("export"),
            "{e}"
        );
    }

    #[test]
    fn env_source_is_a_private_temp_file_removed_on_drop() {
        let v = value();
        let s = Secrets::from_flags(&["id=t,env=X".into()], &|_| Some(v.clone())).unwrap();
        let p = s.path("t").unwrap().to_path_buf();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), v);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(s);
        assert!(!p.exists());
    }

    #[test]
    fn preflight_checks_mounts_and_warns_about_unused_flags() {
        let (s, _d) = secrets_with("npm");
        let step = |m: Vec<Mount>| Step::Run {
            line: 3,
            user: None,
            command: "x".into(),
            mounts: m,
        };
        let e = preflight(&[step(vec![secret("other")])], &s, false, None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--secret id=other") && e.contains("MAC_SECRET_MISSING"),
            "{e}"
        );
        let e = preflight(&[step(vec![Mount::Ssh])], &s, false, None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--ssh default") && e.contains("MAC_SSH_AGENT_MISSING"),
            "{e}"
        );
        let e = preflight(&[step(vec![Mount::Ssh])], &s, true, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("ssh-agent"), "{e}");
        assert!(preflight(&[step(vec![Mount::Ssh])], &s, true, Some("/")).is_ok());
        let w = preflight(&[step(vec![])], &s, true, None).unwrap();
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn a_secret_is_put_used_and_removed_and_never_in_a_command() {
        let (s, _d) = secrets_with("npm");
        let mut h = Fake::default();
        block_on(run_mounted(
            &mut h,
            &s,
            "builder",
            "npm ci",
            &[secret("npm")],
        ))
        .unwrap();
        let all = h.log.join("\n");
        assert!(!all.contains(&value()), "{all}");
        let n = h.log.len();
        assert!(h.log[0].contains("umask 077; mkdir -p -- '/private/tmp/fluxvm-secrets'"));
        assert!(
            h.log[1].starts_with("put ") && h.log[1].ends_with("/private/tmp/fluxvm-secrets/npm")
        );
        assert!(h.log[2].contains("chmod 600"));
        assert!(
            h.log[3].contains("export FLUXVM_SECRET_NPM='/private/tmp/fluxvm-secrets/npm'; npm ci")
        );
        assert!(h.log[n - 1].contains("rm -f -- '/private/tmp/fluxvm-secrets/npm'"));
        assert!(h.agent.iter().all(|a| !a));
    }

    #[test]
    fn the_secret_is_removed_even_when_the_step_fails() {
        let (s, _d) = secrets_with("npm");
        let mut h = Fake {
            fail_on: Some("npm ci"),
            ..Default::default()
        };
        assert!(block_on(run_mounted(&mut h, &s, "u", "npm ci", &[secret("npm")])).is_err());
        assert!(h.log.last().unwrap().contains("rm -f --"));
    }

    #[test]
    fn a_secret_that_cannot_be_removed_fails_an_otherwise_good_step() {
        let (s, _d) = secrets_with("npm");
        let mut h = Fake {
            fail_on: Some("rm -f"),
            ..Default::default()
        };
        let e = block_on(run_mounted(&mut h, &s, "u", "true", &[secret("npm")]))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_SECRET_CLEANUP") && e.contains("not stored"),
            "{e}"
        );
    }

    #[test]
    fn ssh_forwards_the_agent_only_for_that_step_and_custom_targets_work() {
        let (s, _d) = secrets_with("k");
        let mut h = Fake::default();
        let m = [
            Mount::Secret {
                id: "k".into(),
                target: Some("/opt/keys/k.txt".into()),
            },
            Mount::Ssh,
        ];
        block_on(run_mounted(&mut h, &s, "u", "git clone x", &m)).unwrap();
        // `agent` records only the `run` calls, so index among those, not among every logged step.
        let i = h
            .log
            .iter()
            .filter(|l| l.starts_with("run "))
            .position(|l| l.contains("git clone x"))
            .unwrap();
        assert!(h.agent[i]);
        assert_eq!(h.agent.iter().filter(|a| **a).count(), 1);
        assert!(h.log[0].contains("'/opt/keys'"));
        block_on(run_mounted(
            &mut h,
            &Secrets::default(),
            "u",
            "git pull",
            &[Mount::Ssh],
        ))
        .unwrap();
        assert!(h.log.last().unwrap().contains("git pull") && *h.agent.last().unwrap());
    }

    #[test]
    fn redact_masks_values_in_our_own_text() {
        let (s, _d) = secrets_with("npm");
        let t = s.redact(&format!("failed with {} in it", value()));
        assert_eq!(t, "failed with *** in it");
    }
}
