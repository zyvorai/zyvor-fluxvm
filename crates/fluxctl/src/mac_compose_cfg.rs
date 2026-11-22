// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The compose FILE features of `fluxctl mac compose`, kept apart from the commands: `${VAR}` interpolation, `.env`, `include`,
//! `extends`, `x-` extension keys and YAML merge keys, `secrets` / `configs` as read-only file mounts, and the `config` output.
//!
//! Everything here is pure or touches only the files it is told to read, so it is tested with temporary directories and a fake
//! variable lookup. Nothing was run against a real Mac or daemon.
//!
//! Interpolation differs from Docker in two small ways: a bare `$VAR` is left alone (only `${VAR}` and `$$` are interpreted, so a
//! guest command such as `echo $HOME` keeps working), and an unset `${VAR}` becomes an empty string without a warning.

use anyhow::{Context, Result, bail, ensure};
use serde_yaml::{Mapping, Value};
use std::path::{Path, PathBuf};

/// Where a secret appears in the guest when it names no `target`. macOS has no `/run` and a read-only root, so this is the closest
/// writable equivalent of Docker's `/run/secrets`.
pub const SECRETS_DIR: &str = "/private/var/run/secrets";
/// Where a config appears when it names no `target` (Docker uses `/NAME`, which a macOS guest cannot create).
pub const CONFIGS_DIR: &str = "/private/var/run/configs";

const MAX_INCLUDE_DEPTH: usize = 8;

