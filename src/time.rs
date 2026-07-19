//! A small, self-contained UTC calendar, shared by the interpreter and the KVM
//! (zero dependencies, like `src/json.rs` and `src/regex.rs`).
//!
//! Everything here operates on a Unix timestamp (seconds since 1970-01-01
//! 00:00:00 UTC) using pure integer arithmetic — no locale, no leap seconds.
//! The days↔civil-date conversion is Howard Hinnant's well-known algorithm,
//! which is correct for the full i64 range including negative (pre-1970)
//! timestamps. Because it is pure integer math, `cgen.rs` mirrors it exactly,
//! so `format_time` and the extractors are byte-identical on every engine.
//!
//! `make`/`date_make`/`parse_iso` accept civil components (year, month, day, …)
//! directly from arbitrary caller input, unlike the day↔civil conversion the
//! other direction uses (always fed a `days` count already bounded by a real
//! i64 epoch_secs) — so `make` validates that its result actually fits in i64
//! and returns `Err` rather than overflowing (PR-it635).

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
    // Plain TRUNCATING division here, not `floor_div` (PR-it682, a REAL bug):
    // Hinnant's algorithm already compensates for negative `z` by subtracting
    // `146_096` (one era-length minus one) BEFORE dividing -- that offset is
    // specifically designed so truncating division on the offset value equals
    // floor division on the ORIGINAL value. Wrapping it in `floor_div` (a
    // genuine floor-division function) applies the negative-number correction
    // a SECOND time, off-by-one-ing `era` for every negative-era `z` that
    // isn't an exact multiple of 146_097.
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
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
/// UNCHECKED i64 arithmetic — safe ONLY when `y` is itself already bounded
/// (e.g. derived from `civil_from_days` of a real epoch_secs, as `yearday_of`
/// below does; such a `y` can never approach the overflow threshold below).
/// A caller that receives `y`/`m`/`d` directly from arbitrary, UNTRUSTED input
/// (a user-supplied `date_make`/`parse_iso` argument, unbounded by
/// construction) must use `days_from_civil_checked` instead.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    // Plain TRUNCATING division, not `floor_div` -- see `civil_from_days`'s
    // comment on the identical bug (PR-it682): the `-399` offset already
    // compensates for negative `y`, so wrapping it in `floor_div` too
    // double-corrects.
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// `days_from_civil`, but widened to i128 THROUGHOUT (not just at the one risky
/// step, unlike `floor_mod` above) — a REAL bug found+fixed (production-
/// hardening PR-it635): `days_from_civil`'s `y`/`m` are directly, arbitrarily
/// user-controlled via `date_make`/`parse_iso` (unlike `civil_from_days`'s `z`,
/// always derived from an already-i64-bounded epoch_secs), so an extreme value
/// (e.g. `date_make(9223372036854775807, ...)`, or `parse_iso` on a
/// syntactically-fine-but-astronomically-large year like
/// "99999999999999-01-01") drove `153 * (m - 3)`-style terms past i64's range
/// — a raw Rust arithmetic overflow, which PANICS (crashing the whole process
/// with an "internal compiler error", confirmed live) in a debug build, or
/// silently wraps to a meaningless, WRONG timestamp in a release build.
/// Because every intermediate term here is bounded by a small constant
/// multiple of an i64-range value, i128 (unlike i64) has enough headroom that
/// NO possible y/m/d input can overflow it — `None` only if the FINAL day
/// count itself doesn't fit back into i64.
fn days_from_civil_checked(y: i64, m: i64, d: i64) -> Option<i64> {
    let (y, m, d) = (y as i128, m as i128, d as i128);
    // A REAL bug found+fixed (production-hardening PR-it868, an Explore
    // survey finding, independently re-verified live before implementing):
    // the single-year shift below (`y - 1` / `m + 9`, the classic Howard
    // Hinnant month-normalization) only correctly compensates for a month
    // within ONE year of the valid `[1, 12]` range -- matching the existing
    // PR-it635/it682 test coverage (`m = 13`, exactly one year over). A
    // month requiring a 2+ year shift silently produced a WRONG date with
    // no error, contradicting this function's own caller's (`make`'s)
    // documented contract that "out-of-range values normalize... matching
    // civil arithmetic" for ANY out-of-range month, not just a one-year
    // overflow. Confirmed live BEFORE this fix, byte-identical wrong on all
    // three engines: `date_make(2024, 25, 1, 0, 0, 0)` (month 25 of 2024 is
    // month 1 TWO years later) returned a timestamp 2 DAYS off from
    // `date_make(2026, 1, 1, 0, 0, 0)`'s correct value; swept `m` from 15
    // to 30 and -10 to -20, every value outside `[-9, 14]` mismatched by
    // 1-5 days while `m` within that single-year window was already
    // correct. Fixed by normalizing `m` into `[1, 12]` (adjusting `y` by
    // however many whole years the overflow represents, via Euclidean
    // division so negative months normalize correctly too) BEFORE the
    // existing single-year shift, which now only ever sees an
    // already-in-range `m` exactly as it did before this fix.
    let extra_years = (m - 1).div_euclid(12);
    let m = (m - 1).rem_euclid(12) + 1;
    let y = y + extra_years;
    let y = if m <= 2 { y - 1 } else { y };
    // Plain TRUNCATING division, not `floor_div_128` -- same bug/fix as
    // `days_from_civil` above (PR-it682).
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::try_from(era * 146_097 + doe - 719_468).ok()
}

