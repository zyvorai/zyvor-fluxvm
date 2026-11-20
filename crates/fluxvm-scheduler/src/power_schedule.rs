// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Cron-driven power schedules (`power_schedule`, or the `fluxvm.io/power-start`
//! and `fluxvm.io/power-stop` labels): parsing, the next fire time, and the
//! decision for one reaper tick. Everything here is pure; `VmManager::
//! run_power_schedules` applies the decision with `start`/`stop`.
//!
//! Cron is the classic five fields in UTC: `min hour day-of-month month
//! day-of-week`, each `*`, a number, a range `a-b`, a list `a,b`, with an
//! optional step `/n`. Day-of-week is 0-7 (0 and 7 are Sunday). When both
//! day-of-month and day-of-week are restricted a day matches if either does,
//! like Vixie cron. `@hourly @daily @weekly @monthly @yearly` are accepted.

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use fluxvm_core::lifecycle::{POWER_START_LABEL, POWER_STOP_LABEL, PowerSchedule};
use fluxvm_core::model::{VmRecord, VmStatus};

/// A parsed cron expression: one bit set per allowed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    dow: u8,
    dom_star: bool,
    dow_star: bool,
}

fn parse_field(src: &str, min: u32, max: u32, what: &str) -> Result<(u64, bool)> {
    let mut bits = 0u64;
    let mut star = false;
    for part in src.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{what}: bad step {s:?}"))?;
                if step == 0 {
                    bail!("{what}: step must be at least 1");
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            if step == 1 {
                star = true;
            }
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a, what)?, num(b, what)?)
        } else {
            let n = num(range, what)?;
            // `5/10` means 5-max stepping by 10, as in cron.
            (n, if step > 1 { max } else { n })
        };
        if lo < min || hi > max || lo > hi {
            bail!("{what}: {part:?} is outside {min}-{max}");
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, star))
}

