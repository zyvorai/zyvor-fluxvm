// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl config path | ls | get | set | unset`: read and change settings in the one config file FluxVM already has
//! (`--config`, else `$FLUXVM_CONFIG`, else `~/.config/fluxvm/config.toml`), the same file the daemon and
//! `fluxctl config check` read. There is no second store: `cli.*` keys (`cli.upgrade_url`) live in it too.
//!
//! `set` edits the text in place (comments and order are kept), then runs the same checks as `config check` on the result
//! and refuses, writing nothing, when the new value introduces an error or an unknown key. Secrets (anything named
//! token, secret, password, key or authorization) are never printed by `ls` or `get`.

use crate::cli_errors::coded;
use anyhow::{Context, Result, bail};
use fluxvm_core::config_lint::{Finding, LintEnv, Severity, lint_str};
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug, Clone)]
pub struct GetArgs {
    /// Dotted key, for example `listen` or `cli.upgrade_url`.
    pub key: String,
}

#[derive(clap::Args, Debug, Clone)]
pub struct SetArgs {
    /// Dotted key, for example `cli.upgrade_url`.
    pub key: String,
    /// The value, written as TOML (`true`, `42`, `["a"]`); anything else is taken as text.
    pub value: String,
}

#[derive(clap::Args, Debug, Clone)]
pub struct LsArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// The file settings live in, and whether it exists yet.
pub fn resolve(global: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    global
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("FLUXVM_CONFIG").map(PathBuf::from))
        .or_else(|| home.map(|h| h.join(".config/fluxvm/config.toml")))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn path(global: Option<&Path>) -> Result<String> {
    let p = resolve(global, home().as_deref())
        .context("no config path: set HOME, pass --config or set FLUXVM_CONFIG")?;
    let state = if p.exists() {
        "exists"
    } else {
        "does not exist yet; `fluxctl config set` creates it"
    };
    Ok(format!("{}\n({state})\n", p.display()))
}

fn secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "key",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|w| k.contains(w))
}

fn flatten(prefix: &str, v: &toml::Value, out: &mut Vec<(String, String)>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        // Arrays of tables (auth.tokens, ...) are shown as a count, never expanded: that is where secrets live.
        toml::Value::Array(a) if a.iter().any(toml::Value::is_table) => {
            out.push((
                prefix.to_owned(),
                format!("({} entries; see the file)", a.len()),
            ));
        }
        other => {
            let shown = if secret(prefix) {
                "(hidden)".to_owned()
            } else {
                other.to_string()
            };
            out.push((prefix.to_owned(), shown));
        }
    }
}

fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Every key written in the file with its value.
pub fn list(text: &str) -> Result<Vec<(String, String)>> {
    let v: toml::Value = toml::from_str(text)
        .context("the config file is not valid TOML (`fluxctl config check` says where)")?;
    let mut out = Vec::new();
    flatten("", &v, &mut out);
    Ok(out)
}

