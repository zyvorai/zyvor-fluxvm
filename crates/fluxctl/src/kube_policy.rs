//! When `kube supervise` should let the cluster sleep for reasons of the Mac's own life rather than the cluster's traffic:
//! outside working hours (`--active 09:00-19:00`, `--weekdays`), and when the Mac is saving energy or running hot
//! (`--eco`: Low Power Mode, thermal throttling). The decisions are pure; only `Policy::says_sleep` reads `pmset`.

use anyhow::{Result, bail};
use chrono::{Datelike, Timelike};

/// Minutes since midnight, `start` inclusive and `end` exclusive; a window with `end <= start` crosses midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: u16,
    pub end: u16,
}

fn minute_of(hhmm: &str) -> Option<u16> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && h.to_string().len() <= 2 && m.to_string().len() <= 2).then_some(h * 60 + m)
}

/// `09:00-19:00`, `22:00-06:00` (crosses midnight).
pub fn parse_window(s: &str) -> Result<Window> {
    let Some((a, b)) = s.split_once('-') else {
        bail!("--active must look like 09:00-19:00");
    };
    let (Some(start), Some(end)) = (minute_of(a.trim()), minute_of(b.trim())) else {
        bail!("--active must look like 09:00-19:00 (24-hour times)");
    };
    if start == end {
        bail!("--active starts and ends at the same time; leave it out to stay awake all day");
    }
    Ok(Window { start, end })
}

