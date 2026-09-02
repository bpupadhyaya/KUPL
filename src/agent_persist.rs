//! `durable agent` state persistence (`docs/design/AGENTS.md` §5,
//! "identity & memory") — an agent's own `state` fields survive not just
//! an in-process supervised restart (already true for every agent,
//! `durable` or not, see `interp.rs::restart`'s own `!comp.is_agent`
//! check) but a SEPARATE later invocation of the same program.
//!
//! Deliberately narrow, matching this whole initiative's own "ship a
//! real, honest v1 slice, name what's deferred" discipline (the same
//! posture `weight distributed`'s security section took): this covers a
//! `durable agent` whose OWNING PROCESS exits cleanly, for every `weight`
//! value INCLUDING `distributed` — `Interp::serve_distributed_connection`
//! (the `kupl node` server side) calls the SAME `instantiate_local`/
//! `stop_all` functions the load/save hooks below live inside, so the
//! combination just works, no special-casing needed (an initial K1007
//! checker restriction assumed otherwise; retired the same day, once
//! live testing proved that assumption wrong). For `weight distributed`
//! specifically, the state file lives on the NODE's own filesystem
//! (wherever the agent actually runs), not the client's.
//! Explicitly NOT covered, not silently glossed over:
//!
//! - **Crash-consistency.** `stop_all` (and therefore the save hook
//!   below) only runs on a successful `kupl run` — a genuine KUPL panic
//!   or an OS-level crash loses everything accumulated since the last
//!   successful save. This isn't a new gap this feature introduces; it
//!   inherits `run.rs::run_program`'s own pre-existing "`on stop` (and
//!   now persistence) only fires on success" asymmetry.
//! - **Concurrent-writer safety.** Two processes (or two live instances
//!   of the same durable agent TYPE in one process) racing on the same
//!   file is undefined — last `rename` wins, no lock file.
//! - **Schema migration.** Persisting a `PortableValue::Map` KEYED BY
//!   FIELD NAME (not position) gives free tolerance for an added/removed
//!   `state` field between versions (an unknown key in the file is
//!   ignored on load; a `state` field with no matching key in the file
//!   just keeps its ordinary `init` value) — but NOT safety for a field
//!   whose TYPE changed: a stale value of the old type binds to the
//!   current field name and may fail deep inside that agent's own code.
//! - **Transactional guarantees / fsync.** `rename` over a `.tmp` file
//!   avoids a torn write (the OS never observes a half-written file at
//!   the real path); it is not a durability guarantee against power loss.
//! - **Per-instance identity.** Keyed by the agent's DECLARATION name —
//!   one state file per agent type, not per dynamically-spawned instance
//!   (mirrors `weight distributed`'s own "one connection hosts exactly
//!   one actor" v1 narrowing).
//!
//! Reuses `kser`'s existing, already-tested binary encoding entirely —
//! persisted state is just a `PortableValue::Map(field_name -> value)`,
//! the same `to_bytes`/`from_bytes` already used for the network wire
//! format. No new serialization code needed.

use crate::parallel::PortableValue;
use crate::value::{Env, Value};

/// Where persisted `durable agent` state files live -- overridable so a
/// deployment (or a test) can point this somewhere other than the
/// current directory. Mirrors `distribution::KUPL_DISTRIBUTED_NODE_ENV`'s
/// own "env var, not a new CLI flag" convention.
pub const KUPL_AGENT_STATE_DIR_ENV: &str = "KUPL_AGENT_STATE_DIR";

fn state_dir() -> std::path::PathBuf {
    match std::env::var(KUPL_AGENT_STATE_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(".kupl/agent-state"),
    }
}

fn state_path(agent_name: &str) -> std::path::PathBuf {
    state_dir().join(format!("{agent_name}.kstate"))
}

/// Save `agent_name`'s own `state` fields (read from `env`, which the
/// caller guarantees is that instance's OWN env) to disk. Best-effort:
/// an I/O failure (permission denied, disk full, an unwritable
/// directory) is reported to stderr and otherwise silently skipped --
/// a failed save must never turn an otherwise-successful `kupl run`
/// into a hard failure, matching how `stop_all`'s own graceful-shutdown
/// path is itself already best-effort elsewhere in this codebase.
///
/// If any single `state` field's current value isn't portable (holds a
/// closure, a live component/instance reference, or anything else
/// `parallel::to_portable` rejects), the WHOLE save for this agent is
/// skipped -- a partial save (some fields persisted, others silently
/// dropped) would silently corrupt the agent's own next-load identity,
/// which is worse than not persisting at all.
pub fn save(agent_name: &str, state_fields: &[String], env: &Env) {
    let mut fields = Vec::with_capacity(state_fields.len());
    for name in state_fields {
        let Some(v) = env.get(name) else { continue };
        let Some(pv) = crate::parallel::to_portable(&v) else {
            eprintln!(
                "kupl: warning: `durable agent {agent_name}`'s state field `{name}` is not portable (holds a closure, live component reference, or similar) -- state not persisted this run"
            );
            return;
        };
        fields.push((PortableValue::Str(name.clone()), pv));
    }
    let bytes = crate::kser::to_bytes(&PortableValue::Map(fields));
    let path = state_path(agent_name);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        eprintln!("kupl: warning: could not create {} for `durable agent {agent_name}`'s state -- not persisted", parent.display());
        return;
    }
    let tmp = path.with_extension("kstate.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        eprintln!("kupl: warning: could not write {} for `durable agent {agent_name}`'s state: {e} -- not persisted", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("kupl: warning: could not finalize {} for `durable agent {agent_name}`'s state: {e} -- not persisted", path.display());
    }
}

