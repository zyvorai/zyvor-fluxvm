// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Image configuration for `fluxctl mac`: what a Macfile's `ENV`, `WORKDIR`, `LABEL`, `EXPOSE`, `CMD`, `ENTRYPOINT` and
//! `HEALTHCHECK` record, stored as `config.json` next to the template's three files. Pure helpers (shell quoting, `$VAR`
//! expansion, the command a `run` sends over SSH) live here so they are unit tested without a VM.
//!
//! Not verified against a real macOS guest: the generated shell text is only checked as strings.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub const CONFIG_FILE: &str = "config.json";

/// A `CMD` / `ENTRYPOINT` in exec form (`["a","b"]`) or shell form (`a b`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmd {
    Exec(Vec<String>),
    Shell(String),
}

impl Cmd {
    /// Parse the text after `CMD` / `ENTRYPOINT`: a JSON array of strings is exec form, anything else shell form.
    pub fn parse(text: &str) -> std::result::Result<Cmd, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("needs a command".into());
        }
        if t.starts_with('[') {
            let v: Vec<String> = serde_json::from_str(t)
                .map_err(|e| format!("exec form must be a JSON array of strings: {e}"))?;
            if v.is_empty() {
                return Err("the exec-form array is empty".into());
            }
            return Ok(Cmd::Exec(v));
        }
        Ok(Cmd::Shell(t.to_string()))
    }

    /// The shell text that runs this command.
    pub fn to_shell(&self) -> String {
        match self {
            Cmd::Exec(v) => v.iter().map(|w| sq(w)).collect::<Vec<_>>().join(" "),
            Cmd::Shell(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    pub cmd: Cmd,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default)]
    pub created_unix: u64,
    pub created_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    /// In Macfile order; a later `ENV` of the same key wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Cmd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Cmd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
}

impl ImageConfig {
    pub fn set_env(&mut self, k: &str, v: &str) {
        match self.env.iter_mut().find(|(ek, _)| ek == k) {
            Some(e) => e.1 = v.to_string(),
            None => self.env.push((k.to_string(), v.to_string())),
        }
    }

    /// True when nothing but history is set (so a plain import needs no file).
    #[cfg(test)]
    pub fn is_blank(&self) -> bool {
        let mut c = self.clone();
        c.history.clear();
        c == ImageConfig::default()
    }
}

