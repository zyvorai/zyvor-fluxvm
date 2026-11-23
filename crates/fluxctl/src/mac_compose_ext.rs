// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The small pure parts of `fluxctl mac compose` that go beyond the basic file: `depends_on` conditions, the compose
//! `healthcheck:` key, `--scale`, `placement:` (fleet), and the restart-retry book that `compose watch` uses to enforce
//! `restart: on-failure:N`. Nothing here touches a daemon, ssh or the clock, so all of it is unit tested.

use crate::mac_config::{self, Cmd, Healthcheck};
use crate::mac_errors::coded;
use crate::mac_lifecycle::HcSetting;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;

// ---- depends_on ------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Condition {
    /// Up and answering on SSH.
    #[default]
    Started,
    /// The dependency's healthcheck passes.
    Healthy,
    /// The dependency's command ran to the end and exited 0; its VM is then stopped, which frees its macOS slot.
    Completed,
}

/// The condition of one `depends_on` entry (`{condition: ...}`); anything else in the entry is refused.
pub fn condition_of(service: &str, dep: &str, entry: &serde_yaml::Value) -> Result<Condition> {
    let bad = |what: String| {
        coded(
            format!(
                "service {service:?}: depends_on {dep:?}: {what}. Use service_started, service_healthy or service_completed_successfully (unknown condition otherwise)"
            ),
            "MAC_COMPOSE_CONDITION",
        )
    };
    if let Some(m) = entry.as_mapping() {
        for k in m.keys().filter_map(|k| k.as_str()) {
            if !matches!(k, "condition" | "required" | "restart") {
                return Err(bad(format!("the key `{k}` is not understood")));
            }
        }
    }
    Ok(match entry["condition"].as_str() {
        None | Some("service_started") => Condition::Started,
        Some("service_healthy") => Condition::Healthy,
        Some("service_completed_successfully") => Condition::Completed,
        Some(other) => return Err(bad(format!("unknown condition {other:?}"))),
    })
}

// ---- healthcheck: ----------------------------------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Test {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Dur {
    Str(String),
    Int(u64),
}

impl Dur {
    fn secs(&self, what: &str) -> Result<u64> {
        match self {
            Dur::Int(n) => Ok(*n),
            Dur::Str(s) => mac_config::parse_duration_secs(s).map_err(|e| {
                coded(
                    format!("healthcheck {what}: {e:#}"),
                    "MAC_COMPOSE_HEALTHCHECK",
                )
            }),
        }
    }
}

/// `healthcheck:` of a service: it replaces the image's HEALTHCHECK, exactly as `mac run --health-cmd` does.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct HealthcheckCfg {
    #[serde(default)]
    test: Option<Test>,
    #[serde(default)]
    interval: Option<Dur>,
    #[serde(default)]
    timeout: Option<Dur>,
    #[serde(default)]
    retries: Option<u32>,
    #[serde(default)]
    start_period: Option<Dur>,
    #[serde(default)]
    disable: Option<bool>,
}

impl HealthcheckCfg {
    /// `Off` for `disable: true` or `test: [NONE]`, else the check as the daemon records it on the instance.
    pub fn resolve(&self, service: &str) -> Result<HcSetting> {
        let bad = |m: &str| {
            coded(
                format!("service {service:?}: healthcheck {m}"),
                "MAC_COMPOSE_HEALTHCHECK",
            )
        };
        if self.disable == Some(true) {
            return Ok(HcSetting::Off);
        }
        let cmd = match &self.test {
            None => return Err(bad("has no `test:`")),
            Some(Test::One(s)) => Cmd::Shell(s.clone()),
            Some(Test::Many(l)) => match l.first().map(String::as_str) {
                Some("NONE") => return Ok(HcSetting::Off),
                Some("CMD") if l.len() > 1 => Cmd::Exec(l[1..].to_vec()),
                Some("CMD-SHELL") if l.len() == 2 => Cmd::Shell(l[1].clone()),
                _ => {
                    return Err(bad(
                        "`test:` list must be [CMD, ...], [CMD-SHELL, text] or [NONE]",
                    ));
                }
            },
        };
        ensure!(
            !cmd.to_shell().trim().is_empty(),
            "{}",
            bad("`test:` is empty")
        );
        let get = |d: &Option<Dur>, def: u64, what: &str| -> Result<u64> {
            d.as_ref().map_or(Ok(def), |d| d.secs(what))
        };
        let retries = self.retries.unwrap_or(3);
        ensure!(retries >= 1, "{}", bad("`retries` must be at least 1"));
        Ok(HcSetting::Cmd(Healthcheck {
            cmd,
            interval_secs: get(&self.interval, 30, "interval")?.max(1),
            timeout_secs: get(&self.timeout, 30, "timeout")?.max(1),
            retries,
            start_period_secs: get(&self.start_period, 0, "start_period")?,
        }))
    }
}

