// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl cluster report`: the cluster passport, ONE document a person can hand to someone else: the machines and their
//! roles, versions, a health summary, how to add a machine, how to back up and restore, how to undo, where the logs are,
//! and a contacts/notes field. See docs/cluster-passport.md.
//!
//! Secrets are kept out twice. The document is built from a typed struct that has no field for a token, credential,
//! password or key. And every rendered byte then goes through [`scrub`], which redacts anything shaped like one (so a
//! token someone pasted into the notes file, a node label or a URL does not leave with the report).

use crate::cluster_pair::ServerArgs;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Clone, Debug)]
pub struct ReportArgs {
    #[command(flatten)]
    pub server: ServerArgs,
    /// md (default), html or json.
    #[arg(long, default_value = "md", value_parser = ["md", "html", "json"])]
    pub format: String,
    /// Write the report here instead of printing it.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Contacts and notes to include. Default: ~/.config/fluxvm/passport-notes.md if it exists.
    #[arg(long)]
    pub notes_file: Option<PathBuf>,
    /// Serve the report as a read-only page on 127.0.0.1 instead of printing it (Ctrl-C to stop).
    #[arg(long)]
    pub serve: bool,
    /// Port for --serve.
    #[arg(long, default_value = "7801")]
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub role: String,
    pub vcpus: u64,
    pub memory_gib: u64,
    pub vms: u64,
    pub agent_version: String,
    pub status: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Passport {
    pub generated_at: String,
    pub fluxctl_version: String,
    pub server: String,
    /// Why the machine list is empty or partial (server not reachable, no token), for a person to read.
    pub server_note: Option<String>,
    pub machines: Vec<Machine>,
    pub pending_requests: usize,
    pub pairing_on: Option<bool>,
    pub recipes: Vec<String>,
    pub notes: String,
    pub state_dir: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Health {
    pub total: usize,
    pub online: usize,
    pub offline: usize,
    pub cordoned: usize,
    pub outdated: usize,
}

impl Passport {
    pub fn health(&self) -> Health {
        let mut h = Health { total: self.machines.len(), ..Default::default() };
        for m in &self.machines {
            match m.status.as_str() {
                "online" => h.online += 1,
                "cordoned" => h.cordoned += 1,
                "outdated" => h.outdated += 1,
                _ => h.offline += 1,
            }
        }
        h
    }

    pub fn versions(&self) -> Vec<(String, usize)> {
        let mut v: std::collections::BTreeMap<String, usize> = Default::default();
        for m in &self.machines {
            *v.entry(if m.agent_version.is_empty() { "unknown".into() } else { m.agent_version.clone() }).or_default() += 1;
        }
        v.into_iter().collect()
    }
}

/// Machines from the `items` of `GET /fleet/nodes`.
pub fn machines_from(v: &serde_json::Value) -> Vec<Machine> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|n| {
                    let healthy = n["healthy"].as_bool().unwrap_or(false);
                    let status = if n["cordoned"].as_bool() == Some(true) {
                        "cordoned"
                    } else if !healthy {
                        "offline"
                    } else if n["outdated"].as_bool() == Some(true) {
                        "outdated"
                    } else {
                        "online"
                    };
                    let mac = !n["apple"].is_null();
                    Machine {
                        name: n["name"].as_str().unwrap_or("?").to_owned(),
                        role: if mac { "Mac (macOS VMs, Docker, k3s)" } else { "Linux (VMs)" }.to_owned(),
                        vcpus: n["vcpus_total"].as_u64().unwrap_or(0),
                        memory_gib: n["memory_mib_total"].as_u64().unwrap_or(0) / 1024,
                        vms: n["vm_count"].as_u64().unwrap_or(0),
                        agent_version: n["agent_version"].as_str().unwrap_or("").to_owned(),
                        status: status.to_owned(),
                        last_seen: n["last_seen"].as_str().unwrap_or("").to_owned(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Redacts anything shaped like a secret. Applied to every format, last.
pub fn scrub(text: &str) -> String {
    const PREFIXES: &[&str] = &[
        "fvnc_", "fvjt_", "fvt_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "sk-", "xoxb-", "xoxp-", "AKIA", "eyJ", "glpat-",
    ];
    const KEYS: &[&str] = &["password", "passwd", "secret", "token", "apikey", "api_key", "api-key", "credential", "authorization"];
    let is_word = |c: char| c.is_ascii_alphanumeric() || "_-.+/=".contains(c);
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_word(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let lower = word.to_lowercase();
        let secret_shaped = PREFIXES.iter().any(|p| word.starts_with(p) && word.len() >= p.len() + 8)
            || (word.len() >= 40 && word.chars().all(|c| c.is_ascii_hexdigit()))
            || (word.len() >= 32 && word.chars().filter(|c| c.is_ascii_digit()).count() >= 4 && word.chars().any(|c| c.is_ascii_uppercase()) && word.chars().any(|c| c.is_ascii_lowercase()));
        // key=value and key: value where the key names a secret: the value goes.
        let key_value = KEYS.iter().find_map(|k| {
            let l = lower.as_str();
            l.find(k).and_then(|at| {
                let after = &word[at + k.len()..];
                after.strip_prefix('=').map(|v| (word.len() - v.len(), v.is_empty()))
            })
        });
        if secret_shaped {
            out.push_str("[redacted]");
        } else if let Some((cut, empty)) = key_value {
            out.push_str(&word[..cut]);
            if empty {
                // `password= value`: the value is the next word, handled below by the preceding-key rule.
            } else {
                out.push_str("[redacted]");
            }
        } else if let Some(prev_key) = prev_key_word(&out) {
            let _ = prev_key;
            out.push_str("[redacted]");
        } else {
            out.push_str(&word);
        }
    }
    strip_userinfo(&out)
}

/// `Bearer xyz`, `password: xyz`, `token xyz`: the word after one of these is a secret.
fn prev_key_word(out: &str) -> Option<&'static str> {
    const TRIGGERS: &[&str] = &["bearer", "password:", "passwd:", "secret:", "token:", "password=", "secret=", "token=", "apikey:", "authorization:"];
    let t = out.trim_end_matches([' ', '\t']);
    if t.len() == out.len() {
        return None;
    }
    let lw = t.rsplit(|c: char| c.is_whitespace()).next().unwrap_or("").to_lowercase();
    TRIGGERS.iter().find(|k| lw.ends_with(*k)).copied()
}

/// `https://user:pass@host` becomes `https://host`.
fn strip_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        rest = &rest[i + 3..];
        let end = rest.find(|c: char| c.is_whitespace() || c == '/' || c == '"' || c == '<' || c == ')').unwrap_or(rest.len());
        let authority = &rest[..end];
        match authority.rfind('@') {
            Some(at) => out.push_str(&authority[at + 1..]),
            None => out.push_str(authority),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The static sections, shared by every format: (title, lines). `server` is the address people should use.
fn sections(p: &Passport) -> Vec<(&'static str, Vec<String>)> {
    let server = if p.server.is_empty() { "SERVER-ADDRESS".to_owned() } else { p.server.clone() };
    let sd = if p.state_dir.is_empty() { "/var/lib/fluxvm-agent".to_owned() } else { p.state_dir.clone() };
    vec![
        (
            "How to add a machine",
            vec![
                "On the new machine (Mac mini or Linux box), with fluxctl installed:".into(),
                "    fluxctl cluster pair".into(),
                "It finds this cluster by itself and shows a short code like ABCDE-FGHJK.".into(),
                "On the server, check that the code matches the machine's screen, then approve it:".into(),
                "    fluxctl cluster pending".into(),
                "    fluxctl cluster approve ABCDE-FGHJK".into(),
                "(or press Approve on the Studio page \"Machines asking to join\").".into(),
                format!("If the network blocks discovery: `fluxctl cluster pair --server {server}`."),
                match p.pairing_on {
                    Some(true) => "Pairing is on for this server.".into(),
                    Some(false) => "Pairing is OFF on this server: restart central with `--pairing` first.".into(),
                    None => "Pairing needs `fluxvm-agent central --pairing`.".into(),
                },
            ],
        ),
        (
            "How to back up and restore",
            vec![
                format!("The server keeps everything in {sd}. Back it up while central is stopped or idle:"),
                format!("    tar czf fluxvm-central-backup.tgz -C {sd} . && chmod 600 fluxvm-central-backup.tgz"),
                "The archive holds the keys that make machine credentials: keep it private and off shared drives.".into(),
                "To restore: stop central, unpack the archive into the same folder, start central again.".into(),
                "Machines keep their own credential; they reconnect by themselves. A machine that lost its own state pairs again.".into(),
                "Apps from recipes: see each recipe's backup line (`fluxctl recipe show NAME`).".into(),
            ],
        ),
        (
            "How to undo",
            vec![
                "Stop sending work to a machine:  fluxctl fleet cordon NAME".into(),
                "Take a machine out:              fluxctl fleet deregister NAME   (its credential stops working)".into(),
                "A machine was approved by mistake: deregister it, then check the Activity page in Studio.".into(),
                "Stop accepting new machines:     restart central without --pairing.".into(),
                "Go back to the previous FluxVM version on a Linux machine: fluxvm-bootstrap rollback".into(),
                "Remove an app from a recipe:     fluxctl recipe uninstall NAME".into(),
            ],
        ),
        (
            "Where the logs are",
            vec![
                "Mac, node agent:  ~/Library/Logs/fluxvm-agent.log".into(),
                "Linux, services:  journalctl -u fluxvm-agent -u fluxvm --since today".into(),
                "Server (central): journalctl -u fluxvm-agent, or the terminal it was started in".into(),
                "Activity and audit trail: Studio, page Activity.".into(),
            ],
        ),
    ]
}

pub fn render_md(p: &Passport) -> String {
    let h = p.health();
    let mut s = String::new();
    s.push_str("# Cluster passport\n\n");
    s.push_str(&format!("Generated {} by fluxctl {}. Server: {}\n\n", p.generated_at, p.fluxctl_version, if p.server.is_empty() { "(not set)" } else { &p.server }));
    s.push_str("This document contains no passwords, tokens or keys.\n\n");
    s.push_str("## Summary\n\n");
    if let Some(n) = &p.server_note {
        s.push_str(&format!("> {n}\n\n"));
    }
    s.push_str(&format!(
        "- Machines: {} ({} online, {} offline, {} cordoned, {} outdated)\n- Machines asking to join: {}\n",
        h.total, h.online, h.offline, h.cordoned, h.outdated, p.pending_requests
    ));
    s.push_str("\n## Machines and roles\n\n");
    if p.machines.is_empty() {
        s.push_str("None yet. See \"How to add a machine\".\n");
    } else {
        s.push_str("| Name | Role | CPUs | Memory | VMs | Version | Status | Last seen |\n|---|---|---|---|---|---|---|---|\n");
        for m in &p.machines {
            s.push_str(&format!("| {} | {} | {} | {} GiB | {} | {} | {} | {} |\n", m.name, m.role, m.vcpus, m.memory_gib, m.vms, if m.agent_version.is_empty() { "unknown" } else { &m.agent_version }, m.status, m.last_seen));
        }
    }
    s.push_str("\n## Versions\n\n");
    s.push_str(&format!("- fluxctl on the machine that made this report: {}\n", p.fluxctl_version));
    for (v, n) in p.versions() {
        s.push_str(&format!("- node agent {v}: {n} machine(s)\n"));
    }
    if p.versions().len() > 1 {
        s.push_str("- Different versions are running; upgrade the older machines.\n");
    }
    for (title, lines) in sections(p) {
        s.push_str(&format!("\n## {title}\n\n"));
        for l in lines {
            if let Some(code) = l.strip_prefix("    ") {
                s.push_str(&format!("    {code}\n"));
            } else {
                s.push_str(&format!("{l}\n"));
            }
        }
    }
    s.push_str("\n## Apps installed from recipes\n\n");
    if p.recipes.is_empty() {
        s.push_str("None.\n");
    } else {
        for r in &p.recipes {
            s.push_str(&format!("- {r}\n"));
        }
    }
    s.push_str("\n## Contacts and notes\n\n");
    if p.notes.trim().is_empty() {
        s.push_str("(Nothing yet. Put who to call, where the spare parts are and anything else worth knowing in ~/.config/fluxvm/passport-notes.md; it appears here.)\n");
    } else {
        s.push_str(p.notes.trim());
        s.push('\n');
    }
    s
}

pub fn render_html(p: &Passport) -> String {
    let h = p.health();
    let mut s = String::from("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Cluster passport</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem;color:#1b1f23}table{border-collapse:collapse;width:100%}td,th{border:1px solid #d0d7de;padding:.3rem .6rem;text-align:left}th{background:#f6f8fa}pre{background:#f6f8fa;padding:.6rem;overflow:auto}.offline{color:#b42318}.online{color:#067647}</style></head><body>\n");
    s.push_str("<h1>Cluster passport</h1>\n");
    s.push_str(&format!("<p>Generated {} by fluxctl {}. Server: {}</p>\n<p><em>This document contains no passwords, tokens or keys.</em></p>\n", esc(&p.generated_at), esc(&p.fluxctl_version), esc(&p.server)));
    s.push_str("<h2>Summary</h2>\n");
    if let Some(n) = &p.server_note {
        s.push_str(&format!("<blockquote>{}</blockquote>\n", esc(n)));
    }
    s.push_str(&format!("<ul><li>Machines: {} ({} online, {} offline, {} cordoned, {} outdated)</li><li>Machines asking to join: {}</li></ul>\n", h.total, h.online, h.offline, h.cordoned, h.outdated, p.pending_requests));
    s.push_str("<h2>Machines and roles</h2>\n");
    if p.machines.is_empty() {
        s.push_str("<p>None yet.</p>\n");
    } else {
        s.push_str("<table><tr><th>Name</th><th>Role</th><th>CPUs</th><th>Memory</th><th>VMs</th><th>Version</th><th>Status</th><th>Last seen</th></tr>\n");
        for m in &p.machines {
            s.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{} GiB</td><td>{}</td><td>{}</td><td class=\"{}\">{}</td><td>{}</td></tr>\n", esc(&m.name), esc(&m.role), m.vcpus, m.memory_gib, m.vms, esc(&m.agent_version), if m.status == "online" { "online" } else { "offline" }, esc(&m.status), esc(&m.last_seen)));
        }
        s.push_str("</table>\n");
    }
    s.push_str("<h2>Versions</h2>\n<ul>");
    s.push_str(&format!("<li>fluxctl: {}</li>", esc(&p.fluxctl_version)));
    for (v, n) in p.versions() {
        s.push_str(&format!("<li>node agent {}: {n} machine(s)</li>", esc(&v)));
    }
    s.push_str("</ul>\n");
    for (title, lines) in sections(p) {
        s.push_str(&format!("<h2>{title}</h2>\n"));
        let mut code = String::new();
        let flush = |code: &mut String, s: &mut String| {
            if !code.is_empty() {
                s.push_str(&format!("<pre>{}</pre>\n", esc(code.trim_end())));
                code.clear();
            }
        };
        for l in lines {
            if let Some(c) = l.strip_prefix("    ") {
                code.push_str(c);
                code.push('\n');
            } else {
                flush(&mut code, &mut s);
                s.push_str(&format!("<p>{}</p>\n", esc(&l)));
            }
        }
        flush(&mut code, &mut s);
    }
    s.push_str("<h2>Apps installed from recipes</h2>\n");
    if p.recipes.is_empty() {
        s.push_str("<p>None.</p>\n");
    } else {
        s.push_str("<ul>");
        for r in &p.recipes {
            s.push_str(&format!("<li>{}</li>", esc(r)));
        }
        s.push_str("</ul>\n");
    }
    s.push_str(&format!("<h2>Contacts and notes</h2>\n<pre>{}</pre>\n</body></html>\n", esc(if p.notes.trim().is_empty() { "(Nothing yet: ~/.config/fluxvm/passport-notes.md appears here.)" } else { p.notes.trim() })));
    s
}

pub fn render_json(p: &Passport) -> String {
    let mut v = serde_json::to_value(p).expect("serializable");
    v["health"] = serde_json::to_value(p.health()).expect("serializable");
    v["sections"] = serde_json::Value::Array(
        sections(p)
            .into_iter()
            .map(|(t, l)| serde_json::json!({"title": t, "lines": l}))
            .collect(),
    );
    serde_json::to_string_pretty(&v).expect("serializable") + "\n"
}

/// The report in one format, scrubbed.
pub fn render(p: &Passport, format: &str) -> Result<String> {
    Ok(scrub(&match format {
        "md" => render_md(p),
        "html" => render_html(p),
        "json" => render_json(p),
        other => bail!("--format must be md, html or json, got {other}"),
    }))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn recipe_names(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join(".config/fluxvm/recipes"))
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Gathers what the report needs. A server that cannot be reached still yields a report, with a note saying so.
pub async fn gather(a: &ReportArgs) -> Passport {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let notes_path = a.notes_file.clone().unwrap_or_else(|| home.join(".config/fluxvm/passport-notes.md"));
    let notes = std::fs::read_to_string(&notes_path).unwrap_or_default();
    let server = a.server.server.clone().unwrap_or_else(|| "http://127.0.0.1:7799".into());
    let mut p = Passport {
        generated_at: now_rfc3339(),
        fluxctl_version: env!("CARGO_PKG_VERSION").to_owned(),
        server: crate::cluster_pair::normalize_server(&server).unwrap_or_default(),
        server_note: None,
        machines: vec![],
        pending_requests: 0,
        pairing_on: None,
        recipes: recipe_names(&home),
        notes,
        state_dir: String::new(),
    };
    // On the server itself the address other machines use is the LAN one, not 127.0.0.1.
    if p.server.contains("127.0.0.1") && let Some(ip) = fluxvm_core::mdns::local_ipv4() {
        p.server = p.server.replace("127.0.0.1", &ip.to_string());
    }
    let nodes = crate::cluster_pair::admin_get(&a.server, "/fleet/nodes").await;
    match nodes {
        Ok(v) => {
            p.machines = machines_from(&v);
            if let Ok(pv) = crate::cluster_pair::admin_get(&a.server, "/fleet/pair/pending").await {
                p.pending_requests = pv["items"].as_array().map(Vec::len).unwrap_or(0);
                p.pairing_on = pv["enabled"].as_bool();
            }
        }
        Err(e) => p.server_note = Some(format!("The server could not be asked ({e:#}); the machine list is empty. Run this on the server, or pass --server and --token-file.")),
    }
    p
}

/// One HTTP response for the read-only page. Only GET, only for a loopback Host (so a web page elsewhere cannot make the
/// browser read it through DNS rebinding), and nothing else is served.
pub fn respond(head: &str, port: u16, render: &dyn Fn(&str) -> String) -> Vec<u8> {
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut it = first.split_whitespace();
    let (method, path) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let host = lines.find_map(|l| l.strip_prefix("Host:").or_else(|| l.strip_prefix("host:"))).map(str::trim).unwrap_or("");
    let ok_host = [format!("127.0.0.1:{port}"), format!("localhost:{port}")].iter().any(|h| h == host);
    let reply = |code: &str, ctype: &str, body: &str| {
        format!("HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    };
    if method != "GET" {
        return reply("405 Method Not Allowed", "text/plain", "read-only\n");
    }
    if !ok_host {
        return reply("403 Forbidden", "text/plain", "use http://127.0.0.1\n");
    }
    match path {
        "/" | "/index.html" => reply("200 OK", "text/html; charset=utf-8", &render("html")),
        "/report.md" => reply("200 OK", "text/markdown; charset=utf-8", &render("md")),
        "/report.json" => reply("200 OK", "application/json", &render("json")),
        _ => reply("404 Not Found", "text/plain", "not found\n"),
    }
}

pub async fn run(a: ReportArgs) -> Result<()> {
    if a.serve {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind(("127.0.0.1", a.port)).await.with_context(|| format!("port {} is busy; use --port", a.port))?;
        println!("Cluster passport (read-only): http://127.0.0.1:{}/   Ctrl-C to stop", a.port);
        loop {
            let (mut s, _) = l.accept().await?;
            let mut buf = vec![0u8; 4096];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut buf)).await.ok().and_then(|r| r.ok()).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            // Rendering is a fresh look at the cluster each time; the formats are cheap, the gather is the cost.
            let p = gather(&a).await;
            let out = respond(&head, a.port, &|f| render(&p, f).unwrap_or_default());
            let _ = s.write_all(&out).await;
            let _ = s.shutdown().await;
        }
    }
    let p = gather(&a).await;
    let text = render(&p, &a.format)?;
    match &a.out {
        Some(f) => {
            std::fs::write(f, &text).with_context(|| format!("writing {}", f.display()))?;
            println!("Wrote {} ({} machine(s)). It contains no passwords or tokens.", f.display(), p.machines.len());
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passport() -> Passport {
        Passport {
            generated_at: "2026-11-21T10:00:00Z".into(),
            fluxctl_version: "0.4.21".into(),
            server: "http://192.168.1.20:7799".into(),
            server_note: None,
            machines: vec![
                Machine { name: "mini-1".into(), role: "Mac (macOS VMs, Docker, k3s)".into(), vcpus: 10, memory_gib: 32, vms: 2, agent_version: "0.4.21".into(), status: "online".into(), last_seen: "2026-11-21T09:59:50Z".into() },
                Machine { name: "box-1".into(), role: "Linux (VMs)".into(), vcpus: 8, memory_gib: 16, vms: 0, agent_version: "0.4.20".into(), status: "offline".into(), last_seen: "2026-11-20T10:00:00Z".into() },
            ],
            pending_requests: 1,
            pairing_on: Some(true),
            recipes: vec!["registry".into()],
            notes: "Call Ana on 555-0101 if the power fails.".into(),
            state_dir: "/var/lib/fluxvm-agent".into(),
        }
    }

    fn golden(name: &str, got: &str) {
        let path = format!("{}/src/recipes_golden/passport.{name}", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(&path, got).unwrap();
        }
        assert_eq!(got, std::fs::read_to_string(&path).unwrap_or_default(), "golden {path}; UPDATE_GOLDEN=1 to accept");
    }

    #[test]
    fn the_passport_matches_its_golden_files() {
        let p = passport();
        golden("md", &render(&p, "md").unwrap());
        golden("html", &render(&p, "html").unwrap());
        golden("json", &render(&p, "json").unwrap());
        let j: serde_json::Value = serde_json::from_str(&render(&p, "json").unwrap()).unwrap();
        assert_eq!(j["health"]["offline"], 1);
        assert!(render(&p, "pdf").is_err());
    }

    #[test]
    fn it_has_every_section_a_person_needs() {
        let md = render(&passport(), "md").unwrap();
        for s in ["Machines and roles", "Versions", "How to add a machine", "fluxctl cluster pair", "How to back up and restore", "How to undo", "Where the logs are", "Contacts and notes", "Call Ana"] {
            assert!(md.contains(s), "missing {s}");
        }
        assert!(md.contains("Different versions are running"));
    }

    /// Everything that could be a secret and is not supposed to leave: pasted into the places a person controls.
    const SECRETS: &[&str] = &[
        "fvnc_0123456789abcdef.3.9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        "fvjt_abcdef012345.0123456789abcdef0123456789abcdef",
        "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        "github_pat_11AAAAAAA0abcdefghijklmnop",
        "hunter2hunter2",
        "s3cr3t-value",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
        "0123456789abcdef0123456789abcdef01234567",
        "pa55w0rd",
    ];

    #[test]
    fn secrets_pasted_anywhere_never_leave_in_any_format() {
        let mut p = passport();
        p.notes = format!(
            "Admin token: {}\njoin token {}\nGitHub: {}\nPAT {}\npassword={}\nsecret: {}\nBearer {}\nhash {}\nwifi password: {}\nurl https://admin:{}@nas.local/x",
            SECRETS[0], SECRETS[1], SECRETS[2], SECRETS[3], SECRETS[4], SECRETS[5], SECRETS[6], SECRETS[7], SECRETS[8], SECRETS[4]
        );
        p.server_note = Some(format!("token={}", SECRETS[1]));
        p.machines[0].name = format!("mini-{}", SECRETS[2]);
        p.server = format!("https://root:{}@192.168.1.20:7799", SECRETS[4]);
        for f in ["md", "html", "json"] {
            let out = render(&p, f).unwrap();
            for s in SECRETS {
                assert!(!out.contains(s), "{f} leaked {s}\n{out}");
            }
        }
        // The harmless words around them survive.
        let md = render(&p, "md").unwrap();
        assert!(md.contains("Call") || md.contains("Admin token") && md.contains("[redacted]"));
        assert!(md.contains("192.168.1.20:7799") && !md.contains("root:"));
    }

    #[test]
    fn ordinary_text_is_not_redacted() {
        let t = "Generated 2026-11-21T10:00:00Z by fluxctl 0.4.21. fluxctl cluster approve ABCDE-FGHJK; tar czf backup.tgz -C /var/lib/fluxvm-agent .";
        assert_eq!(scrub(t), t);
        let md = render(&passport(), "md").unwrap();
        assert!(!md.contains("[redacted]"), "{md}");
        let html = render(&passport(), "html").unwrap();
        assert!(!html.contains("[redacted]"));
    }

    #[test]
    fn the_report_struct_has_no_field_that_could_hold_a_secret() {
        let j = serde_json::to_string(&passport()).unwrap().to_lowercase();
        for banned in ["token", "password", "credential", "secret", "private"] {
            assert!(!j.contains(&format!("\"{banned}")), "a field named like {banned}");
        }
    }

    #[test]
    fn html_is_escaped() {
        let mut p = passport();
        p.notes = "<script>alert(1)</script>".into();
        p.machines[0].name = "a<b>".into();
        let h = render(&p, "html").unwrap();
        assert!(!h.contains("<script>") && !h.contains("a<b>"));
    }

    #[test]
    fn machines_are_read_from_the_fleet_listing() {
        let v = serde_json::json!({"items": [
            {"name": "m1", "healthy": true, "apple": {"x": 1}, "vcpus_total": 10, "memory_mib_total": 32768, "vm_count": 1, "agent_version": "0.4.21", "last_seen": "t"},
            {"name": "b1", "healthy": true, "apple": null, "outdated": true},
            {"name": "c1", "healthy": true, "cordoned": true},
            {"name": "d1", "healthy": false},
        ]});
        let m = machines_from(&v);
        assert_eq!(m.iter().map(|m| m.status.as_str()).collect::<Vec<_>>(), ["online", "outdated", "cordoned", "offline"]);
        assert!(m[0].role.starts_with("Mac") && m[1].role.starts_with("Linux"));
        assert_eq!(m[0].memory_gib, 32);
    }

    #[test]
    fn the_local_page_is_read_only_and_loopback_only() {
        let r = |f: &str| format!("<{f}>");
        let get = |req: &str| String::from_utf8(respond(req, 7801, &r)).unwrap();
        assert!(get("GET / HTTP/1.1\r\nHost: 127.0.0.1:7801\r\n\r\n").starts_with("HTTP/1.1 200"));
        assert!(get("GET /report.json HTTP/1.1\r\nHost: localhost:7801\r\n\r\n").contains("<json>"));
        assert!(get("POST / HTTP/1.1\r\nHost: 127.0.0.1:7801\r\n\r\n").starts_with("HTTP/1.1 405"));
        assert!(get("GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n").starts_with("HTTP/1.1 403"));
        assert!(get("GET /etc/passwd HTTP/1.1\r\nHost: 127.0.0.1:7801\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(get("").starts_with("HTTP/1.1 405"));
    }
}