pub fn get(text: &str, key: &str) -> Result<String> {
    if secret(key) {
        bail!(
            "{key} holds a secret, which `config get` does not print; open the file (`fluxctl config path`)"
        );
    }
    let all = list(text)?;
    if let Some((_, v)) = all.iter().find(|(k, _)| k == key) {
        return Ok(v.clone());
    }
    // A table: show what is under it.
    let under: Vec<_> = all
        .iter()
        .filter(|(k, _)| k.starts_with(&format!("{key}.")))
        .collect();
    if !under.is_empty() {
        return Ok(under
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    // Not in the file: the built-in default, when it is a known key.
    let defaults = toml::Value::try_from(fluxvm_core::config::Config::default())?;
    let mut d = Vec::new();
    flatten("", &defaults, &mut d);
    if let Some((_, v)) = d.iter().find(|(k, _)| k == key) {
        return Ok(format!("{v}  (default, not set in the file)"));
    }
    Err(coded(
        format!("{key} is not set and is not a known setting"),
        "CONFIG_UNKNOWN_KEY",
    ))
}

/// True when the setting is text by definition (its default is a string), so  is not read as a date.
fn default_is_text(key: &str) -> bool {
    let Ok(mut v) = toml::Value::try_from(fluxvm_core::config::Config::default()) else {
        return false;
    };
    for part in key.split('.') {
        match v.get(part) {
            Some(n) => v = n.clone(),
            None => return false,
        }
    }
    v.is_str()
}

fn lint(text: &str) -> Vec<Finding> {
    lint_str(text, &LintEnv::OFFLINE)
}

fn related(f: &Finding, key: &str) -> bool {
    f.key == "(file)"
        || f.key == key
        || f.key.starts_with(&format!("{key}."))
        || key.starts_with(&format!("{}.", f.key))
}

/// The edited text, or why it was refused. Pure: reads and writes nothing.
pub fn apply_set(text: &str, key: &str, raw: &str) -> Result<String> {
    if key.is_empty()
        || key.split('.').any(|p| {
            p.is_empty()
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    {
        bail!(
            "{key:?} is not a setting name: use letters, digits, `_` and `.` between sections (for example cli.upgrade_url)"
        );
    }
    let before = lint(text);
    // The typed reading first (`true`, `42`), then plain text, so `0.0.0.0:7788` and `2026-01-01` work for text settings.
    let typed: Option<toml_edit::Value> = if default_is_text(key) {
        None
    } else {
        raw.trim().parse().ok()
    };
    let candidates: Vec<toml_edit::Value> = typed
        .into_iter()
        .chain([toml_edit::Value::from(raw)])
        .collect();
    let mut last_problem = None;
    for value in candidates {
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .context("the config file is not valid TOML (`fluxctl config check` says where)")?;
        let mut parts: Vec<&str> = key.split('.').collect();
        let leaf = parts.pop().unwrap_or_default();
        let mut table = doc.as_table_mut();
        for p in parts {
            let entry = table
                .entry(p)
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            table = entry
                .as_table_mut()
                .with_context(|| format!("{p} is not a section, so {key} cannot go under it"))?;
        }
        table.insert(leaf, toml_edit::Item::Value(value));
        let out = doc.to_string();
        let new: Vec<Finding> = lint(&out)
            .into_iter()
            .filter(|f| f.severity >= Severity::Warning && related(f, key) && !before.contains(f))
            .collect();
        if new.is_empty() {
            return Ok(out);
        }
        last_problem = Some(new);
    }
    let problems = last_problem.unwrap_or_default();
    let unknown = problems
        .iter()
        .any(|f| f.message.starts_with("unknown key"));
    let detail = problems
        .iter()
        .map(|f| {
            format!(
                "  {}: {}{}",
                f.key,
                f.message,
                f.fix
                    .as_deref()
                    .map(|x| format!(" ({x})"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(coded(
        format!("not written: {key} = {raw} would not work.\n{detail}"),
        if unknown {
            "CONFIG_UNKNOWN_KEY"
        } else {
            "CONFIG_BAD_VALUE"
        },
    ))
}

/// The text with `key` removed (an empty section is removed with it).
pub fn apply_unset(text: &str, key: &str) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut =
        text.parse().context("the config file is not valid TOML")?;
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().unwrap_or_default();
    let mut table = doc.as_table_mut();
    for p in &parts {
        match table.get_mut(p).and_then(toml_edit::Item::as_table_mut) {
            Some(t) => table = t,
            None => return Ok(None),
        }
    }
    if table.remove(leaf).is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let fail = |e: std::io::Error| {
        coded(
            format!("cannot write {}: {e}", path.display()),
            "CONFIG_NOT_WRITABLE",
        )
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_extension(format!("toml.pcli-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(fail)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(e)
    })
}

/// Sets `key` in the file at `path`; returns the line to tell the user.
pub fn set_in(path: &Path, key: &str, raw: &str) -> Result<String> {
    let text = read(path)?;
    let out = apply_set(&text, key, raw)?;
    if out == text {
        return Ok(format!(
            "{key} is already {raw} in {}; nothing changed",
            path.display()
        ));
    }
    write_atomic(path, &out)?;
    let note = if key.starts_with("cli.") {
        ""
    } else {
        "\nA running daemon reads the file when it starts: restart it for this to take effect."
    };
    Ok(format!("set {key} = {raw} in {}{note}", path.display()))
}

pub fn run(cmd: &crate::config_check::ConfigCommand, global: Option<&Path>) -> Result<i32> {
    use crate::config_check::ConfigCommand as C;
    let need = || {
        resolve(global, home().as_deref())
            .context("no config path: set HOME, pass --config or set FLUXVM_CONFIG")
    };
    match cmd {
        C::Path => print!("{}", path(global)?),
        C::Ls(a) => {
            let rows = list(&read(&need()?)?)?;
            if a.json {
                let m: serde_json::Map<String, serde_json::Value> =
                    rows.into_iter().map(|(k, v)| (k, v.into())).collect();
                println!("{}", serde_json::to_string_pretty(&m)?);
            } else if rows.is_empty() {
                println!(
                    "nothing is set: every setting has its default. `fluxctl config set KEY VALUE` changes one."
                );
            } else {
                let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
                for (k, v) in rows {
                    println!("{k:<w$}  {v}");
                }
            }
        }
        C::Get(a) => println!("{}", get(&read(&need()?)?, &a.key)?),
        C::Set(a) => println!("{}", set_in(&need()?, &a.key, &a.value)?),
        C::Unset(a) => {
            let p = need()?;
            match apply_unset(&read(&p)?, &a.key)? {
                Some(out) => {
                    write_atomic(&p, &out)?;
                    println!(
                        "removed {} from {}; it has its default again",
                        a.key,
                        p.display()
                    );
                }
                None => println!("{} is not set in {}; nothing changed", a.key, p.display()),
            }
        }
        C::Check(_) => unreachable!("handled by config_check"),
    }
    Ok(0)
}

/// Best-effort read of one boolean `cli.*` setting, false when anything is missing.
pub fn flag(global: Option<&Path>, key: &str) -> bool {
    resolve(global, home().as_deref())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .and_then(|v| v.get("cli")?.get(key)?.as_bool())
        .unwrap_or(false)
}

/// Best-effort read of one text `cli.*` setting.
pub fn text_setting(global: Option<&Path>, key: &str) -> Option<String> {
    resolve(global, home().as_deref())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .and_then(|v| v.get("cli")?.get(key)?.as_str().map(str::to_owned))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_keeps_comments_and_other_keys() {
        let out = apply_set(
            "# mine\nlisten = \"127.0.0.1:7788\"\n",
            "cli.upgrade_url",
            "https://m.example/releases",
        )
        .unwrap();
        assert!(out.contains("# mine") && out.contains("listen"));
        assert!(
            out.contains("[cli]") && out.contains("upgrade_url = \"https://m.example/releases\"")
        );
    }

    #[test]
    fn typed_values_and_text_that_looks_like_something_else() {
        let t = apply_set("", "cli.completion_offered", "true").unwrap();
        assert!(t.contains("completion_offered = true"));
        // a date-looking value for a text setting falls back to text
        let t = apply_set("", "cli.upgrade_url", "2026-01-01").unwrap();
        assert!(t.contains("\"2026-01-01\""));
    }

    #[test]
    fn an_unknown_key_is_refused_with_its_code() {
        let e = apply_set("", "cli.upgrde_url", "x")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("CONFIG_UNKNOWN_KEY") && e.contains("not written"),
            "{e}"
        );
    }

    #[test]
    fn a_wrong_type_is_refused() {
        let e = apply_set("", "cli.completion_offered", "maybe")
            .unwrap_err()
            .to_string();
        assert!(e.contains("CONFIG_BAD_VALUE"), "{e}");
    }

    #[test]
    fn bad_names_are_refused() {
        assert!(apply_set("", "a..b", "1").is_err());
        assert!(apply_set("", "a b", "1").is_err());
    }

    #[test]
    fn get_shows_the_file_then_the_default_and_hides_secrets() {
        let t = "listen = \"0.0.0.0:9\"\n[auth]\nrequire = true\n";
        assert_eq!(get(t, "listen").unwrap(), "\"0.0.0.0:9\"");
        assert!(
            get(t, "cli.completion_offered")
                .unwrap()
                .contains("default")
        );
        assert!(
            get(t, "auth.token")
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
        assert!(
            get(t, "nope.nothing")
                .unwrap_err()
                .to_string()
                .contains("CONFIG_UNKNOWN_KEY")
        );
    }

    #[test]
    fn ls_never_prints_a_secret() {
        let t = "[[auth.tokens]]\nname = \"a\"\ntoken = \"s3cr3t\"\nrole = \"admin\"\n[cli]\nupgrade_url = \"u\"\n";
        let rows = list(t).unwrap();
        assert!(rows.iter().all(|(_, v)| !v.contains("s3cr3t")));
        assert!(
            rows.iter()
                .any(|(k, v)| k == "cli.upgrade_url" && v == "\"u\"")
        );
    }

    #[test]
    fn unset_removes_only_that_key() {
        let t = apply_set("", "cli.upgrade_url", "u").unwrap();
        let back = apply_unset(&t, "cli.upgrade_url").unwrap().unwrap();
        assert!(!back.contains("upgrade_url"));
        assert!(apply_unset(&back, "cli.upgrade_url").unwrap().is_none());
    }

    #[test]
    fn set_in_creates_the_file_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/config.toml");
        assert!(
            set_in(&p, "cli.completion_offered", "true")
                .unwrap()
                .starts_with("set ")
        );
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .contains("completion_offered = true")
        );
        assert!(
            set_in(&p, "cli.completion_offered", "true")
                .unwrap()
                .contains("nothing changed")
        );
    }

    #[test]
    fn resolution_prefers_the_flag() {
        let h = Path::new("/h");
        assert_eq!(
            resolve(Some(Path::new("/x.toml")), Some(h)).unwrap(),
            Path::new("/x.toml")
        );
    }
}
