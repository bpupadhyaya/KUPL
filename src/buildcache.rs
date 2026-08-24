//! A content-addressed, opt-in-by-default build cache for `kupl build`/
//! `bundle`/`native` — closes the "no incremental or persistent compilation
//! cache" gap named in `docs/PRODUCTION.md`'s Known Limitations.
//!
//! Scope, deliberately: this caches the FINAL ARTIFACT bytes for the three
//! commands whose entire job is producing a persisted output file from
//! source (`.kx` bytecode, a bundled executable, or a native machine-code
//! binary) — not `kupl run`/`kupl run --vm`. Those two need the CHECKED
//! `Program` (`ProgramDb`/`ProgramImage`, for the `par_map`/`par_filter`
//! real-thread fast path and `concurrent component` support), which has no
//! stable serialized form today; caching only the compiled bytecode
//! wouldn't let them skip parsing/checking at all, so there's no real
//! speedup to offer there without a much larger, riskier change to how
//! those two source-execution paths work. `build`/`bundle`/`native`, by
//! contrast, need NOTHING beyond the final bytes on a cache hit — the
//! genuinely safe, high-value target.
//!
//! Cache key = a namespace tag (so `build`/`bundle`/`native` never collide
//! with each other even for byte-identical source) + the running `kupl`
//! BINARY's own content hash (`self_hash`, below — NOT just
//! `CARGO_PKG_VERSION`: this project's crate version does not bump per
//! internal change, and a genuine `compile_module`/`emit_c`/`cgen.rs`
//! change must still invalidate every prior entry, or a stale cache hit
//! could silently mask a real compiler regression behind old output) +
//! caller-supplied `extra` bytes (e.g. `native`'s `cc` identifier — a
//! different C compiler, or an upgraded one at the same path, can produce
//! different machine code for identical generated C) + every resolved
//! source file's path and content, length-prefixed (not NUL-delimited: a
//! `.kupl` file's comment can legally contain a literal NUL byte and still
//! be valid UTF-8 text, which `loader::SourceMap` accepts, so a
//! NUL-delimited encoding would have a real, if exotic, key-collision
//! risk; length-prefixing has none, regardless of file content). Hashed
//! with `sha256_hex`/`sha256_hex_bytes` — this is a LOCAL, single-machine
//! cache key, not a security boundary, but reusing the already-shared
//! cryptographic hash avoids introducing a second algorithm for no reason.

use std::path::PathBuf;
use std::sync::OnceLock;

/// `~/.kupl/build-cache/` — `None` when neither `$HOME` nor
/// `$USERPROFILE` is set (production-hardening 1217, a REAL gap found by
/// code-reading, matching the SAME "when identity/location can't be
/// safely determined, don't cache" precedent `self_hash`'s own `None`
/// return already established at PR-it1215). The OLD behavior fell back
/// to `std::env::temp_dir()` (typically the world-writable, SHARED `/tmp`
/// on Unix) — `registry::cache_dir()` has the IDENTICAL fallback and was
/// deliberately left unchanged here (see this function's own callers'
/// doc comments for why: `registry.rs`'s own consumer, `fetch_package`,
/// re-verifies every file's hash against a FRESH network fetch on every
/// call regardless of cache state — "v1 always re-fetches and
/// re-verifies, no cache-skip" is that module's own long-established
/// design — so a poisoned entry there is never blindly trusted the way a
/// build-cache HIT is here). A predictable, shared `/tmp/.kupl/
/// build-cache/` path is a real, if narrow, local cache-poisoning
/// precondition on a multi-user system or a `$HOME`-less service
/// account: whichever process wins the race to create it first (or can
/// simply write into it, since nothing enforces per-user isolation
/// there) can plant an entry a LATER, unrelated process would trust and
/// execute — `lookup`'s own `store`d bytes are used directly, with no
/// re-verification step, unlike `registry.rs`'s. `None` here means
/// `lookup`/`store` both silently no-op, so an unset `$HOME` degrades to
/// "always recompile, never persist a cache entry" rather than ever
/// touching a shared, predictable location at all.
pub fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".kupl").join("build-cache"))
}

