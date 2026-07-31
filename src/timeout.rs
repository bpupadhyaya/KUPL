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
pub fn parse_flag(args: &[String]) -> Result<Option<u64>, String> {
    for a in args {
        if let Some(v) = a.strip_prefix("--timeout=") {
            return match v.parse::<u64>() {
                Ok(0) => Err("--timeout value must be greater than 0".to_string()),
                Ok(n) => Ok(Some(n)),
                Err(_) => {
                    Err(format!("invalid --timeout value `{v}` (expected a positive whole number of seconds)"))
                }
            };
        }
    }
    Ok(None)
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
}
