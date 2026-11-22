// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Instance TTL, ownership labels and the reaper. `mac run --ttl 2h` stamps `fluxvm.mac.expires` (unix seconds) on the instance;
//! every instance also gets `fluxvm.mac.owner` (the user who ran it) and `fluxvm.mac.created`. `mac reap` deletes the instances whose
//! TTL passed (`mac supervise` does the same on every pass). A macOS VM holds one of only two slots on a Mac, so a forgotten CI VM is
//! expensive; the TTL is the safety net. Never run against a real daemon; the decisions are unit tested.

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub const EXPIRES_LABEL: &str = "fluxvm.mac.expires";
pub const OWNER_LABEL: &str = "fluxvm.mac.owner";
pub const CREATED_LABEL: &str = "fluxvm.mac.created";

/// `90s`, `30m`, `2h`, `1d`, or bare seconds. Zero and overflow are refused.
pub fn parse_ttl(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix('d') {
        (n, 86_400u64)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3_600)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1)
    } else {
        (t, 1)
    };
    let Ok(n) = num.parse::<u64>() else {
        bail!("--ttl {s:?} is not a duration (90s, 30m, 2h, 1d)");
    };
    ensure!(n > 0, "--ttl must be more than zero");
    n.checked_mul(mult)
        .filter(|v| *v < 10 * 365 * 86_400)
        .ok_or_else(|| anyhow::anyhow!("--ttl {s:?} is too large"))
}

/// The labels `mac run` stamps after launch.
pub fn stamp(owner: Option<&str>, ttl_secs: Option<u64>, now: u64) -> Value {
    let mut l = serde_json::Map::new();
    if let Some(o) = owner.filter(|o| !o.is_empty()) {
        l.insert(OWNER_LABEL.into(), json!(o));
    }
    l.insert(CREATED_LABEL.into(), json!(now.to_string()));
    if let Some(t) = ttl_secs {
        l.insert(EXPIRES_LABEL.into(), json!((now + t).to_string()));
    }
    json!({ "labels": l })
}

pub fn expires_at(vm: &Value) -> Option<u64> {
    vm["labels"][EXPIRES_LABEL].as_str()?.parse().ok()
}

/// Has the TTL passed? An instance without one never expires.
pub fn expired(vm: &Value, now: u64) -> bool {
    expires_at(vm).is_some_and(|e| now >= e)
}

pub fn owner_of(vm: &Value) -> Option<&str> {
    vm["labels"][OWNER_LABEL].as_str()
}

/// The instances `mac reap` removes: expired ones, narrowed by `--owner`. `--all` removes every expired one regardless of owner;
/// otherwise only the current user's (an instance with no owner label counts as nobody's, so only `--all` reaps it).
pub fn victims<'a>(
    items: &'a [Value],
    now: u64,
    me: &str,
    all: bool,
    owner: Option<&str>,
) -> Vec<&'a Value> {
    items
        .iter()
        .filter(|v| expired(v, now))
        .filter(|v| match owner {
            Some(o) => owner_of(v) == Some(o),
            None => all || owner_of(v) == Some(me),
        })
        .collect()
}

