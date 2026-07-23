//! Package registry index parsing (v1 design — see the production-hardening
//! campaign's design memo for the full scope: index format, cache layout,
//! security bar). A v1 registry is a static per-package JSON index at
//! `{registry_url}/{name}.json`, resolved by EXACT version match only — no
//! ranges, matching `loader.rs`'s existing "ranges are a future addition"
//! scope for local path dependencies.
//!
//! This module implements the parse/resolve/verify/materialize core, plus a
//! `fetch_package` orchestration layer on top of it (`curl`-based, matching
//! `interp.rs`'s HTTP builtins and `ai.rs`'s provider calls — the same zero-
//! dependency transport approach used everywhere else in this codebase).
//! The core (`parse_index`/`resolve_version`/`verify_hash`/`materialize`) is
//! pure and deterministic — no network or filesystem access of its own — so
//! it is fully unit-testable without a live registry; `fetch_package`'s own
//! tests inject a canned, in-memory fetcher (`fetch_package_with`) rather
//! than depending on live network access. Wiring this into a CLI subcommand
//! (`kupl pkg fetch`) is a deliberately separate, later slice.
//!
//! Index shape (one JSON document per package name):
//!
//! ```json
//! {
//!   "name": "json2",
//!   "versions": {
//!     "1.2.0": {
//!       "entry": "main.kupl",
//!       "files": {
//!         "kupl.toml": {"url": "https://example.com/json2/1.2.0/kupl.toml", "hash": "a1b2c3"},
//!         "main.kupl": {"url": "https://example.com/json2/1.2.0/main.kupl", "hash": "d4e5f6"}
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Every listed file carries an FNV-1a hex `hash` — the SAME primitive
//! `loader::ResolvedDep` already uses for local-dependency drift detection
//! (`encoding::hash_fnv`/`hex_encode`) — which a fetched file's bytes MUST
//! match before the package is ever used. Hash verification is mandatory,
//! never optional or silently skipped: a mismatch is always a hard error.

use std::collections::HashMap;

use crate::lsp::{parse_json, Json};

/// The v1 registry URL: a single, hardcoded location, not a `--registry`
/// flag or environment-variable override. Deliberate, per the design memo's
/// security bar (production-hardening PR-it630): letting a per-invocation
/// value substitute an arbitrary registry is a supply-chain risk surface
/// (a compromised build script or CI variable could silently redirect every
/// fetch to an attacker-controlled index) — deferred to a v2 that doesn't
/// exist yet. No live service is deployed at this address yet; `kupl pkg
/// fetch` against a real registry-only dependency fails with a clean
/// network error until one is, exactly like any other unreachable host.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.kupl-lang.org";

/// One file a registry package version is made of: where to fetch it, and
/// the hash its downloaded content must match.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryFile {
    pub url: String,
    pub hash: String,
}

/// One resolved version of a registry package.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryVersion {
    /// The package's entry file path (e.g. `"main.kupl"`), a key into `files`.
    pub entry: String,
    /// Relative file path (e.g. `"kupl.toml"`, `"main.kupl"`) -> where to
    /// fetch it and what its content must hash to.
    pub files: HashMap<String, RegistryFile>,
}

/// A parsed registry index for one package name.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryIndex {
    pub name: String,
    pub versions: HashMap<String, RegistryVersion>,
}

/// Parse a registry index JSON document. `Err` on malformed JSON or a
/// missing/wrong-shaped required field — never panics, since this parses
/// untrusted data fetched over the network. Reuses `lsp::parse_json`
/// (already hardened with a recursion-depth guard, production-hardening
/// PR-it620) rather than writing a parallel JSON parser.
pub fn parse_index(text: &str) -> Result<RegistryIndex, String> {
    let json = parse_json(text).map_err(|e| format!("invalid registry index JSON: {e}"))?;
    let name = json
        .get("name")
        .and_then(Json::str)
        .ok_or("registry index missing `name`")?
        .to_string();
    let versions_json = json.get("versions").ok_or("registry index missing `versions`")?;
    let Json::Obj(version_pairs) = versions_json else {
        return Err("registry index `versions` must be an object".into());
    };
    let mut versions = HashMap::new();
    for (version, entry_json) in version_pairs {
        let entry = entry_json
            .get("entry")
            .and_then(Json::str)
            .ok_or_else(|| format!("registry index version `{version}` missing `entry`"))?
            .to_string();
        let files_json = entry_json
            .get("files")
            .ok_or_else(|| format!("registry index version `{version}` missing `files`"))?;
        let Json::Obj(file_pairs) = files_json else {
            return Err(format!("registry index version `{version}`'s `files` must be an object"));
        };
        let mut files = HashMap::new();
        // Tracks each already-inserted path's own case-FOLDED form -> the
        // original path, so a SECOND, distinctly-spelled path that would
        // collide with it on disk can be reported (and reject BOTH names
        // in the message). A REAL, SEVERE bug found+fixed (production-
        // hardening PR-it921, an Explore survey finding, independently
        // re-verified live before implementing, including confirming the
        // exact NONDETERMINISTIC consequence): `materialize` (below)
        // writes each declared file to `staging.join(path)` keyed by its
        // OWN distinct string, with no cross-check that two DIFFERENT
        // declared paths (e.g. `"main.kupl"` and `"Main.kupl"`) address
        // the SAME real file on a case-INSENSITIVE filesystem -- the
        // DEFAULT for both macOS (APFS) and Windows (NTFS), not an
        // exotic edge case. A malicious or compromised registry index can
        // legally declare both spellings with DIFFERENT content, each
        // independently passing its own hash check -- whichever one
        // `HashMap`'s RANDOMIZED-per-process iteration order happens to
        // write LAST silently wins on disk, with `materialize` returning
        // `Ok(())` and zero diagnostic. Live-confirmed BEFORE this fix,
        // across 5 separate process invocations of the IDENTICAL index:
        // the winning content (and even which CASE VARIANT appeared in a
        // directory listing) was different EVERY time -- a genuinely
        // non-reproducible silent-value-corruption bug, meaning an
        // attacker's malicious variant has roughly even odds of becoming
        // the ACTUAL `entry` file `loader.rs` loads and runs on any given
        // `kupl pkg fetch`. Rejected here, at parse time (the single
        // earliest enforcement point, mirroring `is_safe_relative_path`'s
        // own precedent immediately below), using a full Unicode-aware
        // case fold (not just ASCII) to stay conservative -- a registry
        // index has no legitimate reason to declare two paths that only
        // differ by case at all.
        let mut folded: HashMap<String, String> = HashMap::new();
        for (path, file_json) in file_pairs {
            // A registry index is untrusted, network-supplied data. A file
            // path is later joined onto a local cache directory
            // (`materialize`, below) and written to disk — without this
            // check, a malicious or compromised registry could supply a
            // path like `"../../../.ssh/authorized_keys"` (path traversal)
            // or `"/etc/passwd"` (absolute path) to write OUTSIDE the
            // intended cache directory entirely. Rejected here, at parse
            // time, the single earliest enforcement point, so every
            // downstream consumer of a successfully parsed `RegistryIndex`
            // can trust every file path is safe without re-checking.
            if !is_safe_relative_path(path) {
                return Err(format!(
                    "registry index version `{version}` has an unsafe file path `{path}` \
                     (must be a relative path with no `..` component)"
                ));
            }
            let fold = path.to_lowercase();
            if let Some(other) = folded.insert(fold, path.clone()) {
                if other != *path {
                    return Err(format!(
                        "registry index version `{version}` declares two files that would collide \
                         on a case-insensitive filesystem: `{other}` and `{path}`"
                    ));
                }
            }
            let url = file_json
                .get("url")
                .and_then(Json::str)
                .ok_or_else(|| format!("registry index version `{version}` file `{path}` missing `url`"))?
                .to_string();
            if !is_safe_registry_url(&url) {
                return Err(format!(
                    "registry index version `{version}` file `{path}` has an unsafe url `{url}` \
                     (must be an http:// or https:// url)"
                ));
            }
            let hash = file_json
                .get("hash")
                .and_then(Json::str)
                .ok_or_else(|| format!("registry index version `{version}` file `{path}` missing `hash`"))?
                .to_string();
            files.insert(path.clone(), RegistryFile { url, hash });
        }
        if files.is_empty() {
            return Err(format!("registry index version `{version}` has no files"));
        }
        if !files.contains_key(&entry) {
            return Err(format!(
                "registry index version `{version}`'s entry `{entry}` is not listed in `files`"
            ));
        }
        versions.insert(version.clone(), RegistryVersion { entry, files });
    }
    Ok(RegistryIndex { name, versions })
}

/// Resolve an EXACT version from a parsed index. `Err` (naming every
/// available version, sorted) if the requested version isn't listed —
/// matches `loader.rs`'s existing "exact match in v1; ranges are a future
/// addition" scope for local path dependency version pins.
pub fn resolve_version<'a>(index: &'a RegistryIndex, version: &str) -> Result<&'a RegistryVersion, String> {
    index.versions.get(version).ok_or_else(|| {
        let mut available: Vec<&str> = index.versions.keys().map(String::as_str).collect();
        available.sort();
        format!(
            "registry package `{}` has no version `{version}` (available: {})",
            index.name,
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        )
    })
}

/// Verify a fetched file's content matches its expected hash. `Err` on
/// mismatch — a hard, mandatory check per the v1 design's security bar,
/// never optional or silent. Uses the SAME FNV-1a-hex-of-decimal-string
/// encoding `loader::resolve_deps` already computes for local dependencies
/// (`encoding::hex_encode(&format!("{}", encoding::hash_fnv(content)))`),
/// so a hash computed either way for the same bytes always matches.
pub fn verify_hash(content: &str, expected_hash: &str) -> Result<(), String> {
    let actual = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(content)));
    if actual == expected_hash {
        Ok(())
    } else {
        Err(format!(
            "hash mismatch: expected {expected_hash}, got {actual} — refusing to use unverified content"
        ))
    }
}

/// Whether `path` is safe to join onto a local directory and write to: not
/// absolute (`/etc/passwd`), and no `..` component that could climb out of
/// it (`../../.ssh/authorized_keys`). Deliberately conservative — a `.`
/// component or a Windows-style drive prefix are also rejected, since a
/// registry index has no legitimate reason to need either.
///
/// `pub(crate)` (production-hardening PR-it919): reused by
/// `manifest.rs::parse_dep` for the identical write-side... now READ-side
/// hazard on a version-only dependency's own `name`/`version` fields --
/// see that call site's own doc comment for the full writeup.
pub(crate) fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return false;
    }
    p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Like `is_safe_relative_path`, but additionally rejects a value with MORE
