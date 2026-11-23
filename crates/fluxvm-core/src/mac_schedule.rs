// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Scheduled Mac runs (`fluxctl mac schedule`): a small five-field cron parser and the schedule file both `fluxctl` and the daemon's
//! `/v1/mac/schedules` routes read and write. The file is `mac-schedules.json` next to the Mac image store (or `$FLUXVM_MAC_SCHEDULES`).
//! Times are the Mac's local time, like cron. Nothing here runs a VM: `fluxctl mac schedule tick` does that.

use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A parsed cron expression: `minute hour day-of-month month day-of-week`, or an `@alias`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    dom_star: bool,
    dow_star: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(
    text: &str,
    lo: u32,
    hi: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let mut star = false;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let n: u32 = s
                    .parse()
                    .map_err(|_| format!("{what}: step {s:?} is not a number"))?;
                if n == 0 {
                    return Err(format!("{what}: a step of 0 never matches"));
                }
                (r, n)
            }
            None => (part, 1),
        };
        let value = |s: &str| -> Result<u32, String> {
            if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
                return Ok(i as u32 + name_base);
            }
            s.parse::<u32>()
                .map_err(|_| format!("{what}: {s:?} is not a number or a name"))
        };
        let (a, b) = if range == "*" {
            if step == 1 && part == "*" {
                star = true;
            }
            (lo, hi)
        } else if let Some((x, y)) = range.split_once('-') {
            (value(x)?, value(y)?)
        } else {
            let v = value(range)?;
            // `5/10` means from 5 to the end, stepping.
            if part.contains('/') { (v, hi) } else { (v, v) }
        };
        if a < lo || b > hi || a > b {
            return Err(format!("{what}: {part:?} is outside {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, star))
}

impl Cron {
    pub fn parse(text: &str) -> Result<Cron, String> {
        let t = text.trim();
        let expanded = match t {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "{text:?} is not a cron expression: it needs 5 fields (minute hour day-of-month month day-of-week), for example `*/15 * * * *` or `0 3 * * mon-fri`; or one of @hourly @daily @weekly @monthly"
            ));
        }
        let (minutes, _) = field(f[0], 0, 59, &[], 0, "minute")?;
        let (hours, _) = field(f[1], 0, 23, &[], 0, "hour")?;
        let (days, dom_star) = field(f[2], 1, 31, &[], 0, "day of month")?;
        let (months, _) = field(f[3], 1, 12, &MONTHS, 1, "month")?;
        // Day of week: 0-7, with 7 meaning Sunday.
        let (mut wd, dow_star) = field(f[4], 0, 7, &DAYS, 0, "day of week")?;
        if wd & (1 << 7) != 0 {
            wd |= 1;
        }
        Ok(Cron {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: (wd & 0x7f) as u8,
            dom_star,
            dow_star,
        })
    }

    fn day_ok(&self, t: &NaiveDateTime) -> bool {
        let dom = self.days & (1 << t.day()) != 0;
        let dow = self.weekdays & (1 << t.weekday().num_days_from_sunday()) != 0;
        // Vixie cron: when both day fields are restricted, either may match.
        match (self.dom_star, self.dow_star) {
            (false, false) => dom || dow,
            (false, true) => dom,
            (true, false) => dow,
            (true, true) => true,
        }
    }

    pub fn matches(&self, t: &NaiveDateTime) -> bool {
        self.minutes & (1 << t.minute()) != 0
            && self.hours & (1 << t.hour()) != 0
            && self.months & (1 << t.month()) != 0
            && self.day_ok(t)
    }

    /// The first matching minute strictly after `t`, looking at most five years ahead (`None` for `0 0 31 2 *`).
    pub fn next_after(&self, t: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut c = t.with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
        let limit = t + chrono::Duration::days(366 * 5);
        while c <= limit {
            if self.months & (1 << c.month()) == 0 || !self.day_ok(&c) {
                c = c.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1 << c.hour()) == 0 {
                c = c.with_minute(0)? + chrono::Duration::hours(1);
                continue;
            }
            if self.minutes & (1 << c.minute()) != 0 {
                return Some(c);
            }
            c += chrono::Duration::minutes(1);
        }
        None
    }
}

fn local_naive(unix: u64) -> NaiveDateTime {
    Local.timestamp_opt(unix as i64, 0).single().map_or_else(
        || chrono::DateTime::UNIX_EPOCH.naive_utc(),
        |d| d.naive_local(),
    )
}

