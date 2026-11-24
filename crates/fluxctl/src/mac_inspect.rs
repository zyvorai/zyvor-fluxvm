// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Pure views of `fluxctl mac` instances: `ps` filters and `--format`, `inspect`, `port`, `stats`, `top`.
//!
//! Everything here takes the daemon's VM record (JSON) and returns text or JSON, so it is unit tested without a daemon.
//! The field names follow `VmRecord` in `fluxvm-core`; a field the daemon leaves out shows as `-`.
//! Not verified against records from a real daemon.

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub const INSTANCE_LABEL: &str = "fluxvm.mac";
pub const IMAGE_LABEL: &str = "fluxvm.mac.image";
pub const RESTART_LABEL: &str = "fluxvm.mac.restart";
pub const PROJECT_LABEL: &str = "fluxvm.mac.project";
pub const SERVICE_LABEL: &str = "fluxvm.mac.service";
/// Set by `mac stop` so a `unless-stopped` policy is not undone by `mac reconcile`.
pub const STOPPED_LABEL: &str = "fluxvm.mac.stopped";
/// The guest paths `-v` asked for, as `fs0=/guest/path,...` (the share tags are positional).
pub const MOUNTS_LABEL: &str = "fluxvm.mac.mounts";

/// Labels the user did not set (ours all start with `fluxvm.mac`).
pub fn is_internal_label(k: &str) -> bool {
    k == INSTANCE_LABEL || k.starts_with("fluxvm.mac.")
}

pub fn status(vm: &Value) -> &str {
    vm["status"].as_str().unwrap_or("unknown")
}

pub fn short_id(vm: &Value) -> String {
    vm["id"]
        .as_str()
        .unwrap_or("-")
        .chars()
        .filter(|c| *c != '-')
        .take(12)
        .collect()
}