/// `config.json` of the template directory `dir`, or the default when there is none.
pub fn load(dir: &Path) -> Result<ImageConfig> {
    let p = dir.join(CONFIG_FILE);
    match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("{} is corrupt", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ImageConfig::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(dir: &Path, cfg: &ImageConfig) -> Result<()> {
    let p = dir.join(CONFIG_FILE);
    let tmp = dir.join(".config.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))
}

/// Single-quote `s` for a POSIX shell.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Docker-style `$VAR`, `${VAR}`, `${VAR:-default}` expansion; `\$` is a literal `$`. An unset variable is empty.
pub fn expand(text: &str, vars: &BTreeMap<String, String>) -> String {
    let cs: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && cs.get(i + 1) == Some(&'$') {
            out.push('$');
            i += 2;
        } else if c == '$' && cs.get(i + 1) == Some(&'{') {
            if let Some(end) = cs[i + 2..].iter().position(|&c| c == '}') {
                let body: String = cs[i + 2..i + 2 + end].iter().collect();
                let (name, default) = match body.split_once(":-") {
                    Some((n, d)) => (n, Some(d)),
                    None => (body.as_str(), None),
                };
                match vars.get(name).filter(|v| !v.is_empty()) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(default.unwrap_or("")),
                }
                i += end + 3;
            } else {
                out.push(c);
                i += 1;
            }
        } else if c == '$'
            && cs
                .get(i + 1)
                .is_some_and(|&n| is_name(n) && !n.is_ascii_digit())
        {
            let mut j = i + 1;
            while j < cs.len() && is_name(cs[j]) {
                j += 1;
            }
            let name: String = cs[i + 1..j].iter().collect();
            out.push_str(vars.get(&name).map_or("", String::as_str));
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `K=V` (or just `K`, taken from `lookup`), as `-e` / `environment:` give it.
pub fn parse_env_pair(
    s: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<(String, String)>> {
    let (k, v) = match s.split_once('=') {
        Some((k, v)) => (k, Some(v.to_string())),
        None => (s, None),
    };
    ensure!(
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit()),
        "{s:?} is not a valid environment variable (NAME=value)"
    );
    Ok(match v {
        Some(v) => Some((k.to_string(), v)),
        None => lookup(k).map(|v| (k.to_string(), v)),
    })
}

/// Lines of an env file: `K=V`, `#` comments and blank lines ignored; a bare `K` is taken from `lookup`.
pub fn parse_env_file(
    text: &str,
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(p) =
            parse_env_pair(t, lookup).with_context(|| format!("env file line {}", i + 1))?
        {
            out.push(p);
        }
    }
    Ok(out)
}

/// `export K='v'; ` for each pair, later pairs overriding earlier ones with the same key.
fn exports(pairs: &[(String, String)]) -> String {
    let mut merged: Vec<(&str, &str)> = Vec::new();
    for (k, v) in pairs {
        match merged.iter_mut().find(|(mk, _)| mk == k) {
            Some(e) => e.1 = v,
            None => merged.push((k, v)),
        }
    }
    merged
        .iter()
        .map(|(k, v)| format!("export {k}={}; ", sq(v)))
        .collect()
}

/// The shell prefix that sets `env` and changes into `workdir` before a command.
pub fn prefix(env: &[(String, String)], workdir: Option<&str>) -> String {
    let mut p = exports(env);
    if let Some(w) = workdir {
        p.push_str(&format!("cd {} && ", sq(w)));
    }
    p
}

/// Turn the words after `--` into shell text. One word is shell text as written (`-- "make && make test"`); several are
/// quoted individually (argv, like `docker run`).
pub fn argv_to_shell(argv: &[String]) -> String {
    match argv {
        [one] => one.clone(),
        many => many.iter().map(|w| sq(w)).collect::<Vec<_>>().join(" "),
    }
}

/// What `run` / `exec` send over SSH: the image's ENTRYPOINT and CMD (or the words after `--`), with the image's ENV and
/// WORKDIR, then the caller's `-e` / `--workdir` on top. `None` when there is nothing to run.
pub fn effective_command(
    cfg: &ImageConfig,
    user_cmd: &[String],
    env: &[(String, String)],
    workdir: Option<&str>,
) -> Option<String> {
    let tail = if user_cmd.is_empty() {
        cfg.cmd.as_ref().map(Cmd::to_shell)
    } else {
        Some(argv_to_shell(user_cmd))
    };
    let body = match (&cfg.entrypoint, tail) {
        (Some(e), Some(t)) => format!("{} {t}", e.to_shell()),
        (Some(e), None) => e.to_shell(),
        (None, t) => t?,
    };
    let mut all = cfg.env.clone();
    all.extend(env.iter().cloned());
    let wd = workdir.or(cfg.workdir.as_deref());
    Some(format!("{}{body}", prefix(&all, wd)))
}

/// `30s`, `5m`, `1h`, `1500ms` (rounded up to seconds) or bare seconds.
pub fn parse_duration_secs(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult_ms) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (t, 1000)
    };
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("{s:?} is not a duration (30s, 5m, 1h)"))?;
    Ok((n * mult_ms).div_ceil(1000))
}

