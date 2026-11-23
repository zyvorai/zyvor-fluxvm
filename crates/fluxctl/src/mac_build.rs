//! Macfile parser and build planner for `fluxctl mac build`.
//!
//! Pure and dependency-light: no I/O, no VM access. A Macfile is parsed into
//! [`Macfile`] and turned into an ordered list of [`Step`]s by [`plan`]; the
//! executor (not part of this module) runs those steps in a throwaway
//! instance. See `docs/macos-containers-design.md`, "Build".
//!
//! Syntax: one directive per line (`FROM`, `USER`, `RUN`, `COPY`, `ADD`, `ENV`,
//! `ARG`, `WORKDIR`, `LABEL`, `EXPOSE`, `CMD`, `ENTRYPOINT`, `HEALTHCHECK`,
//! `SNAPSHOT`), `#` comment lines, blank lines, a trailing `\` joins the next
//! line, and `"..."` / `'...'` quote arguments for `FROM`, `USER`, `COPY`
//! and `SNAPSHOT`. `RUN` keeps its text verbatim for the shell.

use crate::mac_config::{
    Cmd, Healthcheck, HistoryEntry, ImageConfig, expand, parse_duration_secs, prefix, sq,
};
use std::collections::BTreeMap;
use std::fmt;

/// What a Macfile builds on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// A template reference such as `macos-base:15`.
    Template(String),
    /// `IPSW latest`: install from the latest restore image.
    IpswLatest,
    /// `IPSW /abs/path/UniversalMac.ipsw`: install from a restore image on this Mac.
    IpswPath(String),
}

/// One parsed directive (without `FROM`, which lives on [`Macfile`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    User(String),
    Run(String),
    Copy {
        src: String,
        dst: String,
    },
    /// Like COPY, but `src` may be an `http(s)://` URL (downloaded in the guest with `curl`).
    Add {
        src: String,
        dst: String,
    },
    /// `ENV K=V ...`: set for later RUN steps and recorded in the image config.
    Env(Vec<(String, String)>),
    /// `ARG NAME[=default]`: a build-time variable (`--build-arg NAME=V`), exported to later RUN steps.
    Arg {
        name: String,
        default: Option<String>,
    },
    Workdir(String),
    Label(Vec<(String, String)>),
    Expose(Vec<String>),
    Cmd(Cmd),
    Entrypoint(Cmd),
    /// `HEALTHCHECK NONE` is `None`. Recorded in the image config only: nothing runs it.
    Healthcheck(Option<Healthcheck>),
    Snapshot(String),
}

/// A directive with the line it started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub line: usize,
    pub directive: Directive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macfile {
    pub from: Base,
    pub from_line: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Macfile line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        line,
        message: message.into(),
    })
}

/// Join backslash-continued lines, dropping comment and blank lines.
/// Returns `(first line number, text)` pairs.
fn logical_lines(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    for (i, raw) in src.lines().enumerate() {
        let n = i + 1;
        let raw = raw.trim_end_matches('\r');
        if cur.is_none() {
            let t = raw.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
        } else if raw.trim_start().starts_with('#') {
            // A comment line inside a continuation is skipped.
            continue;
        }
        let (text, cont) = match raw.trim_end().strip_suffix('\\') {
            Some(t) => (t, true),
            None => (raw, false),
        };
        match cur.as_mut() {
            Some((_, s)) => {
                s.push(' ');
                s.push_str(text.trim());
            }
            None => cur = Some((n, text.trim().to_string())),
        }
        if !cont {
            out.extend(cur.take());
        }
    }
    // A dangling continuation at EOF still counts as a line.
    out.extend(cur.take());
    out
}

/// Split into words, honouring `"..."` and `'...'`.
fn split_args(line: usize, s: &str) -> Result<Vec<String>, ParseError> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    args.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return err(line, "unterminated quote");
    }
    if in_word {
        args.push(cur);
    }
    Ok(args)
}

