//! `kupl run --sandbox`: an opt-in OS-level confinement wrapper.
//!
//! `docs/PRODUCTION.md`'s own threat model is explicit that KUPL's effect
//! system is a *compile-time* discipline, not a runtime sandbox — a
//! program that declares `uses io` can do arbitrary I/O, including
//! spawning subprocesses (`exec`) and reaching the network, and the
//! existing resource limits (`--timeout`, `--max-memory`) bound CPU time
//! and allocation, not *what* a program is allowed to touch. This module
//! is the first real answer to that gap: a genuine OS-enforced
//! confinement layer, not a second in-process check that inherits the
//! same "only as strong as this process's own code" limitation the effect
//! system already has.
//!
//! **Mechanism.** A process cannot sandbox itself from the inside after
//! it has already started — the confinement has to be applied by the OS
//! at (or before) the point the confined code begins running. This module
//! re-executes the SAME `kupl` binary as a child process under the
//! platform's own sandboxing tool (`sandbox-exec` on macOS), stripping
//! `--sandbox` from the forwarded arguments so the child doesn't try to
//! sandbox itself again, and forwards the child's stdio and exit code
//! transparently.
//!
//! **Policy (v1, deliberately narrow and stated precisely, not vaguely
//! "sandboxed")**: no network access at all, and filesystem writes
//! confined to the OS temp directory — the two risks `docs/PRODUCTION.md`
//! names most concretely ("exfiltrating data" needs network; "touch disk"
//! needs writes outside a safe area). Filesystem READS are deliberately
//! NOT restricted in this v1 (a named scope limitation, not an oversight)
//! — restricting reads meaningfully (without breaking ordinary program
//! behavior) needs a per-invocation allowlist this first cut doesn't
//! attempt. Process spawning (`exec` builtin) is allowed, and any child
//! process it spawns inherits the SAME sandbox restrictions (confirmed
//! live: `sandbox-exec` confines the whole process tree it launches, not
//! just the top-level process) — this makes `exec("curl", ...)` fail the
//! same way a direct network call would, not a way to escape the sandbox.
//!
//! **Platform support: macOS only in this v1.** `sandbox-exec` is
//! deprecated by Apple in favor of the App Sandbox entitlement system,
//! but remains a real, functional, widely-used mechanism for exactly this
//! kind of CLI-tool confinement (Nix and other developer tools rely on
//! it the same way). Linux support (a `bubblewrap`/seccomp-based
//! equivalent) is a follow-up increment, not implemented here — `kupl run
//! --sandbox` on any other platform reports a clean, honest error rather
//! than silently running unconfined.

/// Whether `--sandbox` is present in the raw CLI args. Scoped identically
/// to `timeout::parse_flag`/`memcap::parse_flag` — a bare boolean flag
/// (no `=value`), so no malformed-value error path exists.
pub fn requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--sandbox")
}

/// Whether this platform has a real, implemented sandboxing backend.
pub fn is_supported() -> bool {
    cfg!(target_os = "macos")
}

/// The `sandbox-exec` profile enforcing this module's own documented v1
/// policy. `(deny default)` first, then an explicit allowlist — a
/// deny-by-default profile with narrow allows is the safe direction to
/// err in if a future edit to this list gets a clause wrong, unlike an
/// allow-by-default profile with narrow denies.
fn macos_profile() -> String {
    // `std::env::temp_dir()` reports `/var/folders/...` on macOS, but `/var`
    // is itself a symlink to `/private/var` -- and `sandbox-exec` enforces
    // `(subpath ...)` against the CANONICAL (symlink-resolved) path a
    // syscall actually touches, not the literal string given here. Without
    // resolving the symlink first, a real write to the reported temp dir is
    // denied even though it's exactly the directory this profile means to
    // allow (confirmed live: a sandboxed `touch` under the un-resolved path
    // failed with "Operation not permitted"). `canonicalize` needs the path
    // to exist, which the OS temp dir always does; fall back to the raw path
    // if that ever isn't true rather than panicking.
    let tmp = std::env::temp_dir();
    let tmp = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    let tmp = tmp.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"(version 1)
(deny default)
(deny network*)
(allow process-fork)
(allow process-exec)
(allow file-read*)
(allow file-write* (subpath "{tmp}"))
(allow mach-lookup)
(allow sysctl-read)
(allow signal (target self))
"#
    )
}