fn feed(buf: &mut String, s: &str) {
    buf.push_str(&s.len().to_string());
    buf.push(':');
    buf.push_str(s);
}

/// SHA-256 of the CURRENTLY RUNNING `kupl` binary's own bytes, computed
/// once per process (`OnceLock`) and reused by every `content_key` call in
/// this invocation. Deliberately re-derived from `current_exe()` rather
/// than trusting `CARGO_PKG_VERSION` alone: this project's own commit
/// history is full of `compile_module`/`emit_c`/`cgen.rs` changes that
/// land on the SAME crate version (`1.0.0-alpha` throughout) — a cache key
/// that only changed on a version bump would let a stale entry from BEFORE
/// such a change silently survive an in-place `cargo build`, serving OLD
/// compiled output under NEW compiler logic with no diagnostic at all.
/// Tying the key to the binary's actual content means any rebuild that
/// changes what the compiler DOES necessarily changes this hash too.
///
/// `None` on a `current_exe()`/read failure (implausible for an
/// already-running process, but not impossible under an unusual sandbox —
/// production-hardening 1215, a REAL gap found+fixed: an earlier version of
/// this function degraded to a FIXED, shared `sha256_hex_bytes(&[])` hash
/// in this case instead, directly contradicting its own "impossible by
/// construction" claim -- confirmed live: two GENUINELY DIFFERENT `kupl`
/// binaries, each unable to read its own `current_exe()`, both collapsed to
/// the identical fallback hash, so a cache entry stored under binary A's
/// (fake, shared) identity was served right back to binary B, a compiler
/// with potentially different `compile_module`/`emit_c` logic — a real,
/// if narrow, cache-poisoning-by-coincidence risk this cache's own design
/// exists to prevent). `content_key` propagates this `None` outward, and
/// every caller treats it as "do not use the cache for this invocation at
/// all" (skip both lookup and store) — degrading to "just always
/// recompile" is always safe; degrading to a shared placeholder identity
/// is not.
fn self_hash() -> Option<&'static str> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| {
        std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()).map(|b| crate::encoding::sha256_hex_bytes(&b))
    })
    .as_deref()
}

/// `true` for a regular, executable file — `is_file()` alone would also
/// match a non-executable file that happens to share `cc`'s name earlier
/// in `$PATH`, which the OS's own `execvp`-style resolution would never
/// actually select.
#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

/// SHA-256 of the ACTUAL `cc` executable's own bytes (production-hardening
/// 1219, the LAST item from PR-it1212's own buildcache queue) — mirrors
/// `self_hash()`'s exact reasoning, applied to the C compiler instead of
/// the `kupl` binary itself: `native`'s cache key used to fold in only
/// `cc`'s STRING identifier (e.g. `"cc"`, from `$CC` or the default), so
/// an in-place `cc` upgrade at the same path (a realistic package-manager-
/// driven scenario — `apt upgrade`, `brew upgrade llvm`, a toolchain
/// container rebuild) silently served a stale machine-code artifact from
/// the OLD compiler, with no diagnostic. Resolves `cc` exactly the way
/// `std::process::Command::new(cc)` itself would: used directly if it
/// already contains a path separator, otherwise searched across `$PATH`
/// for the first executable regular file with that name — the SAME
/// resolution order the OS's own `execvp` uses, so this hashes whichever
/// binary would ACTUALLY run. `None` on any resolution or read failure
/// (a `$CC` naming a nonexistent program, a `$PATH`-less environment, a
/// permissions issue) — per this module's own established "when identity
/// can't be safely determined, don't cache" precedent (`self_hash`,
/// `cache_dir`), never a degraded fallback like trusting the bare string
/// alone would be.
pub fn cc_hash(cc: &str) -> Option<String> {
    let path: PathBuf = if cc.contains(std::path::MAIN_SEPARATOR) {
        PathBuf::from(cc)
    } else {
        let path_var = std::env::var_os("PATH")?;
        std::env::split_paths(&path_var).map(|dir| dir.join(cc)).find(|p| is_executable_file(p))?
    };
    let bytes = std::fs::read(&path).ok()?;
    Some(crate::encoding::sha256_hex_bytes(&bytes))
}