/// THAN ONE path component -- for a dependency's `name`/`version`, which
/// have no legitimate reason to be nested, unlike a registry FILE path
/// (`is_safe_relative_path`'s other class of caller, `parse_index`/
/// `materialize` below, which legitimately needs multi-component paths like
/// `"src/main.kupl"`).
///
/// A REAL, live-confirmed DESTRUCTIVE cache-corruption bug found+fixed
/// (production-hardening PR-it1096, a two-phase self-scoping survey
/// finding): `is_safe_relative_path`'s own multi-component allowance,
/// needed for file paths, was reused UNCHANGED for `name`/`version` too --
/// so a multi-component version like `"beta/preview"` passed cleanly.
/// `fetch_package_with`'s `dest = cache_dir.join(name).join(version)`
/// builds an ORDINARY intermediate directory (`cache_dir/name/beta/`) for
/// each path component of a nested version -- indistinguishable on disk
/// from a genuine top-level version directory. A LATER, entirely ordinary
/// fetch of a plain SIBLING version that happens to equal that ancestor
/// path segment (`version = "beta"`) passes the existing version-collision
/// guard cleanly (that guard only compares `cache_dir/name`'s own DIRECT
/// children against the exact `version` string -- `"beta"` legitimately
/// matches its own intermediate directory entry, since to that guard this
/// looks like an ordinary same-version re-fetch, not an ancestor
/// collision), then `atomic_replace`'s own unconditional
/// `remove_dir_all(dest)` silently DELETES THE ENTIRE `beta/` SUBTREE --
/// including the previously-fetched, already-hash-verified `beta/preview/`
/// version -- before writing the new `beta` version's own content. Live-
/// confirmed: fetching `widgets` version `"beta/preview"`, then `widgets`
/// version `"beta"`, returns `Ok` for the second fetch (no diagnostic at
/// all) while completely destroying `beta/preview/`'s own directory. No
/// traversal or case-folding trick is needed -- an entirely benign
/// manifest/registry using a plain "channel-style" versioning convention
/// (`"1.0"` vs `"1.0/rc1"`) reaches this on an ORDINARY re-fetch, since
/// this module's own design deliberately never cache-skips. Since
/// `cache_dir` is a single GLOBAL, per-user directory shared across every
/// project on the machine (PR-it930's own established threat model), this
/// can silently corrupt an entirely unrelated project's own cached
/// dependency the moment either project's `kupl pkg fetch` runs again.
pub(crate) fn is_safe_relative_path_single_component(path: &str) -> bool {
    is_safe_relative_path(path) && std::path::Path::new(path).components().count() == 1
}

/// Whether `url` is safe to hand to `curl_get`: an `http://` or `https://`
/// URL, and nothing else. A registry index is untrusted, network-supplied
/// data -- a file's `url` is later passed DIRECTLY to `curl` (`curl_get`,
/// below) with no scheme restriction of its own. Without this check, a
/// malicious or compromised registry could supply `"file:///etc/passwd"`
/// (local-file-disclosure: `curl` reads and returns the file's content as
/// if it were a fetched package file) or point at an internal-only host
/// (SSRF, e.g. a cloud metadata endpoint) via an ordinary `http://` URL to
/// a non-public address -- `curl`'s own default scheme support has no
/// allow-list. Rejected here, at parse time, the SAME "single earliest
/// enforcement point" pattern `is_safe_relative_path` above already uses
/// for `path` (production-hardening PR-it748, closing a real gap: every
/// OTHER untrusted field in this module -- `path`, content hashes, the
/// index's own name -- already has a dedicated check; `url` was the one
/// exception). Deliberately does NOT attempt to block SSRF against
/// internal/link-local HOSTS (that would need a live DNS/IP-range check
/// this parse-time function can't safely perform), only the FAR simpler
/// and more clearly wrong-in-EVERY-case "not http(s) at all" scheme class
/// -- `file://` and any other non-network scheme.
fn is_safe_registry_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Write a registry package version's ALREADY-FETCHED, ALREADY-VERIFIED file
/// contents into `cache_dir` (created if needed), producing an ordinary
/// local directory that `loader.rs`'s EXISTING local-path-dependency
/// machinery (`pkg_ctx`, `load_with`) can consume completely unchanged —
/// the registry layer's only job is "produce a directory containing the
/// package's files"; everything downstream already works. `contents` maps
/// each file's relative path (matching `RegistryVersion::files`'s keys) to
/// its fetched text — fetching those bytes over HTTP is a deliberately
/// separate, later slice; this function has no network dependency of its
/// own, so it's fully unit-testable without one. Re-verifies EVERY file's
/// hash again here (not just trusting a caller already did), and rejects
/// a `contents` map that doesn't exactly match what the index declared
/// (missing or extra entries) — either would silently diverge from what
/// the index promised.
pub fn materialize(
    version: &RegistryVersion,
    contents: &HashMap<String, String>,
    cache_dir: &std::path::Path,
) -> Result<(), String> {
    for path in version.files.keys() {
        if !contents.contains_key(path) {
            return Err(format!("missing fetched content for `{path}`"));
        }
    }
    for path in contents.keys() {
        if !version.files.contains_key(path) {
            return Err(format!("fetched content for `{path}`, which the index did not declare"));
        }
    }
    // Defense in depth (production-hardening PR-it921): `parse_index`
    // already rejects two declared paths that would collide on a case-
    // insensitive filesystem before a `RegistryVersion` can exist at all
    // (see that check's own doc comment for the full writeup) -- but
    // re-checking here, mirroring `is_safe_relative_path`'s own identical
    // "safe to call with a hand-built `RegistryVersion` too" precedent
    // immediately below, means a hand-built `RegistryVersion` (a test, or
    // a future caller that doesn't route through `parse_index`) can't
    // silently overwrite one file's verified content with another's on
    // disk either.
    let mut folded: HashMap<String, &String> = HashMap::new();
    for path in version.files.keys() {
        let fold = path.to_lowercase();
        if let Some(other) = folded.insert(fold, path) {
            if other != path {
                return Err(format!(
                    "declared files `{other}` and `{path}` would collide on a case-insensitive filesystem"
                ));
            }
        }
    }
    for (path, content) in contents {
        // `parse_index` already rejects an unsafe path before a
        // `RegistryVersion` can exist at all, but re-checking here means
        // this function is safe to call with a hand-built `RegistryVersion`
        // too (e.g. from a test, or a future caller that doesn't route
        // through `parse_index`), not just ones that came from it.
        if !is_safe_relative_path(path) {
            return Err(format!("unsafe file path `{path}`"));
        }
        verify_hash(content, &version.files[path].hash)?;
    }
    // Stage into a sibling temp directory and only atomically rename it into
    // place once EVERY file has been written successfully (production-
    // hardening PR-it700). `loader.rs::pkg_ctx`'s "already fetched" check
    // relies on `cache_dir` being reliable: it treats a cache directory as
    // fully fetched purely because `kupl.toml` exists in it, with no
    // verification that every OTHER declared file was actually written. An
    // interrupted `kupl pkg fetch` (process killed mid-write) used to leave
    // a PARTIAL `cache_dir` (`kupl.toml` present, entry file missing) that a
    // LATER `kupl run`/`pkg tree` would then trust as "already fetched" and
    // skip re-fetching entirely -- failing deep in module loading with a
    // generic "cannot read module file ... No such file or directory" error,
    // no hint the CACHE itself was corrupted. Confirmed live before this
    // fix. Writing to a staging directory first means a mid-write
    // interruption leaves the ORIGINAL `cache_dir` (if any) untouched and
    // the staging directory an orphaned `.tmp-*` sibling `pkg_ctx` never
    // looks at -- never a half-written `cache_dir` for its existence check
    // to be fooled by.
    let staging = cache_dir.with_file_name(format!(
        "{}.tmp-{}",
        cache_dir.file_name().and_then(|n| n.to_str()).unwrap_or("pkg"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging); // a stale leftover from a prior crash, if any
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    for (path, content) in contents {
        let dest = staging.join(path);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(format!("cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = std::fs::write(&dest, content) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("cannot write {}: {e}", dest.display()));
        }
    }
    // Every file landed in staging -- now atomically replace `cache_dir`
    // (a re-fetch's prior contents, if any, are simply superseded; v1
    // deliberately never cache-skips a re-fetch, per this module's own
    // established design).
    atomic_replace(&staging, cache_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("cannot finalize {}: {e}", cache_dir.display())
    })
}

/// Atomically replace `dest` with `staging` via `remove_dir_all` +
/// `rename`, retrying a bounded number of times on failure.
///
/// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it1006,
/// dispatched to a fresh scoping agent after `compile.rs`'s bare-Ident
/// register-aliasing bug class was fully exhausted at PR-it1005): the
/// staging directory `materialize` builds into is named
/// `{cache_dir}.tmp-{pid}` (unique per PROCESS), so two CONCURRENT `kupl
/// pkg fetch` invocations for the SAME package never collide while
/// WRITING their own staged files -- but the final `remove_dir_all` +
/// `rename` step can still interleave BETWEEN two racing processes: this
/// call's own `remove_dir_all(dest)` finds `dest` already gone (a
/// concurrent call just removed it), the CONCURRENT call's `rename` lands
/// FIRST (repopulating `dest`, now non-empty), and THIS call's own
/// single-attempt `rename` then fails -- a SPURIOUS error even though the
/// package IS now correctly cached, by the concurrent call, with
/// HASH-VERIFIED-IDENTICAL content (`materialize`'s own `verify_hash`
/// call runs before EITHER racer ever reaches this point, so two racing
/// callers for the same package/version can only ever be replacing `dest`
/// with the SAME verified bytes). Live-confirmed the underlying
/// filesystem behavior directly on this platform: renaming a directory
/// onto an existing NON-EMPTY destination returns `Err("Directory not
/// empty (os error 66)")`, not a silent replace -- so this raced exactly
/// as described the instant two `materialize` calls interleaved this way.
/// FIXED by retrying the remove+rename sequence a bounded number of times:
/// each retry's own `remove_dir_all` clears out whatever a concurrent
/// winner just placed, so a losing racer's NEXT attempt succeeds instead
/// of surfacing a spurious error for a package that is, in fact, already
/// correctly cached. This can never corrupt `dest` with the WRONG
/// content -- every racer's own `staging` directory holds the identical,
/// already-hash-verified bytes for this exact package/version, so
/// whichever racer's rename ultimately wins produces an equivalent,
/// correct result.
fn atomic_replace(staging: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    // PR-it1012: `cargo test`'s own full-suite parallel execution (many
    // concurrent test threads/processes contending for CPU) reliably
    // reproduced a genuine flake here that never showed up when this
    // test ran in isolation -- 5 attempts at a fixed 5ms backoff was
    // insufficient under REAL heavy contention (confirmed via a
    // deliberate artificial-load experiment, saturating every core with
    // `yes > /dev/null` background processes: 4 of 8 racers failed
    // reliably). This isn't just a test artifact -- a CI machine running
    // many concurrent `kupl pkg fetch` jobs under heavy load is a
    // realistic production scenario with the SAME shape. Raised the
    // retry budget substantially (50 attempts, unchanged 5ms backoff --
    // more CHANCES to win the race matters more here than a LONGER wait
    // per attempt, since the underlying race window itself is normally
    // microseconds) -- empirically re-validated under the SAME
    // artificial-load condition before finalizing (0 failures across
    // repeated stress runs). Worst case adds ~250ms of latency to a
    // `kupl pkg fetch` under PERSISTENT, extreme, whole-machine
    // contention -- imperceptible next to the network I/O `fetch_package`
    // already does, and only ever paid on the rare unlucky-timing path.
    const MAX_ATTEMPTS: u32 = 50;
    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(dest);
        match std::fs::rename(staging, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// A response larger than this is rejected (curl exit 63, `--max-filesize`)
/// rather than fully buffered into memory -- a REAL, live-confirmed
/// resource-exhaustion gap found+fixed (production-hardening PR-it751): a
/// malicious or merely misbehaving registry/mirror could return an
/// arbitrarily large body for an ordinary `kupl pkg fetch`, which
/// `cmd.output()` below buffers ENTIRELY into memory before this function
/// gets a chance to look at it. A HIGHER-severity trust boundary than
/// `interp.rs`'s equivalent gap (fixed in the SAME iteration): here the
/// registry itself (untrusted, per this module's own established threat
/// model -- see `is_safe_registry_url`/`is_safe_relative_path`) controls
/// both the URL AND the response for a routine dependency-fetch operation,
/// not something the KUPL program author opted into per-call. Mirrors
/// `interp.rs`'s own `MAX_HTTP_RESPONSE_SIZE` (10MB) for consistency.
const MAX_REGISTRY_RESPONSE_SIZE: u64 = 10 * 1024 * 1024;

/// Fetch `url` via `curl` — the same zero-dependency, subprocess-based
/// transport `interp.rs`'s `http_get`/`http_post` builtins and `ai.rs`'s
/// provider calls already use (`-sS --fail` so a non-2xx status becomes an
/// `Err`, `--max-time 30` so a stalled/unreachable host can't hang the CLI
/// forever, `--max-filesize` so an oversized response can't exhaust
/// memory). `Err` on a non-2xx status, an unreachable host, curl being
/// missing, a response that isn't valid UTF-8, or a response exceeding
/// `MAX_REGISTRY_RESPONSE_SIZE`.
fn curl_get(url: &str) -> Result<String, String> {
    let mut cmd = build_curl_get_cmd(url);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = cmd.output().map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("request to {url} failed (curl exit {})", out.status.code().unwrap_or(-1))
        } else {
            format!("request to {url} failed: {err}")
        });
    }
    String::from_utf8(out.stdout).map_err(|_| format!("response from {url} is not valid UTF-8"))
}

/// Build (but don't spawn) `curl_get`'s command, split out purely so a unit
/// test can introspect the exact args via `Command::get_args()` without
/// spawning a real `curl` subprocess -- this codebase's registry.rs tests
/// deliberately never invoke real `curl` (every `fetch_package_with` test
/// injects a canned/mock fetcher instead), so a network-dependent test here
/// would be the first of its kind and break that established, deliberate
/// portability convention. Testing the args a real invocation WOULD use
/// still catches the actual regression this fix guards against (the
/// `--max-filesize` flag being silently dropped in a future edit).
fn build_curl_get_cmd(url: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "--fail", "--max-time", "30"]);
    cmd.args(["--max-filesize", &MAX_REGISTRY_RESPONSE_SIZE.to_string()]);
    cmd.arg(url);
    cmd
}