/// Re-exec the current `kupl` binary under the platform's own sandboxing
/// tool, with `--sandbox` stripped from `args` (the child must not try to
/// sandbox itself again — this process IS the sandboxing step for it).
/// Returns the child's own exit code, or an error message if the
/// sandboxing tool itself couldn't be launched (a setup failure, not the
/// sandboxed program's own exit status).
///
/// Deliberately spawn-and-wait rather than a real `exec()` syscall
/// replacement (which would need `unsafe` and platform-specific FFI to
/// replace this process's own image) — simpler, portable, and the
/// performance cost of one extra process in the tree is irrelevant next
/// to the cost of spawning `sandbox-exec` itself.
pub fn run_sandboxed(current_exe: &std::path::Path, args: &[String]) -> Result<i32, String> {
    if !is_supported() {
        return Err(
            "`--sandbox` is not yet supported on this platform (macOS only in this release) -- \
             see docs/PRODUCTION.md's Known Limitations. Run without --sandbox, or inside your \
             own OS-level sandbox (container, VM, seccomp/cgroup limits) instead."
                .to_string(),
        );
    }
    let forwarded: Vec<&str> = args.iter().filter(|a| a.as_str() != "--sandbox").map(String::as_str).collect();
    let profile = macos_profile();
    let status = std::process::Command::new("sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg(current_exe)
        .args(&forwarded)
        .status()
        .map_err(|e| format!("could not launch sandbox-exec: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_detects_the_bare_flag_only() {
        assert!(requested(&["run".to_string(), "--sandbox".to_string(), "x.kupl".to_string()]));
        assert!(!requested(&["run".to_string(), "x.kupl".to_string()]));
        // not a substring match -- a real flag some caller might confuse this with
        assert!(!requested(&["run".to_string(), "--sandbox-strict".to_string()]));
    }

    #[test]
    fn macos_profile_contains_the_documented_policy_shape() {
        let p = macos_profile();
        assert!(p.contains("(deny default)"));
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(allow file-write*"));
    }

    /// The actual, live-verified property this whole module exists for:
    /// a real subprocess, actually sandboxed, cannot reach the network --
    /// not a claim, a fact confirmed by running a real network client
    /// (`curl`) under the exact profile this module generates and
    /// observing it fail to connect (not merely fail DNS resolution,
    /// which would be a much weaker, easily-misread signal).
    #[test]
    #[cfg(target_os = "macos")]
    fn the_generated_profile_actually_blocks_a_real_network_connection() {
        if std::process::Command::new("sandbox-exec").arg("-h").output().is_err() {
            return; // sandbox-exec itself unavailable in this environment -- skip, don't fail
        }
        if std::process::Command::new("curl").arg("--version").output().is_err() {
            return; // curl unavailable -- skip, don't fail
        }
        let profile = macos_profile();
        // A direct IP (not a hostname) so a failure can only mean the
        // CONNECT itself was blocked, not that DNS resolution failed --
        // a real, previously-considered false-positive risk for this
        // exact test (see this module's own doc comment on how the
        // property was originally verified by hand).
        let out = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("curl")
            .args(["-s", "-m", "3", "http://93.184.215.14"])
            .output()
            .expect("sandbox-exec runs");
        assert!(!out.status.success(), "a sandboxed curl must fail to connect, got: {out:?}");
    }

    /// The companion positive control: the SAME sandbox profile must
    /// still allow a write to the OS temp directory -- proving the
    /// profile doesn't accidentally deny everything (which would make the
    /// network-denial test above meaningless -- a profile that blocks
    /// EVERYTHING trivially "blocks" network too, without proving the
    /// policy is actually the narrow one this module documents).
    #[test]
    #[cfg(target_os = "macos")]
    fn the_generated_profile_still_allows_writing_to_the_temp_directory() {
        if std::process::Command::new("sandbox-exec").arg("-h").output().is_err() {
            return;
        }
        let profile = macos_profile();
        let marker = std::env::temp_dir().join(format!("kupl-sandbox-write-probe-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let out = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/usr/bin/touch")
            .arg(&marker)
            .output()
            .expect("sandbox-exec runs");
        assert!(out.status.success(), "a sandboxed write to the temp dir must succeed: {out:?}");
        assert!(marker.exists(), "the file must actually have been created");
        let _ = std::fs::remove_file(&marker);
    }
}
