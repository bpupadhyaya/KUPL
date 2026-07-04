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
    a - floor_div(a, b) * b
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
    fn weekdays() {
        // 1970-01-01 Thu(4), +1 Fri(5), +2 Sat(6), +3 Sun(0)
        assert_eq!(weekday_of(0), 4);
        assert_eq!(weekday_of(86_400), 5);
        assert_eq!(weekday_of(2 * 86_400), 6);
        assert_eq!(weekday_of(3 * 86_400), 0);
    }
}
