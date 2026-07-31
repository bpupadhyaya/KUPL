//! An operational, opt-in total-memory cap for `kupl run`/`kupl run --vm`,
//! via `--max-memory=<MB>`. Like `timeout.rs`, this is a CLI-level safety
//! net, not a language feature — see `docs/PRODUCTION.md`'s threat-model
//! section.
//!
//! [`CappedAlloc`] wraps the system allocator, tracking total allocated
//! bytes in a process-wide atomic counter. Because `interp.rs`, `vm.rs`, and
//! `kx.rs` all run inside the *same* Rust process, registering this as the
//! process's `#[global_allocator]` (done in `main.rs`, the binary crate only —
//! never here, so library embedders of `Interp`/`Vm` are never forced into
//! this) caps all three engines uniformly with zero per-engine code changes.
//! `kupl native`'s generated output is a *standalone* C executable that
//! never links this allocator at all — a memory cap there needs a wholly
//! separate mechanism (its own C-side accounting, or `ulimit -v`/cgroups);
//! that gap remains open, and is documented as such in
//! `docs/PRODUCTION.md`.
//!
//! Approximate under concurrent allocation (see [`CappedAlloc::alloc`]'s own
//! doc comment) — this is a soft safety net, not a hard security boundary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static CAP: AtomicUsize = AtomicUsize::new(usize::MAX);
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Set the process-wide allocation cap, in bytes. Call once, before any
/// KUPL program runs. Unset (the default) means unlimited.
pub fn set_cap_bytes(bytes: usize) {
    CAP.store(bytes, Ordering::Relaxed);
}

/// Parse a `--max-memory=<MB>` flag. Returns `Ok(None)` if absent,
/// `Ok(Some(bytes))` if present and a valid positive integer (converted from
/// MB to bytes), `Err(msg)` if present but malformed.
pub fn parse_flag(args: &[String]) -> Result<Option<usize>, String> {
    for a in args {
        if let Some(v) = a.strip_prefix("--max-memory=") {
            return match v.parse::<usize>() {
                Ok(0) => Err("--max-memory value must be greater than 0".to_string()),
                Ok(mb) => Ok(Some(mb.saturating_mul(1024 * 1024))),
                Err(_) => Err(format!(
                    "invalid --max-memory value `{v}` (expected a positive whole number of megabytes)"
                )),
            };
        }
    }
    Ok(None)
}

/// The capped global allocator. Register with `#[global_allocator]` in the
/// binary crate only (`main.rs`) — see this module's own doc comment.
pub struct CappedAlloc;

unsafe impl GlobalAlloc for CappedAlloc {
    /// Checked before delegating to the system allocator: an allocation that
    /// would push the running total past the configured cap is rejected
    /// (returns null) with a one-time `K0902` diagnostic on stderr, instead
    /// of silently succeeding. A null return triggers Rust's own
    /// `handle_alloc_error`, which aborts the process — unavoidable on
    /// stable Rust (there is no catchable `Result` from a failed
    /// allocation); the `eprintln!` here is what delivers the clean message
    /// before that abort.
    ///
    /// The check-then-add is not atomic as a single step (a `load` followed
    /// by a conditional `fetch_add`), so two threads racing near the cap
    /// could both pass the check and jointly overshoot it slightly. This is
    /// an accepted, documented approximation for a soft operational safety
    /// net, not a hard security boundary — the same framing already used
    /// for `--timeout`.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let cap = CAP.load(Ordering::Relaxed);
        if cap != usize::MAX && ALLOCATED.load(Ordering::Relaxed).saturating_add(size) > cap {
            if !REPORTED.swap(true, Ordering::Relaxed) {
                eprintln!("error[K0902]: memory limit of {cap} bytes exceeded -- aborting");
            }
            return std::ptr::null_mut();
        }
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            ALLOCATED.fetch_add(size, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flag_absent_is_none() {
        assert_eq!(parse_flag(&["run".to_string(), "foo.kupl".to_string()]), Ok(None));
    }

    #[test]
    fn parse_flag_converts_mb_to_bytes() {
        assert_eq!(
            parse_flag(&["run".to_string(), "--max-memory=16".to_string(), "foo.kupl".to_string()]),
            Ok(Some(16 * 1024 * 1024))
        );
    }

    #[test]
    fn parse_flag_rejects_zero() {
        assert!(parse_flag(&["run".to_string(), "--max-memory=0".to_string()]).is_err());
    }

    #[test]
    fn parse_flag_rejects_non_numeric() {
        assert!(parse_flag(&["run".to_string(), "--max-memory=lots".to_string()]).is_err());
    }

    #[test]
    fn parse_flag_rejects_negative() {
        assert!(parse_flag(&["run".to_string(), "--max-memory=-16".to_string()]).is_err());
    }
}