/// Load `agent_name`'s own previously-persisted state, if any. `None`
/// (not an error) for "no state file exists yet" (the ordinary case: the
/// very first run of a durable agent) -- the caller falls back to each
/// `state` field's own `init` expression, exactly like a fresh, never-
/// persisted agent already does. A CORRUPT or malformed file (a
/// genuinely unexpected case: hand-edited, truncated by a crash mid-
/// write despite the rename-based save, or written by an incompatible
/// future format) is reported to stderr and also treated as `None` --
/// falling back to fresh `init` defaults rather than failing the whole
/// program's startup over a damaged persistence file.
pub fn load(agent_name: &str) -> Option<Vec<(String, Value)>> {
    let path = state_path(agent_name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("kupl: warning: could not read {} for `durable agent {agent_name}`'s state: {e} -- starting fresh", path.display());
            return None;
        }
    };
    let pv = match crate::kser::from_bytes(&bytes) {
        Ok(pv) => pv,
        Err(e) => {
            eprintln!("kupl: warning: {} for `durable agent {agent_name}` is corrupt ({e}) -- starting fresh", path.display());
            return None;
        }
    };
    let PortableValue::Map(entries) = pv else {
        eprintln!("kupl: warning: {} for `durable agent {agent_name}` has an unexpected shape -- starting fresh", path.display());
        return None;
    };
    let mut out = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        if let PortableValue::Str(name) = k {
            out.push((name, crate::parallel::from_portable(&v)));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own isolated state directory (a fresh temp dir,
    /// pointed at via `KUPL_AGENT_STATE_DIR_ENV`) so parallel test
    /// threads never race on the same files -- `std::env::set_var` is
    /// process-wide, so this ALSO serializes against any other test in
    /// this same process that touches the env var, via a shared mutex.
    fn with_isolated_state_dir<F: FnOnce()>(f: F) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kupl-agent-persist-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY (rustc 2024-edition-style lint on set_var): single-
        // threaded with respect to THIS env var specifically, enforced by
        // `LOCK` above -- no other test reads/writes
        // `KUPL_AGENT_STATE_DIR_ENV` concurrently.
        unsafe { std::env::set_var(KUPL_AGENT_STATE_DIR_ENV, &dir) };
        f();
        unsafe { std::env::remove_var(KUPL_AGENT_STATE_DIR_ENV) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_with_no_prior_state_file_returns_none() {
        with_isolated_state_dir(|| {
            assert!(load("NeverPersistedAgent").is_none());
        });
    }

    #[test]
    fn save_then_load_roundtrips_state_fields() {
        with_isolated_state_dir(|| {
            let env = Env::new();
            env.define("count", Value::Int(42));
            env.define("name", Value::Str(std::rc::Rc::new("alice".to_string())));
            save("Rep", &["count".to_string(), "name".to_string()], &env);

            let loaded = load("Rep").expect("must find the just-saved state");
            let map: std::collections::HashMap<_, _> = loaded.into_iter().collect();
            assert!(matches!(map.get("count"), Some(Value::Int(42))));
            assert!(matches!(map.get("name"), Some(Value::Str(s)) if s.as_str() == "alice"));
        });
    }

    /// A field present in the FILE but no longer in the CURRENT agent's
    /// own `state` list (a stale key from before a field was removed)
    /// must not crash the load -- `load` returns everything the file
    /// has; it's the CALLER's job (interp.rs's own load hook) to only
    /// apply entries whose name matches a real `state` field. This test
    /// only proves `load` itself is tolerant.
    #[test]
    fn load_tolerates_extra_keys_the_caller_will_filter() {
        with_isolated_state_dir(|| {
            let env = Env::new();
            env.define("kept", Value::Int(1));
            env.define("removed_field", Value::Int(2));
            save("Rep", &["kept".to_string(), "removed_field".to_string()], &env);

            let loaded = load("Rep").unwrap();
            assert_eq!(loaded.len(), 2, "load itself returns everything in the file, unfiltered");
        });
    }

    #[test]
    fn a_corrupt_state_file_is_reported_and_treated_as_absent() {
        with_isolated_state_dir(|| {
            let path = state_path("Corrupt");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"not a valid kser file at all").unwrap();
            assert!(load("Corrupt").is_none(), "a corrupt file must fall back to None, not panic or return garbage");
        });
    }

    #[test]
    fn saving_a_non_portable_field_skips_the_whole_save_not_a_partial_one() {
        with_isolated_state_dir(|| {
            let env = Env::new();
            env.define("ok_field", Value::Int(1));
            // A `Value::Fun` (closure/function reference) is never portable.
            env.define("bad_field", Value::Fun(std::rc::Rc::new("some_fun".to_string())));
            save("PartiallyBad", &["ok_field".to_string(), "bad_field".to_string()], &env);
            assert!(load("PartiallyBad").is_none(), "a non-portable field must skip the WHOLE save, not persist ok_field alone");
        });
    }

    #[test]
    fn state_dir_env_var_is_honored() {
        with_isolated_state_dir(|| {
            let env = Env::new();
            env.define("x", Value::Int(7));
            save("EnvDirAgent", &["x".to_string()], &env);
            let expected = state_dir().join("EnvDirAgent.kstate");
            assert!(expected.exists(), "save must write under KUPL_AGENT_STATE_DIR: {expected:?}");
        });
    }
}