// ---- scale -----------------------------------------------------------------------------------------------------------

/// `--scale web=3` (repeatable).
pub fn parse_scale(flags: &[String]) -> Result<BTreeMap<String, u32>> {
    let mut m = BTreeMap::new();
    for f in flags {
        let bad = |why: &str| coded(format!("--scale {f:?}: {why}"), "MAC_COMPOSE_SCALE");
        let (s, n) = f
            .split_once('=')
            .ok_or_else(|| bad("write it as SERVICE=N, for example --scale web=2"))?;
        let n: u32 = n.parse().map_err(|_| bad("the count is not a number"))?;
        if !(1..=64).contains(&n) {
            return Err(bad("the count must be from 1 to 64"));
        }
        m.insert(s.to_string(), n);
    }
    Ok(m)
}

/// `project-service`, or `project-service-N` when the service has several copies.
pub fn instance_name(project: &str, service: &str, replica: u32, replicas: u32) -> String {
    if replicas > 1 {
        format!("{project}-{service}-{replica}")
    } else {
        format!("{project}-{service}")
    }
}

// ---- placement (fleet) -----------------------------------------------------------------------------------------------

/// Where a service may run in a Mac fleet (`--fleet`). Ignored on one Mac.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlacementCfg {
    /// Run on this Mac only (its name in the inventory).
    #[serde(default)]
    pub host: Option<String>,
    /// Only on Macs that carry all of these inventory labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Never on a Mac that runs on battery.
    #[serde(default)]
    pub ac_only: bool,
    /// Only on a Mac with at least this much free disk.
    #[serde(default)]
    pub disk_gib: u64,
}

impl PlacementCfg {
    pub fn check(&self, service: &str) -> Result<()> {
        for l in &self.labels {
            crate::mac_fleet_sched::check_label(l).map_err(|e| {
                coded(
                    format!("service {service:?}: placement label {l:?}: {e:#}"),
                    "MAC_COMPOSE_PLACEMENT",
                )
            })?;
        }
        Ok(())
    }
}

// ---- restart: on-failure:N -------------------------------------------------------------------------------------------

/// The retry count of `on-failure:N`; `None` means no limit. A count on any other policy is a mistake and is refused.
pub fn restart_limit(policy: &str) -> Result<Option<u32>> {
    let Some((name, n)) = policy.split_once(':') else {
        return Ok(None);
    };
    let bad = |why: String| coded(format!("restart {policy:?}: {why}"), "MAC_COMPOSE_RESTART");
    if name != "on-failure" {
        return Err(bad(format!("only on-failure takes a count, not {name}")));
    }
    n.parse::<u32>()
        .map(Some)
        .map_err(|_| bad(format!("{n:?} is not a number")))
}

/// How many times `compose watch` has started each instance again; enforces `restart: on-failure:N`.
#[derive(Debug, Default)]
pub struct RestartBook {
    tries: BTreeMap<String, u32>,
}

impl RestartBook {
    /// May this instance be started again? Counts the attempt. With a limit of `N`, the N+1th failure is refused for good.
    pub fn allow(&mut self, instance: &str, limit: Option<u32>) -> bool {
        let n = self.tries.entry(instance.to_string()).or_default();
        if limit.is_some_and(|l| *n >= l) {
            return false;
        }
        *n += 1;
        true
    }

