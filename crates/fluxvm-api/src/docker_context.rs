// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Docker CLI context named `fluxvm`, which points the `docker` command at the engine `fluxctl docker up` runs.
//!
//! The Docker CLI keeps a context in its config directory (`$DOCKER_CONFIG`, else `~/.docker`):
//!
//! * `contexts/meta/<lower-case hex SHA-256 of the context name>/meta.json` holds
//!   `{"Name":…,"Metadata":{"Description":…},"Endpoints":{"docker":{"Host":"unix:///path","SkipTLSVerify":false}}}`.
//! * `config.json` has `"currentContext": "<name>"` for the context `docker` uses when nothing else says.
//!
//! This module writes exactly those files, and only for the name [`NAME`]. A context with any other name is never read for
//! writing, changed or removed. `config.json` is rewritten only for `--use` (and by `uninstall` when `fluxvm` was current), and
//! every key it does not own is kept. All writes are atomic (a temp file in the same directory, then a rename).
//! The Studio page reads [`status`] through `GET /v1/docker/context`; installing stays a CLI command.

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// The only context name this module ever touches.
pub const NAME: &str = "fluxvm";
const DESCRIPTION: &str = "FluxVM Docker engine (fluxctl docker up)";

/// A failure with the plain-English next step in `message`, and a code `fluxctl explain` knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtxError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for CtxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (more: fluxctl explain {})", self.message, self.code)
    }
}

impl std::error::Error for CtxError {}

fn fail(code: &'static str, message: impl Into<String>) -> CtxError {
    CtxError {
        code,
        message: message.into(),
    }
}

#[derive(Serialize)]
struct Endpoint {
    #[serde(rename = "Host")]
    host: String,
    #[serde(rename = "SkipTLSVerify")]
    skip_tls_verify: bool,
}

#[derive(Serialize)]
struct Endpoints {
    docker: Endpoint,
}

#[derive(Serialize)]
struct MetaData {
    #[serde(rename = "Description")]
    description: String,
}

#[derive(Serialize)]
struct Meta {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Metadata")]
    metadata: MetaData,
    #[serde(rename = "Endpoints")]
    endpoints: Endpoints,
}

/// `unix:///path/to.sock`, the form the Docker CLI takes for a local socket.
pub fn host_of(socket: &Path) -> String {
    format!("unix://{}", socket.display())
}