/// `8g`, `512m`, `4096` (MiB) → MiB.
pub fn parse_memory_mib(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = t
        .strip_suffix("gib")
        .or(t.strip_suffix("gb"))
        .or(t.strip_suffix('g'))
    {
        (n, 1024)
    } else if let Some(n) = t
        .strip_suffix("mib")
        .or(t.strip_suffix("mb"))
        .or(t.strip_suffix('m'))
    {
        (n, 1)
    } else {
        (t.as_str(), 1)
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{s:?} is not a memory size (8g, 512m)"))?;
    let mib = n * mult;
    if mib == 0 {
        bail!("memory must be more than 0");
    }
    Ok(mib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(p: &[(&str, &str)]) -> BTreeMap<String, String> {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn cmd_forms() {
        assert_eq!(
            Cmd::parse(r#"["make","-j","4"]"#).unwrap(),
            Cmd::Exec(vec!["make".into(), "-j".into(), "4".into()])
        );
        assert_eq!(
            Cmd::parse("make && ls").unwrap(),
            Cmd::Shell("make && ls".into())
        );
        assert!(Cmd::parse("[1,2]").is_err());
        assert!(Cmd::parse("[]").is_err());
        assert!(Cmd::parse("  ").is_err());
        assert_eq!(
            Cmd::Exec(vec!["echo".into(), "a b".into()]).to_shell(),
            "'echo' 'a b'"
        );
    }

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(sq("it's"), r"'it'\''s'");
    }

    #[test]
    fn expansion_rules() {
        let v = vars(&[("A", "1"), ("EMPTY", "")]);
        assert_eq!(expand("x$A-${A}", &v), "x1-1");
        assert_eq!(expand("${B:-dflt}/${EMPTY:-e}", &v), "dflt/e");
        assert_eq!(expand(r"\$A and $B.", &v), "$A and .");
        assert_eq!(expand("cost $5 and $", &v), "cost $5 and $");
        assert_eq!(expand("${unterminated", &v), "${unterminated");
    }

    #[test]
    fn env_pairs_and_files() {
        let look = |k: &str| (k == "HOME").then(|| "/Users/x".to_string());
        assert_eq!(
            parse_env_pair("A=b=c", look).unwrap(),
            Some(("A".into(), "b=c".into()))
        );
        assert_eq!(
            parse_env_pair("HOME", look).unwrap(),
            Some(("HOME".into(), "/Users/x".into()))
        );
        assert_eq!(parse_env_pair("UNSET", look).unwrap(), None);
        assert!(parse_env_pair("1A=b", look).is_err());
        assert!(parse_env_pair("=b", look).is_err());
        assert!(parse_env_pair("A B=c", look).is_err());
        let f = parse_env_file("# c\n\nX=1\nHOME\nY=two words\n", look).unwrap();
        assert_eq!(
            f,
            vec![
                ("X".into(), "1".into()),
                ("HOME".into(), "/Users/x".into()),
                ("Y".into(), "two words".into())
            ]
        );
        let e = parse_env_file("ok=1\n9bad\n", look)
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn effective_command_layers_image_and_flags() {
        let mut cfg = ImageConfig::default();
        cfg.set_env("A", "1");
        cfg.workdir = Some("/opt/app".into());
        cfg.cmd = Some(Cmd::Shell("make test".into()));
        assert_eq!(
            effective_command(&cfg, &[], &[], None).unwrap(),
            "export A='1'; cd '/opt/app' && make test"
        );
        assert_eq!(
            effective_command(
                &cfg,
                &["ls".into(), "a b".into()],
                &[("A".into(), "2".into())],
                Some("/tmp")
            )
            .unwrap(),
            "export A='2'; cd '/tmp' && 'ls' 'a b'"
        );
        cfg.entrypoint = Some(Cmd::Exec(vec!["xcodebuild".into()]));
        assert_eq!(
            effective_command(&cfg, &["-version".into()], &[], None).unwrap(),
            "export A='1'; cd '/opt/app' && 'xcodebuild' -version"
        );
        assert_eq!(
            effective_command(&ImageConfig::default(), &[], &[], None),
            None
        );
    }

    #[test]
    fn one_word_is_shell_text_several_are_argv() {
        assert_eq!(argv_to_shell(&["a && b".into()]), "a && b");
        assert_eq!(argv_to_shell(&["a".into(), "b c".into()]), "'a' 'b c'");
    }

    #[test]
    fn durations_and_memory() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("2m").unwrap(), 120);
        assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
        assert_eq!(parse_duration_secs("1500ms").unwrap(), 2);
        assert_eq!(parse_duration_secs("7").unwrap(), 7);
        assert!(parse_duration_secs("soon").is_err());
        assert_eq!(parse_memory_mib("8g").unwrap(), 8192);
        assert_eq!(parse_memory_mib("512M").unwrap(), 512);
        assert_eq!(parse_memory_mib("4096").unwrap(), 4096);
        assert!(parse_memory_mib("0").is_err());
        assert!(parse_memory_mib("lots").is_err());
    }

    #[test]
    fn config_round_trips_through_a_directory() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_blank());
        let mut c = ImageConfig::default();
        c.set_env("K", "v");
        c.set_env("K", "w");
        c.expose.push("8080/tcp".into());
        c.history.push(HistoryEntry {
            created_unix: 5,
            created_by: "RUN x".into(),
            comment: String::new(),
        });
        save(d.path(), &c).unwrap();
        let back = load(d.path()).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.env, vec![("K".to_string(), "w".to_string())]);
        std::fs::write(d.path().join(CONFIG_FILE), b"{").unwrap();
        assert!(load(d.path()).is_err());
    }
}