/// Compose a UTC timestamp from civil components (seconds since 1970-01-01).
/// Components are not range-checked; out-of-range values normalize (e.g. a
/// `month` of 13 rolls into the next year), matching civil arithmetic — but
/// `Err` (never a panic/overflow) if the components are so extreme the result
/// genuinely cannot be represented as an i64 second count (PR-it635).
pub fn make(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> Result<i64, String> {
    let overflow = || "date component out of representable range".to_string();
    let days = days_from_civil_checked(y, m, d).ok_or_else(overflow)?;
    let total = (days as i128) * 86_400 + (hh as i128) * 3600 + (mm as i128) * 60 + (ss as i128);
    i64::try_from(total).map_err(|_| overflow())
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
        // A REAL bug found+fixed (production-hardening PR-it880, the THIRD
        // sibling instance of this function's "forgot one input shape in the
        // validation chain" pattern -- it111 leap-year-aware day validation,
        // it635 collapsing an unrepresentable-year internal error into this
        // SAME generic wording): an explicit `T`/space separator with
        // NOTHING after it (e.g. `"2024-01-01T"`) produced an EMPTY `time`
        // slice, indistinguishable from the `None` arm below (genuinely no
        // separator at all, i.e. a valid date-only string) -- so the
        // `!time.is_empty()` check just below silently skipped ALL time
        // validation and defaulted to midnight, exactly as if the caller had
        // written the well-formed date-only `"2024-01-01"`. Confirmed live
        // before this fix: `parse_iso("2024-01-01T")` returned
        // `Ok(1704067200)` -- the SAME timestamp as the genuinely valid
        // `parse_iso("2024-01-01")` -- instead of rejecting the malformed
        // input the way `parse_iso("2024-01-01T12")` (a partial, incomplete
        // time) already correctly does. An explicit separator PROMISES a
        // time component follows; an empty one after it is malformed, not a
        // date-only string in disguise.
        Some(i) if s[i + 1..].is_empty() => return Err(bad()),
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
    // A syntactically fine but astronomically large year (e.g. "99999999999999")
    // parses to a valid i64 `y`, but `make` can still fail to represent the
    // resulting timestamp (PR-it635) -- collapsed into the SAME "invalid
    // ISO-8601 timestamp" wording as every other validation failure here,
    // rather than leaking `make`'s own internal message.
    make(y, mo, d, hh, mi, ss).map_err(|_| bad())
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

    /// A coverage-closing test, per production-hardening PR-it621's convention
    /// (a fix/behavior can be REASONED correct without a test exercising that
    /// exact shape -- close the gap with a cheap permanent test even when no
    /// bug is found). `leap_year_boundary` above only covers POSITIVE years
    /// (2000, 1900); no existing test exercised a NEGATIVE (proleptic-
    /// Gregorian) year's leap-year boundary, despite this module's own header
    /// comment naming "the full i64 range including negative timestamps" as a
    /// design goal. `is_leap`'s `== 0` divisibility checks are sign-invariant
    /// under Rust's `%` (truncating) convention, so that part was never
    /// buggy -- but the day-count CONVERSION itself was (see
    /// `negative_era_day_count_matches_the_400_year_periodicity_invariant`
    /// below, PR-it682): these golden values were originally computed against
    /// the pre-it682-fix binary and have been refreshed to the CORRECT values
    /// now that `days_from_civil`/`civil_from_days`'s era computation is
    /// fixed. PR-it681, values corrected PR-it682.
    #[test]
    fn negative_year_leap_year_boundary() {
        // -400 (400 BCE-equivalent) is a leap year: divisible by 400.
        assert_eq!(parse_iso("-0400-02-29"), Ok(-74_784_902_400));
        // -100 is NOT a leap year: divisible by 100 but not by 400.
        assert!(parse_iso("-0100-02-29").is_err());
        assert_eq!(parse_iso("-0100-02-28"), Ok(-65_317_881_600));
        // -4 IS a leap year: divisible by 4, not by 100.
        assert_eq!(parse_iso("-0004-02-29"), Ok(-62_288_352_000));
        // -3 is NOT a leap year: not divisible by 4 at all.
        assert!(parse_iso("-0003-02-29").is_err());
        // every accepted date round-trips back through `iso`.
        for s in ["-0400-02-29", "-0004-02-29"] {
            let t = parse_iso(s).unwrap();
            assert_eq!(iso(t), format!("{s}T00:00:00Z"));
        }
    }

    /// THE BUG (production-hardening PR-it682, MAJOR): `days_from_civil`,
    /// `days_from_civil_checked`, and `civil_from_days` all computed their
    /// `era` via `floor_div(offset_y, 400)` (or `/146_097` for the day-based
    /// inverse) -- but Howard Hinnant's original algorithm's `-399`/`-146_096`
    /// offset is ITSELF the negative-number correction, specifically designed
    /// so a plain TRUNCATING division on the offset value equals floor
    /// division on the un-offset value. Wrapping that already-corrected value
    /// in a genuine floor-division function applied the correction a SECOND
    /// time, off-by-one-ing `era` (and therefore the returned day count / civil
    /// date) for every negative-era year that wasn't an exact multiple of 400
    /// (where truncating and floor division happen to coincide). Every
    /// existing test before this fix only ever exercised `era == 0` (the
    /// `z >= 0`/`y >= 0` branch, where floor vs. truncating division never
    /// differs) -- negative-era dates (roughly year 0 and earlier) were
    /// entirely untested. This is a hard mathematical fact independent of any
    /// specific date algorithm: the Gregorian calendar repeats EXACTLY every
    /// 400 years (146_097 days, since 146_097 / 7 = 20_871 exactly) -- so
    /// `days_from_civil(y, m, d)` and `days_from_civil(y + 400, m, d)` must
    /// differ by EXACTLY 146_097 for ANY `y`, positive or negative. Confirmed
    /// live before the fix: the pre-fix binary gave 146_098 for `y = -4`,
    /// `-100`, `-3` (all off by exactly one day) but the CORRECT 146_097 for
    /// `y = -400` (the one case where the double-correction happened to land
    /// on an exact multiple, coincidentally masking the bug).
    #[test]
    fn negative_era_day_count_matches_the_400_year_periodicity_invariant() {
        for (y, m, d) in [(-4, 1, 1), (-4, 12, 31), (-100, 2, 28), (-3, 12, 31), (-799, 6, 15)] {
            let a = days_from_civil(y, m, d);
            let b = days_from_civil(y + 400, m, d);
            assert_eq!(b - a, 146_097, "periodicity broke for {y}-{m:02}-{d:02}");
        }
        // the same invariant holds through `civil_from_days`'s inverse
        // direction: converting a day count back to civil components 400
        // years apart must land on the identical month/day, year+400.
        for days in [-865_566i64, -755_994, -720_930, -1_200_000] {
            let (y, m, d) = civil_from_days(days);
            let (y2, m2, d2) = civil_from_days(days + 146_097);
            assert_eq!((y + 400, m, d), (y2, m2, d2), "periodicity broke for day {days}");
        }
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
        assert_eq!(make(1970, 1, 1, 0, 0, 0), Ok(0));
        assert_eq!(make(2001, 9, 9, 1, 46, 40), Ok(1_000_000_000));
        assert_eq!(make(2000, 2, 29, 0, 0, 0), Ok(951_782_400));
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
        assert_eq!(yearday_of(make(2000, 12, 31, 0, 0, 0).unwrap()), 366); // leap year
        assert_eq!(yearday_of(make(2001, 12, 31, 0, 0, 0).unwrap()), 365);
        assert_eq!(yearday_of(make(2023, 3, 1, 0, 0, 0).unwrap()), 60); // 31+28+1
    }

    /// A REAL bug found+fixed (production-hardening PR-it635): `date_make`
    /// with an extreme component (e.g. `i64::MAX` as the year OR the month)
    /// used to overflow `days_from_civil`'s raw i64 arithmetic -- a Rust-level
    /// panic (crashing the WHOLE process with an "internal compiler error" in
    /// a debug build, confirmed live) rather than a clean `Err`. `make` itself
    /// has no natural upper bound on inputs the way an epoch-derived civil
    /// date does, so this is reachable through entirely ordinary, type-correct
    /// KUPL code (`date_make(9223372036854775807, ...)`), not just internal
    /// misuse. Now a clean `Err`, never a crash -- and an ordinary "reasonable
    /// rollover" (month 13) still normalizes exactly as before, unaffected.
    #[test]
    fn make_rejects_components_too_extreme_to_represent_instead_of_overflowing() {
        assert!(make(i64::MAX, 1, 1, 0, 0, 0).is_err());
        assert!(make(i64::MIN, 1, 1, 0, 0, 0).is_err());
        assert!(make(2024, i64::MAX, 1, 0, 0, 0).is_err());
        assert!(make(2024, 1, i64::MAX, 0, 0, 0).is_err());
        assert!(make(2024, 1, 1, i64::MAX, 0, 0).is_err());
        // an ordinary, moderate "rollover" out-of-range value is UNAFFECTED --
        // month 13 still normalizes into the next year, exactly as documented.
        assert_eq!(make(2024, 13, 1, 0, 0, 0), make(2025, 1, 1, 0, 0, 0));
    }

    /// A REAL bug found+fixed (production-hardening PR-it868, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// `days_from_civil_checked`'s single-year shift (the classic Howard
    /// Hinnant month-normalization) only correctly compensates for a month
    /// within ONE year of the valid `[1, 12]` range -- exactly matching
    /// `make_rejects_components_too_extreme_to_represent_instead_of_
    /// overflowing`'s own `month = 13` coverage above, but a month requiring
    /// a 2+ year shift silently produced a WRONG date with no error, even
    /// though this function's own doc comment promises normalization for
    /// ANY out-of-range month. Confirmed live BEFORE this fix, byte-
    /// identical wrong on all three engines: `date_make(2024, 25, 1, 0, 0,
    /// 0)` (month 25 of 2024 is month 1 TWO years later) returned a
    /// timestamp 2 DAYS off from `date_make(2026, 1, 1, 0, 0, 0)`'s correct
    /// value.
    #[test]
    fn make_normalizes_a_month_requiring_a_multi_year_shift() {
        // positive overflow, 2 years (month 25 -> month 1, +2 years).
        assert_eq!(make(2024, 25, 1, 0, 0, 0), make(2026, 1, 1, 0, 0, 0));
        // positive overflow, 3 years, non-January remainder (month 37 = 12*3+1).
        assert_eq!(make(2024, 37, 1, 0, 0, 0), make(2027, 1, 1, 0, 0, 0));
        // negative overflow, 1 year (month -11 -> month 1, -1 year).
        assert_eq!(make(2024, -11, 1, 0, 0, 0), make(2023, 1, 1, 0, 0, 0));
        // negative overflow, 2 years, with a non-trivial day.
        assert_eq!(make(2024, -23, 15, 0, 0, 0), make(2022, 1, 15, 0, 0, 0));
        // the existing single-year case (month 13) is unaffected by this fix.
        assert_eq!(make(2024, 13, 1, 0, 0, 0), make(2025, 1, 1, 0, 0, 0));
    }

    /// The SAME overflow, reached via `parse_iso` on a year string that's
    /// perfectly valid i64-parseable text (so it's NOT rejected by the
    /// existing `i64::parse()`-failure path -- that's a DIFFERENT, already-
    /// fixed gap, PR-it560's `strtol` clamping fix, for years with too many
    /// DIGITS to even parse) but still astronomically too large for the
    /// resulting timestamp to fit in i64. Collapses into the SAME "invalid
    /// ISO-8601 timestamp" error every other malformed input already gets,
    /// not a distinct internal message.
    #[test]
    fn parse_iso_rejects_a_syntactically_valid_but_unrepresentable_year() {
        let err = parse_iso("99999999999999-01-01").unwrap_err();
        assert!(err.contains("invalid ISO-8601 timestamp"), "{err}");
        assert!(err.contains("99999999999999"), "{err}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it880, the THIRD sibling
    /// instance of this function's "forgot one input shape in the validation
    /// chain" pattern -- it111 leap-year-aware day validation, it635 collapsing
    /// an unrepresentable-year internal error into this SAME generic wording):
    /// an explicit `T`/space separator with NOTHING after it used to produce an
    /// EMPTY `time` slice, indistinguishable from the date-only case (no
    /// separator at all), so the `!time.is_empty()` guard silently skipped ALL
    /// time validation and defaulted to midnight -- confirmed live before this
    /// fix: `parse_iso("2024-01-01T")` returned the SAME `Ok` timestamp as the
    /// genuinely valid `parse_iso("2024-01-01")`, unlike `parse_iso(
    /// "2024-01-01T12")` (a partial, incomplete time), which was ALREADY
    /// correctly rejected.
    #[test]
    fn parse_iso_rejects_a_trailing_separator_with_nothing_after_it() {
        let err = parse_iso("2024-01-01T").unwrap_err();
        assert!(err.contains("invalid ISO-8601 timestamp"), "{err}");
        // trimmed whitespace/`Z` after the bare `T` reduce to the SAME shape.
        assert!(parse_iso("2024-01-01T   ").is_err());
        assert!(parse_iso("2024-01-01TZ").is_err());
        // genuinely well-formed neighbors are all completely unaffected.
        assert_eq!(parse_iso("2024-01-01").unwrap(), parse_iso("2024-01-01T00:00:00").unwrap());
        assert!(parse_iso("2024-01-01T12").is_err(), "a partial time must still be rejected");
        assert!(parse_iso("2024-01-01T00:00:00").is_ok());
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