/// Build the cache key for one compile. `extra` lets a caller fold in
/// anything beyond source content and the running compiler's own identity
/// that can change the OUTPUT bytes for the same source (see the module
/// doc comment above for why `native`'s `cc` identifier needs this).
/// Returns `None` when the running binary's own identity can't be
/// determined (see `self_hash`'s own doc comment) — the caller must treat
/// this as "skip the cache entirely for this invocation."
pub fn content_key(namespace: &str, map: &crate::loader::SourceMap, extra: &[u8]) -> Option<String> {
    let self_hash = self_hash()?;
    let mut buf = String::new();
    feed(&mut buf, namespace);
    feed(&mut buf, self_hash);
    // Production-hardening 1216: a REAL, live-confirmed-by-code-reading
    // landmine, not yet triggerable but a real footgun for the next
    // caller -- `extra` used to be fed via `String::from_utf8_lossy`,
    // directly contradicting `sha256_hex_bytes`'s own doc comment one
    // field above (`self_hash`), which exists SPECIFICALLY because a
    // lossy UTF-8 round-trip is not injective (two different byte
    // sequences can decode to the identical replacement-character
    // string, and therefore hash the same). Both of TODAY's callers
    // (`b""`, and `native`'s `cc.as_bytes()`, always plain ASCII) never
    // actually hit this, but a future caller passing genuinely non-UTF-8
    // `extra` would silently risk a real key collision. Hashing `extra`
    // first (rather than feeding its raw bytes as a length-prefixed
    // string, which would ALSO work but needlessly duplicates
    // `feed`'s own string-only signature) sidesteps the whole class: a
    // SHA-256 digest is always valid ASCII, fixed-length, and -- unlike
    // a lossy decode -- collision-resistant regardless of what `extra`
    // itself contains.
    feed(&mut buf, &crate::encoding::sha256_hex_bytes(extra));
    for f in &map.files {
        feed(&mut buf, &f.path);
        feed(&mut buf, &f.src);
    }
    Some(crate::encoding::sha256_hex(&buf))
}

/// Look up a cached artifact by key. `None` on any miss, read error, OR an
/// undeterminable cache location (`cache_dir()` returning `None` — see its
/// own doc comment) — all three degrade identically: always safe to just
/// recompile, never a hard failure.
pub fn lookup(key: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_dir()?.join(key)).ok()
}