/// The local on-disk cache `kupl pkg fetch` materializes registry packages
/// into: `~/.kupl/registry-cache` (`$HOME`, or `%USERPROFILE%` on Windows
/// where `HOME` isn't set), falling back to a temp directory if neither is
/// set — degrades gracefully rather than panicking, matching this
/// codebase's existing convention (e.g. `csv.rs`/`url.rs`'s malformed-input
/// handling). A fixed, well-known location so re-running `kupl pkg fetch`
/// reuses the same cache across invocations instead of re-downloading, and
/// so `loader.rs`'s `pkg_ctx` can independently compute the SAME path a
/// registry-only dependency would materialize to, to detect whether it has
/// already been fetched (production-hardening PR-it641).
pub fn cache_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    home.join(".kupl").join("registry-cache")
}

/// Fetch and materialize one package version from a registry: the index at
/// `{registry_url}/{name}.json`, then every file the resolved version
/// lists, verifying every hash before anything is written to disk (via
/// `materialize`, which re-checks regardless of what this function already
/// trusts). Returns the directory the package was materialized into — an
/// ordinary local directory `loader.rs`'s existing local-path-dependency
/// machinery can consume unchanged, exactly like a hand-written `{ path =
/// ".." }` dependency, matching this module's own central design claim
/// (verified end to end by this module's tests).
///
/// v1 deliberately does not cache-skip a re-fetch of an already-materialized
/// version — always fetches and re-verifies fresh. Caching is a
/// deliberately separate, later concern (out of scope here, same as the
/// module doc comment's network/caching split for `materialize` itself).
pub fn fetch_package(
    registry_url: &str,
    name: &str,
    version: &str,
    cache_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    fetch_package_with(curl_get, registry_url, name, version, cache_dir)
}

/// A simple, dependency-free, cross-PROCESS advisory lock over the whole
/// registry cache directory, held for `fetch_package_with`'s entire
/// critical section (production-hardening PR-it1100).
///
/// A REAL, LIVE-CONFIRMED DESTRUCTIVE cache-corruption bug found+fixed: this
/// module's own two case-collision guards (PR-it930's name-level check,
/// PR-it1060's version-level check, both at the top of `fetch_package_with`
/// below) only defend against a collision with a package ALREADY on disk at
/// scan time — they never close the window between that scan and
/// `materialize`'s own eventual write. Two CONCURRENT `kupl pkg fetch`
/// processes fetching two DIFFERENT, case-colliding names (e.g. `Lib` and
/// `lib`) for the FIRST time can each pass their own independent scan
/// (neither is cached yet), then race to materialize into what is, on a
/// case-insensitive filesystem (the default on macOS/Windows), the SAME
/// physical directory — structurally distinct from `atomic_replace`'s own
/// already-fixed same-name/same-version race (PR-it1006), which is safe
/// specifically because both racers there always hold IDENTICAL,
/// hash-verified content for the exact same package; here the two racers
/// hold GENUINELY DIFFERENT content for two DIFFERENT packages that merely
/// happen to case-collide. Live-confirmed via a real two-thread repro
/// (mirroring PR-it1006's own "real threads standing in for real processes"
/// technique) injecting a small unconditional delay into each racer's mock
/// "network" fetch to widen the window: reliably reproducible across every
/// repeated run (4 of 4), the dominant failure mode being the WORST case --
/// both `fetch_package_with` calls return `Ok`, with zero diagnostic, while
/// one package's on-disk content is silently destroyed by the other's;
/// occasionally instead an unrelated, confusing `No such file or directory`
/// I/O error surfaces from `atomic_replace`'s own rename target vanishing
/// mid-flight. FIXED by acquiring this lock before either case-collision
/// scan runs and holding it through `materialize`'s own final write,
/// serializing every `fetch_package_with` call against this cache
/// directory (across processes, since the lock is a real file on disk) --
/// this doesn't just narrow the race window, it eliminates it entirely: a
/// second racer's OWN scan now correctly runs strictly after the first
/// racer's write has either landed or failed, so the EXISTING collision
/// guards work exactly as already designed and tested, no new detection
/// logic needed. `kupl pkg fetch` is not a throughput-sensitive hot path
/// (`run.rs::pkg_fetch_with` already fetches every dependency strictly
/// SEQUENTIALLY within one process, confirmed via that function's own
/// source), so full serialization costs nothing in the common case and only
/// ever adds latency, never contention, for the realistic concurrent-
/// process scenario this defends (e.g. two CI jobs or two terminals sharing
/// one `$HOME` cache). Implemented via `OpenOptions::create_new` on a lock
/// file in the OS temp directory, deterministically named from `cache_dir`
/// itself (see `acquire`'s own doc comment for why NOT inside `cache_dir`)
/// -- atomic at the OS level (only one creator can ever win), polled with a
/// short backoff since this codebase is deliberately zero-dependency and
/// has no blocking-flock primitive available; released by removing the
/// lock file when the guard drops (including on an early `?`/panic-unwind
/// return, via `Drop`).
struct CacheLock {
    path: std::path::PathBuf,
}