fn to_unix(t: NaiveDateTime) -> Option<u64> {
    // A local time skipped by a clock change has no instant; take the earliest that exists.
    let d = Local.from_local_datetime(&t).earliest()?;
    u64::try_from(d.timestamp()).ok()
}

/// Unix time of the next run after `after_unix`, in the Mac's local time.
pub fn next_run_unix(cron: &Cron, after_unix: u64) -> Option<u64> {
    to_unix(cron.next_after(local_naive(after_unix))?)
}

/// `90s`, `30m`, `2h`, `1d`, or bare seconds (the same grammar as `mac run --ttl`).
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (num, mult) = match t.chars().last() {
        Some('d') => (&t[..t.len() - 1], 86_400u64),
        Some('h') => (&t[..t.len() - 1], 3_600),
        Some('m') => (&t[..t.len() - 1], 60),
        Some('s') => (&t[..t.len() - 1], 1),
        _ => (t, 1),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{s:?} is not a duration (90s, 30m, 2h, 1d)"))?;
    if n == 0 {
        return Err("a duration must be more than zero".into());
    }
    n.checked_mul(mult)
        .filter(|v| *v < 10 * 365 * 86_400)
        .ok_or_else(|| format!("{s:?} is too large"))
}

/// One scheduled run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub name: String,
    pub cron: String,
    pub image: String,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Stop and remove the instance after this long (`2h`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    /// Desktop notification when the run ends.
    #[serde(default)]
    pub notify: bool,
    /// Also POST the result to this URL. Treat as a secret: it is never printed in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_unix: Option<u64>,
    /// `ok`, `failed` or `running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_detail: Option<String>,
    /// Set by `run-now` (or the Studio button): the next tick runs it once regardless of the cron.
    #[serde(default)]
    pub run_now: bool,
}

fn yes() -> bool {
    true
}

impl Schedule {
    /// The webhook with its path and query hidden: `https://hooks.example.com/…`.
    pub fn webhook_hint(&self) -> Option<String> {
        let w = self.webhook.as_deref()?;
        let (scheme, rest) = w.split_once("://")?;
        let host = rest.split(['/', '?', '#']).next()?;
        let host = host.rsplit('@').next()?;
        Some(format!("{scheme}://{host}/…"))
    }

    /// When the cron next fires (`now` when `run-now` is pending); `None` when disabled or the expression never matches.
    /// The schedule as `ls --json`, the API and Studio show it. The webhook is only ever its host.
    pub fn view(&self, now: u64) -> serde_json::Value {
        serde_json::json!({
            "name": self.name, "cron": self.cron, "image": self.image, "cmd": self.cmd, "ttl": self.ttl,
            "notify": self.notify, "webhook": self.webhook_hint(), "enabled": self.enabled,
            "next_run_unix": self.next_run(now), "last_run_unix": self.last_run_unix,
            "last_status": self.last_status, "last_detail": self.last_detail, "run_now": self.run_now,
        })
    }

    pub fn next_run(&self, now: u64) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        if self.run_now {
            return Some(now);
        }
        let cron = Cron::parse(&self.cron).ok()?;
        next_run_unix(&cron, self.last_run_unix.unwrap_or(self.created_unix))
    }

    /// Is it time to run? Once per due minute: a Mac that slept through several runs runs once when it wakes.
    pub fn due(&self, now: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if self.run_now {
            return true;
        }
        if self.last_status.as_deref() == Some("running")
            && self.last_run_unix.is_some_and(|l| now < l + 6 * 3600)
        {
            // A run that has not reported back is given six hours before the cron may start another.
            return false;
        }
        let Ok(cron) = Cron::parse(&self.cron) else {
            return false;
        };
        let since = self.last_run_unix.unwrap_or(self.created_unix);
        next_run_unix(&cron, since).is_some_and(|n| n <= now)
    }
}

/// A name is a short word usable as an instance name and in a URL.
pub fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with(['-', '.']);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a usable schedule name: use letters, digits, '-', '_' or '.' (up to 40), for example nightly-build"
        ))
    }
}

