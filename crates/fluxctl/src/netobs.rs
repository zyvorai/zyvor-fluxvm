// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl netobs ...`: guest-network observability over the daemon's `/v1/netobs/*` routes. The observer lives in
//! the daemon process, so these commands always talk REST (to `--server`/the current context, or to the local
//! daemon's `listen` address).

use crate::output::{self, Column, col};
use crate::remote::{Remote, encode_query};
use anyhow::{Context, Result};
use clap::Subcommand;
use reqwest::Method;
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Subcommand, Debug)]
pub enum NetobsCommand {
    /// Host-wide and per-VM roll-up: counters, top talkers, top errors, capture sessions.
    Summary {
        #[arg(long)]
        json: bool,
    },
    /// Flow table. With `--follow`, tails the live flow/DNS/TLS/HTTP/drop event stream (SSE) instead.
    Flows {
        /// Only this VM (id, name or unique id prefix).
        #[arg(long, value_parser = output::parse_vm_ref)]
        vm: Option<Uuid>,
        /// tcp, udp, icmp, icmpv6 or a protocol number.
        #[arg(long)]
        proto: Option<String>,
        /// Match either endpoint's port.
        #[arg(long)]
        port: Option<u16>,
        /// Match either endpoint's IP.
        #[arg(long)]
        host: Option<std::net::IpAddr>,
        /// Verdict kind as shown in the VERDICT column.
        #[arg(long)]
        verdict: Option<String>,
        /// Include recently closed flows.
        #[arg(long)]
        closed: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Stream live events instead of listing the table (`GET /v1/netobs/flows/stream`).
        #[arg(long, short = 'f')]
        follow: bool,
        /// With `--follow`: resume after this event sequence number.
        #[arg(long, requires = "follow")]
        since: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Recent DNS transactions (names are subject to the daemon's redaction policy).
    Dns {
        #[arg(long, value_parser = output::parse_vm_ref)]
        vm: Option<Uuid>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Retained roll-up windows.
    History {
        #[arg(long)]
        json: bool,
    },
    /// Bounded, filtered packet capture (admin token; holds packet bytes).
    Capture {
        #[command(subcommand)]
        command: CaptureCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum CaptureCommand {
    /// Start capturing one VM's traffic. At least one of `--host`, `--port`, `--proto` is required.
    Start {
        #[arg(long, value_parser = output::parse_vm_ref)]
        vm: Uuid,
        #[arg(long)]
        host: Option<std::net::IpAddr>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        proto: Option<String>,
        /// Session lifetime; the daemon caps it.
        #[arg(long)]
        ttl_secs: Option<u64>,
        #[arg(long)]
        snaplen: Option<usize>,
        #[arg(long)]
        max_packets: Option<usize>,
        #[arg(long)]
        max_bytes: Option<usize>,
    },
    /// Stop and discard a VM's capture session.
    Stop {
        #[arg(long, value_parser = output::parse_vm_ref)]
        vm: Uuid,
    },
    /// Capture sessions.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Download the capture as a pcap file (`-O -` writes to stdout).
    Download {
        #[arg(long, value_parser = output::parse_vm_ref)]
        vm: Uuid,
        #[arg(long, short = 'O')]
        output: PathBuf,
    },
}

const FLOW_COLUMNS: &[Column] = &[
    col("VM", "/vm_id"),
    col("PROTO", "/proto"),
    col("CLIENT", "/_client"),
    col("SERVER", "/_server"),
    col("VERDICT", "/verdict"),
    col("APP", "/app"),
    col("SNI", "/sni"),
    col("BYTES_OUT", "/bytes_c2s"),
    col("BYTES_IN", "/bytes_s2c"),
];

const DNS_COLUMNS: &[Column] = &[
    col("VM", "/vm_id"),
    col("QNAME", "/qname"),
    col("QTYPE", "/qtype"),
    col("RCODE", "/rcode"),
    col("RESPONSE", "/response"),
    col("LATENCY_US", "/latency_us"),
];

const CAPTURE_COLUMNS: &[Column] = &[
    col("VM", "/vm"),
    col("ACTIVE", "/active"),
    col("PACKETS", "/packets"),
    col("BYTES", "/bytes"),
    col("MATCHED", "/matched"),
    col("OVERWRITTEN", "/overwritten"),
    col("EXPIRES_US", "/expires_us"),
];

const TALKER_COLUMNS: &[Column] = &[
    col("VM", "/vm_id"),
    col("PEER", "/peer"),
    col("PORT", "/port"),
    col("PROTO", "/proto"),
    col("DIR", "/direction"),
    col("FLOWS", "/red/flows"),
    col("BYTES", "/red/bytes"),
    col("DROPS", "/red/drops"),
    col("RESETS", "/red/resets"),
];

const PER_VM_COLUMNS: &[Column] = &[
    col("VM", "/vm_id"),
    col("ACTIVE", "/flows_active"),
    col("FLOWS", "/red/flows"),
    col("BYTES", "/red/bytes"),
    col("DROPS", "/red/drops"),
    col("DNS_ERRORS", "/red/dns_errors"),
];

/// `GET /v1/netobs/flows` query string for the given filters.
pub fn flows_query(
    vm: Option<Uuid>,
    proto: Option<&str>,
    port: Option<u16>,
    host: Option<std::net::IpAddr>,
    verdict: Option<&str>,
    closed: bool,
    limit: usize,
) -> String {
    let mut q: Vec<String> = Vec::new();
    if let Some(v) = vm {
        q.push(format!("vm={v}"));
    }
    if let Some(p) = proto {
        q.push(format!("proto={}", encode_query(p)));
    }
    if let Some(p) = port {
        q.push(format!("port={p}"));
    }
    if let Some(h) = host {
        q.push(format!("host={}", encode_query(&h.to_string())));
    }
    if let Some(v) = verdict {
        q.push(format!("verdict={}", encode_query(v)));
    }
    if closed {
        q.push("closed=true".into());
    }
    q.push(format!("limit={limit}"));
    q.join("&")
}

/// Body of `POST /v1/netobs/capture`; unset knobs are omitted so the daemon's defaults apply.
#[allow(clippy::too_many_arguments)]
pub fn capture_body(
    vm: Uuid,
    host: Option<std::net::IpAddr>,
    port: Option<u16>,
    proto: Option<&str>,
    ttl_secs: Option<u64>,
    snaplen: Option<usize>,
    max_packets: Option<usize>,
    max_bytes: Option<usize>,
) -> Value {
    let mut filter = serde_json::Map::new();
    if let Some(h) = host {
        filter.insert("host".into(), h.to_string().into());
    }
    if let Some(p) = port {
        filter.insert("port".into(), p.into());
    }
    if let Some(p) = proto {
        filter.insert("proto".into(), p.into());
    }
    let mut body = serde_json::Map::new();
    body.insert("vm".into(), vm.to_string().into());
    body.insert("filter".into(), Value::Object(filter));
    for (k, v) in [
        ("ttl_secs", ttl_secs.map(Value::from)),
        ("snaplen", snaplen.map(Value::from)),
        ("max_packets", max_packets.map(Value::from)),
        ("max_bytes", max_bytes.map(Value::from)),
    ] {
        if let Some(v) = v {
            body.insert(k.into(), v);
        }
    }
    Value::Object(body)
}

/// Adds `_client`/`_server` `ip:port` strings to each flow so the table can show them.
fn with_endpoints(items: &Value) -> Value {
    let ep = |e: &Value| {
        format!(
            "{}:{}",
            e["ip"].as_str().unwrap_or("-"),
            e["port"].as_u64().unwrap_or(0)
        )
    };
    Value::Array(
        items
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .map(|mut f| {
                f["_client"] = ep(&f["client"]).into();
                f["_server"] = ep(&f["server"]).into();
                f
            })
            .collect(),
    )
}

fn disabled_note(v: &Value) -> &'static str {
    if v["enabled"] == Value::Bool(false) {
        "network observability is not enabled on this host (netobs.enabled / FLUXVM_NETOBS)\n"
    } else {
        ""
    }
}

pub fn render_flows(resp: &Value) -> String {
    let mut out = disabled_note(resp).to_string();
    out.push_str(&output::render_table(
        &with_endpoints(&resp["items"]),
        FLOW_COLUMNS,
        false,
    ));
    out
}

pub fn render_dns(resp: &Value) -> String {
    let mut out = disabled_note(resp).to_string();
    out.push_str(&output::render_table(&resp["items"], DNS_COLUMNS, false));
    out
}

pub fn render_capture_status(resp: &Value) -> String {
    let mut out = disabled_note(resp).to_string();
    out.push_str(&output::render_table(
        &resp["sessions"],
        CAPTURE_COLUMNS,
        false,
    ));
    out
}

pub fn render_summary(s: &Value) -> String {
    let mut out = disabled_note(s).to_string();
    if s["enabled"] == Value::Bool(false) {
        return out;
    }
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let c = &s["counters"];
    out.push_str(&format!(
        "flows active: {}   created: {}   frames: {}   bytes: {}   malformed: {}\n",
        n(&s["flows_active"]),
        n(&c["flows_created"]),
        n(&c["frames"]),
        n(&c["bytes"]),
        n(&c["malformed"]),
    ));
    out.push_str("\nPER VM\n");
    out.push_str(&output::render_table(&s["per_vm"], PER_VM_COLUMNS, false));
    out.push_str("\nTOP TALKERS\n");
    out.push_str(&output::render_table(
        &s["top_talkers"],
        TALKER_COLUMNS,
        false,
    ));
    out.push_str("\nTOP ERRORS\n");
    out.push_str(&output::render_table(
        &s["top_errors"],
        TALKER_COLUMNS,
        false,
    ));
    if s["capture"].as_array().is_some_and(|a| !a.is_empty()) {
        out.push_str("\nCAPTURE\n");
        out.push_str(&output::render_table(&s["capture"], CAPTURE_COLUMNS, false));
    }
    out
}

fn render_history(h: &Value) -> String {
    let mut out = disabled_note(h).to_string();
    out.push_str(&format!("{:<18} {:<18} ROWS\n", "START_US", "END_US"));
    for w in h["windows"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{:<18} {:<18} {}\n",
            w["start_us"].as_u64().unwrap_or(0),
            w["end_us"].as_u64().unwrap_or(0),
            w["rows"].as_array().map_or(0, Vec::len),
        ));
    }
    out
}

/// One line for a streamed event: `<kind>  <vm>  <summary>`.
pub fn render_event(kind: &str, data: &Value) -> String {
    let vm = data["vm_id"].as_str().unwrap_or("-");
    let detail = if kind.contains("dns") {
        format!(
            "{} {} {}",
            data["qname"].as_str().unwrap_or("-"),
            data["qtype"].as_str().unwrap_or(""),
            data["rcode"].as_str().unwrap_or("")
        )
    } else {
        let ep = |e: &Value| {
            e["ip"]
                .as_str()
                .map(|ip| format!("{ip}:{}", e["port"].as_u64().unwrap_or(0)))
        };
        match (ep(&data["client"]), ep(&data["server"])) {
            (Some(c), Some(s)) => format!("{c} -> {s} {}", data["verdict"].as_str().unwrap_or("")),
            _ => data.to_string(),
        }
    };
    format!("{kind:<18} {vm}  {}", detail.trim_end())
}

fn emit(json: bool, v: &Value, table: impl FnOnce(&Value) -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        print!("{}", table(v));
    }
    Ok(())
}

pub async fn run(r: &Remote, cmd: NetobsCommand) -> Result<()> {
    match cmd {
        NetobsCommand::Summary { json } => {
            let v = r.call(Method::GET, "/v1/netobs/summary", None).await?;
            emit(json, &v, render_summary)
        }
        NetobsCommand::Flows {
            vm,
            proto,
            port,
            host,
            verdict,
            closed,
            limit,
            follow,
            since,
            json,
        } => {
            if follow {
                let mut q: Vec<String> = Vec::new();
                if let Some(v) = vm {
                    q.push(format!("vm={v}"));
                }
                if let Some(s) = since {
                    q.push(format!("since={s}"));
                }
                let path = format!("/v1/netobs/flows/stream?{}", q.join("&"));
                return r
                    .follow_sse(&path, |kind, data| {
                        if json {
                            println!("{data}");
                        } else {
                            let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
                            println!("{}", render_event(kind, &v));
                        }
                        Ok(())
                    })
                    .await;
            }
            let q = flows_query(
                vm,
                proto.as_deref(),
                port,
                host,
                verdict.as_deref(),
                closed,
                limit,
            );
            let v = r
                .call(Method::GET, &format!("/v1/netobs/flows?{q}"), None)
                .await?;
            emit(json, &v, render_flows)
        }
        NetobsCommand::Dns { vm, limit, json } => {
            let mut q = format!("limit={limit}");
            if let Some(v) = vm {
                q.push_str(&format!("&vm={v}"));
            }
            let v = r
                .call(Method::GET, &format!("/v1/netobs/dns?{q}"), None)
                .await?;
            emit(json, &v, render_dns)
        }
        NetobsCommand::History { json } => {
            let v = r.call(Method::GET, "/v1/netobs/history", None).await?;
            emit(json, &v, render_history)
        }
        NetobsCommand::Capture { command } => match command {
            CaptureCommand::Start {
                vm,
                host,
                port,
                proto,
                ttl_secs,
                snaplen,
                max_packets,
                max_bytes,
            } => {
                let body = capture_body(
                    vm,
                    host,
                    port,
                    proto.as_deref(),
                    ttl_secs,
                    snaplen,
                    max_packets,
                    max_bytes,
                );
                let v = r
                    .call(Method::POST, "/v1/netobs/capture", Some(body))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&v)?);
                Ok(())
            }
            CaptureCommand::Stop { vm } => {
                r.call(Method::DELETE, &format!("/v1/netobs/capture?vm={vm}"), None)
                    .await?;
                println!("{}", serde_json::json!({"vm": vm, "stopped": true}));
                Ok(())
            }
            CaptureCommand::Status { json } => {
                let v = r.call(Method::GET, "/v1/netobs/capture", None).await?;
                emit(json, &v, render_capture_status)
            }
            CaptureCommand::Download { vm, output } => {
                let (status, body) = r
                    .get_raw(&format!("/v1/netobs/capture.pcap?vm={vm}"))
                    .await?;
                if !status.is_success() {
                    anyhow::bail!(
                        "GET /v1/netobs/capture.pcap: {status}: {}",
                        String::from_utf8_lossy(&body)
                    );
                }
                if output.as_os_str() == "-" {
                    use std::io::Write;
                    std::io::stdout().write_all(&body)?;
                } else {
                    std::fs::write(&output, &body)
                        .with_context(|| format!("writing {}", output.display()))?;
                    eprintln!("wrote {} bytes to {}", body.len(), output.display());
                }
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;
    use serde_json::json;

    fn parse(args: &[&str]) -> NetobsCommand {
        let mut full = vec!["fluxctl", "netobs"];
        full.extend_from_slice(args);
        match Cli::try_parse_from(full).unwrap().command {
            Command::Netobs { command } => command,
            _ => panic!("expected Command::Netobs"),
        }
    }

    #[test]
    fn parses_every_subcommand() {
        let id = Uuid::nil().to_string();
        assert!(matches!(
            parse(&["summary", "--json"]),
            NetobsCommand::Summary { json: true }
        ));
        assert!(matches!(
            parse(&["history"]),
            NetobsCommand::History { json: false }
        ));
        assert!(matches!(
            parse(&["dns", "--vm", &id, "--limit", "5"]),
            NetobsCommand::Dns {
                limit: 5,
                vm: Some(_),
                ..
            }
        ));
        let NetobsCommand::Flows {
            follow,
            port,
            closed,
            vm,
            ..
        } = parse(&[
            "flows", "--follow", "--port", "443", "--closed", "--vm", &id,
        ])
        else {
            panic!()
        };
        assert!(follow && closed && port == Some(443) && vm == Some(Uuid::nil()));
        assert!(matches!(
            parse(&[
                "capture",
                "start",
                "--vm",
                &id,
                "--port",
                "53",
                "--ttl-secs",
                "30"
            ]),
            NetobsCommand::Capture {
                command: CaptureCommand::Start {
                    port: Some(53),
                    ttl_secs: Some(30),
                    ..
                }
            }
        ));
        assert!(matches!(
            parse(&["capture", "stop", "--vm", &id]),
            NetobsCommand::Capture {
                command: CaptureCommand::Stop { .. }
            }
        ));
        assert!(matches!(
            parse(&["capture", "download", "--vm", &id, "-O", "x.pcap"]),
            NetobsCommand::Capture {
                command: CaptureCommand::Download { .. }
            }
        ));
    }

    #[test]
    fn rejects_bad_args() {
        let id = Uuid::nil().to_string();
        assert!(Cli::try_parse_from(["fluxctl", "netobs"]).is_err());
        assert!(Cli::try_parse_from(["fluxctl", "netobs", "capture", "start"]).is_err());
        assert!(
            Cli::try_parse_from(["fluxctl", "netobs", "capture", "download", "--vm", &id]).is_err()
        );
        assert!(Cli::try_parse_from(["fluxctl", "netobs", "flows", "--since", "3"]).is_err());
        assert!(Cli::try_parse_from(["fluxctl", "netobs", "flows", "--port", "99999"]).is_err());
    }

    #[test]
    fn flows_query_encodes_filters() {
        assert_eq!(
            flows_query(None, None, None, None, None, false, 50),
            "limit=50"
        );
        let q = flows_query(
            Some(Uuid::nil()),
            Some("tcp"),
            Some(443),
            Some("10.0.0.1".parse().unwrap()),
            Some("drop reason"),
            true,
            7,
        );
        assert_eq!(
            q,
            "vm=00000000-0000-0000-0000-000000000000&proto=tcp&port=443&host=10.0.0.1&verdict=drop%20reason&closed=true&limit=7"
        );
    }

    #[test]
    fn capture_body_omits_unset_knobs() {
        let b = capture_body(
            Uuid::nil(),
            None,
            Some(53),
            Some("udp"),
            Some(30),
            None,
            None,
            None,
        );
        assert_eq!(b["filter"], json!({"port": 53, "proto": "udp"}));
        assert_eq!(b["ttl_secs"], 30);
        assert!(b.get("snaplen").is_none() && b.get("max_bytes").is_none());
        assert_eq!(b["vm"], Uuid::nil().to_string());
    }

    #[test]
    fn tables_render_and_note_disabled() {
        let flows = json!({"enabled": true, "items": [{
            "vm_id": "vm1", "proto": "tcp", "client": {"ip": "10.0.0.2", "port": 5000},
            "server": {"ip": "1.1.1.1", "port": 443}, "verdict": "allow", "app": "tls",
            "sni": "example.com", "bytes_c2s": 10, "bytes_s2c": 20}]});
        let t = render_flows(&flows);
        assert!(t.starts_with("VM"), "{t}");
        assert!(
            t.contains("10.0.0.2:5000") && t.contains("1.1.1.1:443") && t.contains("example.com")
        );
        let off = json!({"enabled": false, "items": []});
        assert!(render_flows(&off).contains("not enabled"));
        assert!(render_summary(&json!({"enabled": false})).contains("not enabled"));
    }

    #[test]
    fn summary_and_event_lines() {
        let s = json!({"enabled": true, "flows_active": 2,
            "counters": {"flows_created": 5, "frames": 9, "bytes": 99, "malformed": 0},
            "per_vm": [{"vm_id": "vm1", "flows_active": 2, "red": {"flows": 5, "bytes": 99, "drops": 1, "dns_errors": 0}}],
            "top_talkers": [], "top_errors": [], "capture": []});
        let t = render_summary(&s);
        assert!(t.contains("flows active: 2") && t.contains("PER VM") && t.contains("vm1"));
        let line = render_event(
            "netobs.flow",
            &json!({"vm_id": "vm1", "client": {"ip": "10.0.0.2", "port": 1},
                    "server": {"ip": "8.8.8.8", "port": 53}, "verdict": "allow"}),
        );
        assert!(line.contains("10.0.0.2:1 -> 8.8.8.8:53 allow"), "{line}");
        let dns = render_event(
            "netobs.dns",
            &json!({"vm_id": "vm1", "qname": "a.b", "qtype": "A", "rcode": "NXDOMAIN"}),
        );
        assert!(dns.contains("a.b A NXDOMAIN"));
    }
}