/// A COPY source must be a relative path that stays inside the build context.
fn check_copy_source(line: usize, src: &str) -> Result<(), ParseError> {
    let bytes = src.as_bytes();
    let drive = bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    if src.starts_with('/') || src.starts_with('\\') || src.starts_with('~') || drive {
        return err(
            line,
            format!("COPY source '{src}' must be relative to the build context"),
        );
    }
    let mut depth: i32 = 0;
    for part in src.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return err(
                        line,
                        format!("COPY source '{src}' escapes the build context"),
                    );
                }
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn valid_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// `K=V K2=V2` or, for ENV only, the old `K some value` form.
fn parse_pairs(
    line: usize,
    kw: &str,
    args: &[String],
    allow_legacy: bool,
) -> Result<Vec<(String, String)>, ParseError> {
    if args.is_empty() {
        return err(line, format!("{kw} needs KEY=value"));
    }
    if !args[0].contains('=') {
        if allow_legacy && args.len() >= 2 && valid_name(&args[0]) {
            return Ok(vec![(args[0].clone(), args[1..].join(" "))]);
        }
        return err(line, format!("{kw} needs KEY=value, got '{}'", args[0]));
    }
    let mut out = Vec::new();
    for a in args {
        let Some((k, v)) = a.split_once('=') else {
            return err(line, format!("{kw} needs KEY=value, got '{a}'"));
        };
        // LABEL keys may contain dots and dashes (`org.example.x`); ENV keys may not.
        let ok = if kw == "ENV" {
            valid_name(k)
        } else {
            !k.is_empty() && !k.contains(char::is_whitespace)
        };
        if !ok {
            return err(line, format!("{kw} key '{k}' is not valid"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn check_port(line: usize, p: &str) -> Result<(), ParseError> {
    let (num, proto) = p.split_once('/').unwrap_or((p, "tcp"));
    let ok = num.parse::<u16>().is_ok_and(|n| n > 0) && matches!(proto, "tcp" | "udp");
    if ok {
        Ok(())
    } else {
        err(
            line,
            format!("EXPOSE '{p}' is not a port (1-65535, optional /tcp or /udp)"),
        )
    }
}

/// `NONE`, or `[--interval=D] [--timeout=D] [--retries=N] CMD command`.
fn parse_healthcheck(line: usize, rest: &str) -> Result<Option<Healthcheck>, ParseError> {
    let mut rest = rest.trim();
    if rest.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let (mut interval, mut timeout, mut retries, mut start_period) = (30u64, 30u64, 3u32, 0u64);
    while let Some(opt) = rest.strip_prefix("--") {
        let (word, tail) = opt.split_once(char::is_whitespace).unwrap_or((opt, ""));
        let Some((k, v)) = word.split_once('=') else {
            return err(line, format!("HEALTHCHECK option '--{word}' needs =value"));
        };
        let dur = |v: &str| {
            parse_duration_secs(v).map_err(|e| ParseError {
                line,
                message: format!("HEALTHCHECK --{k}: {e}"),
            })
        };
        match k {
            "interval" => interval = dur(v)?,
            "timeout" => timeout = dur(v)?,
            "start-period" => start_period = dur(v)?,
            "retries" => {
                retries = v.parse().map_err(|_| ParseError {
                    line,
                    message: format!("HEALTHCHECK --retries '{v}' is not a number"),
                })?
            }
            _ => return err(line, format!("unknown HEALTHCHECK option '--{k}'")),
        }
        rest = tail.trim_start();
    }
    let Some(cmd) = rest
        .split_once(char::is_whitespace)
        .filter(|(k, _)| k.eq_ignore_ascii_case("cmd"))
        .map(|(_, c)| c)
    else {
        return err(line, "HEALTHCHECK needs NONE or [options] CMD command");
    };
    let cmd = Cmd::parse(cmd).map_err(|m| ParseError {
        line,
        message: format!("HEALTHCHECK CMD {m}"),
    })?;
    Ok(Some(Healthcheck {
        cmd,
        interval_secs: interval,
        timeout_secs: timeout,
        retries,
        start_period_secs: start_period,
    }))
}

/// What a `RUN --mount=...` hands to one step. Nothing here holds a secret's value, only its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// `type=secret,id=NAME[,target=/path]`: the `--secret id=NAME,...` file, present in the guest for this step only.
    Secret { id: String, target: Option<String> },
    /// `type=ssh`: the host's SSH agent is forwarded into the guest for this step (`--ssh default`).
    Ssh,
}

fn mount_word_ok(s: &str, extra: &[char]) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') || extra.contains(&c)
        })
}

/// Split the leading `--mount=...` flags off a `RUN` line. Returns the mounts and the command that follows them.
pub fn split_mounts(cmd: &str) -> Result<(Vec<Mount>, &str), String> {
    let mut rest = cmd.trim_start();
    let mut mounts = Vec::new();
    while let Some(after) = rest.strip_prefix("--mount=") {
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let (spec, tail) = after.split_at(end);
        let (mut ty, mut id, mut target) = (None, None, None);
        for kv in spec.split(',') {
            match kv.split_once('=') {
                Some(("type", v)) => ty = Some(v),
                Some(("id", v)) => id = Some(v),
                Some(("target" | "dst" | "destination", v)) => target = Some(v),
                // Docker accepts these on secret mounts; they change nothing here.
                Some(("required" | "mode" | "uid" | "gid", _)) => {}
                _ => {
                    return Err(format!(
                        "RUN --mount={spec}: '{kv}' is not understood; use type=secret,id=NAME[,target=/path] or type=ssh"
                    ));
                }
            }
        }
        match ty {
            Some("secret") => {
                let id = id
                    .filter(|i| mount_word_ok(i, &[]))
                    .ok_or("RUN --mount=type=secret needs id=NAME (letters, digits, _ . -)")?;
                if let Some(t) = target
                    && !(t.starts_with('/') && mount_word_ok(&t[1..], &['/']) && !t.contains(".."))
                {
                    return Err(format!(
                        "RUN --mount target {t:?} must be an absolute path of letters, digits, _ . - and /"
                    ));
                }
                mounts.push(Mount::Secret {
                    id: id.to_string(),
                    target: target.map(str::to_string),
                });
            }
            Some("ssh") => {
                if !mounts.contains(&Mount::Ssh) {
                    mounts.push(Mount::Ssh)
                }
            }
            Some(other) => {
                return Err(format!(
                    "RUN --mount=type={other} is not supported on macOS builds; only type=secret and type=ssh are"
                ));
            }
            None => return Err("RUN --mount needs type=secret or type=ssh".into()),
        }
        rest = tail.trim_start();
    }
    Ok((mounts, rest))
}

/// The text after the directive keyword, trimmed.
fn rest_of(text: &str) -> &str {
    text.split_once(char::is_whitespace)
        .map(|(_, r)| r.trim())
        .unwrap_or("")
}

pub fn parse(src: &str) -> Result<Macfile, ParseError> {
    let mut from: Option<(Base, usize)> = None;
    let mut lines: Vec<Line> = Vec::new();
    let mut snapshot_line: Option<usize> = None;

    for (n, text) in logical_lines(src) {
        let keyword = text.split_whitespace().next().unwrap_or("");
        let kw = keyword.to_ascii_uppercase();
        if let Some(sl) = snapshot_line {
            if kw == "SNAPSHOT" {
                return err(
                    n,
                    format!("only one SNAPSHOT is allowed (first at line {sl})"),
                );
            }
            return err(
                n,
                format!("SNAPSHOT (line {sl}) must be the last directive"),
            );
        }
        if from.is_none() && kw != "FROM" {
            return err(n, "the first directive must be FROM");
        }
        match kw.as_str() {
            "FROM" => {
                if let Some((_, fl)) = &from {
                    return err(n, format!("duplicate FROM (first at line {fl})"));
                }
                let args = split_args(n, rest_of(&text))?;
                let base = match args.as_slice() {
                    [] => return err(n, "FROM needs a template reference or 'IPSW latest'"),
                    [a, b] if a.eq_ignore_ascii_case("ipsw") => {
                        if b.eq_ignore_ascii_case("latest") {
                            Base::IpswLatest
                        } else {
                            match crate::mac_ipsw::check_path_syntax(b) {
                                Ok(()) => Base::IpswPath(b.clone()),
                                Err(m) => {
                                    return err(
                                        n,
                                        format!(
                                            "unsupported IPSW selector '{b}' (only 'latest', or an absolute path to a .ipsw file): {m}"
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    [a] if a.eq_ignore_ascii_case("ipsw") => {
                        return err(n, "FROM IPSW needs a selector ('latest')");
                    }
                    [a] => Base::Template(a.clone()),
                    _ => return err(n, "FROM takes exactly one template reference"),
                };
                from = Some((base, n));
            }
            "USER" => {
                let args = split_args(n, rest_of(&text))?;
                match args.as_slice() {
                    [u] => lines.push(Line {
                        line: n,
                        directive: Directive::User(u.clone()),
                    }),
                    _ => return err(n, "USER takes exactly one user name"),
                }
            }
            "RUN" => {
                let cmd = rest_of(&text);
                let (_, body) = match split_mounts(cmd) {
                    Ok(x) => x,
                    Err(m) => return err(n, m),
                };
                if body.is_empty() {
                    return err(n, "RUN needs a command");
                }
                lines.push(Line {
                    line: n,
                    directive: Directive::Run(cmd.to_string()),
                });
            }
            "COPY" => {
                let args = split_args(n, rest_of(&text))?;
                let [src, dst] = args.as_slice() else {
                    return err(
                        n,
                        format!(
                            "COPY needs exactly 2 arguments (src dst), got {}",
                            args.len()
                        ),
                    );
                };
                check_copy_source(n, src)?;
                lines.push(Line {
                    line: n,
                    directive: Directive::Copy {
                        src: src.clone(),
                        dst: dst.clone(),
                    },
                });
            }
            "ADD" => {
                let args = split_args(n, rest_of(&text))?;
                let [src, dst] = args.as_slice() else {
                    return err(
                        n,
                        format!(
                            "ADD needs exactly 2 arguments (src dst), got {}",
                            args.len()
                        ),
                    );
                };
                if !is_url(src) {
                    check_copy_source(n, src)?;
                }
                lines.push(Line {
                    line: n,
                    directive: Directive::Add {
                        src: src.clone(),
                        dst: dst.clone(),
                    },
                });
            }
            "ENV" => {
                let args = split_args(n, rest_of(&text))?;
                lines.push(Line {
                    line: n,
                    directive: Directive::Env(parse_pairs(n, "ENV", &args, true)?),
                });
            }
            "LABEL" => {
                let args = split_args(n, rest_of(&text))?;
                lines.push(Line {
                    line: n,
                    directive: Directive::Label(parse_pairs(n, "LABEL", &args, false)?),
                });
            }
            "ARG" => {
                let args = split_args(n, rest_of(&text))?;
                let [a] = args.as_slice() else {
                    return err(n, "ARG takes exactly one NAME or NAME=default");
                };
                let (name, default) = match a.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (a.as_str(), None),
                };
                if !valid_name(name) {
                    return err(n, format!("ARG name '{name}' is not valid"));
                }
                lines.push(Line {
                    line: n,
                    directive: Directive::Arg {
                        name: name.to_string(),
                        default,
                    },
                });
            }
            "WORKDIR" => {
                let args = split_args(n, rest_of(&text))?;
                match args.as_slice() {
                    [w] => lines.push(Line {
                        line: n,
                        directive: Directive::Workdir(w.clone()),
                    }),
                    _ => return err(n, "WORKDIR takes exactly one path"),
                }
            }
            "EXPOSE" => {
                let args = split_args(n, rest_of(&text))?;
                if args.is_empty() {
                    return err(n, "EXPOSE needs at least one port");
                }
                for a in &args {
                    // `$VAR` is expanded at plan time, so only literal ports are checked here.
                    if !a.contains('$') {
                        check_port(n, a)?;
                    }
                }
                lines.push(Line {
                    line: n,
                    directive: Directive::Expose(args),
                });
            }
            "CMD" | "ENTRYPOINT" => {
                let c = Cmd::parse(rest_of(&text)).map_err(|m| ParseError {
                    line: n,
                    message: format!("{kw} {m}"),
                })?;
                lines.push(Line {
                    line: n,
                    directive: if kw == "CMD" {
                        Directive::Cmd(c)
                    } else {
                        Directive::Entrypoint(c)
                    },
                });
            }
            "HEALTHCHECK" => {
                lines.push(Line {
                    line: n,
                    directive: Directive::Healthcheck(parse_healthcheck(n, rest_of(&text))?),
                });
            }
            "SNAPSHOT" => {
                let args = split_args(n, rest_of(&text))?;
                match args.as_slice() {
                    [t] => lines.push(Line {
                        line: n,
                        directive: Directive::Snapshot(t.clone()),
                    }),
                    _ => return err(n, "SNAPSHOT takes exactly one tag"),
                }
                snapshot_line = Some(n);
            }
            _ => return err(n, format!("unknown directive '{keyword}'")),
        }
    }

    match from {
        Some((from, from_line)) => Ok(Macfile {
            from,
            from_line,
            lines,
        }),
        None => err(1, "empty Macfile: a FROM directive is required"),
    }
}

/// One executable build step, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create the build instance from a template.
    CloneTemplate { reference: String },
    /// Install a fresh macOS from the latest IPSW.
    InstallIpswLatest,
    /// Install a fresh macOS from the restore image at `path`.
    InstallIpswPath { path: String },
    /// Run `command` over SSH as `user` (None = the template's default user).
    Run {
        line: usize,
        user: Option<String>,
        command: String,
        /// Secrets and the SSH agent handed to this step only (`RUN --mount=...`).
        mounts: Vec<Mount>,
    },
    /// Copy `src` (relative to the build context) to `dst` in the guest.
    Copy {
        line: usize,
        user: Option<String>,
        src: String,
        dst: String,
    },
    /// Stop the instance and store it as the result template.
    StoreTemplate,
    /// Also save running state under `tag` (host-bound).
    Snapshot { line: usize, tag: String },
}

/// Turn a parsed Macfile into ordered steps. `USER` is folded into the
/// `user` of the following `RUN`/`COPY` steps rather than being a step.
#[cfg(test)]
pub fn plan(file: &Macfile) -> Vec<Step> {
    plan_with(file, &BTreeMap::new()).0
}

/// [`plan`] with `--build-arg` values, also returning the image configuration the build records.
///
/// `ENV`, `ARG` and `WORKDIR` are folded into the `RUN` steps that follow (`export K='v'; cd 'dir' && cmd`) and into the
/// config, so the executor needs no new step kinds. `ARG` values are visible to `RUN` and to `$VAR` expansion in later
/// directives, but are not recorded in the image. `ADD URL` becomes a `curl` RUN step.
pub fn plan_with(
    file: &Macfile,
    build_args: &BTreeMap<String, String>,
) -> (Vec<Step>, ImageConfig) {
    let mut steps = vec![match &file.from {
        Base::Template(r) => Step::CloneTemplate {
            reference: r.clone(),
        },
        Base::IpswLatest => Step::InstallIpswLatest,
        Base::IpswPath(p) => Step::InstallIpswPath { path: p.clone() },
    }];
    let mut cfg = ImageConfig::default();
    let mut user: Option<String> = None;
    let mut snapshot: Option<Step> = None;
    let mut args: Vec<(String, String)> = Vec::new();
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let mut workdir: Option<String> = None;
    let history = |cfg: &mut ImageConfig, text: String| {
        cfg.history.push(HistoryEntry {
            created_unix: 0,
            created_by: text,
            comment: String::new(),
        })
    };
    let dest = |wd: &Option<String>, dst: &str| match wd {
        Some(w) if !dst.starts_with('/') && !dst.starts_with('~') => {
            format!("{}/{dst}", w.trim_end_matches('/'))
        }
        _ => dst.to_string(),
    };
    for l in &file.lines {
        match &l.directive {
            Directive::User(u) => {
                let u = expand(u, &vars);
                history(&mut cfg, format!("USER {u}"));
                cfg.user = Some(u.clone());
                user = Some(u);
            }
            Directive::Run(c) => {
                history(&mut cfg, format!("RUN {c}"));
                let (mounts, body) = split_mounts(c).unwrap_or((Vec::new(), c));
                let mut env = args.clone();
                env.extend(cfg.env.iter().cloned());
                steps.push(Step::Run {
                    line: l.line,
                    user: user.clone(),
                    command: format!("{}{body}", prefix(&env, workdir.as_deref())),
                    mounts,
                })
            }
            Directive::Copy { src, dst } => {
                let (src, dst) = (expand(src, &vars), expand(dst, &vars));
                history(&mut cfg, format!("COPY {src} {dst}"));
                steps.push(Step::Copy {
                    line: l.line,
                    user: user.clone(),
                    src,
                    dst: dest(&workdir, &dst),
                })
            }
            Directive::Add { src, dst } => {
                let (src, dst) = (expand(src, &vars), expand(dst, &vars));
                history(&mut cfg, format!("ADD {src} {dst}"));
                let dst = dest(&workdir, &dst);
                if is_url(&src) {
                    let mut env = args.clone();
                    env.extend(cfg.env.iter().cloned());
                    steps.push(Step::Run {
                        line: l.line,
                        user: user.clone(),
                        command: format!(
                            "{}curl -fsSL {} -o {}",
                            prefix(&env, None),
                            sq(&src),
                            sq(&dst)
                        ),
                        mounts: Vec::new(),
                    });
                } else {
                    steps.push(Step::Copy {
                        line: l.line,
                        user: user.clone(),
                        src,
                        dst,
                    });
                }
            }
            Directive::Env(pairs) => {
                for (k, v) in pairs {
                    let v = expand(v, &vars);
                    history(&mut cfg, format!("ENV {k}={v}"));
                    vars.insert(k.clone(), v.clone());
                    cfg.set_env(k, &v);
                }
            }
            Directive::Arg { name, default } => {
                let v = build_args
                    .get(name)
                    .cloned()
                    .or_else(|| default.as_ref().map(|d| expand(d, &vars)))
                    .unwrap_or_default();
                history(&mut cfg, format!("ARG {name}"));
                vars.insert(name.clone(), v.clone());
                match args.iter_mut().find(|(k, _)| k == name) {
                    Some(a) => a.1 = v,
                    None => args.push((name.clone(), v)),
                }
            }
            Directive::Workdir(w) => {
                let w = expand(w, &vars);
                let full = match &workdir {
                    Some(cur) if !w.starts_with('/') => {
                        format!("{}/{w}", cur.trim_end_matches('/'))
                    }
                    _ => w,
                };
                history(&mut cfg, format!("WORKDIR {full}"));
                cfg.workdir = Some(full.clone());
                workdir = Some(full);
            }
            Directive::Label(pairs) => {
                for (k, v) in pairs {
                    let v = expand(v, &vars);
                    history(&mut cfg, format!("LABEL {k}={v}"));
                    cfg.labels.insert(k.clone(), v);
                }
            }
            Directive::Expose(ports) => {
                for p in ports {
                    let p = expand(p, &vars);
                    history(&mut cfg, format!("EXPOSE {p}"));
                    let p = if p.contains('/') {
                        p
                    } else {
                        format!("{p}/tcp")
                    };
                    if !cfg.expose.contains(&p) {
                        cfg.expose.push(p);
                    }
                }
            }
            Directive::Cmd(c) => {
                history(&mut cfg, format!("CMD {}", c.to_shell()));
                cfg.cmd = Some(c.clone());
            }
            Directive::Entrypoint(c) => {
                history(&mut cfg, format!("ENTRYPOINT {}", c.to_shell()));
                cfg.entrypoint = Some(c.clone());
            }
            Directive::Healthcheck(h) => {
                history(
                    &mut cfg,
                    match h {
                        Some(h) => format!("HEALTHCHECK CMD {}", h.cmd.to_shell()),
                        None => "HEALTHCHECK NONE".into(),
                    },
                );
                cfg.healthcheck = h.clone();
            }
            Directive::Snapshot(t) => {
                snapshot = Some(Step::Snapshot {
                    line: l.line,
                    tag: t.clone(),
                })
            }
        }
    }
    steps.push(Step::StoreTemplate);
    // SNAPSHOT is validated to be last, so it stays last in the plan.
    steps.extend(snapshot);
    (steps, cfg)
}

/// ` [secret NAME, ssh agent]`, naming what a step mounts. Never a value.
fn mount_note(mounts: &[Mount]) -> String {
    if mounts.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = mounts
        .iter()
        .map(|m| match m {
            Mount::Secret { id, .. } => format!("secret {id}"),
            Mount::Ssh => "ssh agent".to_string(),
        })
        .collect();
    format!(" [{}]", parts.join(", "))
}

/// The only platform a macOS image runs on: Apple silicon, as `darwin/arm64`.
pub const PLATFORM: &str = "darwin/arm64";

/// Check a `--platform` value. `darwin/arm64`, `macos/arm64`, `darwin`, `macos` and `arm64` all mean this Mac's guests.
pub fn check_platform(p: &str) -> Result<(), String> {
    let p = p.trim().to_ascii_lowercase();
    let ok = matches!(
        p.as_str(),
        "darwin"
            | "macos"
            | "arm64"
            | "aarch64"
            | "darwin/arm64"
            | "macos/arm64"
            | "darwin/aarch64"
    );
    if ok {
        return Ok(());
    }
    Err(format!(
        "--platform {p}: a macOS image can only be {PLATFORM} (Apple silicon). Use --platform {PLATFORM}, or leave it out. \
         For a Linux image use `docker build --platform {p}` instead"
    ))
}

/// Problems with `--build-arg` that are worth telling the user about before a long build starts. None stops the build.
pub fn check_build_args(file: &Macfile, args: &BTreeMap<String, String>) -> Vec<String> {
    let declared: Vec<&str> = file
        .lines
        .iter()
        .filter_map(|l| match &l.directive {
            Directive::Arg { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for (k, _) in args {
        if !declared.contains(&k.as_str()) {
            let near = declared
                .iter()
                .find(|d| d.eq_ignore_ascii_case(k) || edit_distance_1(d, k));
            out.push(match near {
                Some(d) => format!(
                    "--build-arg {k} is not used: the Macfile declares ARG {d}. Check the spelling (names are case-sensitive)"
                ),
                None => format!(
                    "--build-arg {k} is not used: the Macfile has no `ARG {k}` line. Add `ARG {k}` before the step that needs it, or drop the flag"
                ),
            });
        }
        let up = k.to_ascii_uppercase();
        if [
            "TOKEN",
            "PASSWORD",
            "PASSWD",
            "SECRET",
            "APIKEY",
            "API_KEY",
            "PRIVATE_KEY",
        ]
        .iter()
        .any(|w| up.contains(w))
        {
            out.push(format!(
                "--build-arg {k} looks like a secret. Build args are visible to every RUN and to anyone who can read the build command; pass it with `--secret id={},src=FILE` and `RUN --mount=type=secret,id={}` instead",
                k.to_ascii_lowercase(),
                k.to_ascii_lowercase()
            ));
        }
    }
    // An ARG with no default, no --build-arg, that a later line expands: it silently becomes empty.
    let text: String = file
        .lines
        .iter()
        .filter(|l| !matches!(l.directive, Directive::Arg { .. }))
        .map(|l| format!("{:?}\n", l.directive))
        .collect();
    for l in &file.lines {
        if let Directive::Arg {
            name,
            default: None,
        } = &l.directive
            && !args.contains_key(name)
            && (text.contains(&format!("${name}")) || text.contains(&format!("${{{name}")))
        {
            out.push(format!(
                "line {}: ARG {name} has no default and no --build-arg {name}=..., so it expands to an empty string. Pass --build-arg {name}=VALUE or give the ARG a default",
                l.line
            ));
        }
    }
    out
}

fn edit_distance_1(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let i = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let (ta, tb) = (&a[i..], &b[i..]);
    match ta.len().cmp(&tb.len()) {
        std::cmp::Ordering::Equal => ta.get(1..) == tb.get(1..),
        std::cmp::Ordering::Less => ta == &tb[1..],
        std::cmp::Ordering::Greater => &ta[1..] == tb,
    }
}

/// `--label KEY=VALUE` on the command line: they override LABEL lines of the same name, as in Docker.
pub fn apply_labels(cfg: &mut ImageConfig, labels: &[String]) -> Result<(), String> {
    for l in labels {
        let (k, v) = l
            .split_once('=')
            .filter(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
            .ok_or_else(|| {
                format!("--label {l:?} must be KEY=VALUE, with no spaces in KEY (for example --label team=ci)")
            })?;
        cfg.history.push(HistoryEntry {
            created_unix: 0,
            created_by: format!("LABEL {k}={v}"),
            comment: String::new(),
        });
        cfg.labels.insert(k.to_string(), v.to_string());
    }
    Ok(())
}

/// Human-readable dry-run listing, one numbered step per line.
pub fn render_dry_run(steps: &[Step]) -> String {
    let who = |u: &Option<String>| u.as_deref().map(|u| format!(" as {u}")).unwrap_or_default();
    let mut out = String::new();
    for (i, s) in steps.iter().enumerate() {
        let text = match s {
            Step::CloneTemplate { reference } => format!("clone template {reference}"),
            Step::InstallIpswLatest => "install macOS from the latest IPSW".to_string(),
            Step::InstallIpswPath { path } => format!("install macOS from the IPSW {path}"),
            Step::Run {
                line,
                user,
                command,
                mounts,
            } => format!(
                "run{} (line {line}): {command}{}",
                who(user),
                mount_note(mounts)
            ),
            Step::Copy {
                line,
                user,
                src,
                dst,
            } => format!("copy{} (line {line}): {src} -> {dst}", who(user)),
            Step::StoreTemplate => "stop instance and store as template".to_string(),
            Step::Snapshot { line, tag } => {
                format!("save running state as '{tag}' (line {line}; host-bound)")
            }
        };
        out.push_str(&format!("{}. {text}\n", i + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(src: &str) -> ParseError {
        parse(src).unwrap_err()
    }

    #[test]
    fn parses_design_example() {
        let f = parse(
            "# build\nFROM macos-base:15\nUSER builder\nRUN softwareupdate --install-rosetta\n\
             COPY ./ci-tools /opt/ci-tools\nRUN brew install xcodes && xcodes install 15.4\nSNAPSHOT warm\n",
        )
        .unwrap();
        assert_eq!(f.from, Base::Template("macos-base:15".into()));
        assert_eq!(f.from_line, 2);
        assert_eq!(f.lines.len(), 5);
    }

    #[test]
    fn trailing_comment_on_from_is_not_stripped() {
        // Only whole-line comments exist; this keeps RUN text verbatim.
        assert!(parse("FROM macos-base:15  # a template\n").is_err());
    }

    #[test]
    fn from_ipsw_latest() {
        let f = parse("FROM IPSW latest\n").unwrap();
        assert_eq!(f.from, Base::IpswLatest);
        assert_eq!(plan(&f)[0], Step::InstallIpswLatest);
    }

    #[test]
    fn directives_case_insensitive() {
        let f = parse("from ipsw LATEST\nrun echo hi\n").unwrap();
        assert_eq!(f.from, Base::IpswLatest);
        assert_eq!(f.lines.len(), 1);
    }

    #[test]
    fn from_bad_ipsw_selector() {
        assert!(e("FROM IPSW 15.2\n").message.contains("only 'latest'"));
        assert!(e("FROM IPSW\n").message.contains("selector"));
    }

    #[test]
    fn from_args_errors() {
        assert!(e("FROM\n").message.contains("FROM needs"));
        assert!(e("FROM a b\n").message.contains("exactly one"));
    }

    #[test]
    fn comments_and_blanks_skipped_with_line_numbers() {
        let f = parse("\n# hi\n\nFROM base:1\n\n  # indented\n  RUN true\n").unwrap();
        assert_eq!(f.from_line, 4);
        assert_eq!(f.lines[0].line, 7);
    }

    #[test]
    fn continuation_joins_lines() {
        let f = parse("FROM b:1\nRUN brew install \\\n   git \\\n   jq\nRUN true\n").unwrap();
        assert_eq!(
            f.lines[0].directive,
            Directive::Run("brew install git jq".into())
        );
        assert_eq!(f.lines[0].line, 2);
        assert_eq!(f.lines[1].line, 5);
    }

    #[test]
    fn continuation_skips_embedded_comment_and_eof() {
        let f = parse("FROM b:1\nRUN a \\\n# note\n b \\\n").unwrap();
        assert_eq!(f.lines[0].directive, Directive::Run("a b".into()));
    }

    #[test]
    fn crlf_tolerated() {
        let f = parse("FROM b:1\r\nRUN true\r\n").unwrap();
        assert_eq!(f.lines[0].directive, Directive::Run("true".into()));
    }

    #[test]
    fn copy_quoted_args() {
        let f = parse("FROM b:1\nCOPY \"my tools/dir\" '/opt/my tools'\n").unwrap();
        assert_eq!(
            f.lines[0].directive,
            Directive::Copy {
                src: "my tools/dir".into(),
                dst: "/opt/my tools".into()
            }
        );
    }

    #[test]
    fn unterminated_quote() {
        let x = e("FROM b:1\nCOPY \"a b\n");
        assert_eq!(x.line, 2);
        assert!(x.message.contains("unterminated"));
    }

    #[test]
    fn run_keeps_quotes_verbatim() {
        let f = parse("FROM b:1\nRUN echo \"a  b\" 'c'\n").unwrap();
        assert_eq!(
            f.lines[0].directive,
            Directive::Run("echo \"a  b\" 'c'".into())
        );
    }

    #[test]
    fn from_must_be_first() {
        let x = e("# c\nRUN true\nFROM b:1\n");
        assert_eq!(x.line, 2);
        assert!(x.message.contains("must be FROM"));
    }

    #[test]
    fn empty_file_errors() {
        assert!(e("").message.contains("FROM"));
        assert!(e("# only comment\n").message.contains("FROM"));
    }

    #[test]
    fn duplicate_from() {
        let x = e("FROM a:1\nFROM b:1\n");
        assert_eq!(x.line, 2);
        assert!(x.message.contains("duplicate FROM"));
    }

    #[test]
    fn unknown_directive() {
        let x = e("FROM a:1\nMAINTAINER me\n");
        assert_eq!(x.line, 2);
        assert!(x.message.contains("unknown directive 'MAINTAINER'"));
    }

    #[test]
    fn copy_arg_count() {
        assert!(e("FROM a:1\nCOPY onlyone\n").message.contains("exactly 2"));
        assert!(e("FROM a:1\nCOPY a b c\n").message.contains("got 3"));
        assert!(e("FROM a:1\nCOPY\n").message.contains("exactly 2"));
    }

    #[test]
    fn copy_rejects_escape_and_absolute() {
        for src in [
            "../x",
            "a/../../x",
            "/etc/passwd",
            "~/x",
            "..",
            "C:\\x",
            "a\\..\\..\\b",
        ] {
            let x = e(&format!("FROM a:1\nCOPY {src} /d\n"));
            assert_eq!(x.line, 2, "{src}");
            assert!(x.message.contains("COPY source"), "{src}: {}", x.message);
        }
    }

    #[test]
    fn copy_allows_inner_dotdot_and_dot() {
        for src in ["a/../b", "./x", ".", "dir/sub/file.txt"] {
            parse(&format!("FROM a:1\nCOPY {src} /d\n")).unwrap_or_else(|x| panic!("{src}: {x}"));
        }
    }

    #[test]
    fn copy_destination_may_be_absolute_or_dotdot() {
        parse("FROM a:1\nCOPY x /opt/x\nCOPY y ../y\n").unwrap();
    }

    #[test]
    fn run_and_user_need_args() {
        assert!(e("FROM a:1\nRUN\n").message.contains("RUN needs"));
        assert!(e("FROM a:1\nUSER\n").message.contains("USER takes"));
        assert!(e("FROM a:1\nUSER a b\n").message.contains("USER takes"));
    }

    #[test]
    fn snapshot_rules() {
        let x = e("FROM a:1\nSNAPSHOT a\nSNAPSHOT b\n");
        assert_eq!(x.line, 3);
        assert!(x.message.contains("only one SNAPSHOT"));
        let x = e("FROM a:1\nSNAPSHOT a\nRUN true\n");
        assert_eq!(x.line, 3);
        assert!(x.message.contains("must be the last"));
        assert!(
            e("FROM a:1\nSNAPSHOT\n")
                .message
                .contains("exactly one tag")
        );
        parse("FROM a:1\nRUN true\nSNAPSHOT warm\n# trailing\n\n").unwrap();
    }

    #[test]
    fn error_display_has_line() {
        let x = e("FROM a:1\nBOGUS\n");
        assert_eq!(x.to_string(), "Macfile line 2: unknown directive 'BOGUS'");
    }

    #[test]
    fn plan_orders_steps_and_folds_user() {
        let f = parse(
            "FROM macos-base:15\nRUN a\nUSER builder\nCOPY x /y\nRUN b\nUSER root\nRUN c\nSNAPSHOT warm\n",
        )
        .unwrap();
        let p = plan(&f);
        assert_eq!(
            p,
            vec![
                Step::CloneTemplate {
                    reference: "macos-base:15".into()
                },
                Step::Run {
                    line: 2,
                    user: None,
                    command: "a".into(),
                    mounts: Vec::new()
                },
                Step::Copy {
                    line: 4,
                    user: Some("builder".into()),
                    src: "x".into(),
                    dst: "/y".into()
                },
                Step::Run {
                    line: 5,
                    user: Some("builder".into()),
                    command: "b".into(),
                    mounts: Vec::new()
                },
                Step::Run {
                    line: 7,
                    user: Some("root".into()),
                    command: "c".into(),
                    mounts: Vec::new()
                },
                Step::StoreTemplate,
                Step::Snapshot {
                    line: 8,
                    tag: "warm".into()
                },
            ]
        );
    }

    #[test]
    fn plan_minimal() {
        let p = plan(&parse("FROM a:1\n").unwrap());
        assert_eq!(
            p,
            vec![
                Step::CloneTemplate {
                    reference: "a:1".into()
                },
                Step::StoreTemplate
            ]
        );
    }

    #[test]
    fn dry_run_render() {
        let f = parse("FROM a:1\nUSER bob\nRUN make\nCOPY ./src /opt/src\nSNAPSHOT s\n").unwrap();
        let text = render_dry_run(&plan(&f));
        let expect = "1. clone template a:1\n\
                      2. run as bob (line 3): make\n\
                      3. copy as bob (line 4): ./src -> /opt/src\n\
                      4. stop instance and store as template\n\
                      5. save running state as 's' (line 5; host-bound)\n";
        assert_eq!(text, expect);
    }

    fn args(p: &[(&str, &str)]) -> BTreeMap<String, String> {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_workdir_and_arg_fold_into_run_steps() {
        let f = parse(
            "FROM a:1\nARG VER=1.2\nENV HOME_DIR=/opt/$VER NAME=\"two words\"\nWORKDIR /opt/app\nWORKDIR sub\nRUN make $VER\n",
        )
        .unwrap();
        let (steps, cfg) = plan_with(&f, &BTreeMap::new());
        let Step::Run { command, .. } = &steps[1] else {
            panic!("{steps:?}")
        };
        assert_eq!(
            command,
            "export VER='1.2'; export HOME_DIR='/opt/1.2'; export NAME='two words'; cd '/opt/app/sub' && make $VER"
        );
        assert_eq!(cfg.workdir.as_deref(), Some("/opt/app/sub"));
        assert_eq!(cfg.env.len(), 2); // ARG is not recorded in the image
    }

    #[test]
    fn build_arg_overrides_default() {
        let f = parse("FROM a:1\nARG V=old\nRUN echo\n").unwrap();
        let (steps, _) = plan_with(&f, &args(&[("V", "new")]));
        let Step::Run { command, .. } = &steps[1] else {
            panic!()
        };
        assert!(command.starts_with("export V='new'; "), "{command}");
    }

    #[test]
    fn plain_macfile_plan_is_unchanged_by_the_new_directives() {
        let (steps, cfg) = plan_with(&parse("FROM a:1\nRUN x\n").unwrap(), &BTreeMap::new());
        assert_eq!(
            steps[1],
            Step::Run {
                line: 2,
                user: None,
                command: "x".into(),
                mounts: Vec::new()
            }
        );
        assert!(cfg.is_blank());
    }

    #[test]
    fn copy_dst_is_relative_to_workdir_and_vars_expand() {
        let f =
            parse("FROM a:1\nARG D=data\nWORKDIR /srv\nCOPY ./$D files\nCOPY ./a /abs\n").unwrap();
        let (steps, _) = plan_with(&f, &BTreeMap::new());
        assert_eq!(
            steps[1],
            Step::Copy {
                line: 4,
                user: None,
                src: "./data".into(),
                dst: "/srv/files".into()
            }
        );
        assert!(matches!(&steps[2], Step::Copy { dst, .. } if dst == "/abs"));
    }

    #[test]
    fn add_url_becomes_a_curl_step_and_a_path_a_copy() {
        let f = parse("FROM a:1\nADD https://x.test/a.pkg /tmp/a.pkg\nADD ./local /opt/local\n")
            .unwrap();
        let steps = plan(&f);
        let Step::Run { command, line, .. } = &steps[1] else {
            panic!("{steps:?}")
        };
        assert_eq!(*line, 2);
        assert_eq!(command, "curl -fsSL 'https://x.test/a.pkg' -o '/tmp/a.pkg'");
        assert!(matches!(&steps[2], Step::Copy { src, .. } if src == "./local"));
        assert!(e("FROM a:1\nADD ../x /y\n").message.contains("escapes"));
        assert!(e("FROM a:1\nADD one\n").message.contains("exactly 2"));
    }

    #[test]
    fn metadata_directives_land_in_the_config() {
        let f = parse(
            "FROM a:1\nLABEL org.x.v=1 team=\"mac ci\"\nEXPOSE 8080 53/udp\nEXPOSE 8080\n\
             ENTRYPOINT [\"xcodebuild\"]\nCMD -version\nHEALTHCHECK --interval=1m --retries=5 CMD curl -f localhost\nUSER ci\n",
        )
        .unwrap();
        let (_, cfg) = plan_with(&f, &BTreeMap::new());
        assert_eq!(cfg.labels["team"], "mac ci");
        assert_eq!(cfg.labels["org.x.v"], "1");
        assert_eq!(cfg.expose, ["8080/tcp", "53/udp"]);
        assert_eq!(cfg.entrypoint, Some(Cmd::Exec(vec!["xcodebuild".into()])));
        assert_eq!(cfg.cmd, Some(Cmd::Shell("-version".into())));
        let h = cfg.healthcheck.unwrap();
        assert_eq!((h.interval_secs, h.timeout_secs, h.retries), (60, 30, 5));
        assert_eq!(h.cmd, Cmd::Shell("curl -f localhost".into()));
        assert_eq!(cfg.user.as_deref(), Some("ci"));
        assert!(
            cfg.history
                .iter()
                .any(|h| h.created_by == "EXPOSE 8080/tcp")
                || cfg
                    .history
                    .iter()
                    .any(|h| h.created_by.starts_with("EXPOSE"))
        );
    }

    #[test]
    fn healthcheck_none_clears() {
        let f = parse("FROM a:1\nHEALTHCHECK CMD true\nHEALTHCHECK NONE\n").unwrap();
        assert_eq!(plan_with(&f, &BTreeMap::new()).1.healthcheck, None);
    }

    #[test]
    fn new_directives_report_their_line_numbers() {
        let cases: &[(&str, usize, &str)] = &[
            ("FROM a\nENV\n", 2, "ENV needs"),
            ("FROM a\n\nENV 9X=1\n", 3, "not valid"),
            ("FROM a\nENV A=1 B\n", 2, "KEY=value"),
            ("FROM a\nARG\n", 2, "exactly one"),
            ("FROM a\nARG 1x\n", 2, "not valid"),
            ("FROM a\nWORKDIR\n", 2, "exactly one"),
            ("FROM a\nWORKDIR a b\n", 2, "exactly one"),
            ("FROM a\nEXPOSE\n", 2, "at least one"),
            ("FROM a\nEXPOSE 99999\n", 2, "not a port"),
            ("FROM a\nEXPOSE 80/sctp\n", 2, "not a port"),
            ("FROM a\nCMD\n", 2, "needs a command"),
            ("FROM a\nCMD [oops\n", 2, "JSON array"),
            ("FROM a\nENTRYPOINT []\n", 2, "empty"),
            ("FROM a\nHEALTHCHECK\n", 2, "needs NONE"),
            (
                "FROM a\nHEALTHCHECK --bogus=1 CMD x\n",
                2,
                "unknown HEALTHCHECK option",
            ),
            (
                "FROM a\nHEALTHCHECK --interval=x CMD x\n",
                2,
                "not a duration",
            ),
            ("FROM a\nHEALTHCHECK --retries=x CMD x\n", 2, "not a number"),
            ("FROM a\nLABEL novalue\n", 2, "KEY=value"),
        ];
        for (src, line, msg) in cases {
            let x = e(src);
            assert_eq!(x.line, *line, "{src:?}: {x}");
            assert!(x.message.contains(msg), "{src:?}: {x}");
        }
    }

    #[test]
    fn legacy_env_form_takes_the_rest_of_the_line() {
        let f = parse("FROM a\nENV GREETING hello big world\n").unwrap();
        assert_eq!(
            plan_with(&f, &BTreeMap::new()).1.env,
            vec![("GREETING".to_string(), "hello big world".to_string())]
        );
    }

    #[test]
    fn continuation_lines_work_for_new_directives() {
        let f = parse("FROM a\nENV A=1 \\\n    B=2\n").unwrap();
        assert_eq!(plan_with(&f, &BTreeMap::new()).1.env.len(), 2);
    }

    #[test]
    fn run_mount_flags_become_step_mounts_and_leave_the_command_alone() {
        let f = parse(
            "FROM b\nRUN --mount=type=secret,id=npm --mount=type=ssh npm ci\nRUN --mount=type=secret,id=k,target=/opt/k.txt use\n",
        )
        .unwrap();
        let steps = plan(&f);
        let Step::Run {
            command, mounts, ..
        } = &steps[1]
        else {
            panic!()
        };
        assert_eq!(command, "npm ci");
        assert_eq!(
            mounts,
            &[
                Mount::Secret {
                    id: "npm".into(),
                    target: None
                },
                Mount::Ssh
            ]
        );
        let text = render_dry_run(&steps);
        assert!(text.contains("npm ci [secret npm, ssh agent]"), "{text}");
        // A plain RUN is unchanged.
        let Step::Run { mounts, .. } = &plan(&parse("FROM b\nRUN x\n").unwrap())[1] else {
            panic!()
        };
        assert!(mounts.is_empty());
    }

    #[test]
    fn bad_mounts_are_refused_with_the_line() {
        for (src, want) in [
            (
                "FROM b\nRUN --mount=type=cache,target=/x y\n",
                "not supported",
            ),
            ("FROM b\nRUN --mount=type=secret y\n", "needs id="),
            (
                "FROM b\nRUN --mount=type=secret,id=a,target=rel y\n",
                "absolute path",
            ),
            ("FROM b\nRUN --mount=type=secret,id=a\n", "needs a command"),
            ("FROM b\nRUN --mount=id=a y\n", "needs type="),
        ] {
            let e = parse(src).unwrap_err();
            assert_eq!(e.line, 2, "{src}");
            assert!(e.message.contains(want), "{src}: {}", e.message);
        }
    }

    #[test]
    fn platform_accepts_this_macs_guests_only() {
        for ok in ["darwin/arm64", "macos/arm64", "arm64", "Darwin/ARM64"] {
            assert!(check_platform(ok).is_ok(), "{ok}");
        }
        for bad in ["linux/amd64", "darwin/amd64", "windows/arm64", ""] {
            let e = check_platform(bad).unwrap_err();
            assert!(
                e.contains("darwin/arm64") && e.contains("docker build"),
                "{e}"
            );
        }
    }

    #[test]
    fn build_arg_checks_name_the_fix() {
        let f = parse("FROM b\nARG VERSION\nARG NEEDED\nWORKDIR /app/$NEEDED\nRUN x\n").unwrap();
        let mut a = BTreeMap::new();
        a.insert("VERSON".to_string(), "1".to_string());
        a.insert("Z".to_string(), "1".to_string());
        a.insert("NPM_TOKEN".to_string(), "x".to_string());
        let w = check_build_args(&f, &a).join("\n");
        assert!(w.contains("declares ARG VERSION"), "{w}");
        assert!(w.contains("no `ARG Z` line"), "{w}");
        assert!(
            w.contains("looks like a secret") && w.contains("--secret id=npm_token"),
            "{w}"
        );
        assert!(w.contains("line 3: ARG NEEDED has no default"), "{w}");
        // Declared, supplied, no warnings.
        let mut ok = BTreeMap::new();
        ok.insert("VERSION".to_string(), "1".to_string());
        ok.insert("NEEDED".to_string(), "n".to_string());
        assert!(check_build_args(&f, &ok).is_empty());
    }

    #[test]
    fn cli_labels_override_macfile_labels_and_are_checked() {
        let (_, mut cfg) = plan_with(
            &parse("FROM b\nLABEL team=old keep=1\n").unwrap(),
            &BTreeMap::new(),
        );
        apply_labels(&mut cfg, &["team=ci".into(), "org.x.v=2".into()]).unwrap();
        assert_eq!(cfg.labels["team"], "ci");
        assert_eq!(cfg.labels["keep"], "1");
        assert_eq!(cfg.labels["org.x.v"], "2");
        for bad in ["novalue", "=x", "a b=c"] {
            assert!(
                apply_labels(&mut cfg, &[bad.into()])
                    .unwrap_err()
                    .contains("KEY=VALUE")
            );
        }
    }
}
