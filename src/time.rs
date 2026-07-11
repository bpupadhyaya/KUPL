//! A small, self-contained UTC calendar, shared by the interpreter and the KVM
//! (zero dependencies, like `src/json.rs` and `src/regex.rs`).
//!
//! Everything here operates on a Unix timestamp (seconds since 1970-01-01
//! 00:00:00 UTC) using pure integer arithmetic — no locale, no leap seconds.
//! The days↔civil-date conversion is Howard Hinnant's well-known algorithm,
//! which is correct for the full i64 range including negative (pre-1970)
//! timestamps. Because it is pure integer math, `cgen.rs` mirrors it exactly,
//! so `format_time` and the extractors are byte-identical on every engine.

/// Floor-divide (round toward negative infinity), so pre-1970 timestamps split
/// into days/seconds correctly.
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a % b < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

fn floor_mod(a: i64, b: i64) -> i64 {
    // The final result is always in [0, b) (tiny for every real caller here --
    // 86400, 400, 146097, 7...), but the INTERMEDIATE product `floor_div(a, b) * b`
    // can transiently exceed i64 range when `a` spans the full i64 timestamp range
    // (e.g. a timestamp near i64::MIN/MAX) even though `a` itself and the final
    // subtracted result both fit -- a classic "intermediate overflow, final value in
    // range" trap. Widen to i128 for the multiply/subtract so this never overflows;
    // the final cast back to i64 is always in-bounds by construction.
    (a as i128 - floor_div(a, b) as i128 * b as i128) as i64
}

/// (year, month 1..=12, day 1..=31) from a count of days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = floor_div(if z >= 0 { z } else { z - 146_096 }, 146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Count of days since 1970-01-01 for a civil (year, month 1..=12, day 1..=31).
/// The inverse of `civil_from_days` — Howard Hinnant's well-known algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = floor_div(if y >= 0 { y } else { y - 399 }, 400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Compose a UTC timestamp from civil components (seconds since 1970-01-01).
/// Components are not range-checked; out-of-range values normalize (e.g. a
/// `month` of 13 rolls into the next year), matching civil arithmetic.
pub fn make(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> i64 {
    days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss
}

/// Day of the year, 1 = Jan 1 … 365/366 = Dec 31.
pub fn yearday_of(epoch_secs: i64) -> i64 {
    let days = split(epoch_secs).0;
    let (y, _, _) = civil_from_days(days);
    days - days_from_civil(y, 1, 1) + 1
}

/// UTC ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp.
pub fn iso(epoch_secs: i64) -> String {
    let (days, secs) = split(epoch_secs);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if y < 0 {
        format!("-{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", -y, m, d, hh, mm, ss)
    } else {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
    }
}

/// Parse an ISO-8601-ish UTC timestamp into epoch seconds. Accepts
/// `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SS`, and `YYYY-MM-DD HH:MM:SS`, each with an
/// optional trailing `Z`. Returns `Err` with a message on malformed input.
/// Whether `y` is a Gregorian leap year.
fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Number of days in month `mo` (1..=12) of year `y`. Returns 0 for an out-of-range
/// month so the caller's range check still rejects it.
fn days_in_month(y: i64, mo: i64) -> i64 {
    match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 0,
    }
}

pub fn parse_iso(s: &str) -> Result<i64, String> {
    let s = s.trim().trim_end_matches('Z');
    let bad = || format!("invalid ISO-8601 timestamp: {s}");
    // date and optional time, split on 'T' or ' '
    let (date, time) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let dp: Vec<&str> = date.split('-').collect();
    // a leading '-' (negative year) yields an empty first field; rejoin it
    let (yy, mm, dd) = match dp.as_slice() {
        [y, m, d] => (y.to_string(), *m, *d),
        ["", y, m, d] => (format!("-{y}"), *m, *d),
        _ => return Err(bad()),
    };
    let y: i64 = yy.parse().map_err(|_| bad())?;
    let mo: i64 = mm.parse().map_err(|_| bad())?;
    let d: i64 = dd.parse().map_err(|_| bad())?;
    // Validate the day against the actual length of the month (leap-year aware) so
    // an impossible calendar date — 2023-02-29, 2024-02-30, 2024-04-31 — is rejected
    // rather than silently normalized into the following month.
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return Err(bad());
    }
    let (mut hh, mut mi, mut ss) = (0i64, 0i64, 0i64);
    if !time.is_empty() {
        let tp: Vec<&str> = time.split(':').collect();
        if tp.len() != 3 {
            return Err(bad());
        }
        hh = tp[0].parse().map_err(|_| bad())?;
        mi = tp[1].parse().map_err(|_| bad())?;
        ss = tp[2].parse().map_err(|_| bad())?;
        if !(0..=23).contains(&hh) || !(0..=59).contains(&mi) || !(0..=60).contains(&ss) {
            return Err(bad());
        }
    }
    Ok(make(y, mo, d, hh, mi, ss))
}

/// Split a timestamp into (days-since-epoch, second-of-day 0..86399).
fn split(epoch_secs: i64) -> (i64, i64) {
    let days = floor_div(epoch_secs, 86_400);
    let secs = floor_mod(epoch_secs, 86_400);
    (days, secs)
}