/// `1h 5m` style remaining time, or `expired`.
pub fn remaining(vm: &Value, now: u64) -> Option<String> {
    let e = expires_at(vm)?;
    if now >= e {
        return Some("expired".into());
    }
    let s = e - now;
    Some(if s >= 3600 {
        format!("{}h{}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    })
}

pub fn current_user() -> String {
    std::env::var("USER").unwrap_or_default()
}

/// `mac reap [--dry-run] [--all] [--owner NAME]`.
pub async fn reap(
    r: &crate::remote::Remote,
    dry_run: bool,
    all: bool,
    owner: Option<String>,
) -> Result<()> {
    let items = r.list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR)).await?;
    let now = crate::mac_health::now_unix();
    let vs = victims(&items, now, &current_user(), all, owner.as_deref());
    if vs.is_empty() {
        println!("nothing to reap");
    }
    let mut failed = Vec::new();
    for v in vs {
        let name = v["name"].as_str().unwrap_or("?");
        if dry_run {
            println!("would remove {name} (owner {})", owner_of(v).unwrap_or("-"));
            continue;
        }
        let Some(id) = v["id"].as_str() else { continue };
        match crate::mac_cmd::delete_instance(r, id).await {
            Ok(()) => println!("removed {name}"),
            Err(e) => {
                eprintln!("{name}: {e:#}");
                failed.push(name.to_string());
            }
        }
    }
    ensure!(failed.is_empty(), "could not remove: {}", failed.join(", "));
    Ok(())
}

/// `mac ttl NAME DURATION|none`: change or clear an instance's TTL (counted from now).
pub async fn set_ttl(r: &crate::remote::Remote, name: &str, ttl: &str) -> Result<()> {
    let vm = crate::mac_cmd::find_instance(r, name).await?;
    let id = vm["id"].as_str().unwrap_or_default();
    let val = if ttl == "none" {
        Value::Null
    } else {
        json!((crate::mac_health::now_unix() + parse_ttl(ttl)?).to_string())
    };
    r.call(
        reqwest::Method::PATCH,
        &format!("/v1/vms/{id}"),
        Some(json!({"labels": {EXPIRES_LABEL: val}})),
    )
    .await?;
    println!(
        "{name}: ttl {}",
        if ttl == "none" { "cleared" } else { ttl }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, owner: Option<&str>, exp: Option<u64>) -> Value {
        let mut l = json!({});
        if let Some(o) = owner {
            l[OWNER_LABEL] = json!(o);
        }
        if let Some(e) = exp {
            l[EXPIRES_LABEL] = json!(e.to_string());
        }
        json!({"name": name, "labels": l})
    }

    #[test]
    fn ttl_parses_units_and_refuses_nonsense() {
        assert_eq!(parse_ttl("90s").unwrap(), 90);
        assert_eq!(parse_ttl("30m").unwrap(), 1800);
        assert_eq!(parse_ttl("2h").unwrap(), 7200);
        assert_eq!(parse_ttl("1d").unwrap(), 86_400);
        assert_eq!(parse_ttl("45").unwrap(), 45);
        for bad in ["", "0", "0h", "h", "-1h", "2w", "99999999999999d"] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stamp_records_owner_created_and_expiry() {
        let s = stamp(Some("amy"), Some(3600), 1000);
        assert_eq!(s["labels"][OWNER_LABEL], "amy");
        assert_eq!(s["labels"][CREATED_LABEL], "1000");
        assert_eq!(s["labels"][EXPIRES_LABEL], "4600");
        let s = stamp(None, None, 5);
        assert!(s["labels"].get(EXPIRES_LABEL).is_none() && s["labels"].get(OWNER_LABEL).is_none());
    }

    #[test]
    fn expiry_and_remaining() {
        let v = vm("a", None, Some(100));
        assert!(!expired(&v, 99));
        assert!(expired(&v, 100));
        assert!(!expired(&vm("b", None, None), u64::MAX));
        assert_eq!(remaining(&v, 100).as_deref(), Some("expired"));
        assert_eq!(remaining(&v, 40).as_deref(), Some("1m"));
        assert_eq!(
            remaining(&vm("c", None, Some(7300)), 0).as_deref(),
            Some("2h1m")
        );
        assert_eq!(remaining(&vm("d", None, None), 0), None);
    }

    #[test]
    fn reap_defaults_to_my_expired_instances() {
        let items = vec![
            vm("mine-old", Some("me"), Some(10)),
            vm("mine-live", Some("me"), Some(1000)),
            vm("theirs-old", Some("you"), Some(10)),
            vm("ownerless-old", None, Some(10)),
        ];
        let names = |v: Vec<&Value>| -> Vec<String> {
            v.iter()
                .map(|x| x["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names(victims(&items, 100, "me", false, None)), ["mine-old"]);
        assert_eq!(
            names(victims(&items, 100, "me", true, None)),
            ["mine-old", "theirs-old", "ownerless-old"]
        );
        assert_eq!(
            names(victims(&items, 100, "me", false, Some("you"))),
            ["theirs-old"]
        );
    }
}