/// `${VAR}`, `${VAR:-default}`, `${VAR-default}`, `${VAR:+alt}`, `${VAR+alt}`, `${VAR:?message}`, `${VAR?message}` and `$$`.
pub fn interpolate(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != '$' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some('{') => {
                let mut depth = 1;
                let mut j = i + 2;
                while j < b.len() && depth > 0 {
                    match b[j] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                ensure!(
                    depth == 0,
                    "interpolation: {s:?} has a `${{` with no closing `}}`; close it, or write `$$` for a literal dollar sign"
                );
                let inner: String = b[i + 2..j - 1].iter().collect();
                out.push_str(&one(&inner, s, lookup)?);
                i = j;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn one(inner: &str, whole: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let end = inner
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(inner.len());
    let (name, op) = inner.split_at(end);
    ensure!(
        !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()),
        "interpolation: {whole:?} has `${{{inner}}}`, which is not a variable name; use ${{NAME}} (letters, digits, _), or `$$` for a literal dollar sign"
    );
    let value = lookup(name);
    let set_nonempty = value.as_deref().is_some_and(|v| !v.is_empty());
    let set = value.is_some();
    let rest = |n: usize| interpolate(&op[n..], lookup);
    Ok(match op {
        "" => value.unwrap_or_default(),
        o if o.starts_with(":-") => {
            if set_nonempty {
                value.unwrap_or_default()
            } else {
                rest(2)?
            }
        }
        o if o.starts_with('-') => {
            if set {
                value.unwrap_or_default()
            } else {
                rest(1)?
            }
        }
        o if o.starts_with(":+") => {
            if set_nonempty {
                rest(2)?
            } else {
                String::new()
            }
        }
        o if o.starts_with('+') => {
            if set {
                rest(1)?
            } else {
                String::new()
            }
        }
        o if o.starts_with(":?") || o.starts_with('?') => {
            let (ok, skip) = if o.starts_with(":?") {
                (set_nonempty, 2)
            } else {
                (set, 1)
            };
            if ok {
                value.unwrap_or_default()
            } else {
                bail!(
                    "variable {name} is required but not set{}: set it in your shell (`export {name}=...`) or in a `.env` file next to the compose file",
                    match &op[skip..] {
                        "" => String::new(),
                        m => format!(" ({m})"),
                    }
                )
            }
        }
        _ => bail!(
            "interpolation: {whole:?} has `${{{inner}}}`, which this tool does not understand; supported forms are ${{VAR}}, ${{VAR:-default}}, ${{VAR-default}}, ${{VAR:+alt}}, ${{VAR+alt}}, ${{VAR:?message}}, ${{VAR?message}}"
        ),
    })
}

/// Interpolate every string VALUE of a YAML tree (keys are left alone).
pub fn interpolate_tree(v: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    match v {
        Value::String(s) if s.contains('$') => *s = interpolate(s, lookup)?,
        Value::Sequence(l) => {
            for x in l {
                interpolate_tree(x, lookup)?;
            }
        }
        Value::Mapping(m) => {
            for (_, x) in m.iter_mut() {
                interpolate_tree(x, lookup)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `.env` next to the compose file: `NAME=value` lines (comments, `export `, one pair of quotes allowed). Values already in the
/// environment win over the file, as in Docker.
pub fn dotenv_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// A boxed variable lookup.
pub type Lookup = Box<dyn Fn(&str) -> Option<String>>;

/// A variable lookup: the process environment first, then the `.env` file in `dir` (when there is one).
pub fn lookup_with_dotenv(dir: &Path, env: impl Fn(&str) -> Option<String> + 'static) -> Lookup {
    let pairs = std::fs::read_to_string(dir.join(".env"))
        .map(|t| dotenv_pairs(&t))
        .unwrap_or_default();
    Box::new(move |k| {
        env(k).or_else(|| {
            pairs
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        })
    })
}

/// Drop `version` (obsolete, Docker ignores it too) and `x-` extension keys at the top and in each service.
pub fn strip_extensions(top: &mut Mapping) {
    top.remove("version");
    top.retain(|k, _| !k.as_str().is_some_and(|s| s.starts_with("x-")));
    if let Some(Value::Mapping(services)) = top.get_mut("services") {
        for (_, s) in services.iter_mut() {
            if let Value::Mapping(m) = s {
                m.retain(|k, _| !k.as_str().is_some_and(|s| s.starts_with("x-")));
            }
        }
    }
}

fn read_value(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut v: Value =
        serde_yaml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    v.apply_merge()
        .with_context(|| format!("{}: a YAML merge key (`<<`) is wrong", path.display()))?;
    Ok(v)
}

fn abs(dir: &Path, p: &str) -> String {
    if p.starts_with('/') || p.starts_with('~') {
        p.to_string()
    } else {
        dir.join(p.strip_prefix("./").unwrap_or(p))
            .to_string_lossy()
            .into_owned()
    }
}

/// Make the relative paths of one service absolute against `dir` (the directory of the file it came from), so a service that
/// moves into another file by `include` or `extends` still points at the right files.
fn absolutize_service(svc: &mut Mapping, dir: &Path) {
    let set = |m: &mut Mapping, key: &str, f: &dyn Fn(&mut Value)| {
        if let Some(v) = m.get_mut(key) {
            f(v);
        }
    };
    set(svc, "env_file", &|v| match v {
        Value::String(s) => *s = abs(dir, s),
        Value::Sequence(l) => {
            for e in l {
                match e {
                    Value::String(s) => *s = abs(dir, s),
                    Value::Mapping(m) => {
                        if let Some(Value::String(s)) = m.get_mut("path") {
                            *s = abs(dir, s);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Mapping(m) => {
            if let Some(Value::String(s)) = m.get_mut("path") {
                *s = abs(dir, s);
            }
        }
        _ => {}
    });
    set(svc, "build", &|v| match v {
        Value::String(s) => *s = abs(dir, s),
        Value::Mapping(m) => {
            let ctx = m
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string();
            m.insert("context".into(), abs(dir, &ctx).into());
        }
        _ => {}
    });
    set(svc, "volumes", &|v| {
        if let Value::Sequence(l) = v {
            for e in l {
                if let Value::String(s) = e
                    && (s.starts_with("./") || s.starts_with("../") || s.starts_with(".:"))
                {
                    *s = abs(dir, s.split(':').next().unwrap_or(""))
                        + &s[s.find(':').unwrap_or(s.len())..];
                }
            }
        }
    });
}

fn absolutize_sources(top: &mut Mapping, dir: &Path) {
    for key in ["secrets", "configs"] {
        if let Some(Value::Mapping(m)) = top.get_mut(key) {
            for (_, d) in m.iter_mut() {
                if let Value::Mapping(d) = d
                    && let Some(Value::String(f)) = d.get_mut("file")
                {
                    *f = abs(dir, f);
                }
            }
        }
    }
}

/// `environment` / `labels` as a map (a `K=V` list becomes a map; a bare `K` becomes null).
fn as_map(v: &Value) -> Mapping {
    match v {
        Value::Mapping(m) => m.clone(),
        Value::Sequence(l) => l
            .iter()
            .filter_map(Value::as_str)
            .map(|s| match s.split_once('=') {
                Some((k, v)) => (k.into(), v.into()),
                None => (s.into(), Value::Null),
            })
            .collect(),
        _ => Mapping::new(),
    }
}

fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Sequence(l) => l.clone(),
        Value::Null => vec![],
        o => vec![o.clone()],
    }
}

fn mount_target(v: &Value) -> Option<String> {
    v.as_str()
        .and_then(|s| s.split(':').nth(1))
        .map(str::to_string)
}

/// The service in `over` laid over the one in `base`, Docker's `extends` rules: scalars and `command` are replaced,
/// `environment`, `labels`, `depends_on` and `build` are merged key by key, `ports`, `profiles`, `env_file`, `secrets` and
/// `configs` are appended, and a volume with the same guest path replaces the base one.
pub fn merge_service(base: &Mapping, over: &Mapping) -> Mapping {
    let mut out = base.clone();
    for (k, v) in over {
        let key = k.as_str().unwrap_or("");
        let merged = match (key, out.get(k)) {
            ("environment" | "labels", Some(b)) => {
                let mut m = as_map(b);
                for (k2, v2) in as_map(v) {
                    m.insert(k2, v2);
                }
                Value::Mapping(m)
            }
            ("depends_on", Some(b)) => {
                let norm = |x: &Value| -> Mapping {
                    match x {
                        Value::Sequence(l) => l
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|n| {
                                let mut c = Mapping::new();
                                c.insert("condition".into(), "service_started".into());
                                (n.into(), Value::Mapping(c))
                            })
                            .collect(),
                        Value::Mapping(m) => m.clone(),
                        _ => Mapping::new(),
                    }
                };
                let mut m = norm(b);
                for (k2, v2) in norm(v) {
                    m.insert(k2, v2);
                }
                Value::Mapping(m)
            }
            ("build", Some(Value::Mapping(b))) if v.is_mapping() => {
                let mut m = b.clone();
                for (k2, v2) in v.as_mapping().into_iter().flatten() {
                    m.insert(k2.clone(), v2.clone());
                }
                Value::Mapping(m)
            }
            ("ports" | "profiles" | "env_file" | "secrets" | "configs", Some(b)) => {
                let mut l = as_list(b);
                l.extend(as_list(v));
                Value::Sequence(l)
            }
            ("volumes", Some(b)) => {
                let over_targets: Vec<String> =
                    as_list(v).iter().filter_map(mount_target).collect();
                let mut l: Vec<Value> = as_list(b)
                    .into_iter()
                    .filter(|e| mount_target(e).is_none_or(|t| !over_targets.contains(&t)))
                    .collect();
                l.extend(as_list(v));
                Value::Sequence(l)
            }
            _ => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    out
}

/// Read one compose file with its `include`s and `extends`: the result has no `include` or `extends`, relative paths of included
/// or extended services are absolute, and `${VAR}` is already interpolated. `stack` holds the files being read (a loop is an error).
pub fn load_tree(
    path: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    stack: &mut Vec<PathBuf>,
) -> Result<Mapping> {
    let canon = std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
    ensure!(
        !stack.contains(&canon),
        "{}: `include` or `extends` comes back to this file (a loop): {}",
        path.display(),
        stack
            .iter()
            .chain([&canon])
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ")
    );
    ensure!(
        stack.len() < MAX_INCLUDE_DEPTH,
        "{}: `include` nests more than {MAX_INCLUDE_DEPTH} files deep; flatten it",
        path.display()
    );
    stack.push(canon);
    let r = load_tree_inner(path, lookup, stack);
    stack.pop();
    r.with_context(|| path.display().to_string())
}

fn load_tree_inner(
    path: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    stack: &mut Vec<PathBuf>,
) -> Result<Mapping> {
    let mut v = read_value(path)?;
    interpolate_tree(&mut v, lookup)?;
    let Value::Mapping(mut top) = v else {
        bail!(
            "the file must be a mapping with a `services:` key (see docs/macos-containers.md#compose)"
        );
    };
    strip_extensions(&mut top);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    // include: merge the services, secrets and configs of the other files in.
    if let Some(inc) = top.remove("include") {
        for entry in as_list(&inc) {
            let paths: Vec<String> = match &entry {
                Value::String(s) => vec![s.clone()],
                Value::Mapping(m) => {
                    for k in m.keys().filter_map(Value::as_str) {
                        ensure!(
                            k == "path",
                            "`include` option `{k}` is not supported: only the file path is (`include: [other.yaml]` or `- path: other.yaml`). Remove `{k}`; put values you need in a `.env` file next to the including file"
                        );
                    }
                    as_list(m.get("path").unwrap_or(&Value::Null))
                        .iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect()
                }
                _ => bail!(
                    "`include` entries are file paths (`include: [other.yaml]`) or `- path: other.yaml`"
                ),
            };
            ensure!(
                !paths.is_empty(),
                "an `include` entry names no path; write `include: [other.yaml]`"
            );
            // Several paths in one entry are one project in Docker (later files override); here they merge as separate includes.
            for p in paths {
                let file = dir.join(&p);
                ensure!(
                    file.is_file(),
                    "`include` names {}, which does not exist; fix the path (relative to {})",
                    file.display(),
                    dir.display()
                );
                let mut sub = load_tree(&file, lookup, stack)
                    .with_context(|| format!("included from {}", path.display()))?;
                let sub_dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
                absolutize_sources(&mut sub, &sub_dir);
                for key in ["services", "secrets", "configs"] {
                    let Some(Value::Mapping(items)) = sub.remove(key) else {
                        continue;
                    };
                    let dest = top
                        .entry(key.into())
                        .or_insert_with(|| Value::Mapping(Mapping::new()));
                    let Value::Mapping(dest) = dest else {
                        bail!("`{key}` must be a mapping");
                    };
                    for (name, mut def) in items {
                        ensure!(
                            !dest.contains_key(&name),
                            "{key} {:?} is defined both in {} and in an included file ({}); rename one",
                            name.as_str().unwrap_or("?"),
                            path.display(),
                            file.display()
                        );
                        if key == "services"
                            && let Value::Mapping(s) = &mut def
                        {
                            absolutize_service(s, &sub_dir);
                        }
                        dest.insert(name, def);
                    }
                }
            }
        }
    }
    resolve_extends(&mut top, path, &dir, lookup, stack)?;
    Ok(top)
}

fn resolve_extends(
    top: &mut Mapping,
    path: &Path,
    dir: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    stack: &mut Vec<PathBuf>,
) -> Result<()> {
    let Some(Value::Mapping(services)) = top.get("services").cloned() else {
        return Ok(());
    };
    let mut done = Mapping::new();
    for (name, _) in &services {
        let n = name.as_str().unwrap_or("?").to_string();
        let mut seen = vec![n.clone()];
        let svc = expand(&n, &services, path, dir, lookup, stack, &mut seen)?;
        done.insert(name.clone(), Value::Mapping(svc));
    }
    top.insert("services".into(), Value::Mapping(done));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn expand(
    name: &str,
    services: &Mapping,
    path: &Path,
    dir: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    stack: &mut Vec<PathBuf>,
    seen: &mut Vec<String>,
) -> Result<Mapping> {
    let Some(Value::Mapping(me)) = services.get(name) else {
        bail!(
            "service {name:?} is not defined in {}; check the `extends` name",
            path.display()
        );
    };
    let mut me = me.clone();
    let Some(ext) = me.remove("extends") else {
        return Ok(me);
    };
    let (target, file) = match &ext {
        Value::String(s) => (s.clone(), None),
        Value::Mapping(m) => {
            for k in m.keys().filter_map(Value::as_str) {
                ensure!(
                    k == "service" || k == "file",
                    "service {name:?}: `extends` option `{k}` is not supported; use `service:` and optionally `file:`"
                );
            }
            (
                m.get("service")
                    .and_then(Value::as_str)
                    .with_context(|| format!("service {name:?}: `extends` needs `service: NAME`"))?
                    .to_string(),
                m.get("file").and_then(Value::as_str).map(str::to_string),
            )
        }
        _ => bail!("service {name:?}: `extends` is a service name or {{service, file}}"),
    };
    let base = match file {
        None => {
            ensure!(
                !seen.contains(&target),
                "service {name:?}: `extends` loops: {} -> {target}",
                seen.join(" -> ")
            );
            seen.push(target.clone());
            expand(&target, services, path, dir, lookup, stack, seen)
                .with_context(|| format!("service {name:?} extends {target:?}"))?
        }
        Some(f) => {
            let other = dir.join(&f);
            ensure!(
                other.is_file(),
                "service {name:?}: `extends` names the file {}, which does not exist; fix `file:`",
                other.display()
            );
            let sub = load_tree(&other, lookup, stack)
                .with_context(|| format!("service {name:?} extends {target:?} in {f}"))?;
            let Some(Value::Mapping(svcs)) = sub.get("services") else {
                bail!("{} has no services", other.display());
            };
            let Some(Value::Mapping(b)) = svcs.get(&target) else {
                bail!(
                    "service {name:?} extends {target:?}, which {} does not define",
                    other.display()
                );
            };
            let mut b = b.clone();
            absolutize_service(&mut b, other.parent().unwrap_or(Path::new(".")));
            b
        }
    };
    Ok(merge_service(&base, &me))
}

// ---- secrets and configs -------------------------------------------------------------------------------------------------

/// One top-level `secrets:` / `configs:` entry. Only `file:` sources exist here.
#[derive(serde::Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct FileSource {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub external: Option<serde_yaml::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub labels: Option<serde_yaml::Value>,
}

/// A service's `secrets:` / `configs:` entry: a name, or `{source, target, ...}`.
#[derive(serde::Deserialize, Debug)]
#[serde(untagged)]
pub enum FileRef {
    Name(String),
    Full(Box<FileRefFull>),
}

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct FileRefFull {
    source: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    uid: Option<serde_yaml::Value>,
    #[serde(default)]
    gid: Option<serde_yaml::Value>,
    #[serde(default)]
    mode: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Secret,
    Config,
}

impl FileKind {
    fn word(self) -> &'static str {
        match self {
            FileKind::Secret => "secret",
            FileKind::Config => "config",
        }
    }
    fn plural(self) -> &'static str {
        match self {
            FileKind::Secret => "secrets",
            FileKind::Config => "configs",
        }
    }
    fn dir(self) -> &'static str {
        match self {
            FileKind::Secret => SECRETS_DIR,
            FileKind::Config => CONFIGS_DIR,
        }
    }
}

/// A file to put in a guest, read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMount {
    pub kind: FileKind,
    pub name: String,
    /// The file on this Mac.
    pub source: PathBuf,
    /// Its absolute path in the guest.
    pub target: String,
}

/// Resolve a service's `secrets:` / `configs:` against the top-level definitions. Refuses everything that is not a plain file.
pub fn resolve_files(
    service: &str,
    kind: FileKind,
    refs: &[FileRef],
    defs: &std::collections::BTreeMap<String, FileSource>,
    base: &Path,
) -> Result<Vec<FileMount>> {
    let mut out = Vec::new();
    for r in refs {
        let (source, target) = match r {
            FileRef::Name(n) => (n.as_str(), None),
            FileRef::Full(full) => {
                let FileRefFull {
                    source,
                    target,
                    uid,
                    gid,
                    mode,
                } = &**full;
                for (k, v) in [("uid", uid), ("gid", gid), ("mode", mode)] {
                    ensure!(
                        v.is_none(),
                        "service {service:?}: {} {source:?}: `{k}` is not supported (the file is shared read-only exactly as it is on this Mac); remove `{k}`, and change the file's own permissions on the Mac if you need them different",
                        kind.word()
                    );
                }
                (source.as_str(), target.as_deref())
            }
        };
        let w = kind.word();
        let Some(def) = defs.get(source) else {
            bail!(
                "service {service:?} uses the {w} {source:?}, which the top-level `{}:` does not define; add `{}:\n  {source}:\n    file: ./path`",
                kind.plural(),
                kind.plural()
            );
        };
        if def.external.is_some() {
            bail!(
                "{w} {source:?}: `external` is not supported (there is no secret store behind a macOS VM); use `file: ./path` instead"
            );
        }
        if def.environment.is_some() {
            bail!(
                "{w} {source:?}: `environment` sources are not supported; write the value to a file and use `file: ./path`, or pass it with the service's `environment:`"
            );
        }
        if def.content.is_some() {
            bail!(
                "{w} {source:?}: inline `content` is not supported; put the text in a file and use `file: ./path`"
            );
        }
        let Some(file) = &def.file else {
            bail!(
                "{w} {source:?} has no `file:`; only file-based {} are supported (`file: ./path`)",
                kind.plural()
            );
        };
        let path = if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            base.join(file)
        };
        ensure!(
            path.is_file(),
            "service {service:?}: the file {} of {w} {source:?} does not exist or is not a file; create it or fix `file:`",
            path.display()
        );
        ensure!(
            !source.contains('/') && !source.is_empty(),
            "{w} name {source:?} must be a plain name (no `/`); use `target:` for the guest path"
        );
        let target = match target {
            None => format!("{}/{source}", kind.dir()),
            Some(t) if t.starts_with('/') => {
                ensure!(
                    !t.contains(".."),
                    "service {service:?}: {w} target {t:?} must not contain .."
                );
                t.to_string()
            }
            Some(t) => {
                ensure!(
                    !t.contains(".."),
                    "service {service:?}: {w} target {t:?} must not contain .."
                );
                format!("{}/{t}", kind.dir())
            }
        };
        out.push(FileMount {
            kind,
            name: source.to_string(),
            source: path,
            target,
        });
    }
    Ok(out)
}

/// What [`stage_files`] made: the read-only directory shares to add to the instance and the extra links to make in the guest.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Staged {
    pub mounts: Vec<crate::mac_instance::Mount>,
    /// `(share path, target)`: a symlink to make in the guest for a file whose `target` is not the default directory.
    pub links: Vec<(String, String)>,
}

/// Copy the files into `root/PROJECT/SERVICE/{secrets,configs}/NAME` (the directory mode 0700, each file 0444) and describe the
/// shares. `first_index` is the index the first new share will have among the instance's shares (existing volumes come first).
/// A directory is what a virtiofs share can carry, which is why a file is staged instead of mounted directly.
pub fn stage_files(
    root: &Path,
    project: &str,
    service: &str,
    files: &[FileMount],
    first_index: usize,
) -> Result<Staged> {
    let mut st = Staged::default();
    if files.is_empty() {
        return Ok(st);
    }
    let dir = root.join(project).join(service);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    for kind in [FileKind::Secret, FileKind::Config] {
        let of: Vec<&FileMount> = files.iter().filter(|f| f.kind == kind).collect();
        if of.is_empty() {
            continue;
        }
        let kdir = dir.join(kind.plural());
        std::fs::create_dir_all(&kdir).with_context(|| format!("creating {}", kdir.display()))?;
        for f in &of {
            let dest = kdir.join(&f.name);
            std::fs::copy(&f.source, &dest)
                .with_context(|| format!("copying {} to {}", f.source.display(), dest.display()))?;
            set_mode(&dest, 0o444)?;
        }
        set_mode(&kdir, 0o755)?;
        let idx = first_index + st.mounts.len();
        st.mounts.push(crate::mac_instance::Mount {
            host: kdir,
            guest: kind.dir().to_string(),
            read_only: true,
        });
        for f in of {
            let default = format!("{}/{}", kind.dir(), f.name);
            if f.target != default {
                st.links.push((
                    format!(
                        "{}/fs{idx}/{}",
                        crate::mac_instance::GUEST_SHARES_DIR,
                        f.name
                    ),
                    f.target.clone(),
                ));
            }
        }
    }
    // Only the owner can look in the staging tree; the guest reads it through the share.
    set_mode(&dir, 0o700)?;
    if let Some(p) = dir.parent() {
        set_mode(p, 0o700)?;
    }
    Ok(st)
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", p.display()))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

/// Shell text (each link ends in `; `) that points each custom `target` at its staged file. Best effort like the volume links:
/// without sudo first, then `sudo -n`, else a warning on stderr.
pub fn link_prefix(links: &[(String, String)]) -> String {
    use crate::mac_config::sq;
    links
        .iter()
        .map(|(src, dst)| {
            let parent = Path::new(dst)
                .parent()
                .and_then(|p| p.to_str())
                .filter(|p| !p.is_empty())
                .unwrap_or("/");
            format!(
                "{{ {{ mkdir -p {p} && ln -sfn {s} {d}; }} 2>/dev/null || {{ sudo -n mkdir -p {p} && sudo -n ln -sfn {s} {d}; }} 2>/dev/null || echo {w} >&2; }}; ",
                p = sq(parent),
                s = sq(src),
                d = sq(dst),
                w = sq(&format!("warning: could not link {dst} to its staged file"))
            )
        })
        .collect()
}

/// Where `compose up` stages secrets and configs.
pub fn stage_root() -> PathBuf {
    std::env::temp_dir().join("fluxvm-compose-files")
}

/// The explanation for a serde "unknown field" failure: names the key and says what to do.
pub fn unsupported_key(e: &serde_yaml::Error) -> Option<String> {
    let msg = e.to_string();
    let rest = msg.split("unknown field `").nth(1)?;
    let key = rest.split('`').next()?;
    Some(format!(
        "compose key `{key}` is not supported for macOS guests (it is refused, not silently ignored): remove it from the file. The keys that work are listed in docs/macos-containers.md#compose (image, build, command, environment, env_file, ports, volumes, working_dir, depends_on, labels, restart, cpus, mem_limit, user, profiles, secrets, configs, extends)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> Option<String> {
        match k {
            "A" => Some("1".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn interpolation_forms() {
        let i = |s: &str| interpolate(s, &env);
        assert_eq!(i("x-${A}-y").unwrap(), "x-1-y");
        assert_eq!(i("${NOPE:-dflt}").unwrap(), "dflt");
        assert_eq!(i("${EMPTY:-dflt}").unwrap(), "dflt");
        assert_eq!(i("${EMPTY-dflt}").unwrap(), "");
        assert_eq!(i("${A:+yes}").unwrap(), "yes");
        assert_eq!(i("${NOPE:+yes}").unwrap(), "");
        assert_eq!(i("${NOPE:-${A}}").unwrap(), "1");
        assert_eq!(i("cost $$5 and $HOME").unwrap(), "cost $5 and $HOME");
        assert_eq!(i("${NOPE}").unwrap(), "");
    }

    #[test]
    fn interpolation_errors_say_what_to_do() {
        let e = interpolate("${TOKEN:?need a token}", &env)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("TOKEN") && e.contains("need a token") && e.contains(".env"),
            "{e}"
        );
        let e = interpolate("${A", &env).unwrap_err().to_string();
        assert!(e.contains("closing") && e.contains("$$"), "{e}");
        let e = interpolate("${A%x}", &env).unwrap_err().to_string();
        assert!(e.contains("supported forms"), "{e}");
        assert!(interpolate("${}", &env).is_err());
    }

    #[test]
    fn dotenv_loses_to_the_environment() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".env"),
            "# c\nA=from-file\nB='quoted'\nexport C=3\n",
        )
        .unwrap();
        let l = lookup_with_dotenv(d.path(), |k| (k == "A").then(|| "from-env".to_string()));
        assert_eq!(l("A").unwrap(), "from-env");
        assert_eq!(l("B").unwrap(), "quoted");
        assert_eq!(l("C").unwrap(), "3");
        assert!(l("D").is_none());
    }

    fn write(d: &Path, name: &str, text: &str) -> PathBuf {
        let p = d.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn tree(p: &Path) -> Result<Mapping> {
        load_tree(p, &env, &mut vec![])
    }

    #[test]
    fn include_merges_services_and_makes_paths_absolute() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "sub/db.yaml",
            "services:\n  db:\n    image: pg:${A}\n    env_file: db.env\n    volumes: ['./data:/d']\n    build: ./b\n",
        );
        let main = write(
            d.path(),
            "main.yaml",
            "version: '3'\nx-note: ignored\ninclude:\n  - sub/db.yaml\nservices:\n  app:\n    image: app:1\n",
        );
        let t = tree(&main).unwrap();
        let svcs = t["services"].as_mapping().unwrap();
        assert!(svcs.contains_key("app") && svcs.contains_key("db"));
        assert!(!t.contains_key("version") && !t.contains_key("x-note"));
        let db = svcs["db"].as_mapping().unwrap();
        assert_eq!(db["image"], "pg:1");
        let sub = d.path().join("sub");
        assert_eq!(
            db["env_file"].as_str().unwrap(),
            sub.join("db.env").to_str().unwrap()
        );
        assert_eq!(
            db["build"].as_str().unwrap(),
            sub.join("b").to_str().unwrap()
        );
        assert!(
            db["volumes"][0]
                .as_str()
                .unwrap()
                .starts_with(sub.to_str().unwrap())
        );
    }

    #[test]
    fn include_problems_are_named() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.yaml", "services:\n  x: {image: i}\n");
        let m = write(
            d.path(),
            "m.yaml",
            "include: [a.yaml]\nservices:\n  x: {image: j}\n",
        );
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(e.contains("\"x\"") && e.contains("rename one"), "{e}");
        let m = write(d.path(), "m2.yaml", "include: [nope.yaml]\nservices: {}\n");
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(
            e.contains("does not exist") && e.contains("nope.yaml"),
            "{e}"
        );
        let m = write(
            d.path(),
            "m3.yaml",
            "include:\n  - path: a.yaml\n    project_directory: .\nservices: {}\n",
        );
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(
            e.contains("project_directory") && e.contains("not supported"),
            "{e}"
        );
        write(d.path(), "l1.yaml", "include: [l2.yaml]\nservices: {}\n");
        write(d.path(), "l2.yaml", "include: [l1.yaml]\nservices: {}\n");
        let e = format!("{:#}", tree(&d.path().join("l1.yaml")).unwrap_err());
        assert!(e.contains("loop"), "{e}");
    }

    #[test]
    fn extends_merges_by_docker_rules() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "base.yaml",
            "services:\n  base:\n    image: b:1\n    env_file: base.env\n    environment: {K: base, L: base}\n    ports: ['1:1']\n    volumes: ['./x:/data', './y:/keep']\n    command: old\n",
        );
        let m = write(
            d.path(),
            "m.yaml",
            "services:\n  common:\n    image: c:1\n    labels: [a=1]\n  web:\n    extends: {file: base.yaml, service: base}\n    environment: [K=web]\n    ports: ['2:2']\n    volumes: ['./z:/data']\n    command: new\n  api:\n    extends: common\n    labels: {b: '2'}\n",
        );
        let t = tree(&m).unwrap();
        let s = t["services"].as_mapping().unwrap();
        let web = s["web"].as_mapping().unwrap();
        assert!(!web.contains_key("extends"));
        assert_eq!(web["command"], "new");
        assert_eq!(web["environment"]["K"], "web");
        assert_eq!(web["environment"]["L"], "base");
        assert_eq!(web["ports"].as_sequence().unwrap().len(), 2);
        let vols = web["volumes"].as_sequence().unwrap();
        assert_eq!(vols.len(), 2, "{vols:?}");
        assert!(vols[0].as_str().unwrap().ends_with(":/keep"));
        assert!(vols[1].as_str().unwrap().contains("/z:/data"));
        assert!(
            web["env_file"].as_str().unwrap().ends_with("base.env")
                && web["env_file"].as_str().unwrap().starts_with('/')
        );
        let api = s["api"].as_mapping().unwrap();
        assert_eq!(api["image"], "c:1");
        assert_eq!(api["labels"]["a"], "1");
        assert_eq!(api["labels"]["b"], "2");
    }

    #[test]
    fn extends_problems_are_named() {
        let d = tempfile::tempdir().unwrap();
        let m = write(
            d.path(),
            "m.yaml",
            "services:\n  a: {extends: b}\n  b: {extends: a}\n",
        );
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(e.contains("loops"), "{e}");
        let m = write(d.path(), "m2.yaml", "services:\n  a: {extends: zzz}\n");
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(e.contains("zzz") && e.contains("not defined"), "{e}");
        let m = write(
            d.path(),
            "m3.yaml",
            "services:\n  a:\n    extends: {service: b, file: nope.yaml}\n",
        );
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(
            e.contains("nope.yaml") && e.contains("does not exist"),
            "{e}"
        );
        let m = write(
            d.path(),
            "m4.yaml",
            "services:\n  a:\n    extends: {service: b, other: 1}\n  b: {image: x}\n",
        );
        let e = format!("{:#}", tree(&m).unwrap_err());
        assert!(e.contains("`other`") && e.contains("not supported"), "{e}");
    }

    #[test]
    fn yaml_merge_keys_work() {
        let d = tempfile::tempdir().unwrap();
        let m = write(
            d.path(),
            "m.yaml",
            "x-c: &c\n  image: c:1\n  restart: always\nservices:\n  a:\n    <<: *c\n    user: me\n",
        );
        let t = tree(&m).unwrap();
        let a = t["services"]["a"].as_mapping().unwrap();
        assert_eq!(a["image"], "c:1");
        assert_eq!(a["user"], "me");
    }

    fn defs(y: &str) -> std::collections::BTreeMap<String, FileSource> {
        serde_yaml::from_str(y).unwrap()
    }

    fn refs(y: &str) -> Vec<FileRef> {
        serde_yaml::from_str(y).unwrap()
    }

    #[test]
    fn secrets_resolve_to_files_with_default_and_custom_targets() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "tok.txt", "s3cret");
        let df = defs("tok: {file: tok.txt}\n");
        let r = resolve_files(
            "a",
            FileKind::Secret,
            &refs("[tok, {source: tok, target: /etc/tok}, {source: tok, target: rel/t}]"),
            &df,
            d.path(),
        )
        .unwrap();
        assert_eq!(r[0].target, "/private/var/run/secrets/tok");
        assert_eq!(r[1].target, "/etc/tok");
        assert_eq!(r[2].target, "/private/var/run/secrets/rel/t");
        assert_eq!(r[0].source, d.path().join("tok.txt"));
    }

    #[test]
    fn secrets_and_configs_that_are_not_plain_files_are_refused_with_a_reason() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "f", "x");
        let go = |defs_y: &str, refs_y: &str| {
            resolve_files(
                "a",
                FileKind::Secret,
                &refs(refs_y),
                &defs(defs_y),
                d.path(),
            )
            .map_err(|e| e.to_string())
            .unwrap_err()
        };
        assert!(go("s: {external: true}", "[s]").contains("`external` is not supported"));
        assert!(go("s: {environment: FOO}", "[s]").contains("`environment` sources"));
        assert!(go("s: {content: hi}", "[s]").contains("inline `content`"));
        assert!(go("s: {}", "[s]").contains("only file-based"));
        assert!(go("s: {file: f}", "[nope]").contains("does not define"));
        assert!(go("s: {file: missing}", "[s]").contains("does not exist"));
        let e = go("s: {file: f}", "[{source: s, mode: 0400}]");
        assert!(
            e.contains("`mode` is not supported") && e.contains("read-only"),
            "{e}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_copies_files_read_only_and_describes_the_shares() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "tok.txt", "s3cret");
        write(d.path(), "app.conf", "k=v");
        let files = vec![
            FileMount {
                kind: FileKind::Secret,
                name: "tok".into(),
                source: d.path().join("tok.txt"),
                target: format!("{SECRETS_DIR}/tok"),
            },
            FileMount {
                kind: FileKind::Config,
                name: "app".into(),
                source: d.path().join("app.conf"),
                target: "/etc/app.conf".into(),
            },
        ];
        let root = d.path().join("stage");
        let st = stage_files(&root, "proj", "web", &files, 1).unwrap();
        assert_eq!(st.mounts.len(), 2);
        assert!(st.mounts.iter().all(|m| m.read_only && m.host.is_dir()));
        assert_eq!(st.mounts[0].guest, SECRETS_DIR);
        assert_eq!(st.mounts[1].guest, CONFIGS_DIR);
        let staged = root.join("proj/web/secrets/tok");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "s3cret");
        assert_eq!(
            std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(
            std::fs::metadata(root.join("proj/web"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // The secret sits at its default place (the share itself); only the config needs a link, to share fs2.
        assert_eq!(
            st.links,
            [(
                "/Volumes/My Shared Files/fs2/app".to_string(),
                "/etc/app.conf".to_string()
            )]
        );
        let l = link_prefix(&st.links);
        assert!(
            l.contains("ln -sfn '/Volumes/My Shared Files/fs2/app' '/etc/app.conf'"),
            "{l}"
        );
        assert!(l.ends_with("; "));
        assert_eq!(
            stage_files(&root, "p", "s", &[], 0).unwrap(),
            Staged::default()
        );
    }

    #[test]
    fn unknown_field_errors_name_the_key() {
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        struct T {
            #[allow(dead_code)]
            a: u8,
        }
        let e = serde_yaml::from_value::<T>(serde_yaml::from_str("a: 1\nnetworks: 2").unwrap())
            .unwrap_err();
        let m = unsupported_key(&e).unwrap();
        assert!(m.contains("`networks`") && m.contains("remove it"), "{m}");
    }
}