impl Window {
    pub fn contains(self, minute: u16) -> bool {
        if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}

/// A local time: the weekday (0 = Monday) and the minute of the day.
pub type Now = (u32, u16);

pub fn local_now() -> Now {
    let t = chrono::Local::now();
    (
        t.weekday().num_days_from_monday(),
        (t.hour() * 60 + t.minute()) as u16,
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    window: Option<Window>,
    weekdays: bool,
    eco: bool,
}

/// Why the cluster should sleep now.
#[derive(Debug, PartialEq, Eq)]
pub struct Reason {
    pub why: String,
    /// The working-hours schedule, which also wakes the cluster again when the window opens.
    pub schedule: bool,
}

impl Policy {
    pub fn from_flags(active: Option<&str>, weekdays: bool, eco: bool) -> Result<Policy> {
        Ok(Policy {
            window: active.map(parse_window).transpose()?,
            weekdays,
            eco,
        })
    }

    fn scheduled(&self) -> bool {
        self.window.is_some() || self.weekdays
    }

    pub fn enabled(&self) -> bool {
        self.scheduled() || self.eco
    }

    /// Whether the working hours (if any) are open at `now`.
    pub fn active_at(&self, (weekday, minute): Now) -> bool {
        if self.weekdays && weekday >= 5 {
            return false;
        }
        self.window.is_none_or(|w| w.contains(minute))
    }

    /// The cluster is parked and the working hours have just opened.
    pub fn wake_due(&self, now: Now) -> bool {
        self.scheduled() && self.active_at(now)
    }

    /// The pure decision, given what `pmset` reported.
    pub fn decide(&self, now: Now, low_power: bool, hot: bool) -> Option<Reason> {
        if self.scheduled() && !self.active_at(now) {
            return Some(Reason {
                why: "outside working hours".into(),
                schedule: true,
            });
        }
        if self.eco && low_power {
            return Some(Reason {
                why: "Low Power Mode is on".into(),
                schedule: false,
            });
        }
        if self.eco && hot {
            return Some(Reason {
                why: "the Mac is throttling for heat".into(),
                schedule: false,
            });
        }
        None
    }

    /// [`decide`](Self::decide) with the Mac's real power state (`pmset` is only run for `--eco`).
    pub fn says_sleep(&self, now: Now) -> Option<Reason> {
        let (low_power, hot) = if self.eco {
            let out = |args: &[&str]| {
                std::process::Command::new("pmset")
                    .args(args)
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default()
            };
            (
                low_power_mode(&out(&["-g"])),
                thermal_pressure(&out(&["-g", "therm"])),
            )
        } else {
            (false, false)
        };
        self.decide(now, low_power, hot)
    }
}

/// `pmset -g` lists ` lowpowermode         1` when Low Power Mode is on.
pub fn low_power_mode(pmset: &str) -> bool {
    pmset.lines().any(|l| {
        let mut w = l.split_whitespace();
        w.next() == Some("lowpowermode") && w.next() == Some("1")
    })
}

/// `pmset -g therm` lists `CPU_Speed_Limit = 70` while the Mac is throttling; below 100 means it is.
pub fn thermal_pressure(pmset_therm: &str) -> bool {
    pmset_therm.lines().any(|l| {
        l.trim()
            .strip_prefix("CPU_Speed_Limit")
            .and_then(|r| r.trim_start().strip_prefix('='))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|limit| limit < 100)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: u32 = 0;
    const SAT: u32 = 5;

    fn at(h: u16, m: u16) -> u16 {
        h * 60 + m
    }

    #[test]
    fn windows_are_read_and_bad_ones_refused() {
        assert_eq!(
            parse_window("09:00-19:30").unwrap(),
            Window {
                start: at(9, 0),
                end: at(19, 30)
            }
        );
        assert_eq!(parse_window(" 9:05 - 17:00 ").unwrap().start, at(9, 5));
        for bad in [
            "",
            "9",
            "09:00",
            "09:00-",
            "25:00-26:00",
            "09:60-10:00",
            "ab-cd",
            "09:00-09:00",
            "09:00-19:00-20:00",
        ] {
            assert!(parse_window(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_window_includes_its_start_excludes_its_end_and_may_cross_midnight() {
        let day = parse_window("09:00-19:00").unwrap();
        assert!(day.contains(at(9, 0)) && day.contains(at(18, 59)));
        assert!(!day.contains(at(8, 59)) && !day.contains(at(19, 0)));
        let night = parse_window("22:00-06:00").unwrap();
        assert!(night.contains(at(23, 0)) && night.contains(at(0, 0)) && night.contains(at(5, 59)));
        assert!(
            !night.contains(at(6, 0)) && !night.contains(at(12, 0)) && !night.contains(at(21, 59))
        );
    }

    #[test]
    fn working_hours_and_weekdays_decide_when_the_cluster_may_be_awake() {
        let p = Policy::from_flags(Some("09:00-19:00"), true, false).unwrap();
        assert!(p.active_at((MON, at(10, 0))));
        assert!(!p.active_at((MON, at(20, 0))));
        assert!(
            !p.active_at((SAT, at(10, 0))),
            "a Saturday is outside --weekdays"
        );
        // --weekdays alone: awake all day Monday to Friday, asleep at the weekend.
        let w = Policy::from_flags(None, true, false).unwrap();
        assert!(w.active_at((MON, at(3, 0))) && !w.active_at((SAT, at(12, 0))));
        // Nothing set: always active, never says sleep.
        let none = Policy::default();
        assert!(!none.enabled() && none.active_at((SAT, 0)));
        assert_eq!(none.decide((SAT, 0), true, true), None);
    }

    #[test]
    fn outside_hours_sleeps_and_the_opening_of_the_window_wakes() {
        let p = Policy::from_flags(Some("09:00-19:00"), false, false).unwrap();
        let r = p.decide((MON, at(22, 0)), false, false).unwrap();
        assert_eq!(r.why, "outside working hours");
        assert!(r.schedule);
        assert_eq!(p.decide((MON, at(10, 0)), false, false), None);
        assert!(!p.wake_due((MON, at(8, 0))));
        assert!(p.wake_due((MON, at(9, 0))));
        // Without a schedule there is nothing to wake for.
        assert!(
            !Policy::from_flags(None, false, true)
                .unwrap()
                .wake_due((MON, 0))
        );
    }

    #[test]
    fn eco_sleeps_for_low_power_mode_and_heat_but_not_unless_asked() {
        let eco = Policy::from_flags(None, false, true).unwrap();
        let r = eco.decide((MON, at(10, 0)), true, false).unwrap();
        assert_eq!(r.why, "Low Power Mode is on");
        assert!(!r.schedule, "an energy reason does not schedule a wake-up");
        assert!(
            eco.decide((MON, at(10, 0)), false, true)
                .unwrap()
                .why
                .contains("heat")
        );
        assert_eq!(eco.decide((MON, at(10, 0)), false, false), None);
        let off = Policy::default();
        assert_eq!(off.decide((MON, at(10, 0)), true, true), None);
    }

    #[test]
    fn pmset_output_is_read() {
        assert!(low_power_mode(
            "System-wide power settings:\n sleep  1\n lowpowermode         1\n"
        ));
        assert!(!low_power_mode(" lowpowermode         0\n"));
        assert!(!low_power_mode(""));
        assert!(thermal_pressure(
            "CPU Power notify\n  CPU_Scheduler_Limit \t= 100\n  CPU_Speed_Limit \t= 70\n"
        ));
        assert!(!thermal_pressure("  CPU_Speed_Limit \t= 100\n"));
        assert!(!thermal_pressure(
            "Note: No thermal warning level has been recorded\n"
        ));
        assert!(!thermal_pressure(""));
    }
}
