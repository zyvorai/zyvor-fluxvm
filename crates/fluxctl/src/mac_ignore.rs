// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `.macignore` for `fluxctl mac build`: files in the build context that `COPY` / `ADD` must not send to the guest.
//!
//! Same rules as `.dockerignore`: one pattern per line, `#` comments, patterns are relative to the context root (so `*.md`
//! matches only at the top; use `**/*.md` for any depth), `*` and `?` do not cross `/`, `**` matches any number of
//! directories, a leading `!` re-includes, and the last matching pattern decides. A pattern that matches a directory
//! excludes everything under it. Without a `.macignore`, nothing is filtered and `scp` reads the context directly.
//!
//! Not verified against a real build: tested on temporary directories only.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const IGNORE_FILE: &str = ".macignore";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    negate: bool,
    segments: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    pub fn parse(text: &str) -> Ignore {
        let mut rules = Vec::new();
        for raw in text.lines() {
            let t = raw.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let (negate, body) = match t.strip_prefix('!') {
                Some(b) => (true, b.trim()),
                None => (false, t),
            };
            let body = body.trim_start_matches('/').trim_end_matches('/');
            if body.is_empty() {
                continue;
            }
            // Normalise `a//b` and `./a`.
            let segments: Vec<String> = body
                .split('/')
                .filter(|s| !s.is_empty() && *s != ".")
                .map(str::to_string)
                .collect();
            if !segments.is_empty() {
                rules.push(Rule { negate, segments });
            }
        }
        Ignore { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether the context-relative path (`/`-separated) is ignored.
    pub fn is_ignored(&self, rel: &str) -> bool {
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let mut ignored = false;
        for rule in &self.rules {
            // A rule matches a path when it matches the path or any ancestor directory of it.
            let hit = (1..=parts.len()).any(|n| seg_match(&rule.segments, &parts[..n]));
            if hit {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

fn seg_match(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((p, rest)) if p == "**" => {
            // `**` swallows zero or more directories.
            (0..=path.len()).any(|k| seg_match(rest, &path[k..]))
        }
        Some((p, rest)) => match path.split_first() {
            Some((s, tail)) => glob(p.as_bytes(), s.as_bytes()) && seg_match(rest, tail),
            None => false,
        },
    }
}

/// `*` (any run, no `/` here because segments are already split) and `?` (one byte) over one path segment.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|k| glob(rest, &s[k..])),
        Some((b'?', rest)) => !s.is_empty() && glob(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

/// Read `CONTEXT/.macignore` (empty when absent).
pub fn load(context: &Path) -> Result<Ignore> {
    match std::fs::read_to_string(context.join(IGNORE_FILE)) {
        Ok(t) => Ok(Ignore::parse(&t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ignore::default()),
        Err(e) => {
            Err(e).with_context(|| format!("reading {}", context.join(IGNORE_FILE).display()))
        }
    }
}

/// A path ready for `scp`, plus the scratch copy to delete afterwards when a filtered copy was made.
#[derive(Debug)]
pub struct Staged {
    pub path: PathBuf,
    cleanup: Option<PathBuf>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(d) = &self.cleanup {
            std::fs::remove_dir_all(d).ok();
        }
    }
}

/// The local path to send for `COPY src`: the source itself when nothing is ignored, otherwise a filtered copy.
/// A source that is itself ignored is an error (as in Docker, it "does not exist" in the context).
pub fn stage(context: &Path, src: &str) -> Result<Staged> {
    let full = context.join(src);
    let ig = load(context)?;
    if ig.is_empty() {
        return Ok(Staged {
            path: full,
            cleanup: None,
        });
    }
    let rel = rel_to_context(context, &full)?;
    if !rel.is_empty() && ig.is_ignored(&rel) {
        bail!("COPY source {src} is excluded by {IGNORE_FILE}");
    }
    if !full.is_dir() {
        return Ok(Staged {
            path: full,
            cleanup: None,
        });
    }
    let tmp =
        std::env::temp_dir().join(format!("fluxvm-macbuild-{}", uuid::Uuid::new_v4().simple()));
    let name = full
        .file_name()
        .map_or_else(|| "context".into(), |n| n.to_os_string());
    let dest = tmp.join(&name);
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let staged = Staged {
        path: dest.clone(),
        cleanup: Some(tmp),
    };
    copy_filtered(context, &full, &dest, &ig)?;
    Ok(staged)
}

fn rel_to_context(context: &Path, full: &Path) -> Result<String> {
    // Lexical: `./a/../b` is normalised without touching the disk (the parser already forbids escaping the context).
    let mut parts: Vec<String> = Vec::new();
    for c in full.strip_prefix(context).unwrap_or(full).components() {
        match c {
            std::path::Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    Ok(parts.join("/"))
}

fn copy_filtered(context: &Path, from: &Path, to: &Path, ig: &Ignore) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = rel_to_context(context, &path)?;
        if ig.is_ignored(&rel) {
            // A directory pattern excludes its children; `!` re-includes of children are honoured only for
            // directories that are themselves kept (same trade-off as Docker's own simple matcher).
            continue;
        }
        let dest = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&path)?;
            std::os::unix::fs::symlink(target, &dest)?;
        } else if ft.is_dir() {
            std::fs::create_dir_all(&dest)?;
            copy_filtered(context, &path, &dest, ig)?;
        } else {
            std::fs::copy(&path, &dest).with_context(|| format!("copying {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(t: &str) -> Ignore {
        Ignore::parse(t)
    }

    #[test]
    fn patterns_are_rooted_like_dockerignore() {
        let i = ig("*.md\nbuild\n");
        assert!(i.is_ignored("README.md"));
        assert!(!i.is_ignored("docs/README.md"), "*.md is top level only");
        assert!(i.is_ignored("build"));
        assert!(
            i.is_ignored("build/out/a.o"),
            "a directory takes its children"
        );
        assert!(!i.is_ignored("src/build.rs"));
    }

    #[test]
    fn double_star_and_question_mark() {
        let i = ig("**/*.log\n?.tmp\nsrc/**/gen\n");
        assert!(i.is_ignored("a.log"));
        assert!(i.is_ignored("x/y/z.log"));
        assert!(i.is_ignored("a.tmp"));
        assert!(!i.is_ignored("ab.tmp"));
        assert!(i.is_ignored("src/gen"));
        assert!(i.is_ignored("src/a/b/gen/f"));
        assert!(!i.is_ignored("lib/gen"));
    }

    #[test]
    fn negation_and_last_match_wins() {
        let i = ig("*.txt\n!keep.txt\n");
        assert!(i.is_ignored("a.txt"));
        assert!(!i.is_ignored("keep.txt"));
        let j = ig("!keep.txt\n*.txt\n");
        assert!(j.is_ignored("keep.txt"), "later rule wins");
    }

    #[test]
    fn comments_blanks_slashes_and_dot_segments() {
        let i = ig("# note\n\n/abs\ndir/\n./rel\n");
        assert!(i.is_ignored("abs"));
        assert!(i.is_ignored("dir/x"));
        assert!(i.is_ignored("rel"));
        assert!(!i.is_ignored("note"));
        assert!(ig("").is_empty());
        assert!(ig("#only\n").is_empty());
    }

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, c: &str| {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        };
        w("app/main.swift", "m");
        w("app/notes.md", "n");
        w("app/.build/cache", "c");
        w("app/keep/a.md", "a");
        w("other.txt", "o");
        d
    }

    #[test]
    fn without_a_macignore_the_source_is_used_directly() {
        let d = tree();
        let s = stage(d.path(), "app").unwrap();
        assert_eq!(s.path, d.path().join("app"));
    }

    #[test]
    fn a_filtered_copy_leaves_out_ignored_files_and_is_cleaned_up() {
        let d = tree();
        std::fs::write(
            d.path().join(IGNORE_FILE),
            "**/*.md\napp/.build\n!app/keep/a.md\n",
        )
        .unwrap();
        let (path, parent);
        {
            let s = stage(d.path(), "./app").unwrap();
            path = s.path.clone();
            parent = path.parent().unwrap().to_path_buf();
            assert_eq!(path.file_name().unwrap(), "app");
            assert!(path.join("main.swift").is_file());
            assert!(!path.join("notes.md").exists());
            assert!(!path.join(".build").exists());
            assert!(path.join("keep/a.md").is_file(), "explicit re-include");
        }
        assert!(!parent.exists(), "scratch copy removed on drop");
    }

    #[test]
    fn an_ignored_source_is_refused() {
        let d = tree();
        std::fs::write(d.path().join(IGNORE_FILE), "other.txt\n").unwrap();
        let e = stage(d.path(), "other.txt").unwrap_err().to_string();
        assert!(e.contains("excluded by .macignore"), "{e}");
        assert!(stage(d.path(), "app/main.swift").is_ok());
    }

    #[test]
    fn symlinks_are_kept_as_symlinks() {
        let d = tree();
        std::os::unix::fs::symlink("main.swift", d.path().join("app/link")).unwrap();
        std::fs::write(d.path().join(IGNORE_FILE), "zzz\n").unwrap();
        let s = stage(d.path(), "app").unwrap();
        assert!(
            std::fs::symlink_metadata(s.path.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