/// Store a freshly-compiled artifact under `key`. Best-effort: a failure to
/// write the cache (permissions, disk full, an undeterminable cache
/// location) is silently ignored — the artifact was already successfully
/// written to the CALLER's own requested output path by this point, so a
/// cache-write failure must never turn a successful build into a reported
/// error. Uses the SAME atomic write-to-temp-then-rename `loader::
/// write_atomically` every other persistent-artifact write in this
/// codebase already uses, so a reader racing a concurrent cache fill never
/// observes a torn/partial entry.
pub fn store(key: &str, bytes: &[u8]) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = crate::loader::write_atomically(&dir.join(key), bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{SourceFile, SourceMap};

    fn map(files: &[(&str, &str)]) -> SourceMap {
        let mut concat = String::new();
        let mut out = Vec::new();
        for (path, src) in files {
            let base = concat.len() as u32;
            concat.push_str(src);
            out.push(SourceFile { path: (*path).to_string(), src: (*src).to_string(), base });
        }
        SourceMap { files: out, concat }
    }

    #[test]
    fn content_key_is_stable_for_identical_input() {
        let m = map(&[("main.kupl", "fun main() {}\n")]);
        assert_eq!(content_key("build", &m, b""), content_key("build", &m, b""));
    }

    #[test]
    fn content_key_differs_across_namespaces_for_identical_source() {
        let m = map(&[("main.kupl", "fun main() {}\n")]);
        assert_ne!(
            content_key("build", &m, b""),
            content_key("native", &m, b""),
            "build vs native must never collide, even for byte-identical source"
        );
    }

    #[test]
    fn content_key_changes_when_any_file_content_changes() {
        let a = map(&[("main.kupl", "fun main() {}\n")]);
        let b = map(&[("main.kupl", "fun main() { print(1) }\n")]);
        assert_ne!(content_key("build", &a, b""), content_key("build", &b, b""));
    }

    #[test]
    fn content_key_changes_when_extra_bytes_change() {
        let m = map(&[("main.kupl", "fun main() {}\n")]);
        assert_ne!(
            content_key("native", &m, b"cc"),
            content_key("native", &m, b"clang"),
            "a different cc identity must produce a different key"
        );
    }

    /// Production-hardening 1216: the exact landmine a lossy-UTF8-decoded
    /// `extra` would have hit -- `0xFF` and `0xFE` are each individually
    /// invalid UTF-8 (never a valid leading byte), so `String::from_utf8_
    /// lossy` maps BOTH to the identical single U+FFFD replacement
    /// character. Before this fix, `content_key(_, _, &[0xFF])` and
    /// `content_key(_, _, &[0xFE])` would have collided; hashing `extra`
    /// via `sha256_hex_bytes` first means they must not.
    #[test]
    fn content_key_does_not_collide_for_different_non_utf8_extra_bytes() {
        assert_eq!(
            String::from_utf8_lossy(&[0xFF]),
            String::from_utf8_lossy(&[0xFE]),
            "sanity check on the premise: both must lossy-decode to the SAME replacement char"
        );
        let m = map(&[("main.kupl", "fun main() {}\n")]);
        assert_ne!(
            content_key("native", &m, &[0xFF]),
            content_key("native", &m, &[0xFE]),
            "two different non-UTF-8 extra byte sequences must never collide, even if they'd lossy-decode identically"
        );
    }

    /// Production-hardening 1215: `content_key`/`self_hash` return `Option`
    /// now (a `current_exe()`/read failure means "don't cache this
    /// invocation," never a degraded shared identity) -- the ordinary case,
    /// running as an actual test binary that can read its own executable,
    /// must still return `Some` with a stable value, not silently degrade
    /// to `None` for everyday use.
    #[test]
    fn content_key_is_some_in_the_ordinary_case() {
        let m = map(&[("main.kupl", "fun main() {}\n")]);
        let key = content_key("build", &m, b"");
        assert!(key.is_some(), "an ordinary test binary must be able to read its own current_exe()");
        assert_eq!(key, content_key("build", &m, b""), "must still be stable across repeated calls");
    }

    /// The exact class of ambiguity a NUL-delimited (rather than
    /// length-prefixed) encoding would be vulnerable to: two DIFFERENT
    /// two-file source sets that would concatenate to the identical raw
    /// bytes under a naive separator scheme must still hash differently.
    #[test]
    fn content_key_does_not_collide_across_a_boundary_shift() {
        let a = map(&[("ab", "cd"), ("e", "f")]);
        let b = map(&[("a", "bcd"), ("e", "f")]);
        assert_ne!(content_key("build", &a, b""), content_key("build", &b, b""));
    }

    #[test]
    fn lookup_is_none_for_an_unknown_key() {
        assert_eq!(lookup("kupl-buildcache-definitely-nonexistent-key"), None);
    }

    #[test]
    fn store_then_lookup_round_trips_the_exact_bytes() {
        let dir = std::env::temp_dir().join(format!("kupl-buildcache-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let real_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);
        let key = "roundtrip-test-key";
        store(key, b"hello cache");
        let got = lookup(key);
        match real_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(got, Some(b"hello cache".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Production-hardening 1217: with NEITHER `$HOME` nor `$USERPROFILE`
    /// set, `cache_dir()` must return `None` (never fall back to the
    /// shared, unnamespaced `std::env::temp_dir()`) -- and `lookup`/
    /// `store` must degrade to a safe no-op rather than touching a
    /// predictable, shared location at all. Manipulates both env vars
    /// directly (the SAME technique `store_then_lookup_round_trips_...`
    /// above already uses for `HOME` alone), restoring both immediately
    /// after the one call that needs them, before any assertion.
    #[test]
    fn cache_dir_and_lookup_and_store_are_all_safe_no_ops_without_home_or_userprofile() {
        let real_home = std::env::var("HOME").ok();
        let real_userprofile = std::env::var("USERPROFILE").ok();
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");

        let dir_is_none = cache_dir().is_none();
        // Must not panic, must not write anywhere -- and must report a
        // clean miss, exactly like an ordinary cache miss would.
        store("home-unset-test-key", b"should never be persisted");
        let got = lookup("home-unset-test-key");

        match real_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match real_userprofile {
            Some(u) => std::env::set_var("USERPROFILE", u),
            None => std::env::remove_var("USERPROFILE"),
        }

        assert!(dir_is_none, "cache_dir() must be None with neither HOME nor USERPROFILE set");
        assert_eq!(got, None, "lookup must report a clean miss, never panic or read from a shared fallback location");
    }

    // Production-hardening 1219: `cc_hash` tests. Mirrors `cgen.rs`'s own
    // established "self-skip when cc isn't available" convention (see
    // `cgen.rs`'s `cc_available` test helper) rather than assuming a C
    // compiler exists in every environment this suite runs in. Deliberately
    // does NOT mutate `$PATH` (unlike the HOME/USERPROFILE tests above) --
    // `$PATH` is read by many OTHER tests in this same process (any test
    // that spawns `cc`), and this test binary's tests run concurrently by
    // default, so a global `$PATH` removal here would risk spuriously
    // breaking an unrelated in-flight subprocess spawn elsewhere.

    fn cc() -> String {
        std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
    }
    fn cc_available() -> bool {
        std::process::Command::new(cc()).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    #[test]
    fn cc_hash_is_some_for_a_resolvable_command() {
        if !cc_available() {
            return;
        }
        assert!(cc_hash(&cc()).is_some(), "a resolvable cc must hash to Some");
    }

    #[test]
    fn cc_hash_is_none_for_an_unresolvable_command() {
        assert_eq!(
            cc_hash("kupl-definitely-not-a-real-compiler-binary-xyz"),
            None,
            "a command name that resolves nowhere on $PATH must hash to None, never a placeholder"
        );
    }

    #[test]
    fn cc_hash_is_stable_across_repeated_calls() {
        if !cc_available() {
            return;
        }
        assert_eq!(cc_hash(&cc()), cc_hash(&cc()));
    }

    /// A `cc` string containing a path separator (e.g. `$CC=/usr/bin/clang`)
    /// must be used DIRECTLY, never searched for on `$PATH` -- the exact
    /// resolution order `std::process::Command::new`/`execvp` themselves
    /// use. Hashing the CURRENTLY RUNNING TEST BINARY's own path (which
    /// always exists and always contains a separator) and comparing
    /// against `self_hash()` -- which independently hashes the same
    /// `current_exe()` file -- confirms both that the separator branch is
    /// actually taken (no `$PATH` search) and that it reads the right
    /// file's content.
    #[test]
    fn cc_hash_treats_a_path_containing_string_as_a_direct_path_not_a_path_search() {
        let exe = std::env::current_exe().expect("test binary must be able to find its own path");
        let exe_str = exe.to_str().expect("test binary path must be valid UTF-8 in this environment");
        assert!(exe_str.contains(std::path::MAIN_SEPARATOR), "sanity check on the premise: must actually contain a separator");
        assert_eq!(
            cc_hash(exe_str),
            self_hash().map(|s| s.to_string()),
            "a direct path must hash the same file self_hash() independently hashes via current_exe()"
        );
    }

    #[test]
    fn cc_hash_differs_for_two_genuinely_different_executables() {
        if !cc_available() {
            return;
        }
        let exe = std::env::current_exe().expect("test binary must be able to find its own path");
        let exe_str = exe.to_str().expect("test binary path must be valid UTF-8 in this environment");
        assert_ne!(cc_hash(exe_str), cc_hash(&cc()), "two different binaries must not collide to the same hash");
    }
}