fn num(s: &str, what: &str) -> Result<u32> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("{what}: {s:?} is not a number"))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let expr = expr.trim();
        let expanded = match expr {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            bail!(
                "cron {expr:?} needs 5 fields (min hour day month weekday), found {}",
                f.len()
            );
        }
        let (minutes, _) = parse_field(f[0], 0, 59, "minute")?;
        let (hours, _) = parse_field(f[1], 0, 23, "hour")?;
        let (dom, dom_star) = parse_field(f[2], 1, 31, "day-of-month")?;
        let (months, _) = parse_field(f[3], 1, 12, "month")?;
        let (dow7, dow_star) = parse_field(f[4], 0, 7, "day-of-week")?;
        // 7 is Sunday too.
        let dow = ((dow7 & 0x7f) | (dow7 >> 7 & 1)) as u8;
        Ok(Cron {
            minutes,
            hours: hours as u32,
            dom: dom as u32,
            months: months as u16,
            dow,
            dom_star,
            dow_star,
        })
    }

    fn day_matches(&self, d: chrono::NaiveDate) -> bool {
        if self.months >> d.month() & 1 == 0 {
            return false;
        }
        let dom_ok = self.dom >> d.day() & 1 == 1;
        let dow_ok = self.dow >> d.weekday().num_days_from_sunday() & 1 == 1;
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// The first fire time strictly after `after` (minute resolution), or
    /// `None` when the expression never fires within five years (`0 0 31 2 *`).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let mut day = start.date_naive();
        for i in 0..(366 * 5) {
            if self.day_matches(day) {
                // On the first day only times at or after `start` qualify.
                let (min_h, min_m) = if i == 0 {
                    (start.hour(), start.minute())
                } else {
                    (0, 0)
                };
                for h in min_h..24 {
                    if self.hours >> h & 1 == 0 {
                        continue;
                    }
                    let from_m = if h == min_h { min_m } else { 0 };
                    for m in from_m..60 {
                        if self.minutes >> m & 1 == 1 {
                            return Utc
                                .with_ymd_and_hms(day.year(), day.month(), day.day(), h, m, 0)
                                .single();
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }
}

/// The next time `expr` fires after `after`.
pub fn next_fire(expr: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    Ok(Cron::parse(expr)?.next_after(after))
}

/// Checks a schedule at create time.
pub fn validate(s: &PowerSchedule) -> Result<()> {
    if s.start.is_none() && s.stop.is_none() {
        bail!("power_schedule needs a start or a stop");
    }
    for (key, v) in [("start", &s.start), ("stop", &s.stop)] {
        if let Some(v) = v {
            Cron::parse(v).map_err(|e| anyhow::anyhow!("power_schedule.{key}: {e}"))?;
        }
    }
    Ok(())
}

/// The schedule that applies to `vm`: the spec field, else the labels.
pub fn effective(vm: &VmRecord) -> Option<PowerSchedule> {
    if let Some(s) = &vm.request.power_schedule {
        return Some(s.clone());
    }
    let start = vm.labels.get(POWER_START_LABEL).cloned();
    let stop = vm.labels.get(POWER_STOP_LABEL).cloned();
    (start.is_some() || stop.is_some()).then_some(PowerSchedule { start, stop })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    None,
    Start,
    Stop,
}

/// What to do for one VM on a tick covering `(window_start, now]`. A fire time
/// inside the window is acted on once; when both start and stop fired the later
/// wins. A start only applies to a Stopped VM and a stop only to a Running one,
/// so an already-correct VM is left alone. A schedule that cannot parse does
/// nothing (it is rejected at create time).
pub fn decide(
    sched: &PowerSchedule,
    status: VmStatus,
    window_start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> PowerAction {
    let fired = |expr: &Option<String>| {
        expr.as_deref()
            .and_then(|e| next_fire(e, window_start).ok().flatten())
            .filter(|t| *t <= now)
    };
    let start = fired(&sched.start);
    let stop = fired(&sched.stop);
    let want = match (start, stop) {
        (None, None) => return PowerAction::None,
        (Some(_), None) => PowerAction::Start,
        (None, Some(_)) => PowerAction::Stop,
        (Some(a), Some(b)) => {
            if a > b {
                PowerAction::Start
            } else {
                PowerAction::Stop
            }
        }
    };
    match (want, status) {
        (PowerAction::Start, VmStatus::Stopped) => PowerAction::Start,
        (PowerAction::Stop, VmStatus::Running) => PowerAction::Stop,
        _ => PowerAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn nf(expr: &str, after: &str) -> String {
        next_fire(expr, t(after))
            .unwrap()
            .unwrap()
            .format("%Y-%m-%dT%H:%M")
            .to_string()
    }

    #[test]
    fn simple_daily() {
        assert_eq!(nf("30 8 * * *", "2026-11-19T08:29:59Z"), "2026-11-19T08:30");
        // Strictly after: standing on the fire time moves to the next day.
        assert_eq!(nf("30 8 * * *", "2026-11-19T08:30:00Z"), "2026-11-20T08:30");
        assert_eq!(nf("@daily", "2026-11-19T12:00:00Z"), "2026-11-20T00:00");
    }

    #[test]
    fn weekdays_and_steps() {
        // 2026-11-19 is a Thursday; Friday is next.
        assert_eq!(
            nf("0 18 * * 1-5", "2026-11-19T18:00:00Z"),
            "2026-11-20T18:00"
        );
        // Friday 18:00 -> Monday.
        assert_eq!(
            nf("0 18 * * 1-5", "2026-11-20T18:00:00Z"),
            "2026-11-23T18:00"
        );
        assert_eq!(
            nf("*/15 * * * *", "2026-11-19T10:16:00Z"),
            "2026-11-19T10:30"
        );
        assert_eq!(
            nf("5/20 * * * *", "2026-11-19T10:06:00Z"),
            "2026-11-19T10:25"
        );
        assert_eq!(nf("0 0 * * 7", "2026-11-19T00:00:00Z"), "2026-11-22T00:00");
    }

    #[test]
    fn month_and_year_rollover() {
        assert_eq!(nf("0 0 1 1 *", "2026-11-19T00:00:00Z"), "2027-01-01T00:00");
        assert_eq!(nf("0 0 29 2 *", "2026-11-19T00:00:00Z"), "2028-02-29T00:00");
        assert_eq!(
            nf("59 23 31 12 *", "2026-12-31T23:59:00Z"),
            "2027-12-31T23:59"
        );
    }

    #[test]
    fn dom_and_dow_are_or() {
        // The 1st, or any Monday: 2026-11-19 (Thu) -> Monday the 23rd.
        assert_eq!(nf("0 0 1 * 1", "2026-11-19T00:00:00Z"), "2026-11-23T00:00");
    }

    #[test]
    fn never_fires() {
        assert_eq!(
            next_fire("0 0 31 2 *", t("2026-01-01T00:00:00Z")).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_bad_expressions() {
        for bad in [
            "",
            "* * * *",
            "61 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "* * * 13 *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn validate_requires_one_side() {
        assert!(validate(&PowerSchedule::default()).is_err());
        assert!(
            validate(&PowerSchedule {
                start: Some("0 8 * * *".into()),
                stop: Some("nope".into())
            })
            .is_err()
        );
        assert!(
            validate(&PowerSchedule {
                start: Some("0 8 * * *".into()),
                stop: None
            })
            .is_ok()
        );
    }

    fn work_hours() -> PowerSchedule {
        PowerSchedule {
            start: Some("0 8 * * 1-5".into()),
            stop: Some("0 18 * * 1-5".into()),
        }
    }

    #[test]
    fn decide_acts_once_per_fire() {
        let s = work_hours();
        // Thu 08:00 fired inside the window and the VM is Stopped.
        let w = t("2026-11-19T07:59:00Z");
        let now = t("2026-11-19T08:00:30Z");
        assert_eq!(decide(&s, VmStatus::Stopped, w, now), PowerAction::Start);
        // Already running: nothing to do.
        assert_eq!(decide(&s, VmStatus::Running, w, now), PowerAction::None);
        // The next window no longer contains the fire.
        assert_eq!(
            decide(&s, VmStatus::Stopped, now, t("2026-11-19T08:01:30Z")),
            PowerAction::None
        );
        // Evening stop.
        assert_eq!(
            decide(
                &s,
                VmStatus::Running,
                t("2026-11-19T17:59:00Z"),
                t("2026-11-19T18:00:10Z")
            ),
            PowerAction::Stop
        );
        // Weekend: no fire.
        assert_eq!(
            decide(
                &s,
                VmStatus::Stopped,
                t("2026-11-21T07:59:00Z"),
                t("2026-11-21T08:01:00Z")
            ),
            PowerAction::None
        );
    }

    #[test]
    fn decide_later_fire_wins_in_a_long_window() {
        // A daemon that was down all day sees both fires; the evening stop wins.
        let s = work_hours();
        let w = t("2026-11-19T06:00:00Z");
        let now = t("2026-11-19T19:00:00Z");
        // next_fire only reports the first fire per expression after the window
        // start, which is 08:00 and 18:00: stop is later.
        assert_eq!(decide(&s, VmStatus::Running, w, now), PowerAction::Stop);
        assert_eq!(decide(&s, VmStatus::Stopped, w, now), PowerAction::None);
    }

    #[test]
    fn paused_and_failed_are_left_alone() {
        let s = work_hours();
        let w = t("2026-11-19T07:59:00Z");
        let now = t("2026-11-19T08:00:30Z");
        for st in [VmStatus::Paused, VmStatus::Failed, VmStatus::Creating] {
            assert_eq!(decide(&s, st, w, now), PowerAction::None);
        }
    }
}
