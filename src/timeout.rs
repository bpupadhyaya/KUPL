//! An operational, opt-in wall-clock execution limit for `kupl run`/`kupl run
//! --vm`, via `--timeout=<seconds>`. This is a CLI-level safety net, not a
//! language feature or an in-engine construct: a watchdog thread hard-kills
//! the whole process after the deadline, uniformly covering both CPU-bound
//! loops and blocking I/O, matching `docs/PRODUCTION.md`'s own framing of
//! resource limits ("run untrusted code inside an OS-level sandbox" remains
//! the recommendation; this only bounds well-behaved-but-runaway programs).
//!
//! The kill is a hard `std::process::exit`, so it skips `on stop`/graceful
//! component shutdown — already-flushed stdout survives (both engines flush
//! per `print` call), matching what an external `kill`/`timeout(1)` would do.

/// Parse a `--timeout=<seconds>` flag. Returns `Ok(None)` if absent,
/// `Ok(Some(secs))` if present and a valid positive integer, `Err(msg)` if
/// present but malformed (a non-numeric or zero value).
///
/// Production-hardening 1229: a REAL, live-confirmed bug found+fixed --
/// this used to `return` on the FIRST occurrence, silently discarding a
/// REPEATED `--timeout=` the same way `run.rs::native`'s `-o` flag did
/// before PR-it999's own fix (see that fix's own doc comment for the
/// established precedent this mirrors). Live-confirmed before this fix:
/// `kupl run runaway.kupl --timeout=60 --timeout=2` silently honored the
/// FIRST value (60s) — a runaway `while true {}` program was still running
/// at 99.7% CPU 22+ seconds in, well past the intended 2s safety limit,
/// with zero diagnostic. A safety-net flag silently using the WRONG
/// (more permissive) of two conflicting values, with no indication either
/// was ever discarded, is exactly the class of CLI robustness gap this
/// campaign's own established `-o`-duplicate precedent already treats as
/// worth a hard, explicit rejection rather than a silent pick. Collects
/// EVERY occurrence first (position-based duplicate detection, matching
/// `-o`'s own convention of flagging a repeat regardless of either
/// value's own validity) before parsing the single remaining one.
pub fn parse_flag(args: &[String]) -> Result<Option<u64>, String> {
    let matches: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix("--timeout=")).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [v] => match v.parse::<u64>() {
            Ok(0) => Err("--timeout value must be greater than 0".to_string()),
            Ok(n) => Ok(Some(n)),
            Err(_) => Err(format!("invalid --timeout value `{v}` (expected a positive whole number of seconds)")),
        },
        _ => Err("--timeout specified more than once".to_string()),
    }
}

/// Spawn the watchdog thread: after `secs` seconds, print a clean diagnostic
/// and terminate the process with exit code 124 (matching the coreutils
/// `timeout(1)` convention, distinct from this project's own panic exit code
/// 101 — see `docs/reference/DIAGNOSTICS.md`).
pub fn arm(secs: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        eprintln!("error[K0901]: execution timed out after {secs}s");
        std::process::exit(124);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flag_absent_is_none() {
        assert_eq!(parse_flag(&["run".to_string(), "foo.kupl".to_string()]), Ok(None));
    }

    #[test]
    fn parse_flag_accepts_a_positive_integer() {
        assert_eq!(
            parse_flag(&["run".to_string(), "--timeout=5".to_string(), "foo.kupl".to_string()]),
            Ok(Some(5))
        );
    }

    #[test]
    fn parse_flag_rejects_zero() {
        assert!(parse_flag(&["run".to_string(), "--timeout=0".to_string()]).is_err());
    }

    #[test]
    fn parse_flag_rejects_non_numeric() {
        assert!(parse_flag(&["run".to_string(), "--timeout=soon".to_string()]).is_err());
    }

    #[test]
    fn parse_flag_rejects_negative() {
        assert!(parse_flag(&["run".to_string(), "--timeout=-5".to_string()]).is_err());
    }

    /// Production-hardening 1229: a REAL, live-confirmed bug -- a REPEATED
    /// `--timeout=` used to silently honor the FIRST occurrence, discarding
    /// the second with zero diagnostic. Live-confirmed before this fix:
    /// `kupl run runaway.kupl --timeout=60 --timeout=2` silently used 60s
    /// (a runaway `while true {}` still running well past the intended 2s
    /// safety limit). Rejected regardless of either value's own validity,
    /// matching `-o`'s own established duplicate-detection precedent
    /// (PR-it999) -- both a repeated VALID value and a repeated malformed
    /// one must be refused the same way.
    #[test]
    fn parse_flag_rejects_a_repeated_flag_instead_of_silently_honoring_the_first() {
        let err = parse_flag(&["run".to_string(), "--timeout=60".to_string(), "--timeout=2".to_string()])
            .expect_err("a repeated --timeout must be refused, not silently resolved to the first value");
        assert!(err.contains("more than once"), "{err}");
    }
}
