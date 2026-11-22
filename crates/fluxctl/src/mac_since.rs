// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `--since` values for `fluxctl mac events` and `mac logs`: a duration back from now (`90s`, `10m`, `2h`, `1d`, `1h30m`), an
//! RFC 3339 timestamp, or a date (`2026-11-21`, midnight UTC). Pure: `now` is passed in, so the tests need no clock.

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Seconds in a duration written as one or more `NUMBERunit` parts (units `s`, `m`, `h`, `d`, `w`). `None` when it is not one.
pub fn parse_duration_secs(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let mut total = 0i64;
    let mut num = String::new();
    let mut parts = 0;
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let mult = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return None,
        };
        let n: i64 = num.parse().ok()?;
        total = total.checked_add(n.checked_mul(mult)?)?;
        num.clear();
        parts += 1;
    }
    (num.is_empty() && parts > 0).then_some(total)
}

/// The moment `--since VALUE` means. Errors say what is accepted and point at `fluxctl explain MAC_TIME_INVALID`.
pub fn parse_since(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let v = value.trim();
    if let Some(secs) = parse_duration_secs(v) {
        return Ok(now - Duration::seconds(secs));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d")
        && let Some(t) = d.and_hms_opt(0, 0, 0)
    {
        return Ok(t.and_utc());
    }
    bail!(
        "{value:?} is not a time: use a duration back from now (30s, 10m, 2h, 1d, 1h30m), an RFC 3339 timestamp \
         (2026-11-21T10:00:00Z) or a date (2026-11-21). More: fluxctl explain MAC_TIME_INVALID"
    )
}

/// The `-t` column of `mac logs -f -t`: the moment fluxctl received the line. Lines already in the log when following starts
/// have no known time and show `-` instead (see [`backlog_line`]).
pub fn stamp_text(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A line that was already in the console log, so its time is unknown. Same width as [`stamp_text`], so the columns line up.
pub fn backlog_line(line: &str) -> String {
    format!("{:<24} {line}\n", "-")
}

/// Turns the follow stream (chunks of any size) into stamped lines. `skip` is the last backlog line: following asks the daemon
/// for one line of history, and that line is dropped when it arrives straight away, because it is the one already printed.
pub struct LineStamper {
    buf: Vec<u8>,
    skip: Option<String>,
    first: bool,
    started: DateTime<Utc>,
}

impl LineStamper {
    pub fn new(skip: Option<String>, started: DateTime<Utc>) -> Self {
        Self {
            buf: Vec::new(),
            skip,
            first: true,
            started,
        }
    }

    fn line(&mut self, raw: &[u8], now: DateTime<Utc>, out: &mut String) {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim_end_matches('\r');
        if std::mem::take(&mut self.first)
            && now - self.started < Duration::seconds(1)
            && self.skip.as_deref() == Some(line)
        {
            return;
        }
        out.push_str(&format!("{} {line}\n", stamp_text(now)));
    }

    /// Stamped text for every complete line in `chunk` (a partial line waits for the rest).
    pub fn push(&mut self, chunk: &[u8], now: DateTime<Utc>) -> String {
        self.buf.extend_from_slice(chunk);
        let mut out = String::new();
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=i).collect();
            self.line(&raw[..raw.len() - 1], now, &mut out);
        }
        out
    }

    /// A last line the VM wrote without a newline.
    pub fn finish(&mut self, now: DateTime<Utc>) -> String {
        let mut out = String::new();
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            self.line(&raw, now, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn durations_count_back_from_now() {
        assert_eq!(
            parse_since("10m", now()).unwrap().to_rfc3339(),
            "2026-11-23T11:50:00+00:00"
        );
        assert_eq!(
            parse_since("1h30m", now()).unwrap().to_rfc3339(),
            "2026-11-23T10:30:00+00:00"
        );
        assert_eq!(
            parse_since("2d", now()).unwrap().to_rfc3339(),
            "2026-11-21T12:00:00+00:00"
        );
    }

    #[test]
    fn timestamps_and_dates() {
        assert_eq!(
            parse_since("2026-11-21T10:00:00+01:00", now())
                .unwrap()
                .to_rfc3339(),
            "2026-11-21T09:00:00+00:00"
        );
        assert_eq!(
            parse_since("2026-11-21", now()).unwrap().to_rfc3339(),
            "2026-11-21T00:00:00+00:00"
        );
    }

    #[test]
    fn nonsense_is_refused_with_what_is_accepted() {
        for bad in ["", "yesterday", "10", "m", "1x", "10m5", "-5m"] {
            let e = parse_since(bad, now()).unwrap_err().to_string();
            assert!(
                e.contains("is not a time") && e.contains("MAC_TIME_INVALID"),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn stamper_drops_the_repeated_history_line_and_stamps_the_rest() {
        let t0 = now();
        let mut s = LineStamper::new(Some("last old".into()), t0);
        assert_eq!(s.push(b"last old\nnew", t0), "");
        let out = s.push(b" one\r\nnew two\n", t0 + Duration::seconds(5));
        assert_eq!(
            out,
            "2026-11-23T12:00:05.000Z new one\n2026-11-23T12:00:05.000Z new two\n"
        );
        assert_eq!(s.finish(t0 + Duration::seconds(6)), "");
    }

    #[test]
    fn stamper_keeps_an_identical_first_line_that_arrives_late() {
        let t0 = now();
        let mut s = LineStamper::new(Some("tick".into()), t0);
        let out = s.push(b"tick\n", t0 + Duration::seconds(30));
        assert!(out.ends_with(" tick\n") && out.starts_with("2026-11-23T12:00:30"));
        let mut s = LineStamper::new(None, t0);
        assert!(s.push(b"tail", t0).is_empty());
        assert!(s.finish(t0).ends_with(" tail\n"));
    }

    #[test]
    fn backlog_lines_show_a_dash_in_the_time_column() {
        assert_eq!(backlog_line("boot"), format!("{:<24} boot\n", "-"));
        assert_eq!(stamp_text(now()).len(), 24);
    }
}
