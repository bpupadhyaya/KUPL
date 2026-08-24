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

/// `~/.kupl/build-cache/` — mirrors `registry::cache_dir()`'s own
/// `~/.kupl/registry-cache/` convention exactly (same `$HOME`/
/// `$USERPROFILE`-with-temp-dir-fallback resolution).
pub fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    home.join(".kupl").join("build-cache")
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
/// changes what the compiler DOES necessarily changes this hash too. A
/// `current_exe()`/read failure (implausible for an already-running
/// process, but not impossible under an unusual sandbox) degrades to an
/// empty-bytes hash rather than panicking — worst case, every entry from
/// that run shares one cache namespace slice, never a correctness issue,
/// since a WRONG-content cache hit is still impossible by construction
/// (the key also covers full source content).
fn self_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let bytes = std::env::current_exe().and_then(std::fs::read).unwrap_or_default();
        crate::encoding::sha256_hex_bytes(&bytes)
    })
}

/// Build the cache key for one compile. `extra` lets a caller fold in
/// anything beyond source content and the running compiler's own identity
/// that can change the OUTPUT bytes for the same source (see the module
/// doc comment above for why `native`'s `cc` identifier needs this).
pub fn content_key(namespace: &str, map: &crate::loader::SourceMap, extra: &[u8]) -> String {
    let mut buf = String::new();
    feed(&mut buf, namespace);
    feed(&mut buf, self_hash());
    feed(&mut buf, &String::from_utf8_lossy(extra));
    for f in &map.files {
        feed(&mut buf, &f.path);
        feed(&mut buf, &f.src);
    }
    crate::encoding::sha256_hex(&buf)
}

/// Look up a cached artifact by key. `None` on any miss OR read error (a
/// corrupted/partially-written cache entry is treated exactly like a
/// miss — always safe to just recompile, never a hard failure).
pub fn lookup(key: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_dir().join(key)).ok()
}

/// Store a freshly-compiled artifact under `key`. Best-effort: a failure to
/// write the cache (permissions, disk full, ...) is silently ignored — the
/// artifact was already successfully written to the CALLER's own requested
/// output path by this point, so a cache-write failure must never turn a
/// successful build into a reported error. Uses the SAME atomic
/// write-to-temp-then-rename `loader::write_atomically` every other
/// persistent-artifact write in this codebase already uses, so a reader
/// racing a concurrent cache fill never observes a torn/partial entry.
pub fn store(key: &str, bytes: &[u8]) {
    let dir = cache_dir();
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
}
