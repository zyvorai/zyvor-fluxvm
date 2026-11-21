//! Macfile parser and build planner for `fluxctl mac build`.
//!
//! Pure and dependency-light: no I/O, no VM access. A Macfile is parsed into
//! [`Macfile`] and turned into an ordered list of [`Step`]s by [`plan`]; the
//! executor (not part of this module) runs those steps in a throwaway
//! instance. See `docs/macos-containers-design.md`, "Build".
//!
//! Syntax: one directive per line (`FROM`, `USER`, `RUN`, `COPY`,
//! `SNAPSHOT`), `#` comment lines, blank lines, a trailing `\` joins the next
//! line, and `"..."` / `'...'` quote arguments for `FROM`, `USER`, `COPY`
//! and `SNAPSHOT`. `RUN` keeps its text verbatim for the shell.

// Not wired to a CLI command yet; remove once `mac build` calls this.
#![allow(dead_code)]

use std::fmt;

/// What a Macfile builds on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// A template reference such as `macos-base:15`.
    Template(String),
    /// `IPSW latest`: install from the latest restore image.
    IpswLatest,
}

/// One parsed directive (without `FROM`, which lives on [`Macfile`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    User(String),
    Run(String),
    Copy { src: String, dst: String },
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
                            return err(
                                n,
                                format!("unsupported IPSW selector '{b}' (only 'latest')"),
                            );
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
                if cmd.is_empty() {
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
    /// Run `command` over SSH as `user` (None = the template's default user).
    Run {
        line: usize,
        user: Option<String>,
        command: String,
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
pub fn plan(file: &Macfile) -> Vec<Step> {
    let mut steps = vec![match &file.from {
        Base::Template(r) => Step::CloneTemplate {
            reference: r.clone(),
        },
        Base::IpswLatest => Step::InstallIpswLatest,
    }];
    let mut user: Option<String> = None;
    let mut snapshot: Option<Step> = None;
    for l in &file.lines {
        match &l.directive {
            Directive::User(u) => user = Some(u.clone()),
            Directive::Run(c) => steps.push(Step::Run {
                line: l.line,
                user: user.clone(),
                command: c.clone(),
            }),
            Directive::Copy { src, dst } => steps.push(Step::Copy {
                line: l.line,
                user: user.clone(),
                src: src.clone(),
                dst: dst.clone(),
            }),
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
    steps
}

/// Human-readable dry-run listing, one numbered step per line.
pub fn render_dry_run(steps: &[Step]) -> String {
    let who = |u: &Option<String>| u.as_deref().map(|u| format!(" as {u}")).unwrap_or_default();
    let mut out = String::new();
    for (i, s) in steps.iter().enumerate() {
        let text = match s {
            Step::CloneTemplate { reference } => format!("clone template {reference}"),
            Step::InstallIpswLatest => "install macOS from the latest IPSW".to_string(),
            Step::Run {
                line,
                user,
                command,
            } => format!("run{} (line {line}): {command}", who(user)),
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
        let x = e("FROM a:1\nEXPOSE 22\n");
        assert_eq!(x.line, 2);
        assert!(x.message.contains("unknown directive 'EXPOSE'"));
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
                    command: "a".into()
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
                    command: "b".into()
                },
                Step::Run {
                    line: 7,
                    user: Some("root".into()),
                    command: "c".into()
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
}