/// UTC `YYYY-MM-DD HH:MM:SS` for a Unix timestamp.
pub fn format_time(epoch_secs: i64) -> String {
    let (days, secs) = split(epoch_secs);
    let (y, m, d) = civil_from_days(days);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    // years are zero-padded to at least 4 digits; a negative year keeps its sign
    if y < 0 {
        format!("-{:04}-{:02}-{:02} {:02}:{:02}:{:02}", -y, m, d, hh, mm, ss)
    } else {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
    }
}

pub fn year_of(epoch_secs: i64) -> i64 {
    civil_from_days(split(epoch_secs).0).0
}
pub fn month_of(epoch_secs: i64) -> i64 {
    civil_from_days(split(epoch_secs).0).1
}
pub fn day_of(epoch_secs: i64) -> i64 {
    civil_from_days(split(epoch_secs).0).2
}
pub fn hour_of(epoch_secs: i64) -> i64 {
    split(epoch_secs).1 / 3600
}
pub fn minute_of(epoch_secs: i64) -> i64 {
    (split(epoch_secs).1 % 3600) / 60
}
pub fn second_of(epoch_secs: i64) -> i64 {
    split(epoch_secs).1 % 60
}
/// Day of week, 0 = Sunday … 6 = Saturday. 1970-01-01 was a Thursday (4).
pub fn weekday_of(epoch_secs: i64) -> i64 {
    floor_mod(split(epoch_secs).0 + 4, 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(weekday_of(0), 4); // Thursday
    }

    #[test]
    fn known_timestamps() {
        // 2001-09-09 01:46:40 UTC
        assert_eq!(format_time(1_000_000_000), "2001-09-09 01:46:40");
        // 2026-07-04 00:00:00 UTC
        assert_eq!(format_time(1_783_123_200), "2026-07-04 00:00:00");
        assert_eq!(year_of(1_783_123_200), 2026);
        assert_eq!(month_of(1_783_123_200), 7);
        assert_eq!(day_of(1_783_123_200), 4);
    }

    #[test]
    fn components() {
        let t = 1_000_000_000;
        assert_eq!(year_of(t), 2001);
        assert_eq!(month_of(t), 9);
        assert_eq!(day_of(t), 9);
        assert_eq!(hour_of(t), 1);
        assert_eq!(minute_of(t), 46);
        assert_eq!(second_of(t), 40);
    }

    #[test]
    fn leap_year_boundary() {
        // 2000 is a leap year: 2000-02-29 exists
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00:00");
        // day after
        assert_eq!(format_time(951_782_400 + 86_400), "2000-03-01 00:00:00");
        // 1900 is NOT a leap year (century, not /400)
        // 1900-02-28 -> next day is 1900-03-01
        let feb28_1900 = -2_203_977_600; // 1900-02-28 00:00:00
        assert_eq!(format_time(feb28_1900), "1900-02-28 00:00:00");
        assert_eq!(format_time(feb28_1900 + 86_400), "1900-03-01 00:00:00");
    }

    #[test]
    fn negative_epoch() {
        assert_eq!(format_time(-1), "1969-12-31 23:59:59");
        assert_eq!(format_time(-86_400), "1969-12-31 00:00:00");
        // a very old date
        assert_eq!(format_time(-62_135_596_800), "0001-01-01 00:00:00");
    }

    #[test]
    fn make_and_roundtrip() {
        assert_eq!(make(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(make(2001, 9, 9, 1, 46, 40), 1_000_000_000);
        assert_eq!(make(2000, 2, 29, 0, 0, 0), 951_782_400);
        // round-trip a spread of timestamps through iso <-> parse_iso
        for &t in &[0i64, 1_000_000_000, 1_783_123_200, 951_782_400, -1, -62_135_596_800] {
            assert_eq!(parse_iso(&iso(t)), Ok(t), "roundtrip {t}");
            // days_from_civil is the exact inverse of civil_from_days
            let d = split(t).0;
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn iso_and_parse() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(parse_iso("2001-09-09T01:46:40Z"), Ok(1_000_000_000));
        assert_eq!(parse_iso("2001-09-09 01:46:40"), Ok(1_000_000_000));
        assert_eq!(parse_iso("1970-01-01"), Ok(0));
        assert!(parse_iso("not-a-date").is_err());
        assert!(parse_iso("2001-13-01").is_err());
        assert!(parse_iso("2001-09-09T25:00:00").is_err());
    }

    #[test]
    fn yeardays() {
        assert_eq!(yearday_of(0), 1); // Jan 1
        assert_eq!(yearday_of(make(2000, 12, 31, 0, 0, 0)), 366); // leap year
        assert_eq!(yearday_of(make(2001, 12, 31, 0, 0, 0)), 365);
        assert_eq!(yearday_of(make(2023, 3, 1, 0, 0, 0)), 60); // 31+28+1
    }

    #[test]
    fn weekdays() {
        // 1970-01-01 Thu(4), +1 Fri(5), +2 Sat(6), +3 Sun(0)
        assert_eq!(weekday_of(0), 4);
        assert_eq!(weekday_of(86_400), 5);
        assert_eq!(weekday_of(2 * 86_400), 6);
        assert_eq!(weekday_of(3 * 86_400), 0);
    }
}