pub fn check_webhook(url: &str) -> Result<(), String> {
    if (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > 10
        && !url.contains(char::is_whitespace)
    {
        Ok(())
    } else {
        Err("a webhook must be an http:// or https:// URL, for example https://hooks.example.com/mac".into())
    }
}

/// Everything `add` checks before it writes anything.
pub fn validate(s: &Schedule) -> Result<(), String> {
    valid_name(&s.name)?;
    Cron::parse(&s.cron)?;
    if s.image.trim().is_empty() {
        return Err("a schedule needs an image to run (see `fluxctl mac images`)".into());
    }
    if let Some(t) = &s.ttl {
        parse_duration(t).map_err(|e| format!("ttl: {e}"))?;
    }
    if let Some(w) = &s.webhook {
        check_webhook(w)?;
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScheduleFile {
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

/// `$FLUXVM_MAC_SCHEDULES`, else `mac-schedules.json` beside the image store (`$FLUXVM_MAC_STORE` or `~/.fluxvm/mac-images`).
pub fn default_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_SCHEDULES").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let root = match std::env::var_os("FLUXVM_MAC_STORE").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(std::env::var_os("HOME")?)
            .join(".fluxvm")
            .join("mac-images"),
    };
    Some(root.parent().unwrap_or(&root).join("mac-schedules.json"))
}

pub fn load(path: &Path) -> Result<ScheduleFile, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}). Fix it, or move it aside and add the schedules again with `fluxctl mac schedule add`",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ScheduleFile::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Write atomically (temp file, then rename), mode 0600 because a webhook URL may carry a secret.
pub fn save(path: &Path, f: &ScheduleFile) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let body = serde_json::to_vec_pretty(f).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub fn add(path: &Path, mut s: Schedule, now: u64) -> Result<(), String> {
    validate(&s)?;
    let mut f = load(path)?;
    if f.schedules.iter().any(|x| x.name == s.name) {
        return Err(format!(
            "a schedule named {:?} already exists. Remove it first (`fluxctl mac schedule rm {}`) or pick another name",
            s.name, s.name
        ));
    }
    s.created_unix = now;
    f.schedules.push(s);
    save(path, &f)
}

pub fn remove(path: &Path, name: &str) -> Result<(), String> {
    let mut f = load(path)?;
    let before = f.schedules.len();
    f.schedules.retain(|s| s.name != name);
    if f.schedules.len() == before {
        return Err(format!(
            "no schedule named {name:?}. See what exists with `fluxctl mac schedule ls`"
        ));
    }
    save(path, &f)
}

