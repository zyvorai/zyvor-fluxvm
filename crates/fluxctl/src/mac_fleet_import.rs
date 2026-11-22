// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet inventory import`: build (or extend) an inventory file from Macs you already have names for. Three sources, no
//! extra dependency, all of them optional and combinable:
//!
//! - `--hosts-file FILE`: one Mac per line, `[user@]host[:port] [label ...]`, `#` comments.
//! - `--ssh-config [FILE]`: the `Host` blocks of an OpenSSH config (default `~/.ssh/config`) that name a single, non-wildcard host, with
//!   `HostName`, `User`, `Port` and `IdentityFile`. `--ssh-pattern GLOB` keeps only the aliases that match (`mini-*`).
//! - `--bonjour`: macOS only; browses `_ssh._tcp` with `dns-sd` for `--bonjour-secs` seconds. A Bonjour instance name is a computer's
//!   *name* (`Mac mini`), not its host name, so `<name with spaces as dashes>.local` is a guess: it is printed, and `--check`
//!   (a read-only `mac-fleet doctor` over ssh) is the way to find out which guesses resolve.
//!
//! Nothing is sent to any Mac by `import`. The result is validated with the inventory parser before it is written.

use crate::mac_fleet_ssh::{Target, valid_host};
use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
pub enum InventoryCommand {
    /// Make or extend an inventory file from a hosts list, an ssh config or Bonjour. Sends nothing to any Mac.
    Import(ImportArgs),
}

#[derive(clap::Args)]
pub struct ImportArgs {
    /// One Mac per line: `[user@]host[:port] [label ...]`.
    #[arg(long, value_name = "FILE")]
    pub hosts_file: Option<PathBuf>,
    /// Read `Host` blocks from an OpenSSH config (default `~/.ssh/config`).
    #[arg(long, value_name = "FILE", num_args = 0..=1, default_missing_value = "~/.ssh/config")]
    pub ssh_config: Option<PathBuf>,
    /// With --ssh-config: only aliases matching this glob (`*` and `?`).
    #[arg(long, value_name = "GLOB")]
    pub ssh_pattern: Option<String>,
    /// Browse `_ssh._tcp` on the local network (macOS `dns-sd`).
    #[arg(long)]
    pub bonjour: bool,
    /// Seconds to browse.
    #[arg(long, default_value_t = 4)]
    pub bonjour_secs: u64,
    /// ssh user for hosts that do not give one.
    #[arg(long)]
    pub user: Option<String>,
    /// Label added to every imported Mac (`key=value` or a word). Repeatable.
    #[arg(long = "label")]
    pub labels: Vec<String>,
    /// Write here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Add to this existing inventory (hosts it already lists are kept as they are; other keys are kept, comments are not).
    #[arg(long, value_name = "FILE", conflicts_with = "out")]
    pub merge: Option<PathBuf>,
    /// Overwrite --out when it exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    pub name: Option<String>,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub labels: Vec<String>,
    pub source: &'static str,
}

// ---- parsers (pure) --------------------------------------------------------------------------------------------------------

pub fn parse_hosts_file(text: &str) -> Result<Vec<Found>> {
    let mut v = vec![];
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let t = Target::parse(words.next().unwrap_or(""))
            .with_context(|| format!("hosts file line {}", i + 1))?;
        let labels: Vec<String> = words.map(str::to_owned).collect();
        for l in &labels {
            crate::mac_fleet_sched::check_label(l)
                .with_context(|| format!("hosts file line {}", i + 1))?;
        }
        v.push(Found {
            name: Some(t.name),
            host: t.host,
            user: t.user,
            port: t.port,
            identity: None,
            labels,
            source: "hosts-file",
        });
    }
    Ok(v)
}