/// The directory name the Docker CLI uses for a context: the lower-case hex SHA-256 of its name.
pub fn meta_dir_name(name: &str) -> String {
    Sha256::digest(name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The exact `meta.json` the Docker CLI writes for a context with a Unix-socket endpoint (compact JSON, field order as Docker's).
pub fn meta_json(name: &str, host: &str) -> String {
    let meta = Meta {
        name: name.into(),
        metadata: MetaData {
            description: DESCRIPTION.into(),
        },
        endpoints: Endpoints {
            docker: Endpoint {
                host: host.into(),
                skip_tls_verify: false,
            },
        },
    };
    serde_json::to_string(&meta).expect("a fixed shape serializes")
}

/// The Docker CLI's config directory: `$DOCKER_CONFIG` when set and not empty, else `$HOME/.docker`.
pub fn config_dir(docker_config: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match docker_config.filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => home
            .filter(|h| !h.is_empty())
            .map(|h| Path::new(h).join(".docker")),
    }
}

/// [`config_dir`] from this process's environment.
pub fn config_dir_from_env() -> Option<PathBuf> {
    config_dir(
        std::env::var("DOCKER_CONFIG").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join("contexts")
        .join("meta")
        .join(meta_dir_name(NAME))
        .join("meta.json")
}

/// `config.json` with `currentContext` set to `name`, or removed for `None` (Docker then uses its `default` context). Every other
/// key is kept. `existing` is the file's text (`None`: no file). Pure. Keys are written in sorted order with a tab indent, as the
/// Docker CLI itself writes them.
pub fn config_with_current(existing: Option<&str>, name: Option<&str>) -> Result<String, CtxError> {
    let mut root = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        None => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => m,
            _ => {
                return Err(fail(
                    "DOCKER_CONFIG_INVALID",
                    "the Docker CLI's config.json is not a JSON object, so FluxVM left it alone. Fix or move it (`docker` will not work well \
                     with it either), then run `fluxctl docker context install --use` again",
                ));
            }
        },
    };
    match name {
        Some(n) => {
            root.insert("currentContext".into(), json!(n));
        }
        None => {
            root.remove("currentContext");
        }
    }
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    Serialize::serialize(&Value::Object(root), &mut ser).expect("a JSON value serializes");
    let mut out = String::from_utf8(buf).expect("serde_json writes UTF-8");
    out.push('\n');
    Ok(out)
}

/// The `currentContext` of a `config.json` text. `None` when the file is missing, not JSON, or has none.
pub fn current_of(config: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(config?).ok()?;
    v.get("currentContext")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The `docker` endpoint host of a `meta.json` text.
pub fn host_in_meta(meta: &str) -> Option<String> {
    let v: Value = serde_json::from_str(meta).ok()?;
    v["Endpoints"]["docker"]["Host"].as_str().map(str::to_owned)
}

fn write_atomic(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn io_fail(what: &str, dir: &Path, e: std::io::Error) -> CtxError {
    fail(
        "DOCKER_CONTEXT_UNWRITABLE",
        format!(
            "could not {what} in {}: {e}. Check that you own that folder (or set DOCKER_CONFIG to one you do), then run \
             `fluxctl docker context install` again",
            dir.display()
        ),
    )
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Installed {
    pub host: String,
    pub meta: PathBuf,
    /// False when the context already pointed there.
    pub changed: bool,
    /// True when `currentContext` was set to `fluxvm` by this call.
    pub made_current: bool,
}

/// Creates or updates the `fluxvm` context in the Docker config directory `dir` to point at `socket`; with `use_current` also makes
/// it the current context. Idempotent. Other contexts and unrelated `config.json` keys are untouched.
pub fn install(dir: &Path, socket: &Path, use_current: bool) -> Result<Installed, CtxError> {
    if !socket.is_absolute() {
        return Err(fail(
            "DOCKER_CONTEXT_UNWRITABLE",
            format!(
                "the socket path {} is not absolute. Give a full path such as /tmp/fluxvm-run-1000/docker-engine.sock",
                socket.display()
            ),
        ));
    }
    let host = host_of(socket);
    let meta = meta_path(dir);
    let want = meta_json(NAME, &host);
    // Validate config.json first so a bad one stops us before anything is written.
    let cfg_path = dir.join("config.json");
    let cfg_new = if use_current {
        let old = read(&cfg_path);
        Some(config_with_current(old.as_deref(), Some(NAME))?)
    } else {
        None
    };
    let changed = read(&meta).as_deref() != Some(want.as_str());
    if changed {
        write_atomic(&meta, want.as_bytes(), 0o644)
            .map_err(|e| io_fail("write the context", dir, e))?;
    }
    let mut made_current = false;
    if let Some(text) = cfg_new {
        if current_of(read(&cfg_path).as_deref()).as_deref() != Some(NAME) {
            made_current = true;
        }
        // Keep the file's mode (Docker's own is 0600); a new one is private too.
        let mode = fs::metadata(&cfg_path)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
            .unwrap_or(0o600);
        write_atomic(&cfg_path, text.as_bytes(), mode)
            .map_err(|e| io_fail("update config.json", dir, e))?;
    }
    Ok(Installed {
        host,
        meta,
        changed,
        made_current,
    })
}

/// What `fluxctl docker context repair` should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    /// There is no `fluxvm` context to repair.
    NoContext,
    /// The context points at something that is not a unix socket; it was not made by `context install`, so it is left alone.
    Foreign(String),
    /// The socket the context points at answers.
    Healthy(PathBuf),
    /// The context points at a socket that does not answer; this one does.
    Repoint { from: PathBuf, to: PathBuf },
    /// Nothing answers, so the context cannot be fixed by pointing it elsewhere.
    NothingAnswers { from: PathBuf },
}

/// The unix socket path of a `unix:///path` host.
pub fn socket_of_host(host: &str) -> Option<PathBuf> {
    host.strip_prefix("unix://").map(PathBuf::from)
}

/// Decide the repair from the context's host and which sockets answer. `candidates` are tried in order.
pub fn plan_repair(
    host: Option<&str>,
    answers: &dyn Fn(&Path) -> bool,
    candidates: &[PathBuf],
) -> Repair {
    let Some(host) = host else {
        return Repair::NoContext;
    };
    let Some(from) = socket_of_host(host) else {
        return Repair::Foreign(host.to_owned());
    };
    if answers(&from) {
        return Repair::Healthy(from);
    }
    match candidates.iter().find(|c| **c != from && answers(c)) {
        Some(to) => Repair::Repoint {
            from,
            to: to.clone(),
        },
        None => Repair::NothingAnswers { from },
    }
}

/// What [`uninstall`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Uninstalled {
    /// False when there was no `fluxvm` context.
    pub removed: bool,
    /// True when `fluxvm` was the current context, so docker is back on its default one.
    pub reset_current: bool,
}

/// Removes the `fluxvm` context. If it was the current context, `currentContext` is removed too (docker would otherwise fail on a
/// context that no longer exists). Nothing else is touched.
pub fn uninstall(dir: &Path) -> Result<Uninstalled, CtxError> {
    let meta = meta_path(dir);
    let cfg_path = dir.join("config.json");
    let cfg = read(&cfg_path);
    let was_current = current_of(cfg.as_deref()).as_deref() == Some(NAME);
    let mut reset_current = false;
    if was_current {
        let text = config_with_current(cfg.as_deref(), None)?;
        let mode = fs::metadata(&cfg_path)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
            .unwrap_or(0o600);
        write_atomic(&cfg_path, text.as_bytes(), mode)
            .map_err(|e| io_fail("update config.json", dir, e))?;
        reset_current = true;
    }
    let removed = match meta.parent() {
        Some(d) if d.is_dir() => {
            fs::remove_dir_all(d).map_err(|e| io_fail("remove the context", dir, e))?;
            true
        }
        _ => false,
    };
    Ok(Uninstalled {
        removed,
        reset_current,
    })
}

/// The first `docker` executable on `path` (a `PATH` value), as `which` would find it.
pub fn docker_cli_in(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|d| d.join("docker"))
        .find(|p| {
            fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// How to get the Docker CLI on this OS.
pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "install it with `brew install docker` (the client alone is enough; FluxVM is the engine)"
    } else {
        "install it with `sudo apt install docker.io`, `sudo dnf install docker-cli` or see https://docs.docker.com/engine/install/ (the client alone is enough; FluxVM is the engine)"
    }
}

/// Everything [`status`] needs; gathered by the caller so [`status`] stays pure.
pub struct StatusInput<'a> {
    pub dir: Option<&'a Path>,
    pub socket: &'a Path,
    pub socket_answers: bool,
    pub docker_cli: bool,
    /// The value of `DOCKER_CONTEXT`, if set.
    pub env_context: Option<&'a str>,
    /// The value of `DOCKER_HOST`, if set.
    pub env_host: Option<&'a str>,
}

/// Where the context stands, as JSON for `fluxctl docker context status --json` and `GET /v1/docker/context`.
/// `state` is one of `no-docker-cli`, `not-installed`, `stale`, `socket-down`, `ready` (exists, not current) or `current`.
pub fn status(i: &StatusInput<'_>) -> Value {
    let (meta, cfg) = match i.dir {
        Some(d) => (read(&meta_path(d)), read(&d.join("config.json"))),
        None => (None, None),
    };
    let exists = meta.is_some();
    let host = meta.as_deref().and_then(host_in_meta);
    let want = host_of(i.socket);
    let points_here = host.as_deref() == Some(want.as_str());
    let current_name = current_of(cfg.as_deref());
    let is_current = current_name.as_deref() == Some(NAME);
    let overridden = if i.env_host.is_some_and(|h| !h.is_empty()) {
        Some("DOCKER_HOST")
    } else if i.env_context.is_some_and(|c| !c.is_empty() && c != NAME) {
        Some("DOCKER_CONTEXT")
    } else {
        None
    };
    let (state, summary, next): (&str, String, Option<&str>) = if !i.docker_cli {
        (
            "no-docker-cli",
            format!(
                "The docker command is not installed on this machine; {}. The context is made for you afterwards.",
                install_hint()
            ),
            Some("fluxctl docker context install"),
        )
    } else if !exists {
        (
            "not-installed",
            "There is no `fluxvm` docker context yet. Create it to use `docker --context fluxvm ...`.".into(),
            Some("fluxctl docker context install"),
        )
    } else if !points_here {
        (
            "stale",
            format!(
                "The `fluxvm` context points at {} but the engine socket is {want}. Refresh it.",
                host.as_deref().unwrap_or("nothing")
            ),
            Some("fluxctl docker context install"),
        )
    } else if !i.socket_answers {
        (
            "socket-down",
            "The `fluxvm` context exists but the engine's socket does not answer. Start the engine.".into(),
            Some("fluxctl docker up"),
        )
    } else if is_current {
        (
            "current",
            "`docker` uses FluxVM's engine by default.".into(),
            None,
        )
    } else {
        (
            "ready",
            "The `fluxvm` context works. Use `docker --context fluxvm ps` for one command, or make it the default for `docker`.".into(),
            Some("fluxctl docker context install --use"),
        )
    };
    let mut summary = summary;
    if let Some(o) = overridden.filter(|_| is_current) {
        summary.push_str(&format!(
            " But {o} is set in the environment, and it wins over the current context."
        ));
    }
    json!({
        "name": NAME,
        "state": state,
        "summary": summary,
        "next": next,
        "docker_cli": i.docker_cli,
        "docker_cli_hint": (!i.docker_cli).then(install_hint),
        "config_dir": i.dir.map(|d| d.display().to_string()),
        "exists": exists,
        "host": host,
        "points_here": points_here,
        "current": is_current,
        "current_context": current_name,
        "overridden_by": overridden,
        "socket": i.socket.display().to_string(),
        "socket_answers": i.socket_answers,
        "try": format!("docker --context {NAME} ps"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_points_a_dead_context_at_a_live_socket() {
        let live = |p: &Path| p == Path::new("/run/b.sock");
        let c = [PathBuf::from("/run/a.sock"), PathBuf::from("/run/b.sock")];
        assert_eq!(plan_repair(None, &live, &c), Repair::NoContext);
        assert_eq!(
            plan_repair(Some("tcp://h:1"), &live, &c),
            Repair::Foreign("tcp://h:1".into())
        );
        assert_eq!(
            plan_repair(Some("unix:///run/b.sock"), &live, &c),
            Repair::Healthy("/run/b.sock".into())
        );
        assert_eq!(
            plan_repair(Some("unix:///run/dead.sock"), &live, &c),
            Repair::Repoint {
                from: "/run/dead.sock".into(),
                to: "/run/b.sock".into()
            }
        );
        assert_eq!(
            plan_repair(Some("unix:///run/dead.sock"), &|_| false, &c),
            Repair::NothingAnswers {
                from: "/run/dead.sock".into()
            }
        );
    }

    #[test]
    fn meta_json_is_dockers_exact_format() {
        assert_eq!(
            meta_json("fluxvm", "unix:///tmp/x.sock"),
            r#"{"Name":"fluxvm","Metadata":{"Description":"FluxVM Docker engine (fluxctl docker up)"},"Endpoints":{"docker":{"Host":"unix:///tmp/x.sock","SkipTLSVerify":false}}}"#
        );
    }

    #[test]
    fn meta_dir_is_sha256_of_the_name() {
        // echo -n fluxvm | sha256sum
        assert_eq!(
            meta_dir_name("fluxvm"),
            "f34e890cdea0036c30b27ad8b02fe1a886f74d0d05f6111bd9e907d6f809347f"
        );
        assert_eq!(meta_dir_name("fluxvm").len(), 64);
        assert_eq!(
            meta_dir_name("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn config_dir_prefers_docker_config() {
        assert_eq!(
            config_dir(Some("/c"), Some("/h")),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            config_dir(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.docker"))
        );
        assert_eq!(config_dir(None, None), None);
    }

    #[test]
    fn current_context_keeps_every_other_key() {
        let old = r#"{"auths":{"r.io":{"auth":"x"}},"credsStore":"desktop","currentContext":"other","plugins":{"a":1}}"#;
        let new = config_with_current(Some(old), Some("fluxvm")).unwrap();
        let v: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(v["currentContext"], "fluxvm");
        assert_eq!(v["credsStore"], "desktop");
        assert_eq!(v["auths"]["r.io"]["auth"], "x");
        assert_eq!(v["plugins"]["a"], 1);
        assert_eq!(v.as_object().unwrap().len(), 4);
        assert!(new.ends_with("}\n") && new.contains("\n\t\"credsStore\""));
        let gone = config_with_current(Some(&new), None).unwrap();
        assert!(!gone.contains("currentContext") && gone.contains("credsStore"));
        assert_eq!(
            config_with_current(None, Some("fluxvm")).unwrap(),
            "{\n\t\"currentContext\": \"fluxvm\"\n}\n"
        );
    }

    #[test]
    fn a_broken_config_is_refused_not_overwritten() {
        for bad in ["{not json", "[1]", "\"x\""] {
            let e = config_with_current(Some(bad), Some("fluxvm")).unwrap_err();
            assert_eq!(e.code, "DOCKER_CONFIG_INVALID");
            assert!(e.to_string().contains("fluxctl explain"));
        }
    }

    #[test]
    fn install_writes_only_the_fluxvm_context_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let other = t.path().join("contexts/meta").join(meta_dir_name("prod"));
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("meta.json"), "PROD").unwrap();
        fs::write(
            t.path().join("config.json"),
            r#"{"currentContext":"prod","x":1}"#,
        )
        .unwrap();
        let sock = Path::new("/tmp/fluxvm-run-1/docker-engine.sock");
        let a = install(t.path(), sock, false).unwrap();
        assert!(a.changed && !a.made_current);
        assert_eq!(
            fs::read_to_string(&a.meta).unwrap(),
            meta_json("fluxvm", "unix:///tmp/fluxvm-run-1/docker-engine.sock")
        );
        // not switched
        assert_eq!(
            current_of(read(&t.path().join("config.json")).as_deref()).as_deref(),
            Some("prod")
        );
        assert!(!install(t.path(), sock, false).unwrap().changed);
        let b = install(t.path(), sock, true).unwrap();
        assert!(b.made_current && !b.changed);
        let cfg: Value =
            serde_json::from_str(&fs::read_to_string(t.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(cfg["currentContext"], "fluxvm");
        assert_eq!(cfg["x"], 1);
        assert_eq!(fs::read_to_string(other.join("meta.json")).unwrap(), "PROD");
        let u = uninstall(t.path()).unwrap();
        assert!(u.removed && u.reset_current);
        assert!(!a.meta.exists());
        assert_eq!(fs::read_to_string(other.join("meta.json")).unwrap(), "PROD");
        let cfg: Value =
            serde_json::from_str(&fs::read_to_string(t.path().join("config.json")).unwrap())
                .unwrap();
        assert!(cfg.get("currentContext").is_none() && cfg["x"] == 1);
        let again = uninstall(t.path()).unwrap();
        assert!(!again.removed && !again.reset_current);
        // no temp files left behind
        let left: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(left.is_empty());
    }

    #[test]
    fn uninstall_leaves_another_current_context_alone() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("config.json"), r#"{"currentContext":"prod"}"#).unwrap();
        install(t.path(), Path::new("/s.sock"), false).unwrap();
        let u = uninstall(t.path()).unwrap();
        assert!(u.removed && !u.reset_current);
        assert!(
            read(&t.path().join("config.json"))
                .unwrap()
                .contains("prod")
        );
    }

    #[test]
    fn install_refuses_a_bad_config_before_writing_anything() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("config.json"), "{oops").unwrap();
        let e = install(t.path(), Path::new("/s.sock"), true).unwrap_err();
        assert_eq!(e.code, "DOCKER_CONFIG_INVALID");
        assert!(!meta_path(t.path()).exists());
        assert_eq!(read(&t.path().join("config.json")).unwrap(), "{oops");
        assert!(install(t.path(), Path::new("rel.sock"), false).is_err());
    }

    #[test]
    fn install_into_an_unwritable_place_says_what_to_do() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("afile");
        fs::write(&file, "x").unwrap();
        let e = install(&file, Path::new("/s.sock"), false).unwrap_err();
        assert_eq!(e.code, "DOCKER_CONTEXT_UNWRITABLE");
        assert!(e.message.contains("DOCKER_CONFIG"));
    }

    fn st<'a>(dir: Option<&'a Path>, sock: &'a Path, answers: bool, cli: bool) -> StatusInput<'a> {
        StatusInput {
            dir,
            socket: sock,
            socket_answers: answers,
            docker_cli: cli,
            env_context: None,
            env_host: None,
        }
    }

    #[test]
    fn status_walks_through_every_state() {
        let t = tempfile::tempdir().unwrap();
        let sock = Path::new("/tmp/e.sock");
        let state = |i: &StatusInput| status(i)["state"].as_str().unwrap().to_owned();
        assert_eq!(
            state(&st(Some(t.path()), sock, true, false)),
            "no-docker-cli"
        );
        let s = status(&st(Some(t.path()), sock, true, false));
        assert!(s["docker_cli_hint"].as_str().unwrap().contains("install"));
        assert_eq!(
            state(&st(Some(t.path()), sock, true, true)),
            "not-installed"
        );
        install(t.path(), Path::new("/tmp/old.sock"), false).unwrap();
        assert_eq!(state(&st(Some(t.path()), sock, true, true)), "stale");
        install(t.path(), sock, false).unwrap();
        let down = status(&st(Some(t.path()), sock, false, true));
        assert_eq!(down["state"], "socket-down");
        assert_eq!(down["next"], "fluxctl docker up");
        let ready = status(&st(Some(t.path()), sock, true, true));
        assert_eq!(ready["state"], "ready");
        assert_eq!(ready["next"], "fluxctl docker context install --use");
        assert_eq!(ready["try"], "docker --context fluxvm ps");
        install(t.path(), sock, true).unwrap();
        let cur = status(&st(Some(t.path()), sock, true, true));
        assert_eq!(cur["state"], "current");
        assert!(cur["next"].is_null() && cur["current"] == true && cur["exists"] == true);
        let mut env = st(Some(t.path()), sock, true, true);
        env.env_host = Some("tcp://x:2375");
        let o = status(&env);
        assert_eq!(o["overridden_by"], "DOCKER_HOST");
        assert!(o["summary"].as_str().unwrap().contains("DOCKER_HOST"));
        assert_eq!(state(&st(None, sock, true, true)), "not-installed");
    }

    #[test]
    fn finds_the_docker_cli_on_a_path() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        assert!(docker_cli_in(Some(t.path().as_os_str())).is_none());
        let p = t.path().join("docker");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        assert!(docker_cli_in(Some(t.path().as_os_str())).is_none());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(docker_cli_in(Some(t.path().as_os_str())), Some(p));
        assert!(docker_cli_in(None).is_none());
    }
}