    pub fn tries(&self, instance: &str) -> u32 {
        self.tries.get(instance).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn conditions_parse_and_unknown_ones_name_the_choices() {
        let c = |s: &str| condition_of("web", "db", &y(s));
        assert_eq!(c("{}").unwrap(), Condition::Started);
        assert_eq!(
            c("{condition: service_healthy}").unwrap(),
            Condition::Healthy
        );
        assert_eq!(
            c("{condition: service_completed_successfully}").unwrap(),
            Condition::Completed
        );
        let e = format!("{:#}", c("{condition: nope}").unwrap_err());
        assert!(
            e.contains("unknown condition") && e.contains("explain MAC_COMPOSE_CONDITION"),
            "{e}"
        );
        assert!(c("{wat: 1}").is_err());
    }

    fn hc(s: &str) -> Result<HcSetting> {
        serde_yaml::from_str::<HealthcheckCfg>(s)
            .unwrap()
            .resolve("db")
    }

    #[test]
    fn healthchecks_take_every_compose_spelling() {
        let HcSetting::Cmd(h) =
            hc("{test: pg_isready, interval: 5s, retries: 2, start_period: 1m}").unwrap()
        else {
            panic!()
        };
        assert_eq!(h.cmd.to_shell(), "pg_isready");
        assert_eq!(
            (
                h.interval_secs,
                h.retries,
                h.start_period_secs,
                h.timeout_secs
            ),
            (5, 2, 60, 30)
        );
        let HcSetting::Cmd(h) = hc("{test: [CMD, curl, -f, 'http://x/y z']}").unwrap() else {
            panic!()
        };
        assert!(h.cmd.to_shell().contains("curl") && h.cmd.to_shell().contains("'http://x/y z'"));
        assert!(matches!(
            hc("{test: [CMD-SHELL, 'a && b']}").unwrap(),
            HcSetting::Cmd(_)
        ));
        assert_eq!(hc("{disable: true}").unwrap(), HcSetting::Off);
        assert_eq!(hc("{test: [NONE]}").unwrap(), HcSetting::Off);
        for bad in [
            "{}",
            "{test: [BOGUS, x]}",
            "{test: x, retries: 0}",
            "{test: x, interval: soon}",
        ] {
            let e = format!("{:#}", hc(bad).unwrap_err());
            assert!(e.contains("MAC_COMPOSE_HEALTHCHECK"), "{bad}: {e}");
        }
    }

    #[test]
    fn scale_flags_and_instance_names() {
        let m = parse_scale(&["web=3".into(), "db=1".into()]).unwrap();
        assert_eq!((m["web"], m["db"]), (3, 1));
        for bad in ["web", "web=x", "web=0", "web=65"] {
            assert!(parse_scale(&[bad.into()]).is_err(), "{bad}");
        }
        assert_eq!(instance_name("p", "web", 2, 3), "p-web-2");
        assert_eq!(instance_name("p", "web", 1, 1), "p-web");
    }

    #[test]
    fn on_failure_n_is_parsed_and_enforced() {
        assert_eq!(restart_limit("always").unwrap(), None);
        assert_eq!(restart_limit("on-failure").unwrap(), None);
        assert_eq!(restart_limit("on-failure:3").unwrap(), Some(3));
        assert!(restart_limit("always:3").is_err());
        assert!(restart_limit("on-failure:x").is_err());
        let mut b = RestartBook::default();
        assert!(b.allow("a", Some(2)) && b.allow("a", Some(2)));
        assert!(!b.allow("a", Some(2)), "the third failure is not restarted");
        assert_eq!(b.tries("a"), 2);
        assert!(b.allow("b", Some(2)), "each instance has its own count");
        assert!(
            (0..10).all(|_| b.allow("c", None)),
            "no limit means no limit"
        );
    }

    #[test]
    fn placement_labels_are_checked() {
        let p: PlacementCfg = serde_yaml::from_str("{host: mini-1, labels: [xcode=15]}").unwrap();
        assert!(p.check("a").is_ok());
        let p: PlacementCfg = serde_yaml::from_str("{labels: ['bad label!']}").unwrap();
        assert!(p.check("a").is_err());
        assert!(serde_yaml::from_str::<PlacementCfg>("{hosts: x}").is_err());
    }
}