/// `HOST->GUEST` pairs of the published ports.
pub fn ports(vm: &Value) -> Vec<(u64, u64)> {
    vm["request"]["network"]["forwards"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| Some((f["host_port"].as_u64()?, f["guest_port"].as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn ports_text(vm: &Value) -> String {
    let p = ports(vm);
    if p.is_empty() {
        "-".into()
    } else {
        p.iter()
            .map(|(h, g)| format!("{h}->{g}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// `mac port NAME [GUEST_PORT]`: one `GUEST -> 127.0.0.1:HOST` line per published port.
pub fn port_lines(vm: &Value, guest: Option<u16>) -> Result<String> {
    let addr = |f: &Value| {
        f["host_addr"]
            .as_str()
            .or_else(|| f["addr"].as_str())
            .unwrap_or("0.0.0.0")
            .to_string()
    };
    let fwd = vm["request"]["network"]["forwards"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut out = String::new();
    for f in &fwd {
        let (Some(h), Some(g)) = (f["host_port"].as_u64(), f["guest_port"].as_u64()) else {
            continue;
        };
        if guest.is_some_and(|w| u64::from(w) != g) {
            continue;
        }
        out.push_str(&format!("{g}/tcp -> {}:{h}\n", addr(f)));
    }
    if out.is_empty() {
        match guest {
            Some(g) => bail!("no published port {g} (publish with `mac run -p HOST:{g}`)"),
            None => bail!("no published ports"),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Name(String),
    Id(String),
    Status(String),
    Image(String),
    Label(String, Option<String>),
    Project(String),
    Service(String),
    Ip(String),
    /// `starting`, `healthy`, `unhealthy` or `none` (no probe has run).
    Health(String),
    /// The restart policy; an instance without one has `no`.
    Restart(String),
    /// A published port, host or guest side.
    Publish(u16),
}

/// The keys `mac ps --filter` accepts, for messages.
pub const FILTER_KEYS: &str =
    "name, id, status, health, image, label, project, service, ip, publish, restart";

/// `--filter name=web`, `status=running`, `id=ab12`, `image=macos`, `label=k` or `label=k=v`.
pub fn parse_filter(s: &str) -> Result<Filter> {
    let Some((k, v)) = s.split_once('=') else {
        bail!(
            "filter {s:?} must be KEY=VALUE ({FILTER_KEYS}); see `fluxctl explain MAC_FILTER_INVALID`"
        );
    };
    ensure!(!v.is_empty(), "filter {s:?} has an empty value");
    Ok(match k {
        "name" => Filter::Name(v.into()),
        "id" => Filter::Id(v.into()),
        "status" => {
            ensure!(
                matches!(v, "creating" | "running" | "paused" | "stopped" | "failed"),
                "status filter {v:?}: use creating, running, paused, stopped or failed"
            );
            Filter::Status(v.into())
        }
        "health" => {
            ensure!(
                matches!(v, "starting" | "healthy" | "unhealthy" | "none"),
                "health filter {v:?}: use starting, healthy, unhealthy or none; see `fluxctl explain MAC_FILTER_INVALID`"
            );
            Filter::Health(v.into())
        }
        "restart" => {
            ensure!(
                matches!(v, "no" | "always" | "unless-stopped" | "on-failure"),
                "restart filter {v:?}: use no, always, unless-stopped or on-failure; see `fluxctl explain MAC_FILTER_INVALID`"
            );
            Filter::Restart(v.into())
        }
        "image" | "ancestor" => Filter::Image(v.into()),
        "label" => match v.split_once('=') {
            Some((lk, lv)) => Filter::Label(lk.into(), Some(lv.into())),
            None => Filter::Label(v.into(), None),
        },
        "project" => Filter::Project(v.into()),
        "service" => Filter::Service(v.into()),
        "ip" => Filter::Ip(v.into()),
        "publish" | "port" => Filter::Publish(v.parse().map_err(|_| {
            anyhow::anyhow!(
                "filter {s:?}: {v:?} is not a port number (publish=8080); see `fluxctl explain MAC_FILTER_INVALID`"
            )
        })?),
        _ => bail!(
            "unknown filter {k:?}: use one of {FILTER_KEYS}; see `fluxctl explain MAC_FILTER_INVALID`"
        ),
    })
}

pub fn parse_filters(f: &[String]) -> Result<Vec<Filter>> {
    f.iter().map(|s| parse_filter(s)).collect()
}

/// Every filter must match; two filters of the same kind are an AND too (unlike Docker, which ORs them).
pub fn matches(vm: &Value, filters: &[Filter]) -> bool {
    filters.iter().all(|f| match f {
        Filter::Name(n) => vm["name"].as_str().is_some_and(|x| x.contains(n.as_str())),
        Filter::Id(p) => {
            short_id(vm).starts_with(p.as_str())
                || vm["id"].as_str().is_some_and(|x| x.starts_with(p.as_str()))
        }
        Filter::Status(s) => status(vm) == s,
        Filter::Image(i) => vm["labels"][IMAGE_LABEL]
            .as_str()
            .is_some_and(|x| x == i || x.starts_with(&format!("{i}:")) || x.contains(i.as_str())),
        Filter::Label(k, v) => match (vm["labels"][k.as_str()].as_str(), v) {
            (Some(x), Some(want)) => x == want,
            (Some(_), None) => true,
            (None, _) => false,
        },
        Filter::Project(p) => vm["labels"][PROJECT_LABEL].as_str() == Some(p.as_str()),
        Filter::Service(p) => vm["labels"][SERVICE_LABEL].as_str() == Some(p.as_str()),
        Filter::Ip(p) => vm["guest_ip"].as_str() == Some(p.as_str()),
        Filter::Health(h) => match crate::mac_health::from_labels(vm) {
            Some(x) => x.status.as_str() == h,
            None => h == "none",
        },
        Filter::Restart(p) => vm["labels"][RESTART_LABEL].as_str().unwrap_or("no") == p,
        Filter::Publish(port) => ports(vm)
            .iter()
            .any(|(h, g)| *h == u64::from(*port) || *g == u64::from(*port)),
    })
}

/// Without `-a` Docker shows running containers only; here that is every state except stopped and failed.
pub fn visible_by_default(vm: &Value) -> bool {
    !matches!(status(vm), "stopped" | "failed")
}

/// Apply `-a` and `--filter`.
pub fn select<'a>(items: &[&'a Value], all: bool, filters: &[Filter]) -> Vec<&'a Value> {
    items
        .iter()
        .copied()
        .filter(|v| {
            (all || visible_by_default(v) || filters.iter().any(|f| matches!(f, Filter::Status(_))))
                && matches(v, filters)
        })
        .collect()
}

fn field(vm: &Value, name: &str) -> Result<String> {
    let s = |p: &Value| p.as_str().unwrap_or("-").to_string();
    Ok(match name {
        "ID" => short_id(vm),
        "FullID" => s(&vm["id"]),
        "Name" | "Names" => s(&vm["name"]),
        "Image" => s(&vm["labels"][IMAGE_LABEL]),
        "Status" | "State" => status(vm).to_string(),
        "IP" => s(&vm["guest_ip"]),
        "Ports" => ports_text(vm),
        "Created" | "CreatedAt" => s(&vm["created_at"]),
        "Restart" => s(&vm["labels"][RESTART_LABEL]),
        "Health" => crate::mac_health::health_text(vm),
        "Project" => s(&vm["labels"][PROJECT_LABEL]),
        "Service" => s(&vm["labels"][SERVICE_LABEL]),
        "Labels" => {
            let mut l: Vec<String> = vm["labels"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| !is_internal_label(k))
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                        .collect()
                })
                .unwrap_or_default();
            l.sort();
            l.join(",")
        }
        other => bail!(
            "unknown field {{{{.{other}}}}} in --format (ID, Name, Image, Status, IP, Ports, Created, Restart, Health, Project, Service, Labels)"
        ),
    })
}

/// Placeholders in a `--format` string, in order.
fn placeholders(fmt: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = fmt;
    while let Some(i) = rest.find("{{") {
        let Some(j) = rest[i..].find("}}") else {
            bail!("--format {fmt:?}: unterminated {{{{");
        };
        out.push(rest[i + 2..i + j].trim().to_string());
        rest = &rest[i + j + 2..];
    }
    Ok(out)
}

/// One formatted line. `{{.Name}}`-style placeholders only (no Go template logic); `{{json .}}` prints the record.
pub fn render_format_row(vm: &Value, fmt: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find("}}") else {
            bail!("--format {fmt:?}: unterminated {{{{");
        };
        let key = rest[i + 2..i + j].trim();
        if key == "json ." {
            out.push_str(&serde_json::to_string(&summary(vm))?);
        } else if let Some(name) = key.strip_prefix('.') {
            out.push_str(&field(vm, name)?);
        } else {
            bail!("--format placeholder {{{{{key}}}}} must look like {{{{.Name}}}}");
        }
        rest = &rest[i + j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `--format` output for every row; `table <fmt>` adds a header row and aligns the columns (tab-separated placeholders).
pub fn render_format(rows: &[&Value], fmt: &str) -> Result<String> {
    let (table, fmt) = match fmt.strip_prefix("table") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
            (true, rest.trim())
        }
        _ => (false, fmt),
    };
    if !table {
        let mut out = String::new();
        for v in rows {
            out.push_str(&render_format_row(v, fmt)?);
            out.push('\n');
        }
        return Ok(out);
    }
    let fmt = if fmt.is_empty() {
        "{{.ID}}\t{{.Name}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"
    } else {
        fmt
    };
    let header: Vec<String> = placeholders(fmt)?
        .iter()
        .map(|p| p.trim_start_matches('.').to_ascii_uppercase())
        .collect();
    let mut grid: Vec<Vec<String>> = vec![header];
    for v in rows {
        grid.push(
            render_format_row(v, fmt)?
                .split('\t')
                .map(str::to_string)
                .collect(),
        );
    }
    Ok(align_grid(&grid))
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
pub fn align_grid(grid: &[Vec<String>]) -> String {
    let cols = grid.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            grid.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in grid {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i + 1 == r.len() {
                    s.clone()
                } else {
                    format!("{s:<w$}", w = widths[i])
                }
            })
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

/// `ps -q`: one short id per line.
pub fn render_quiet(rows: &[&Value]) -> String {
    rows.iter().map(|v| format!("{}\n", short_id(v))).collect()
}

/// A compact record for `ps --json` and `{{json .}}`.
pub fn summary(vm: &Value) -> Value {
    json!({
        "id": short_id(vm),
        "name": vm["name"],
        "image": vm["labels"][IMAGE_LABEL],
        "status": status(vm),
        "ip": vm["guest_ip"],
        "ports": ports(vm).iter().map(|(h, g)| format!("{h}->{g}")).collect::<Vec<_>>(),
        "created": vm["created_at"],
    })
}

/// `mac inspect NAME`: everything about an instance in one document. `image` is the stored image's config when known.
pub fn inspect(vm: &Value, image: Option<Value>) -> Value {
    let labels = vm["labels"].as_object().cloned().unwrap_or_default();
    let user_labels: serde_json::Map<String, Value> = labels
        .iter()
        .filter(|(k, _)| !is_internal_label(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mounts: Vec<Value> = vm["request"]["shared_folders"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| {
                    json!({
                        "source": m["host_path"],
                        "destination": m["guest_path"],
                        "read_only": m["read_only"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": vm["id"],
        "name": vm["name"],
        "image": vm["labels"][IMAGE_LABEL],
        "state": {
            "status": status(vm),
            "pid": vm["pid"],
            "error": vm["error"],
            "restart_policy": vm["labels"][RESTART_LABEL].as_str().unwrap_or("no"),
            "health": crate::mac_health::inspect_value(vm),
            "expires_unix": vm["labels"][crate::mac_reap::EXPIRES_LABEL],
        },
        "created": vm["created_at"],
        "network": {
            "ip": vm["guest_ip"],
            "ports": ports(vm).iter().map(|(h, g)| json!({"host": h, "guest": g})).collect::<Vec<_>>(),
        },
        "resources": {
            "cpus": vm["request"]["vcpus"],
            "memory_mib": vm["request"]["memory_mib"],
        },
        "mounts": mounts,
        "labels": user_labels,
        "disk": vm["disk"],
        "workspace": vm["workspace"],
        "image_config": image.unwrap_or(Value::Null),
        "vm": vm,
    })
}

/// One `stats` line from `GET /v1/vms/{id}/stats` (`VmMetrics`). The CPU figure is averaged over the VM's lifetime.
pub fn stats_row(name: &str, metrics: &Value) -> String {
    format!(
        "{:<20}  {:>7.1}%  {:>10}  {:>10}  {:>10}\n",
        name,
        metrics["cpu_usage_percent"].as_f64().unwrap_or(0.0),
        human_bytes(metrics["memory_usage_bytes"].as_u64().unwrap_or(0)),
        human_bytes(metrics["disk_read_bytes"].as_u64().unwrap_or(0)),
        human_bytes(metrics["disk_write_bytes"].as_u64().unwrap_or(0)),
    )
}

pub fn stats_header() -> String {
    format!(
        "{:<20}  {:>8}  {:>10}  {:>10}  {:>10}\n",
        "NAME", "CPU %", "MEM", "DISK READ", "DISK WRITE"
    )
}

pub fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K {
        format!("{:.1} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.1} KiB", f / K)
    } else {
        format!("{b} B")
    }
}

/// The `ps` command `mac top` runs in the guest; extra words are passed to `ps` as given.
pub fn top_command(extra: &[String]) -> String {
    if extra.is_empty() {
        "ps -axo pid,user,%cpu,%mem,etime,command".into()
    } else {
        format!("ps {}", extra.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, status: &str, image: &str) -> Value {
        json!({
            "id": "0123abcd-4567-89ab-cdef-0123456789ab",
            "name": name,
            "status": status,
            "guest_ip": "192.168.64.5",
            "created_at": "2026-11-01T10:00:00Z",
            "labels": {
                "fluxvm.mac": "instance", "fluxvm.mac.image": image,
                "fluxvm.mac.restart": "always", "team": "ci"
            },
            "request": {
                "vcpus": 4, "memory_mib": 8192,
                "network": {"forwards": [{"host_port": 2222, "guest_port": 22}, {"host_port": 8080, "guest_port": 80, "host_addr": "127.0.0.1"}]},
                "shared_folders": [{"host_path": "/Users/a/src", "guest_path": "/src", "read_only": true}]
            },
            "disk": "/var/vms/x/disk.raw", "workspace": "/var/vms/x", "pid": 99
        })
    }

    #[test]
    fn filters_parse_and_reject_bad_input() {
        assert_eq!(
            parse_filter("name=web").unwrap(),
            Filter::Name("web".into())
        );
        assert_eq!(
            parse_filter("label=a=b").unwrap(),
            Filter::Label("a".into(), Some("b".into()))
        );
        assert_eq!(
            parse_filter("label=a").unwrap(),
            Filter::Label("a".into(), None)
        );
        assert!(parse_filter("name").is_err());
        assert!(parse_filter("name=").is_err());
        assert!(parse_filter("color=red").is_err());
        let e = parse_filter("status=sleeping").unwrap_err().to_string();
        assert!(e.contains("running"), "{e}");
    }

    #[test]
    fn filters_select_instances() {
        let a = vm("build-1", "running", "macos-xcode:15");
        let b = vm("web", "stopped", "macos-base:14");
        let f = |s: &str| parse_filters(&[s.to_string()]).unwrap();
        assert!(matches(&a, &f("name=build")));
        assert!(!matches(&b, &f("name=build")));
        assert!(matches(&a, &f("id=0123ab")));
        assert!(matches(&a, &f("status=running")));
        assert!(matches(&a, &f("image=macos-xcode")));
        assert!(matches(&a, &f("image=macos-xcode:15")));
        assert!(!matches(&b, &f("image=macos-xcode")));
        assert!(matches(&a, &f("label=team=ci")));
        assert!(matches(&a, &f("label=team")));
        assert!(!matches(&a, &f("label=team=dev")));
        assert!(!matches(&a, &f("label=nope")));
        let both = parse_filters(&["name=build".into(), "status=stopped".into()]).unwrap();
        assert!(!matches(&a, &both));
    }

    #[test]
    fn select_hides_stopped_unless_asked() {
        let a = vm("a", "running", "i");
        let b = vm("b", "stopped", "i");
        let c = vm("c", "paused", "i");
        let rows = [&a, &b, &c];
        let names = |v: Vec<&Value>| {
            v.iter()
                .map(|x| x["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(select(&rows, false, &[])), ["a", "c"]);
        assert_eq!(names(select(&rows, true, &[])), ["a", "b", "c"]);
        // an explicit status filter is an explicit ask.
        let st = parse_filters(&["status=stopped".into()]).unwrap();
        assert_eq!(names(select(&rows, false, &st)), ["b"]);
    }

    #[test]
    fn format_placeholders_and_json() {
        let a = vm("web", "running", "macos:15");
        assert_eq!(
            render_format_row(&a, "{{.Name}} is {{ .Status }} on {{.Ports}}").unwrap(),
            "web is running on 2222->22,8080->80"
        );
        assert_eq!(render_format_row(&a, "{{.ID}}").unwrap(), "0123abcd4567");
        assert_eq!(render_format_row(&a, "{{.Labels}}").unwrap(), "team=ci");
        let j: Value = serde_json::from_str(&render_format_row(&a, "{{json .}}").unwrap()).unwrap();
        assert_eq!(j["name"], "web");
        assert!(render_format_row(&a, "{{.Nope}}").is_err());
        assert!(render_format_row(&a, "{{Name}}").is_err());
        assert!(render_format_row(&a, "{{.Name").is_err());
        assert_eq!(
            render_format_row(&json!({}), "{{.Name}}/{{.IP}}").unwrap(),
            "-/-"
        );
    }

    #[test]
    fn table_format_has_a_header_and_aligned_columns() {
        let a = vm("w", "running", "i");
        let b = vm("longer-name", "paused", "i");
        let t = render_format(&[&a, &b], "table {{.Name}}\t{{.Status}}").unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "NAME         STATUS");
        assert_eq!(lines[1], "w            running");
        assert_eq!(lines[2], "longer-name  paused");
        let d = render_format(&[&a], "table").unwrap();
        assert!(d.starts_with("ID"), "{d}");
        let plain = render_format(&[&a, &b], "{{.Name}}").unwrap();
        assert_eq!(plain, "w\nlonger-name\n");
    }

    #[test]
    fn quiet_prints_short_ids() {
        let a = vm("a", "running", "i");
        assert_eq!(render_quiet(&[&a, &a]), "0123abcd4567\n0123abcd4567\n");
    }

    #[test]
    fn port_lines_and_errors() {
        let a = vm("a", "running", "i");
        let all = port_lines(&a, None).unwrap();
        assert!(all.contains("22/tcp -> 0.0.0.0:2222"), "{all}");
        assert!(all.contains("80/tcp -> 127.0.0.1:8080"), "{all}");
        assert_eq!(
            port_lines(&a, Some(80)).unwrap(),
            "80/tcp -> 127.0.0.1:8080\n"
        );
        let e = port_lines(&a, Some(443)).unwrap_err().to_string();
        assert!(e.contains("no published port 443"), "{e}");
        assert!(port_lines(&json!({}), None).is_err());
    }

    #[test]
    fn inspect_hides_internal_labels_and_keeps_the_raw_record() {
        let a = vm("web", "running", "macos:15");
        let i = inspect(&a, Some(json!({"workdir": "/x"})));
        assert_eq!(i["name"], "web");
        assert_eq!(i["image"], "macos:15");
        assert_eq!(i["state"]["restart_policy"], "always");
        assert_eq!(i["labels"], json!({"team": "ci"}));
        assert_eq!(i["resources"]["cpus"], 4);
        assert_eq!(i["resources"]["memory_mib"], 8192);
        assert_eq!(i["mounts"][0]["destination"], "/src");
        assert_eq!(i["mounts"][0]["read_only"], true);
        assert_eq!(i["network"]["ports"][0], json!({"host": 2222, "guest": 22}));
        assert_eq!(i["image_config"]["workdir"], "/x");
        assert_eq!(i["vm"]["pid"], 99);
        let bare = inspect(&json!({"name": "x"}), None);
        assert_eq!(bare["state"]["restart_policy"], "no");
        assert!(bare["image_config"].is_null());
    }

    #[test]
    fn stats_rows() {
        let m = json!({"cpu_usage_percent": 12.34, "memory_usage_bytes": 3 * 1024 * 1024 * 1024u64, "disk_read_bytes": 2048, "disk_write_bytes": 5});
        let r = stats_row("web", &m);
        assert!(
            r.contains("12.3%")
                && r.contains("3.0 GiB")
                && r.contains("2.0 KiB")
                && r.contains("5 B"),
            "{r}"
        );
        assert!(stats_header().starts_with("NAME"));
    }

    #[test]
    fn top_command_defaults_and_passthrough() {
        assert!(top_command(&[]).contains("pid,user"));
        assert_eq!(top_command(&["aux".into()]), "ps aux");
    }

    #[test]
    fn ps_filters_by_project_service_ip_and_published_port() {
        let vm = json!({
            "id": "0b1c2d3e-0000-4000-8000-000000000001", "name": "web-1", "status": "running", "guest_ip": "192.168.64.7",
            "labels": {"fluxvm.mac.project": "shop", "fluxvm.mac.service": "web"},
            "request": {"network": {"forwards": [{"host_port": 2222, "guest_port": 22}]}}
        });
        let m = |f: &str| matches(&vm, &[parse_filter(f).unwrap()]);
        assert!(m("project=shop") && !m("project=other"));
        assert!(m("service=web") && !m("service=db"));
        assert!(m("ip=192.168.64.7") && !m("ip=192.168.64.8"));
        assert!(m("publish=2222") && m("port=22") && !m("publish=80"));
        for bad in ["publish=http", "colour=red", "nokey"] {
            let e = parse_filter(bad).unwrap_err().to_string();
            assert!(e.contains("MAC_FILTER_INVALID"), "{bad}: {e}");
        }
        assert!(
            parse_filter("colour=red")
                .unwrap_err()
                .to_string()
                .contains("project, service, ip, publish")
        );
    }

    #[test]
    fn health_and_restart_filters() {
        let vm = |labels: Value| json!({"name": "a", "status": "running", "labels": labels});
        let sick = vm(json!({"fluxvm.mac.health": "unhealthy", "fluxvm.mac.restart": "always"}));
        let fresh = vm(json!({}));
        let f = |s: &str| parse_filters(&[s.to_string()]).unwrap();
        assert!(matches(&sick, &f("health=unhealthy")) && !matches(&sick, &f("health=healthy")));
        assert!(matches(&fresh, &f("health=none")) && !matches(&sick, &f("health=none")));
        assert!(matches(&sick, &f("restart=always")) && matches(&fresh, &f("restart=no")));
        assert!(!matches(&fresh, &f("restart=always")));
        for bad in ["health=sick", "restart=sometimes"] {
            let e = parse_filter(bad).unwrap_err().to_string();
            assert!(e.contains("MAC_FILTER_INVALID"), "{bad}: {e}");
        }
        assert_eq!(field(&sick, "Health").unwrap(), "unhealthy");
        assert_eq!(field(&fresh, "Health").unwrap(), "-");
    }
}
