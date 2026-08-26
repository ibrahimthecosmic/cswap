//! Date arithmetic, hand-rolled to keep the dependency list at one.
//!
//! Everything here is UTC. Rendering a *local* wall-clock time would need the
//! platform timezone database, which is where a time crate would have earned its
//! place; instead reset times are shown as a countdown ("resets in 4h 42m") plus
//! the UTC instant, which needs no zone and is what you actually read off the
//! line anyway.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg_attr(not(feature = "usage"), allow(dead_code))]
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since the Unix epoch → (year, month, day). Hinnant's civil-from-days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since the Unix epoch. Hinnant's days-from-civil.
#[cfg_attr(not(feature = "usage"), allow(dead_code))]
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// `2026-08-26T22:25:52Z` — the stamp format claude-swap writes to
/// `sequence.json`, matched exactly so both tools read each other's records.
pub fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `Aug 28 05:59 UTC` — for reset times, where the date matters but seconds don't.
pub fn utc_short(secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (_, m, d) = civil_from_days(days);
    format!(
        "{} {d} {:02}:{:02} UTC",
        MONTHS[(m - 1) as usize],
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Parse the RFC 3339 timestamps the usage API returns, e.g.
/// `2026-08-27T03:19:59.544591+00:00` or `2026-08-27T03:19:59Z`. Returns whole
/// seconds since the epoch; the fractional part is parsed only to be skipped.
#[cfg_attr(not(feature = "usage"), allow(dead_code))]
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (n(0, 4)?, n(5, 7)? as u32, n(8, 10)? as u32);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut epoch = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;

    let mut rest = &s[19..];
    if rest.starts_with('.') {
        let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
        rest = &rest[1 + digits..];
    }
    match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => {}
        Some(sign @ (b'+' | b'-')) => {
            // Offsets come as +HH:MM or +HHMM.
            let body = &rest[1..];
            let (oh, om) = match body.split_once(':') {
                Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
                None if body.len() >= 4 => (body[..2].parse().ok()?, body[2..4].parse().ok()?),
                None => (body.parse().ok()?, 0),
            };
            let offset = oh * 3600 + om * 60;
            epoch += if *sign == b'-' { offset } else { -offset };
        }
        _ => return None,
    }
    Some(epoch)
}

/// `4h 42m`, `6d 3h`, `12m`, `now`. Two units at most — the third never
/// changed a decision.
pub fn countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "now".to_string();
    }
    let (d, h, m) = (
        seconds / 86_400,
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        "<1m".to_string()
    }
}

/// `6m ago`, `2h ago` — how stale a cached usage reading is.
pub fn ago(seconds: i64) -> String {
    if seconds < 60 {
        return "just now".to_string();
    }
    let (d, h, m) = (
        seconds / 86_400,
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
    );
    if d > 0 {
        format!("{d}d ago")
    } else if h > 0 {
        format!("{h}h ago")
    } else {
        format!("{m}m ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_the_usage_api_returns() {
        // Fractional seconds with an explicit +00:00 offset is what we actually
        // see; the Z form and a real offset are the other two in the wild.
        assert_eq!(
            parse_rfc3339("2026-08-27T03:19:59.544591+00:00"),
            Some(1787800799)
        );
        assert_eq!(parse_rfc3339("2026-08-27T03:19:59Z"), Some(1787800799));
        assert_eq!(parse_rfc3339("2026-08-27T09:19:59+06:00"), Some(1787800799));
        assert_eq!(parse_rfc3339("2026-08-26T21:19:59-06:00"), Some(1787800799));
    }

    #[test]
    fn rejects_nonsense() {
        for bad in ["", "2026-08-27", "not a date", "2026-13-01T00:00:00Z"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn stamp_and_parse_are_inverses_across_epochs() {
        for secs in [0, 951_782_400, 1_787_793_599, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&utc_stamp(secs)), Some(secs), "{secs}");
        }
    }

    #[test]
    fn stamp_matches_claude_swaps_format() {
        assert_eq!(utc_stamp(1787783152), "2026-08-26T22:25:52Z");
    }

    #[test]
    fn countdown_reads_as_two_units() {
        assert_eq!(countdown(4 * 3600 + 42 * 60), "4h 42m");
        assert_eq!(countdown(6 * 86400 + 3 * 3600), "6d 3h");
        assert_eq!(countdown(12 * 60), "12m");
        assert_eq!(countdown(30), "<1m");
        assert_eq!(countdown(-5), "now");
    }
}