/// Change one schedule in place under the file as it is now.
pub fn update(path: &Path, name: &str, f: impl FnOnce(&mut Schedule)) -> Result<(), String> {
    let mut file = load(path)?;
    let s = file
        .schedules
        .iter_mut()
        .find(|s| s.name == name)
        .ok_or_else(|| {
            format!("no schedule named {name:?}. See what exists with `fluxctl mac schedule ls`")
        })?;
    f(s);
    save(path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn every_fifteen_minutes() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(
            c.next_after(t(2026, 1, 1, 10, 0)),
            Some(t(2026, 1, 1, 10, 15))
        );
        assert_eq!(
            c.next_after(t(2026, 1, 1, 10, 50)),
            Some(t(2026, 1, 1, 11, 0))
        );
    }

    #[test]
    fn weekdays_at_three() {
        let c = Cron::parse("0 3 * * mon-fri").unwrap();
        // 2026-01-02 is a Friday; the next run is Monday the 5th.
        assert_eq!(c.next_after(t(2026, 1, 2, 3, 0)), Some(t(2026, 1, 5, 3, 0)));
    }

    #[test]
    fn lists_ranges_steps_and_sunday_as_seven() {
        let c = Cron::parse("5,35 8-10 * * 7").unwrap();
        assert!(c.matches(&t(2026, 1, 4, 9, 35))); // a Sunday
        assert!(!c.matches(&t(2026, 1, 5, 9, 35)));
        assert!(
            Cron::parse("10-30/10 * * * *")
                .unwrap()
                .matches(&t(2026, 1, 1, 0, 20))
        );
        assert!(
            Cron::parse("5/20 * * * *")
                .unwrap()
                .matches(&t(2026, 1, 1, 0, 45))
        );
    }

    #[test]
    fn day_of_month_or_weekday_when_both_given() {
        let c = Cron::parse("0 0 13 * fri").unwrap();
        assert!(c.matches(&t(2026, 1, 13, 0, 0))); // a Tuesday, the 13th
        assert!(c.matches(&t(2026, 1, 2, 0, 0))); // a Friday
        assert!(!c.matches(&t(2026, 1, 3, 0, 0)));
    }

    #[test]
    fn aliases_and_month_names() {
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );
        let c = Cron::parse("30 6 1 jan,jul *").unwrap();
        assert_eq!(
            c.next_after(t(2026, 2, 1, 0, 0)),
            Some(t(2026, 7, 1, 6, 30))
        );
    }

    #[test]
    fn year_rollover_and_leap_day() {
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            c.next_after(t(2026, 3, 1, 0, 0)),
            Some(t(2028, 2, 29, 0, 0))
        );
        assert_eq!(
            Cron::parse("59 23 31 12 *")
                .unwrap()
                .next_after(t(2026, 12, 31, 23, 59)),
            Some(t(2027, 12, 31, 23, 59))
        );
    }

    #[test]
    fn impossible_dates_have_no_next_run() {
        assert_eq!(
            Cron::parse("0 0 31 2 *")
                .unwrap()
                .next_after(t(2026, 1, 1, 0, 0)),
            None
        );
    }

    #[test]
    fn bad_expressions_say_what_is_wrong() {
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "* * * * * *",
        ] {
            let e = Cron::parse(bad).unwrap_err();
            assert!(!e.is_empty(), "{bad}");
        }
        assert!(Cron::parse("* * * *").unwrap_err().contains("5 fields"));
        assert!(Cron::parse("61 * * * *").unwrap_err().contains("minute"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("45").unwrap(), 45);
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
    }

    fn sched(name: &str) -> Schedule {
        Schedule {
            name: name.into(),
            cron: "*/5 * * * *".into(),
            image: "macos-base:15".into(),
            cmd: vec!["true".into()],
            ttl: Some("1h".into()),
            notify: true,
            webhook: Some("https://hooks.example.com/secret/token?x=1".into()),
            enabled: true,
            created_unix: 0,
            last_run_unix: None,
            last_status: None,
            last_detail: None,
            run_now: false,
        }
    }

    #[test]
    fn validation_and_webhook_hint() {
        assert!(validate(&sched("nightly")).is_ok());
        assert!(validate(&sched("bad name")).is_err());
        let mut s = sched("a");
        s.cron = "nope".into();
        assert!(validate(&s).is_err());
        let mut s = sched("a");
        s.webhook = Some("ftp://x".into());
        assert!(validate(&s).is_err());
        let mut s = sched("a");
        s.image = " ".into();
        assert!(validate(&s).is_err());
        assert_eq!(
            sched("a").webhook_hint().as_deref(),
            Some("https://hooks.example.com/…")
        );
    }

    #[test]
    fn file_add_rm_update_and_due() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        assert!(load(&p).unwrap().schedules.is_empty());
        add(&p, sched("a"), 1_000_000).unwrap();
        assert!(
            add(&p, sched("a"), 1)
                .unwrap_err()
                .contains("already exists")
        );
        let s = load(&p).unwrap().schedules.remove(0);
        assert_eq!(s.created_unix, 1_000_000);
        // created a while ago, never run: due; just run: not due until the next slot.
        assert!(s.due(1_000_000 + 3600));
        update(&p, "a", |s| {
            s.last_run_unix = Some(1_000_000 + 3600);
            s.last_status = Some("ok".into());
        })
        .unwrap();
        let s = load(&p).unwrap().schedules.remove(0);
        assert!(!s.due(1_000_000 + 3600 + 1));
        assert!(s.due(1_000_000 + 3600 + 600));
        update(&p, "a", |s| s.run_now = true).unwrap();
        assert!(load(&p).unwrap().schedules[0].due(1));
        update(&p, "a", |s| s.enabled = false).unwrap();
        assert!(!load(&p).unwrap().schedules[0].due(u64::MAX / 2));
        remove(&p, "a").unwrap();
        assert!(remove(&p, "a").unwrap_err().contains("schedule ls"));
        std::fs::write(&p, "{").unwrap();
        assert!(load(&p).unwrap_err().contains("not valid JSON"));
    }

    #[test]
    fn a_run_that_never_reported_back_blocks_the_next_for_six_hours() {
        let mut s = sched("a");
        s.last_run_unix = Some(1_000_000);
        s.last_status = Some("running".into());
        assert!(!s.due(1_000_000 + 3600));
        assert!(s.due(1_000_000 + 7 * 3600));
    }
}
