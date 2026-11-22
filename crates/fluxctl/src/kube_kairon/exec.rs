// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The one door to the outside world for `kube kairon`: every `helm`, `kubectl`, `kaironctl`, `curl` and `tar` call goes through
//! [`Runner`], so the flows can be tested against a fake that records the calls. Secrets go on stdin, never into arguments.

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub prog: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    /// Let the program write straight to the terminal (long-running `kaironctl selftest`).
    pub stream: bool,
}

impl Cmd {
    pub fn new(prog: &str, args: Vec<String>) -> Self {
        Cmd {
            prog: prog.into(),
            args,
            stdin: None,
            stream: false,
        }
    }
    pub fn stdin(mut self, s: String) -> Self {
        self.stdin = Some(s);
        self
    }
    pub fn stream(mut self) -> Self {
        self.stream = true;
        self
    }
    /// The command as a person would type it. Stdin is not shown.
    pub fn display(&self) -> String {
        let mut s = self.prog.clone();
        for a in &self.args {
            s.push(' ');
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || "\"'$`\\".contains(c))
            {
                s.push('\'');
                s.push_str(&a.replace('\'', "'\\''"));
                s.push('\'');
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

#[derive(Debug, Clone, Default)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    /// Last non-empty line of stderr, then stdout, for error messages.
    pub fn why(&self) -> String {
        let pick = |s: &str| {
            s.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(str::to_string)
        };
        pick(&self.stderr)
            .or_else(|| pick(&self.stdout))
            .unwrap_or_else(|| format!("exit code {}", self.code))
    }
}

pub trait Runner {
    fn run(&self, c: &Cmd) -> Result<Out>;
    /// Whether `prog` is on PATH.
    fn has(&self, prog: &str) -> bool;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, c: &Cmd) -> Result<Out> {
        let mut cmd = Command::new(&c.prog);
        cmd.args(&c.args);
        cmd.stdin(if c.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        if c.stream {
            let mut child = cmd
                .spawn()
                .with_context(|| format!("starting {}", c.prog))?;
            let st = child.wait()?;
            return Ok(Out {
                code: st.code().unwrap_or(1),
                ..Default::default()
            });
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", c.prog))?;
        if let (Some(text), Some(mut si)) = (&c.stdin, child.stdin.take()) {
            si.write_all(text.as_bytes())?;
        }
        let o = child.wait_with_output()?;
        Ok(Out {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        })
    }

    fn has(&self, prog: &str) -> bool {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
            .unwrap_or(false)
    }
}

/// A fake for tests: records every call (including stdin) and answers from a list of `(prefix, output)` rules, first match wins;
/// anything unmatched succeeds with empty output.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Fake {
        pub calls: RefCell<Vec<Cmd>>,
        pub rules: RefCell<Vec<(String, Out)>>,
        pub missing: Vec<String>,
    }

    impl Fake {
        pub fn rule(&self, prefix: &str, code: i32, stdout: &str) {
            self.rules.borrow_mut().push((
                prefix.into(),
                Out {
                    code,
                    stdout: stdout.into(),
                    stderr: String::new(),
                },
            ));
        }
        pub fn shown(&self) -> Vec<String> {
            self.calls.borrow().iter().map(Cmd::display).collect()
        }
    }

    impl Runner for Fake {
        fn run(&self, c: &Cmd) -> Result<Out> {
            self.calls.borrow_mut().push(c.clone());
            let line = c.display();
            for (p, o) in self.rules.borrow().iter() {
                if line.contains(p.as_str()) {
                    return Ok(o.clone());
                }
            }
            Ok(Out::default())
        }
        fn has(&self, prog: &str) -> bool {
            !self.missing.iter().any(|m| m == prog)
        }
    }
}