fn glob(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
            Some('?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(&p, &t)
}

/// `Host` blocks with a single, literal alias. `Host *` blocks and `Match` are ignored: their settings are not inherited here
/// (ssh applies them itself when the fleet connects, because the fleet runs the system `ssh`).
pub fn parse_ssh_config(text: &str, pattern: Option<&str>) -> Vec<Found> {
    let mut out: Vec<Found> = vec![];
    let mut cur: Option<Found> = None;
    let mut skip = false;
    let flush = |cur: &mut Option<Found>, out: &mut Vec<Found>| {
        if let Some(f) = cur.take() {
            out.push(f);
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (
                k.to_ascii_lowercase(),
                v.trim().trim_start_matches('=').trim().trim_matches('"'),
            ),
            None => continue,
        };
        match k.as_str() {
            "host" => {
                flush(&mut cur, &mut out);
                let aliases: Vec<&str> = v.split_whitespace().collect();
                skip = aliases.len() != 1
                    || aliases[0].contains(['*', '?', '!'])
                    || pattern.is_some_and(|p| !glob(p, aliases[0]));
                if !skip {
                    cur = Some(Found {
                        name: Some(crate::mac_fleet_ssh::sanitize_name(aliases[0])),
                        host: aliases[0].to_owned(),
                        source: "ssh-config",
                        ..Default::default()
                    });
                }
            }
            "match" => {
                flush(&mut cur, &mut out);
                skip = true;
            }
            "hostname" if !skip => {
                if let Some(c) = cur.as_mut() {
                    c.host = v.to_owned();
                }
            }
            "user" if !skip => {
                if let Some(c) = cur.as_mut() {
                    c.user = Some(v.to_owned());
                }
            }
            "port" if !skip => {
                if let Some(c) = cur.as_mut() {
                    c.port = v.parse().ok();
                }
            }
            "identityfile" if !skip => {
                if let Some(c) = cur.as_mut() {
                    c.identity.get_or_insert_with(|| v.to_owned());
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// `dns-sd -B _ssh._tcp local.` lines: `... Add  2  4 local.  _ssh._tcp.  Instance Name`.
pub fn parse_bonjour(text: &str) -> Vec<Found> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = vec![];
    for l in text.lines() {
        if !l.contains(" Add ") {
            continue;
        }
        let Some((_, inst)) = l.split_once("_ssh._tcp.") else {
            continue;
        };
        let inst = inst.trim();
        if inst.is_empty() {
            continue;
        }
        let host: String = inst
            .chars()
            .filter(|c| *c != '\'' && *c != '\u{2019}')
            .map(|c| if c.is_whitespace() { '-' } else { c })
            .collect();
        let host = format!("{host}.local");
        if !valid_host(&host) || !seen.insert(host.clone()) {
            continue;
        }
        out.push(Found {
            name: Some(crate::mac_fleet_ssh::sanitize_name(
                host.trim_end_matches(".local"),
            )),
            host,
            source: "bonjour",
            ..Default::default()
        });
    }
    out
}

// ---- building the inventory ------------------------------------------------------------------------------------------------

fn entry(f: &Found, default_user: Option<&str>, extra: &[String]) -> Value {
    let mut m = Mapping::new();
    let key = |s: &str| Value::String(s.into());
    if let Some(n) = &f.name {
        m.insert(key("name"), Value::String(n.clone()));
    }
    m.insert(key("host"), Value::String(f.host.clone()));
    if let Some(u) = f.user.as_deref().or(default_user) {
        m.insert(key("user"), Value::String(u.into()));
    }
    if let Some(p) = f.port {
        m.insert(key("port"), Value::Number(p.into()));
    }
    if let Some(i) = &f.identity {
        m.insert(key("identity"), Value::String(i.clone()));
    }
    let mut labels: Vec<String> = f.labels.clone();
    for l in extra {
        if !labels.contains(l) {
            labels.push(l.clone());
        }
    }
    if !labels.is_empty() {
        m.insert(
            key("labels"),
            Value::Sequence(labels.into_iter().map(Value::String).collect()),
        );
    }
    Value::Mapping(m)
}

fn existing_keys(hosts: &[Value]) -> std::collections::BTreeSet<String> {
    let mut s = std::collections::BTreeSet::new();
    for h in hosts {
        for k in ["name", "host"] {
            if let Some(v) = h.get(k).and_then(Value::as_str) {
                s.insert(v.to_ascii_lowercase());
            }
        }
    }
    s
}

/// The inventory text, and which found Macs were left out because the inventory already had them. `existing` is the YAML of the
/// file being extended, if any.
pub fn build(
    found: &[Found],
    existing: Option<&str>,
    user: Option<&str>,
    labels: &[String],
) -> Result<(String, Vec<String>)> {
    let mut root: Value = match existing {
        Some(t) => serde_yaml::from_str(t).context("the inventory to extend is not valid YAML")?,
        None => Value::Mapping(Mapping::new()),
    };
    let Value::Mapping(map) = &mut root else {
        bail!("the inventory to extend is not a mapping")
    };
    let hk = Value::String("hosts".into());
    let mut hosts: Vec<Value> = map
        .get(&hk)
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default();
    let mut have = existing_keys(&hosts);
    let mut skipped = vec![];
    for f in found {
        let id = f
            .name
            .clone()
            .unwrap_or_else(|| f.host.clone())
            .to_ascii_lowercase();
        if have.contains(&id) || have.contains(&f.host.to_ascii_lowercase()) {
            skipped.push(f.host.clone());
            continue;
        }
        have.insert(id);
        have.insert(f.host.to_ascii_lowercase());
        hosts.push(entry(f, user, labels));
    }
    if hosts.is_empty() {
        bail!("no Mac found: nothing to write");
    }
    map.insert(hk, Value::Sequence(hosts));
    let text = serde_yaml::to_string(&root)?;
    // The same parser `apply` uses: a bad name or host is refused here, not at the first ssh.
    crate::mac_fleet_inv::Inventory::parse(&text)
        .context("the resulting inventory is not valid")?;
    Ok((text, skipped))
}

// ---- running ---------------------------------------------------------------------------------------------------------------

fn browse_bonjour(secs: u64) -> Result<Vec<Found>> {
    if !cfg!(target_os = "macos") {
        bail!("--bonjour needs macOS (`dns-sd`); use --hosts-file or --ssh-config here");
    }
    let mut c = std::process::Command::new("/usr/bin/dns-sd");
    c.args(["-B", "_ssh._tcp", "local."]);
    // dns-sd browses until killed; the deadline kills it and the output so far is what was found.
    let o = crate::mac_fleet_timed::run_with_timeout(
        c,
        None,
        Some(std::time::Duration::from_secs(secs.clamp(1, 60))),
        "dns-sd",
    )?;
    Ok(parse_bonjour(&o.stdout))
}

pub fn import_cmd(a: ImportArgs) -> Result<()> {
    if a.hosts_file.is_none() && a.ssh_config.is_none() && !a.bonjour {
        bail!("name a source: --hosts-file FILE, --ssh-config [FILE] or --bonjour");
    }
    for l in &a.labels {
        crate::mac_fleet_sched::check_label(l)?;
    }
    let mut found = vec![];
    if let Some(p) = &a.hosts_file {
        let p = crate::mac_fleet_inv::expand_home(p);
        found.extend(parse_hosts_file(
            &std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?,
        )?);
    }
    if let Some(p) = &a.ssh_config {
        let p = crate::mac_fleet_inv::expand_home(p);
        found.extend(parse_ssh_config(
            &std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?,
            a.ssh_pattern.as_deref(),
        ));
    }
    if a.bonjour {
        found.extend(browse_bonjour(a.bonjour_secs)?);
    }
    // Unusable ssh-config entries (a HostName that is not a plain host, a bad alias) are named, not silently dropped.
    let (good, bad): (Vec<Found>, Vec<Found>) = found
        .into_iter()
        .partition(|f| valid_host(&f.host) && f.name.as_deref().is_none_or(|n| !n.is_empty()));
    for b in &bad {
        eprintln!("skipped {:?} ({}): not a plain host name", b.host, b.source);
    }
    let merge = a.merge.as_deref().map(crate::mac_fleet_inv::expand_home);
    let existing = match &merge {
        Some(p) => {
            Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)
        }
        None => None,
    };
    let (text, skipped) = build(&good, existing.as_deref(), a.user.as_deref(), &a.labels)?;
    for s in &skipped {
        eprintln!("kept the inventory's own entry for {s}");
    }
    let dest: Option<&Path> = merge.as_deref().or(a.out.as_deref());
    match dest {
        Some(p) => {
            if merge.is_none() && p.exists() && !a.force {
                bail!(
                    "{} exists: use --merge {0} to add to it, or --force",
                    p.display()
                );
            }
            std::fs::write(p, &text).with_context(|| format!("writing {}", p.display()))?;
            eprintln!(
                "wrote {} ({} Mac(s) found)",
                p.display(),
                good.len() - skipped.len()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "
Host *
  ServerAliveInterval 30
Host mini-1
  HostName 10.0.0.11
  User admin
  Port 2222
  IdentityFile ~/.ssh/ci
Host mini-2 mini-2-alt
  HostName 10.0.0.12
Host build-3
  HostName build-3.lab
Host gpu-?
  User root
Match host foo
  User nope
Host studio
  HostName=studio.local
";

    #[test]
    fn ssh_config_gives_single_literal_hosts_with_their_settings() {
        let v = parse_ssh_config(CONFIG, None);
        let names: Vec<_> = v.iter().map(|f| f.name.clone().unwrap()).collect();
        assert_eq!(
            names,
            ["mini-1", "build-3", "studio"],
            "wildcard, multi-alias and Match blocks are ignored"
        );
        assert_eq!(
            (
                v[0].host.as_str(),
                v[0].user.as_deref(),
                v[0].port,
                v[0].identity.as_deref()
            ),
            ("10.0.0.11", Some("admin"), Some(2222), Some("~/.ssh/ci"))
        );
        assert_eq!(v[2].host, "studio.local");
        let only = parse_ssh_config(CONFIG, Some("mini-*"));
        assert_eq!(only.len(), 1);
    }

    #[test]
    fn hosts_files_have_comments_users_ports_and_labels() {
        let v =
            parse_hosts_file("# lab\nadmin@mini-1.local:2222 zone=lab xcode # first\n\n10.0.0.5\n")
                .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(
            (v[0].user.as_deref(), v[0].port, v[0].labels.clone()),
            (
                Some("admin"),
                Some(2222),
                vec!["zone=lab".to_owned(), "xcode".to_owned()]
            )
        );
        assert!(
            parse_hosts_file("-oProxyCommand=x\n").is_err(),
            "option injection is refused"
        );
        assert!(parse_hosts_file("ok bad;label\n").is_err());
    }

    #[test]
    fn bonjour_instances_become_dot_local_guesses() {
        let out = "Browsing for _ssh._tcp\nTimestamp     A/R    Flags  if Domain               Service Type         Instance Name\n10:00:00.456  Add        2   4 local.               _ssh._tcp.           Studio's Mac mini\n10:00:00.457  Add        2   4 local.               _ssh._tcp.           mini-1\n10:00:00.458  Add        2   5 local.               _ssh._tcp.           mini-1\n10:00:01.000  Rmv        0   4 local.               _ssh._tcp.           gone\n";
        let v = parse_bonjour(out);
        assert_eq!(
            v.iter().map(|f| f.host.as_str()).collect::<Vec<_>>(),
            ["Studios-Mac-mini.local", "mini-1.local"]
        );
        assert_eq!(v[1].name.as_deref(), Some("mini-1"));
    }

    #[test]
    fn the_inventory_is_built_validated_and_merged_without_duplicates() {
        let found = parse_ssh_config(CONFIG, None);
        let (text, skipped) = build(&found, None, Some("ci"), &["zone=lab".into()]).unwrap();
        assert!(skipped.is_empty());
        let inv = crate::mac_fleet_inv::Inventory::parse(&text).unwrap();
        let t = inv.targets().unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].port, Some(2222));
        assert_eq!(
            t[1].user.as_deref(),
            Some("ci"),
            "--user fills in the missing ones"
        );
        assert!(t[1].labels.contains(&"zone=lab".to_owned()));

        let existing = "version: 0.4.21\nhosts:\n  - host: 10.0.0.11\n    name: kept\n";
        let (merged, skipped) = build(&found, Some(existing), None, &[]).unwrap();
        assert_eq!(
            skipped,
            ["10.0.0.11"],
            "same host under another name is the same Mac"
        );
        assert!(
            merged.contains("version: 0.4.21")
                && merged.contains("name: kept")
                && merged.contains("build-3")
        );
        assert!(build(&[], None, None, &[]).is_err());
        let bad = vec![Found {
            host: "ok.local".into(),
            name: Some("Bad Name".into()),
            ..Default::default()
        }];
        // a name with a space is refused by the inventory parser, not written
        assert!(build(&bad, None, None, &[]).is_err());
    }

    #[test]
    fn the_glob_matches_stars_and_question_marks() {
        assert!(glob("mini-*", "mini-12") && glob("m?ni", "mini") && !glob("mini-*", "studio"));
    }
}