impl CacheLock {
    fn acquire(cache_dir: &std::path::Path) -> std::io::Result<CacheLock> {
        // The lock file lives in the OS temp directory (always present),
        // deterministically named from `cache_dir`'s own path, rather than
        // inside `cache_dir` itself. `cache_dir` may not exist yet (this
        // race is most likely on a brand-new machine's very first `kupl pkg
        // fetch`), and several existing tests deliberately assert NOTHING
        // is written to `cache_dir` at all when a fetch is cleanly rejected
        // before ever reaching `materialize` (an unsafe name/version, a
        // resolve failure, a hash mismatch) -- eagerly creating `cache_dir`
        // just to host a lock file would silently break that "no footprint
        // on a clean rejection" contract (confirmed live: this was tried
        // first and broke exactly those five tests).
        let key = crate::encoding::hash_fnv(&cache_dir.display().to_string());
        let path = std::env::temp_dir().join(format!("kupl-registry-cache-{key:x}.lock"));
        // 2000 attempts * 5ms = 10s worst-case wait before giving up --
        // generous next to the network I/O a real fetch already does, but
        // still bounded so a crashed process that left a stale lock file
        // behind can't wedge every future `kupl pkg fetch` forever silently;
        // a stuck lock surfaces as a clear timeout error instead.
        const MAX_ATTEMPTS: u32 = 2000;
        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_file) => return Ok(CacheLock { path }),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out waiting for registry cache lock")
        }))
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `fetch_package`, but the transport is injectable — lets a test exercise
/// the full fetch-index/resolve-version/fetch-files/materialize pipeline
/// with a canned, in-memory fetcher instead of live curl/network access
/// (only `fetch_package` above uses real curl; tests call this directly),
/// mirroring `interp.rs`'s `serve_http`/`serve_http_with_read_timeout` test-
/// injection pattern (production-hardening PR-it623). Acquires `CacheLock`
/// (production-hardening PR-it1100) for its ENTIRE body, serializing every
/// call against this `cache_dir` -- see that struct's own doc comment for
/// the destructive cross-name race this closes.
fn fetch_package_with(
    fetch: impl Fn(&str) -> Result<String, String>,
    registry_url: &str,
    name: &str,
    version: &str,
    cache_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    // Held for the REST of this function (production-hardening PR-it1100) --
    // see `CacheLock`'s own doc comment for the destructive cross-name race
    // this closes. Acquired before EITHER case-collision scan below runs,
    // so a second racer's own scan is guaranteed to see whatever the first
    // racer already wrote (or didn't), never an in-between state.
    let _lock = CacheLock::acquire(cache_dir)
        .map_err(|e| format!("cannot acquire registry cache lock in {}: {e}", cache_dir.display()))?;
    // `name`/`version` are NOT registry-supplied like a `RegistryVersion`'s
    // file paths (already guarded by `is_safe_relative_path` in
    // `parse_index`) -- they come from the CALLER, ultimately traced back to
    // the local `kupl.toml`'s dependency table key and version-pin string
    // (`manifest.rs::parse_dep`, which places no restriction on either). A
    // malicious or untrusted project's manifest (e.g. one you `git clone`
    // and build, or a transitively-pulled-in dependency's own manifest) can
    // declare a dependency name or version containing `..`/an absolute path
    // -- without this check, `dest = cache_dir.join(name).join(version)`
    // below builds a path `PathBuf::join` does NOT collapse (confirmed
    // live: joining `../../../../tmp/evil` onto a cache dir yields a path
    // string containing that literal `..` sequence), which `materialize`'s
    // `std::fs::write` WOULD then resolve at the OS level -- an arbitrary
    // file write anywhere the current user can write, entirely outside the
    // intended cache directory. Reuses the SAME `is_safe_relative_path`
    // helper `parse_index`/`materialize` already use for the analogous
    // registry-file-path threat (production-hardening PR-it683). Uses the
    // STRICTER single-component variant (production-hardening PR-it1096) --
    // unlike a registry file path, a name/version has no legitimate reason
    // to be nested, and a multi-component value here caused a genuine
    // destructive cache-corruption bug (see that function's own doc
    // comment for the full writeup).
    if !is_safe_relative_path_single_component(name) {
        return Err(format!("unsafe package name `{name}`"));
    }
    if !is_safe_relative_path_single_component(version) {
        return Err(format!("unsafe package version `{version}`"));
    }
    // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it930,
    // a close-read survey finding re-examining PR-it921's own case-collision
    // fix from a different angle): `cache_dir` (below) is a single, GLOBAL,
    // per-USER directory (`~/.kupl/registry-cache`) SHARED across every
    // project on the machine, keyed by `name` verbatim. On a case-
    // insensitive filesystem (the DEFAULT for macOS/Windows), fetching a
    // package named e.g. `lib` would silently WRITE OVER (materialize
    // unconditionally overwrites — this module's own design never cache-
    // skips) an entirely UNRELATED, already-cached `Lib` package belonging
    // to some OTHER, unrelated project — a genuine cross-project cache
    // CORRUPTION, not merely a read-time confusion (`loader.rs::pkg_ctx`'s
    // OWN sibling fix, this same iteration, defends the READ side; this
    // defends the WRITE side that would otherwise destroy the collided-with
    // package's cache entry in the first place). Checked BEFORE any network
    // fetch (fail fast, no wasted work) by scanning `cache_dir`'s existing
    // top-level entries for a case-insensitive-but-not-exact match against
    // `name`.
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        let name_fold = name.to_lowercase();
        for entry in entries.flatten() {
            let existing = entry.file_name();
            let existing = existing.to_string_lossy();
            if existing.as_ref() != name && existing.to_lowercase() == name_fold {
                return Err(format!(
                    "package name `{name}` collides with the already-cached package \
                     `{existing}` on a case-insensitive filesystem — refusing to fetch \
                     (this would silently overwrite an unrelated package's cache entry)"
                ));
            }
        }
    }
    // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it1060,
    // a background close-read survey finding re-examining PR-it930's own
    // name-level case-collision fix immediately above from a DIFFERENT
    // angle): that check only scans `cache_dir`'s TOP-LEVEL entries (package
    // NAMES) for a case-insensitive collision -- it never scans
    // `cache_dir.join(name)`'s own children for a collision on the VERSION
    // component. `version`, exactly like `name` above, is caller-supplied
    // (a local `kupl.toml` version-pin string, untrusted). On a case-
    // insensitive filesystem, fetching `1.0.0-rc` after `1.0.0-RC` is
    // already cached resolves `atomic_replace`'s destination to the SAME
    // on-disk directory (confirmed live via matching inode numbers), so
    // `materialize` silently overwrites the original, already-hash-
    // verified version's content with the new, differently-cased version's
    // content -- no error, no diagnostic. Worse, `loader.rs::pkg_ctx`'s own
    // "already fetched" check (PR-it930's read-side defense) only re-
    // verifies the cached manifest's NAME, never the version, so this also
    // lets a project silently load a cached directory that was never
    // fetched under the version string it actually declared (see that
    // function's own matching PR-it1060 fix for the read-side half of this
    // same bug class). Guarded the SAME way as the name-level check above,
    // scanning the per-package subdirectory instead of `cache_dir` itself.
    if let Ok(entries) = std::fs::read_dir(cache_dir.join(name)) {
        let version_fold = version.to_lowercase();
        for entry in entries.flatten() {
            let existing = entry.file_name();
            let existing = existing.to_string_lossy();
            if existing.as_ref() != version && existing.to_lowercase() == version_fold {
                return Err(format!(
                    "package version `{version}` for `{name}` collides with the already-\
                     cached version `{existing}` on a case-insensitive filesystem — \
                     refusing to fetch (this would silently overwrite an already-cached \
                     version's cache entry)"
                ));
            }
        }
    }
    let index_url = format!("{}/{name}.json", registry_url.trim_end_matches('/'));
    let index_text =
        fetch(&index_url).map_err(|e| format!("cannot fetch registry index for `{name}`: {e}"))?;
    let index = parse_index(&index_text)?;
    // The index at this URL is untrusted, network-supplied data (the SAME
    // class of concern `is_safe_relative_path` already guards against for
    // file paths) -- without this check, a misconfigured or compromised
    // registry could serve one package's index content at another
    // package's URL, silently installing the wrong code under the name the
    // caller asked for.
    if index.name != name {
        return Err(format!(
            "registry index at {index_url} is for package `{}`, not `{name}` -- refusing a mismatched index",
            index.name
        ));
    }
    let resolved = resolve_version(&index, version)?;
    let mut contents = HashMap::new();
    for (path, file) in &resolved.files {
        let content = fetch(&file.url)
            .map_err(|e| format!("cannot fetch `{name}` {version} file `{path}`: {e}"))?;
        contents.insert(path.clone(), content);
    }
    let dest = cache_dir.join(name).join(version);
    materialize(resolved, &contents, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_is_a_fixed_dot_kupl_registry_cache_location() {
        let dir = super::cache_dir();
        assert_eq!(dir.file_name().unwrap(), "registry-cache");
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), ".kupl");
    }

    fn sample_index() -> &'static str {
        r#"{
            "name": "json2",
            "versions": {
                "1.2.0": {
                    "entry": "main.kupl",
                    "files": {
                        "kupl.toml": {"url": "https://example.com/json2/1.2.0/kupl.toml", "hash": "aa"},
                        "main.kupl": {"url": "https://example.com/json2/1.2.0/main.kupl", "hash": "bb"}
                    }
                },
                "1.1.0": {
                    "entry": "main.kupl",
                    "files": {
                        "main.kupl": {"url": "https://example.com/json2/1.1.0/main.kupl", "hash": "cc"}
                    }
                }
            }
        }"#
    }

    #[test]
    fn parses_a_well_formed_index() {
        let idx = parse_index(sample_index()).expect("parses");
        assert_eq!(idx.name, "json2");
        assert_eq!(idx.versions.len(), 2);
        let v = &idx.versions["1.2.0"];
        assert_eq!(v.entry, "main.kupl");
        assert_eq!(v.files.len(), 2);
        assert_eq!(
            v.files["kupl.toml"],
            RegistryFile { url: "https://example.com/json2/1.2.0/kupl.toml".into(), hash: "aa".into() }
        );
    }

    #[test]
    fn resolve_version_finds_an_exact_match() {
        let idx = parse_index(sample_index()).unwrap();
        let v = resolve_version(&idx, "1.1.0").expect("1.1.0 exists");
        assert_eq!(v.entry, "main.kupl");
        assert_eq!(v.files.len(), 1);
    }

    #[test]
    fn resolve_version_rejects_a_missing_version_and_lists_available_ones() {
        let idx = parse_index(sample_index()).unwrap();
        let err = resolve_version(&idx, "9.9.9").unwrap_err();
        assert!(err.contains("9.9.9"), "{err}");
        assert!(err.contains("1.1.0") && err.contains("1.2.0"), "should list available versions: {err}");
    }

    /// v1 is explicitly exact-match only (no ranges) — a range-shaped
    /// request must be rejected the SAME clean way an unknown version is,
    /// not silently interpreted as "any compatible version."
    #[test]
    fn resolve_version_does_not_interpret_ranges() {
        let idx = parse_index(sample_index()).unwrap();
        assert!(resolve_version(&idx, "^1.0.0").is_err());
        assert!(resolve_version(&idx, ">=1.0.0").is_err());
        assert!(resolve_version(&idx, "1").is_err());
    }

    #[test]
    fn verify_hash_accepts_a_matching_hash_and_rejects_a_mismatch() {
        let content = "pub fun add(a: Int, b: Int) -> Int { a + b }\n";
        let real_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(content)));
        assert!(verify_hash(content, &real_hash).is_ok());
        assert!(verify_hash(content, "0000000000000000").is_err());
        // even a single-byte difference must be rejected, not "close enough"
        let tampered = "pub fun add(a: Int, b: Int) -> Int { a - b }\n";
        assert!(verify_hash(tampered, &real_hash).is_err());
    }

    #[test]
    fn malformed_json_is_a_clean_error_not_a_panic() {
        assert!(parse_index("").is_err());
        assert!(parse_index("not json at all {{{").is_err());
        assert!(parse_index("{}").is_err());
        assert!(parse_index(r#"{"name": "x"}"#).is_err());
        assert!(parse_index(r#"{"name": "x", "versions": "not an object"}"#).is_err());
        assert!(parse_index(r#"{"name": "x", "versions": {"1.0.0": {}}}"#).is_err());
        assert!(parse_index(r#"{"name": "x", "versions": {"1.0.0": {"entry": "main.kupl"}}}"#).is_err());
        // a deeply nested (adversarial) document must not crash the parser,
        // matching `lsp::parse_json`'s existing depth guard (PR-it620) --
        // confirms this module correctly inherits that protection by reuse
        // rather than needing its own.
        let deeply_nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_index(&deeply_nested).is_err());
    }

    #[test]
    fn an_entry_not_listed_in_files_is_rejected() {
        // the `entry` field must name a file that's actually IN `files` --
        // otherwise a package would resolve successfully but have no way to
        // ever locate its own entry point.
        let bad = r#"{
            "name": "x",
            "versions": {
                "1.0.0": {
                    "entry": "main.kupl",
                    "files": {"other.kupl": {"url": "https://x/other.kupl", "hash": "aa"}}
                }
            }
        }"#;
        let err = parse_index(bad).unwrap_err();
        assert!(err.contains("entry") && err.contains("main.kupl"), "{err}");
    }

    #[test]
    fn a_version_with_no_files_is_rejected() {
        let bad = r#"{"name": "x", "versions": {"1.0.0": {"entry": "main.kupl", "files": {}}}}"#;
        assert!(parse_index(bad).is_err());
    }

    /// A REAL security concern this module's `materialize` step introduces
    /// (production-hardening PR-it631): a registry index's file paths are
    /// UNTRUSTED, network-supplied data, and get joined onto a local cache
    /// directory and written to disk — without validation, a malicious or
    /// compromised registry could supply a path traversal (`"../../.ssh/
    /// authorized_keys"`) or an absolute path (`"/etc/passwd"`) to write
    /// OUTSIDE the intended cache directory entirely. Confirmed the check
    /// catches every shape at PARSE time (the single earliest enforcement
    /// point), before a `RegistryVersion` can even be constructed.
    #[test]
    fn unsafe_file_paths_are_rejected_at_parse_time() {
        for bad_path in ["../evil.kupl", "../../.ssh/authorized_keys", "/etc/passwd", "a/../../b", "./x.kupl", ""] {
            let idx = format!(
                r#"{{"name": "x", "versions": {{"1.0.0": {{"entry": "{bad_path}", "files": {{"{bad_path}": {{"url": "https://x/y", "hash": "aa"}}}}}}}}}}"#
            );
            let err = parse_index(&idx);
            assert!(err.is_err(), "path {bad_path:?} should have been rejected, got {err:?}");
        }
        // an ordinary nested-but-safe relative path (a subdirectory) is fine
        let ok = r#"{"name": "x", "versions": {"1.0.0": {"entry": "src/main.kupl", "files": {"src/main.kupl": {"url": "https://x/y", "hash": "aa"}}}}}"#;
        assert!(parse_index(ok).is_ok());
    }

    /// A REAL security bug found+fixed (production-hardening PR-it748): every
    /// OTHER untrusted field in a registry index (`path`, content hashes, the
    /// index's own `name`) already has a dedicated safety check -- `url` was
    /// the one exception, handed DIRECTLY to `curl` (`curl_get`) with no
    /// scheme restriction at all. A malicious or compromised registry could
    /// supply `"file:///etc/passwd"` (local-file-disclosure -- `curl` reads
    /// and returns the file's content as if it were a fetched package file)
    /// or an SSRF-relevant internal host via a plain `http://` URL to a
    /// non-public address. Live-confirmed the underlying transport primitive
    /// before this fix: `curl -sS --fail --max-time 30 file:///etc/hosts`
    /// (the EXACT invocation shape `curl_get` uses) successfully printed
    /// `/etc/hosts`'s content with exit code 0 -- `curl`'s own scheme support
    /// has no allow-list by default.
    #[test]
    fn unsafe_file_urls_are_rejected_at_parse_time() {
        for bad_url in ["file:///etc/passwd", "file://localhost/etc/hosts", "ftp://x/y", "", "not-a-url"] {
            let idx = format!(
                r#"{{"name": "x", "versions": {{"1.0.0": {{"entry": "main.kupl", "files": {{"main.kupl": {{"url": "{bad_url}", "hash": "aa"}}}}}}}}}}"#
            );
            let err = parse_index(&idx);
            assert!(err.is_err(), "url {bad_url:?} should have been rejected, got {err:?}");
        }
        // ordinary http:// and https:// urls are still fine (not an overly
        // broad check that rejects every legitimate registry index).
        for good_url in ["https://x/y", "http://x/y"] {
            let idx = format!(
                r#"{{"name": "x", "versions": {{"1.0.0": {{"entry": "main.kupl", "files": {{"main.kupl": {{"url": "{good_url}", "hash": "aa"}}}}}}}}}}"#
            );
            assert!(parse_index(&idx).is_ok(), "url {good_url:?} should have parsed cleanly");
        }
    }

    #[test]
    fn curl_get_caps_the_response_size_it_will_buffer_into_memory() {
        // A REAL, live-confirmed resource-exhaustion gap found+fixed
        // (production-hardening PR-it751): `curl_get` had no response-size
        // limit at all -- `cmd.output()` buffers the ENTIRE response body
        // into memory before this module gets a chance to look at it, so a
        // malicious or merely misbehaving registry/mirror could return an
        // arbitrarily large body for an ordinary `kupl pkg fetch`.
        // Live-confirmed BEFORE this fix, outside this test (a local test
        // HTTP server serving a 10MB file, run via a real `curl` subprocess
        // with and without `--max-filesize`): without the flag, curl
        // downloaded the full 10MB; with `--max-filesize 1000000` (1MB) set
        // against the SAME 10MB file, curl aborted with exit 63 ("Maximum
        // file size exceeded") and downloaded nothing.
        //
        // This test does NOT spawn a real `curl` subprocess (unlike a
        // network-dependent integration test would) -- every existing test
        // in this module's `fetch_package_with` family deliberately injects
        // a canned/mock fetcher instead of touching real `curl`, and a
        // network-dependent test here would be the first of its kind,
        // breaking that established portability convention. Instead it
        // introspects the ACTUAL `Command` `curl_get` would spawn (via
        // `build_curl_get_cmd`, the same function `curl_get` itself calls)
        // using `Command::get_args()` -- this still catches the real
        // regression the fix guards against (the `--max-filesize` flag
        // being silently dropped in a future edit), without any network
        // dependency or flakiness.
        let cmd = build_curl_get_cmd("https://registry.example.com/x.json");
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let flag_pos = args.iter().position(|a| a == "--max-filesize");
        assert!(flag_pos.is_some(), "curl_get must pass --max-filesize: {args:?}");
        let limit: u64 = args[flag_pos.unwrap() + 1].parse().expect("--max-filesize value must be numeric");
        assert_eq!(limit, MAX_REGISTRY_RESPONSE_SIZE, "{args:?}");
        assert!(limit > 0, "a zero cap would reject every legitimate response too: {args:?}");
    }

    #[test]
    fn materialize_writes_verified_content_to_the_cache_directory() {
        let version = RegistryVersion {
            entry: "main.kupl".to_string(),
            files: HashMap::from([
                (
                    "main.kupl".to_string(),
                    RegistryFile {
                        url: "https://example.com/main.kupl".to_string(),
                        hash: crate::encoding::hex_encode(&format!(
                            "{}",
                            crate::encoding::hash_fnv("pub fun f() -> Int { 1 }\n")
                        )),
                    },
                ),
                (
                    "kupl.toml".to_string(),
                    RegistryFile {
                        url: "https://example.com/kupl.toml".to_string(),
                        hash: crate::encoding::hex_encode(&format!(
                            "{}",
                            crate::encoding::hash_fnv("[project]\nname = \"x\"\n")
                        )),
                    },
                ),
            ]),
        };
        let contents = HashMap::from([
            ("main.kupl".to_string(), "pub fun f() -> Int { 1 }\n".to_string()),
            ("kupl.toml".to_string(), "[project]\nname = \"x\"\n".to_string()),
        ]);
        let dir = std::env::temp_dir().join(format!("kupl-registry-materialize-{}", std::process::id()));
        let result = materialize(&version, &contents, &dir);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read_to_string(dir.join("main.kupl")).unwrap(), "pub fun f() -> Int { 1 }\n");
        assert_eq!(std::fs::read_to_string(dir.join("kupl.toml")).unwrap(), "[project]\nname = \"x\"\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it700): `materialize` used
    /// to write files one at a time directly into `cache_dir`, with no staging or
    /// atomic finalization. `loader.rs::pkg_ctx`'s "already fetched" check treats a
    /// cache directory as fully materialized purely because `kupl.toml` exists in
    /// it -- so an interrupted `kupl pkg fetch` (process killed mid-write) could
    /// leave a PARTIAL cache directory (`kupl.toml` present, entry file missing)
    /// that a LATER `kupl run`/`pkg tree` would trust as complete and skip
    /// re-fetching, failing deep in module loading with a generic, uninformative
    /// "cannot read module file" error. This test simulates exactly that partial/
    /// corrupt state (a `kupl.toml` with garbage content, a stale leftover file, NO
    /// entry file), confirms `materialize` atomically REPLACES the whole directory
    /// (the stale content does not survive, does not get merged with the new
    /// content), and confirms no orphaned staging directory litters the parent.
    #[test]
    fn materialize_atomically_replaces_stale_or_partial_cache_content_and_leaves_no_staging_litter() {
        let version = RegistryVersion {
            entry: "main.kupl".to_string(),
            files: HashMap::from([
                (
                    "main.kupl".to_string(),
                    RegistryFile {
                        url: "https://example.com/main.kupl".to_string(),
                        hash: crate::encoding::hex_encode(&format!(
                            "{}",
                            crate::encoding::hash_fnv("pub fun f() -> Int { 2 }\n")
                        )),
                    },
                ),
                (
                    "kupl.toml".to_string(),
                    RegistryFile {
                        url: "https://example.com/kupl.toml".to_string(),
                        hash: crate::encoding::hex_encode(&format!(
                            "{}",
                            crate::encoding::hash_fnv("[project]\nname = \"x\"\n")
                        )),
                    },
                ),
            ]),
        };
        let contents = HashMap::from([
            ("main.kupl".to_string(), "pub fun f() -> Int { 2 }\n".to_string()),
            ("kupl.toml".to_string(), "[project]\nname = \"x\"\n".to_string()),
        ]);
        let dir = std::env::temp_dir().join(format!("kupl-registry-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Simulate a partial/corrupt cache directory left behind by an
        // interrupted fetch: `kupl.toml` present (what `pkg_ctx`'s "already
        // fetched" check looks for) but with GARBAGE content, the entry file
        // genuinely MISSING, plus a stale leftover a real fetch never wrote.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kupl.toml"), "GARBAGE, not even the right content").unwrap();
        std::fs::write(dir.join("stale_leftover.kupl"), "should not survive").unwrap();
        assert!(!dir.join("main.kupl").is_file(), "sanity: entry file genuinely missing, simulating a partial fetch");

        let result = materialize(&version, &contents, &dir);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read_to_string(dir.join("main.kupl")).unwrap(), "pub fun f() -> Int { 2 }\n");
        assert_eq!(std::fs::read_to_string(dir.join("kupl.toml")).unwrap(), "[project]\nname = \"x\"\n");
        assert!(!dir.join("stale_leftover.kupl").exists(), "old/partial cache content must not survive a re-fetch");

        // No orphaned staging directory litters the parent.
        let parent = dir.parent().unwrap();
        let leftover_staging = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
        assert!(!leftover_staging, "materialize must not leave an orphaned staging directory behind on success");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
    /// PR-it1006): see `atomic_replace`'s own doc comment for the full
    /// root-cause writeup. `materialize`'s staging directory is named
    /// `{cache_dir}.tmp-{pid}` -- unique per PROCESS -- so two CONCURRENT
    /// `kupl pkg fetch` invocations for the SAME package never collide
    /// while writing their own staged files, but the final
    /// `remove_dir_all` + `rename` step used to be a SINGLE attempt: if a
    /// concurrent racer's rename landed in between this call's own
    /// `remove_dir_all` and `rename`, the single-attempt `rename` failed
    /// with a spurious "Directory not empty" error, even though the
    /// package IS now correctly cached (by the concurrent racer, with
    /// hash-verified-identical content). Spawns EIGHT threads, each with
    /// its own distinct "staging"-shaped source directory (standing in
    /// for eight different processes' own `materialize` calls), all
    /// racing `atomic_replace` against the SAME destination with no
    /// artificial synchronization -- empirically confirmed (a standalone
    /// scratch harness, 20 rounds) that eight genuinely concurrent racers
    /// reliably hit the failing interleaving on the OLD single-attempt
    /// logic EVERY round (~1 success, ~7 spurious failures per round) and
    /// reliably ALL succeed with the retry fix in place (0 failures across
    /// all 20 rounds) -- a reliable, non-flaky reproduction of the exact
    /// concurrent-process scenario this bug is reachable from, not a
    /// contrived single-thread simulation.
    #[test]
    fn atomic_replace_recovers_when_concurrent_racers_rename_onto_the_same_destination() {
        const RACERS: usize = 8;
        let base = std::env::temp_dir().join(format!("kupl-registry-atomic-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let dest = base.join("dest");
        // A pre-existing destination, like a prior cache entry every racer
        // is about to replace.
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.kupl"), "stale prior content").unwrap();

        let mut stagings = Vec::with_capacity(RACERS);
        for i in 0..RACERS {
            let s = base.join(format!("staging-{i}"));
            std::fs::create_dir_all(&s).unwrap();
            std::fs::write(s.join("file.kupl"), format!("from racer {i}")).unwrap();
            stagings.push(s);
        }

        let handles: Vec<_> = stagings
            .into_iter()
            .map(|staging| {
                let dest = dest.clone();
                std::thread::spawn(move || atomic_replace(&staging, &dest))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        for (i, r) in results.iter().enumerate() {
            assert!(r.is_ok(), "racer {i}: {r:?}");
        }
        // Whichever racer's rename ultimately won, the destination holds
        // ONE racer's content wholesale, never a mix, never absent, and
        // never the stale pre-existing content.
        let final_content = std::fs::read_to_string(dest.join("file.kupl")).unwrap();
        assert!(
            final_content.starts_with("from racer "),
            "unexpected final content: {final_content:?}"
        );
        assert!(!dest.join("stale.kupl").exists(), "stale prior content must not survive");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn materialize_refuses_content_with_a_wrong_hash() {
        // A tampered-with or corrupted download must never be written to
        // disk, even if the caller otherwise supplies exactly the right set
        // of file paths -- hash verification is re-checked here, not just
        // trusted from an earlier step.
        let version = RegistryVersion {
            entry: "main.kupl".to_string(),
            files: HashMap::from([(
                "main.kupl".to_string(),
                RegistryFile { url: "https://example.com/main.kupl".to_string(), hash: "deadbeef".to_string() },
            )]),
        };
        let contents = HashMap::from([("main.kupl".to_string(), "tampered content".to_string())]);
        let dir = std::env::temp_dir().join(format!("kupl-registry-tamper-{}", std::process::id()));
        let result = materialize(&version, &contents, &dir);
        assert!(result.is_err(), "{result:?}");
        assert!(!dir.join("main.kupl").exists(), "a hash-mismatched file must never be written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_refuses_a_contents_map_that_does_not_match_the_index() {
        let version = RegistryVersion {
            entry: "main.kupl".to_string(),
            files: HashMap::from([(
                "main.kupl".to_string(),
                RegistryFile {
                    url: "https://example.com/main.kupl".to_string(),
                    hash: crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv("x"))),
                },
            )]),
        };
        let dir = std::env::temp_dir().join(format!("kupl-registry-mismatch-{}", std::process::id()));
        // missing a file the index declared
        assert!(materialize(&version, &HashMap::new(), &dir).is_err());
        // an extra file the index never declared
        let extra = HashMap::from([
            ("main.kupl".to_string(), "x".to_string()),
            ("sneaky.kupl".to_string(), "y".to_string()),
        ]);
        assert!(materialize(&version, &extra, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Proves the v1 design's own central claim: a `materialize`d registry
    /// package produces an ORDINARY local directory that `loader.rs`'s
    /// EXISTING local-path-dependency machinery consumes completely
    /// unchanged — no registry-specific code needed anywhere downstream of
    /// `materialize`. Builds a fake registry index for a tiny `math`
    /// package end to end (parse index -> resolve version -> materialize
    /// fetched content to a cache dir), then loads a SEPARATE `app` package
    /// that depends on it via an ordinary `{ path = ".." }` local
    /// dependency pointing AT the materialized cache directory (standing in
    /// for what a later "fetch" slice would wire up automatically via
    /// `kupl.toml`'s registry `{ version = ".." }` syntax) — and confirms
    /// the WHOLE pipeline (loader, checker, interpreter) runs it correctly.
    #[test]
    fn a_materialized_package_loads_and_runs_exactly_like_a_local_dependency() {
        let index_json = r#"{
            "name": "math",
            "versions": {
                "1.0.0": {
                    "entry": "main.kupl",
                    "files": {
                        "kupl.toml": {"url": "https://example.com/math/1.0.0/kupl.toml", "hash": "REPLACED_TOML"},
                        "main.kupl": {"url": "https://example.com/math/1.0.0/main.kupl", "hash": "REPLACED_MAIN"}
                    }
                }
            }
        }"#;
        let toml_content = "[project]\nname = \"math\"\nentry = \"main.kupl\"\n";
        let main_content = "pub fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n";
        let toml_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(toml_content)));
        let main_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(main_content)));
        let index_json = index_json.replace("REPLACED_TOML", &toml_hash).replace("REPLACED_MAIN", &main_hash);

        let index = parse_index(&index_json).expect("index parses");
        let version = resolve_version(&index, "1.0.0").expect("1.0.0 resolves");

        let base = std::env::temp_dir().join(format!("kupl-registry-e2e-{}", std::process::id()));
        let math_cache = base.join("math-1.0.0");
        let contents = HashMap::from([
            ("kupl.toml".to_string(), toml_content.to_string()),
            ("main.kupl".to_string(), main_content.to_string()),
        ]);
        materialize(version, &contents, &math_cache).expect("materializes cleanly");

        // an ordinary app package depending on the materialized cache dir
        // via a plain LOCAL path -- exactly what loader.rs already supports
        // today, proving the registry layer adds nothing downstream needs
        // to special-case.
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            format!(
                "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = {{ path = \"{}\" }}\n",
                math_cache.display()
            ),
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use math\n\nfun main() uses io {\n    print(math.add(2, 3))\n}\n",
        )
        .unwrap();

        let (program, _map) = crate::loader::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its materialized math dependency");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(_) => {}
            Err(_) => panic!("main() should run cleanly against the materialized dependency"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A canned, in-memory fetcher for `fetch_package_with` tests: a URL ->
    /// content map, `Err` for anything not listed. Lets these tests exercise
    /// the real fetch/resolve/materialize pipeline deterministically, with
    /// no live network access.
    fn mock_fetcher(urls: HashMap<String, String>) -> impl Fn(&str) -> Result<String, String> {
        move |url: &str| {
            urls.get(url).cloned().ok_or_else(|| format!("mock: no canned response for {url}"))
        }
    }

    fn mock_index_and_files(name: &str, version: &str, main_content: &str) -> (String, HashMap<String, String>) {
        let main_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(main_content)));
        let index_url = format!("https://registry.example.com/{name}.json");
        let main_url = format!("https://cdn.example.com/{name}/{version}/main.kupl");
        let index_json = format!(
            r#"{{"name": "{name}", "versions": {{"{version}": {{"entry": "main.kupl", "files": {{"main.kupl": {{"url": "{main_url}", "hash": "{main_hash}"}}}}}}}}}}"#
        );
        let urls = HashMap::from([(index_url, index_json), (main_url, main_content.to_string())]);
        (main_hash, urls)
    }

    #[test]
    fn fetch_package_with_materializes_a_resolved_version_from_canned_responses() {
        let (_hash, urls) = mock_index_and_files("json2", "1.2.0", "pub fun id(x: Int) -> Int { x }\n");
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-happy-{}", std::process::id()));
        let result = fetch_package_with(
            mock_fetcher(urls),
            "https://registry.example.com",
            "json2",
            "1.2.0",
            &dir,
        );
        assert!(result.is_ok(), "{result:?}");
        let dest = result.unwrap();
        assert_eq!(dest, dir.join("json2").join("1.2.0"));
        assert_eq!(
            std::fs::read_to_string(dest.join("main.kupl")).unwrap(),
            "pub fun id(x: Int) -> Int { x }\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trailing-slash form of a registry URL must resolve to the SAME
    /// index URL as the non-trailing-slash form -- a config typo (a stray
    /// `/`) shouldn't silently produce a different (404ing) request.
    #[test]
    fn fetch_package_with_tolerates_a_trailing_slash_on_the_registry_url() {
        let (_hash, urls) = mock_index_and_files("json2", "1.0.0", "pub fun f() -> Int { 1 }\n");
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-slash-{}", std::process::id()));
        let result = fetch_package_with(
            mock_fetcher(urls),
            "https://registry.example.com/",
            "json2",
            "1.0.0",
            &dir,
        );
        assert!(result.is_ok(), "{result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_package_with_reports_a_missing_version_cleanly() {
        let (_hash, urls) = mock_index_and_files("json2", "1.2.0", "pub fun f() -> Int { 1 }\n");
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-missing-version-{}", std::process::id()));
        let err = fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "json2", "9.9.9", &dir)
            .unwrap_err();
        assert!(err.contains("9.9.9") && err.contains("1.2.0"), "{err}");
        assert!(!dir.exists(), "nothing should be written on a resolve failure");
    }

    #[test]
    fn fetch_package_with_reports_an_unreachable_index_cleanly() {
        // no canned response for the index URL at all -- simulates a 404 or
        // network failure fetching the index itself.
        let err = fetch_package_with(
            mock_fetcher(HashMap::new()),
            "https://registry.example.com",
            "json2",
            "1.0.0",
            &std::env::temp_dir().join("kupl-registry-fetch-unreachable-does-not-matter"),
        )
        .unwrap_err();
        assert!(err.contains("json2"), "{err}");
    }

    #[test]
    fn fetch_package_with_reports_an_unreachable_file_cleanly() {
        // the index resolves fine, but the file's own URL has no canned
        // response -- simulates the index being reachable while its CDN
        // (a genuinely separate host, in a real deployment) is not.
        let (_hash, mut urls) = mock_index_and_files("json2", "1.0.0", "pub fun f() -> Int { 1 }\n");
        urls.retain(|k, _| k.contains(".json"));
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-unreachable-file-{}", std::process::id()));
        let err =
            fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "json2", "1.0.0", &dir)
                .unwrap_err();
        assert!(err.contains("main.kupl"), "{err}");
        assert!(!dir.exists());
    }

    /// A malicious/compromised registry serving package `evil`'s content
    /// under `honest`'s index URL must be rejected, not silently installed
    /// under the name the caller actually asked for.
    #[test]
    fn fetch_package_with_rejects_an_index_whose_name_does_not_match_the_request() {
        let index_url = "https://registry.example.com/honest.json".to_string();
        let index_json = r#"{"name": "evil", "versions": {"1.0.0": {"entry": "main.kupl", "files": {"main.kupl": {"url": "https://cdn.example.com/x", "hash": "aa"}}}}}"#.to_string();
        let urls = HashMap::from([(index_url, index_json)]);
        let err = fetch_package_with(
            mock_fetcher(urls),
            "https://registry.example.com",
            "honest",
            "1.0.0",
            &std::env::temp_dir().join("kupl-registry-fetch-name-mismatch-does-not-matter"),
        )
        .unwrap_err();
        assert!(err.contains("evil") && err.contains("honest"), "{err}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it683): unlike a
    /// `RegistryVersion`'s own file paths (registry-supplied, already
    /// guarded by `is_safe_relative_path`), `name`/`version` come from the
    /// CALLER -- ultimately a local `kupl.toml`'s dependency table key and
    /// version-pin string, which `manifest.rs` places no restriction on.
    /// Before this fix, a `..`-laden name/version reached
    /// `cache_dir.join(name).join(version)` unchecked, and `PathBuf::join`
    /// does NOT collapse `..` components -- `materialize`'s `std::fs::write`
    /// would resolve that path at the OS level, writing OUTSIDE the intended
    /// cache directory entirely (an arbitrary file write anywhere the
    /// current user can write, exploitable via a malicious/untrusted
    /// project's `kupl.toml`, e.g. one pulled in transitively). Now rejected
    /// cleanly before any network fetch or filesystem write happens.
    #[test]
    fn fetch_package_with_rejects_a_path_traversal_name_or_version() {
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-traversal-{}", std::process::id()));
        let escape_target = std::env::temp_dir().join("kupl-registry-fetch-traversal-escaped-file");
        let _ = std::fs::remove_file(&escape_target);

        // a `..`-laden name: without the fix, `dest` would climb OUT of `dir`
        // entirely (confirmed live: `PathBuf::join` leaves `..` literal).
        let err = fetch_package_with(
            mock_fetcher(HashMap::new()),
            "https://registry.example.com",
            "../../../../tmp/kupl-registry-fetch-traversal-escaped-file",
            "1.0.0",
            &dir,
        )
        .unwrap_err();
        assert!(err.contains("unsafe package name"), "{err}");

        // a `..`-laden version, with an otherwise well-formed name/index --
        // proves the SAME check applies to `version`, not just `name`.
        let (_hash, urls) = mock_index_and_files("json2", "1.0.0", "pub fun f() -> Int { 1 }\n");
        let err = fetch_package_with(
            mock_fetcher(urls),
            "https://registry.example.com",
            "json2",
            "../../../../tmp/kupl-registry-fetch-traversal-escaped-file",
            &dir,
        )
        .unwrap_err();
        assert!(err.contains("unsafe package version"), "{err}");

        // an absolute-path name/version is rejected the same way.
        assert!(fetch_package_with(
            mock_fetcher(HashMap::new()),
            "https://registry.example.com",
            "/etc/passwd",
            "1.0.0",
            &dir,
        )
        .unwrap_err()
        .contains("unsafe package name"));

        // nothing was ever written, inside `dir` OR at the attempted escape target.
        assert!(!dir.exists());
        assert!(!escape_target.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
    /// PR-it930, a close-read survey finding re-examining PR-it921's own
    /// case-collision fix from a different angle): `cache_dir` is a single,
    /// GLOBAL, per-USER directory shared across every project on the
    /// machine, keyed by package name verbatim -- on a case-insensitive
    /// filesystem, fetching `lib` when an unrelated `Lib` is already cached
    /// would silently overwrite `Lib`'s content (this module's own design
    /// never cache-skips, always writes fresh). This test simulates the
    /// case-insensitive-collision scenario directly (no real case-
    /// insensitive filesystem needed to prove the CHECK fires): pre-creates
    /// a cache entry under a name, then fetches a DIFFERENT name that
    /// happens to be case-INSENSITIVE-equal to it -- must be a clean error,
    /// and the pre-existing entry's content must be completely untouched.
    #[test]
    fn fetch_package_with_refuses_to_overwrite_a_case_colliding_cached_package() {
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-case-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let existing = dir.join("Lib").join("1.0.0");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("kupl.toml"), "[project]\nname = \"Lib\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(existing.join("main.kupl"), "pub fun greet() -> Str { \"original-Lib-content\" }\n").unwrap();

        let (_hash, urls) = mock_index_and_files("lib", "1.0.0", "pub fun greet() -> Str { \"malicious-or-unrelated-lib-content\" }\n");
        let err = fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "lib", "1.0.0", &dir)
            .unwrap_err();
        assert!(err.contains("collides"), "{err}");
        assert!(err.contains("Lib"), "{err}");

        // the pre-existing `Lib` cache entry must be completely untouched --
        // NOT `dir.join("lib")` (on a case-insensitive filesystem that IS
        // the same path as `existing` itself, so checking it separately
        // would prove nothing).
        assert_eq!(
            std::fs::read_to_string(existing.join("main.kupl")).unwrap(),
            "pub fun greet() -> Str { \"original-Lib-content\" }\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it1060, a background
    /// close-read survey finding re-examining the case-collision test above
    /// from a different angle): that test only covers a collision on the
    /// package NAME -- this proves the analogous collision on the VERSION
    /// component of the SAME package is caught too. Confirmed live BEFORE
    /// this fix: fetching `1.0.0-rc` after `1.0.0-RC` was already cached
    /// silently overwrote the original version's content (same on-disk
    /// directory on a case-insensitive filesystem), with `Ok(())` returned
    /// and no diagnostic at all.
    #[test]
    fn fetch_package_with_refuses_to_overwrite_a_case_colliding_cached_version() {
        let dir = std::env::temp_dir()
            .join(format!("kupl-registry-fetch-version-case-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let existing = dir.join("json2").join("1.0.0-RC");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("kupl.toml"), "[project]\nname = \"json2\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(existing.join("main.kupl"), "pub fun greet() -> Str { \"original-1.0.0-RC-content\" }\n").unwrap();

        let (_hash, urls) =
            mock_index_and_files("json2", "1.0.0-rc", "pub fun greet() -> Str { \"unrelated-1.0.0-rc-content\" }\n");
        let err = fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "json2", "1.0.0-rc", &dir)
            .unwrap_err();
        assert!(err.contains("collides"), "{err}");
        assert!(err.contains("1.0.0-RC"), "{err}");

        // the pre-existing `1.0.0-RC` cache entry must be completely untouched.
        assert_eq!(
            std::fs::read_to_string(existing.join("main.kupl")).unwrap(),
            "pub fun greet() -> Str { \"original-1.0.0-RC-content\" }\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_package_with_serializes_racing_fetches_for_case_colliding_names_instead_of_silently_corrupting_one(
    ) {
        use std::sync::{Arc, Barrier};
        let dir = std::env::temp_dir().join(format!("kupl-registry-cross-name-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (_hash_a, urls_a) = mock_index_and_files("Lib", "1.0.0", "package-A-content\n");
        let (_hash_b, urls_b) = mock_index_and_files("lib", "1.0.0", "package-B-content\n");

        let fetch_a = move |url: &str| -> Result<String, String> {
            std::thread::sleep(std::time::Duration::from_millis(15));
            urls_a.get(url).cloned().ok_or_else(|| format!("mock: no canned response for {url}"))
        };
        let fetch_b = move |url: &str| -> Result<String, String> {
            std::thread::sleep(std::time::Duration::from_millis(15));
            urls_b.get(url).cloned().ok_or_else(|| format!("mock: no canned response for {url}"))
        };

        let start = Arc::new(Barrier::new(2));
        let start_a = Arc::clone(&start);
        let start_b = Arc::clone(&start);
        let dir_a = dir.clone();
        let dir_b = dir.clone();
        let handle_a = std::thread::spawn(move || {
            start_a.wait();
            fetch_package_with(fetch_a, "https://registry.example.com", "Lib", "1.0.0", &dir_a)
        });
        let handle_b = std::thread::spawn(move || {
            start_b.wait();
            fetch_package_with(fetch_b, "https://registry.example.com", "lib", "1.0.0", &dir_b)
        });
        let result_a = handle_a.join().unwrap();
        let result_b = handle_b.join().unwrap();

        let results = [&result_a, &result_b];
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            ok_count, 1,
            "exactly one of two case-colliding racers may win, never both, never neither: {result_a:?} / {result_b:?}"
        );
        let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert!(
            err.contains("collides with the already-cached package"),
            "the loser must get the SAME clean collision error a non-racing second fetch already gets, \
             not a corrupted silent success or an unrelated I/O error: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, live-confirmed DESTRUCTIVE cache-corruption bug found+fixed
    /// (production-hardening PR-it1096, a two-phase self-scoping survey
    /// finding): `is_safe_relative_path`'s own multi-component allowance
    /// (needed for registry FILE paths) was reused unchanged for `name`/
    /// `version` too, so a nested version like `"beta/preview"` was
    /// accepted. Confirmed live BEFORE this fix, via a standalone probe
    /// mirroring this exact test: fetching `widgets` version
    /// `"beta/preview"`, then ORDINARILY re-fetching `widgets` version
    /// `"beta"` (a plain sibling value, no traversal or case tricks)
    /// returned `Ok` for the second fetch with ZERO diagnostic, while
    /// `atomic_replace`'s own unconditional `remove_dir_all(dest)` silently
    /// destroyed the ENTIRE previously-fetched, already-hash-verified
    /// `beta/preview` version's directory tree -- `dest` for `version =
    /// "beta"` (`cache_dir/widgets/beta`) is the ANCESTOR of `dest` for
    /// `version = "beta/preview"` (`cache_dir/widgets/beta/preview`), and
    /// the existing version-collision guard (PR-it1060) only ever compares
    /// EXACT sibling entries, never detecting an ancestor/descendant
    /// relationship. Now rejected outright at the FIRST (nested) fetch,
    /// since `is_safe_relative_path_single_component` rejects any
    /// multi-component name/version before either fetch can ever reach the
    /// filesystem.
    #[test]
    fn fetch_package_with_rejects_a_multi_component_version_that_could_ancestor_collide() {
        let dir = std::env::temp_dir().join(format!("kupl-registry-nested-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (_h1, urls1) = mock_index_and_files("widgets", "beta/preview", "pub fun v() -> Str { \"preview-content\" }\n");
        let err = fetch_package_with(mock_fetcher(urls1), "https://registry.example.com", "widgets", "beta/preview", &dir)
            .unwrap_err();
        assert!(err.contains("unsafe package version"), "{err}");
        assert!(!dir.exists(), "nothing should be written when the version itself is rejected");

        // a multi-component NAME is rejected the same way.
        let (_h2, urls2) = mock_index_and_files("ns/widgets", "1.0.0", "pub fun v() -> Str { \"x\" }\n");
        let err2 = fetch_package_with(mock_fetcher(urls2), "https://registry.example.com", "ns/widgets", "1.0.0", &dir)
            .unwrap_err();
        assert!(err2.contains("unsafe package name"), "{err2}");

        // an ORDINARY, single-component name/version is completely unaffected.
        let (_h3, urls3) = mock_index_and_files("widgets", "1.0.0", "pub fun v() -> Str { \"ok-content\" }\n");
        let ok = fetch_package_with(mock_fetcher(urls3), "https://registry.example.com", "widgets", "1.0.0", &dir);
        assert!(ok.is_ok(), "{ok:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_package_with_refuses_a_file_whose_fetched_content_fails_the_hash_check() {
        let main_url = "https://cdn.example.com/json2/1.0.0/main.kupl".to_string();
        let index_json = format!(
            r#"{{"name": "json2", "versions": {{"1.0.0": {{"entry": "main.kupl", "files": {{"main.kupl": {{"url": "{main_url}", "hash": "deadbeef"}}}}}}}}}}"#
        );
        let urls = HashMap::from([
            ("https://registry.example.com/json2.json".to_string(), index_json),
            (main_url, "tampered in transit".to_string()),
        ]);
        let dir = std::env::temp_dir().join(format!("kupl-registry-fetch-tampered-{}", std::process::id()));
        let err = fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "json2", "1.0.0", &dir)
            .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(!dir.exists(), "a hash-mismatched fetch must never be written to disk");
    }

    /// Proves the fetch layer's own central claim end to end: a package
    /// fetched (from canned, in-memory responses standing in for a real
    /// registry) through `fetch_package_with` produces a directory that
    /// `loader.rs`'s EXISTING local-path-dependency machinery loads and
    /// runs completely unchanged -- extending PR-it631's equivalent proof
    /// for `materialize` alone to cover the fetch orchestration around it.
    #[test]
    fn a_fetched_package_loads_and_runs_exactly_like_a_local_dependency() {
        let toml_content = "[project]\nname = \"math\"\nentry = \"main.kupl\"\n";
        let main_content = "pub fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n";
        let toml_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(toml_content)));
        let main_hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(main_content)));
        let toml_url = "https://cdn.example.com/math/1.0.0/kupl.toml".to_string();
        let main_url = "https://cdn.example.com/math/1.0.0/main.kupl".to_string();
        let index_json = format!(
            r#"{{"name": "math", "versions": {{"1.0.0": {{"entry": "main.kupl", "files": {{
                "kupl.toml": {{"url": "{toml_url}", "hash": "{toml_hash}"}},
                "main.kupl": {{"url": "{main_url}", "hash": "{main_hash}"}}
            }}}}}}}}"#
        );
        let urls = HashMap::from([
            ("https://registry.example.com/math.json".to_string(), index_json),
            (toml_url, toml_content.to_string()),
            (main_url, main_content.to_string()),
        ]);

        let base = std::env::temp_dir().join(format!("kupl-registry-fetch-e2e-{}", std::process::id()));
        let math_cache =
            fetch_package_with(mock_fetcher(urls), "https://registry.example.com", "math", "1.0.0", &base)
                .expect("fetches and materializes cleanly");

        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            format!(
                "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = {{ path = \"{}\" }}\n",
                math_cache.display()
            ),
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use math\n\nfun main() uses io {\n    print(math.add(2, 3))\n}\n",
        )
        .unwrap();

        let (program, _map) = crate::loader::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its fetched math dependency");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(_) => {}
            Err(_) => panic!("main() should run cleanly against the fetched dependency"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL, SEVERE bug found+fixed (production-hardening PR-it921, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `parse_index`'s own doc comment above explains the
    /// full mechanism -- a registry index declaring two file paths that
    /// only differ by case (`"main.kupl"`/`"Main.kupl"`) address the SAME
    /// real file on a case-insensitive filesystem (macOS/Windows default),
    /// but nothing cross-checked this before this fix. Live-confirmed
    /// BEFORE this fix, across 5 separate process runs of the IDENTICAL
    /// index (each with Rust's own randomized-per-process `HashMap` seed):
    /// `materialize` returned `Ok(())` every time, but WHICH of the two
    /// files' content ended up on disk as `main.kupl` was DIFFERENT nearly
    /// every run -- a genuinely non-reproducible silent-value-corruption
    /// bug, not just a theoretical one.
    #[test]
    fn a_registry_index_declaring_two_case_colliding_file_paths_is_a_clean_parse_error() {
        let honest_hash =
            crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv("pub fun f() -> Int { 1 }\n")));
        let text = format!(
            "{{\"name\":\"pkg\",\"versions\":{{\"1.0.0\":{{\"entry\":\"main.kupl\",\"files\":{{\
             \"main.kupl\":{{\"url\":\"https://example.com/main.kupl\",\"hash\":\"{honest_hash}\"}},\
             \"Main.kupl\":{{\"url\":\"https://example.com/Main.kupl\",\"hash\":\"{honest_hash}\"}}\
             }}}}}}}}"
        );
        let err = parse_index(&text).expect_err("two case-colliding declared paths must be a clean parse error");
        assert!(
            err.contains("collide") && err.contains("main.kupl") && err.contains("Main.kupl"),
            "must name BOTH colliding paths: {err}"
        );

        // an ordinary index with NO case collision is completely unaffected.
        let text_ok = format!(
            "{{\"name\":\"pkg\",\"versions\":{{\"1.0.0\":{{\"entry\":\"main.kupl\",\"files\":{{\
             \"main.kupl\":{{\"url\":\"https://example.com/main.kupl\",\"hash\":\"{honest_hash}\"}},\
             \"kupl.toml\":{{\"url\":\"https://example.com/kupl.toml\",\"hash\":\"{honest_hash}\"}}\
             }}}}}}}}"
        );
        assert!(parse_index(&text_ok).is_ok(), "an ordinary, non-colliding index must still parse cleanly");
    }

    /// The `materialize`-level defense-in-depth twin of the test above --
    /// see that fix's own doc comment (immediately above `materialize`'s
    /// case-fold check) for why this is checked at BOTH layers, mirroring
    /// `is_safe_relative_path`'s own identical dual-check precedent.
    #[test]
    fn materialize_rejects_two_case_colliding_paths_even_in_a_hand_built_registryversion() {
        let honest = "pub fun f() -> Int { 1 }\n";
        let evil = "pub fun f() -> Int { 999 }\n";
        let version = RegistryVersion {
            entry: "main.kupl".to_string(),
            files: HashMap::from([
                (
                    "main.kupl".to_string(),
                    RegistryFile {
                        url: "https://example.com/main.kupl".to_string(),
                        hash: crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(honest))),
                    },
                ),
                (
                    "Main.kupl".to_string(),
                    RegistryFile {
                        url: "https://example.com/Main.kupl".to_string(),
                        hash: crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(evil))),
                    },
                ),
            ]),
        };
        let contents = HashMap::from([
            ("main.kupl".to_string(), honest.to_string()),
            ("Main.kupl".to_string(), evil.to_string()),
        ]);
        let dir = std::env::temp_dir().join(format!("kupl-registry-case-collision-test-{}", std::process::id()));
        let result = materialize(&version, &contents, &dir);
        assert!(
            result.is_err() && result.as_ref().unwrap_err().contains("collide"),
            "a hand-built RegistryVersion with case-colliding paths must be rejected, not silently \
             overwrite one file's verified content with another's on disk: {result:?}"
        );
        assert!(!dir.exists(), "nothing should be written to disk on rejection: {result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
