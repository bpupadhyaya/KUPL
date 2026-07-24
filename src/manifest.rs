//! A tiny, self-contained reader for the `kupl.toml` manifest subset (zero
//! dependencies, like `src/json.rs`). This is NOT a general TOML parser — it
//! understands exactly what `kupl.toml` uses:
//!
//! ```toml
//! [project]
//! name = "my-app"
//! version = "0.1.0"
//! entry = "main.kupl"
//!
//! [dependencies]
//! math  = { path = "../math" }        # inline table
//! util  = "vendor/util"                # bare-string shorthand (a path)
//! json2 = { version = "1.2.0" }        # registry (resolved later)
//! ```

/// A single dependency declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Dep {
    pub name: String,
    pub path: Option<String>,
    pub version: Option<String>,
}

/// A parsed `kupl.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub deps: Vec<Dep>,
}

/// Parse manifest text. Unknown `[project]` keys are ignored; a syntactically
/// malformed line is an error.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let mut name = String::new();
    let mut version = String::new();
    let mut entry = "main.kupl".to_string();
    let mut deps: Vec<Dep> = Vec::new();
    let mut section = "";
    let mut seen_project = false;
    let mut seen_name = false;
    let mut seen_version = false;
    let mut seen_entry = false;

    for (i, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match sec.trim() {
                // A REAL bug found+fixed (production-hardening PR-it784, an
                // Explore survey finding, independently re-verified live
                // before implementing): a SECOND `[project]` section later
                // in the same file was accepted exactly like the first,
                // silently overwriting `name`/`version`/`entry` -- the same
                // "silently last-wins" shape this file already fixed twice
                // for a duplicate dependency NAME (PR-it747) and a
                // duplicate inline-table KEY (PR-it752), both citing "a
                // plausible copy-paste manifest mistake" as the motivating
                // scenario, which applies equally here. Confirmed live: a
                // dependency's OWN `kupl.toml` with `entry = "main.kupl"`
                // then a second `[project]` block with `entry =
                // "other.kupl"` silently compiled `other.kupl` in its
                // place, with NO diagnostic -- `dep.greet()` resolved to
                // `other.kupl`'s definition, not `main.kupl`'s.
                "project" if seen_project => {
                    return Err(format!("line {}: duplicate `[project]` section", i + 1));
                }
                "project" => {
                    seen_project = true;
                    "project"
                }
                "dependencies" => "dependencies",
                other => return Err(format!("line {}: unknown section `[{other}]`", i + 1)),
            };
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected `key = value`", i + 1))?;
        let key = key.trim();
        let value = value.trim();
        match section {
            "project" => {
                // A REAL bug found+fixed (production-hardening PR-it784, an
                // Explore survey finding, independently re-verified live
                // before implementing): this module's OWN doc comment above
                // promises "unknown `[project]` keys are ignored" -- but the
                // OLD code called `parse_string(value)` and propagated its
                // error via `?` BEFORE the `match key` dispatch even ran, so
                // an unrecognized key whose value wasn't a bare quoted
                // string (a bool, number, array, or inline table -- all
                // perfectly valid TOML, not "syntactically malformed" by
                // any reasonable reading) hard-failed the ENTIRE manifest
                // parse instead of being ignored. Confirmed live: `private
                // = true` under `[project]` failed with "line N: expected a
                // string", while the SAME key quoted (`private = "true"`)
                // parsed cleanly and was silently dropped -- the exact
                // "ignored" behavior the doc comment promises, just not for
                // every TOML value shape. This broke forward-compatibility
                // for precisely the case "ignore unknown keys" exists to
                // protect: a newer non-string field read by an older
                // binary. Fixed by moving the string-parse INSIDE each
                // KNOWN key's own arm, so an unknown key's value is never
                // even inspected, regardless of its TOML type.
                let parse_str =
                    |value: &str| parse_string(value).ok_or_else(|| format!("line {}: expected a string", i + 1));
                match key {
                    // A REAL bug found+fixed (production-hardening PR-it860, an
                    // Explore survey finding, independently re-verified live
                    // before implementing): a duplicate KEY within a single
                    // `[project]` block (e.g. `entry` declared twice) used to
                    // silently resolve "last one wins" -- the SAME shape this
                    // file already fixed for a duplicate dependency NAME
                    // (PR-it747), a duplicate inline-table KEY (PR-it752), and
                    // a duplicate `[project]` SECTION (PR-it784). For `entry`
                    // specifically this is silent-value-corruption-tier: it
                    // decides which physical file becomes a package's
                    // compiled module (see PR-it766's own comment below).
                    // Live-confirmed BEFORE this fix: a dependency's own
                    // `kupl.toml` with `entry = "main.kupl"` then a second
                    // `entry = "other.kupl"` line in the SAME `[project]`
                    // block silently compiled `other.kupl` in its place, with
                    // zero diagnostic.
                    "name" if seen_name => {
                        return Err(format!("line {}: duplicate key `name` in [project]", i + 1));
                    }
                    "version" if seen_version => {
                        return Err(format!("line {}: duplicate key `version` in [project]", i + 1));
                    }
                    "entry" if seen_entry => {
                        return Err(format!("line {}: duplicate key `entry` in [project]", i + 1));
                    }
                    "name" => {
                        seen_name = true;
                        name = parse_str(value)?;
                    }
                    "version" => {
                        seen_version = true;
                        version = parse_str(value)?;
                    }
                    "entry" => {
                        seen_entry = true;
                        let s = parse_str(value)?;
                        // A REAL, live-confirmed bug found+fixed (production-hardening
                        // PR-it766): `entry` was accepted VERBATIM with zero path
                        // validation, then resolved as `dep_dir.join(&m.entry)`
                        // (`loader.rs::resolve_deps`) / `dep_ctx.root.join(entry)`
                        // (`loader.rs`'s `use`-resolution path) -- but `PathBuf::join`
                        // silently DISCARDS its left-hand base entirely when the
                        // argument is itself absolute (a well-known Rust footgun), and
                        // a leading `..` component walks straight out of the
                        // dependency's own directory. A dependency's OWN `kupl.toml`
                        // could therefore point `entry` at ANY file the process can
                        // read anywhere on disk, and that file -- not anything inside
                        // the dependency's own directory -- silently became the
                        // dependency's compiled module content, with no diagnostic.
                        // Live-confirmed BEFORE this fix: a dependency `dep` whose
                        // `entry` pointed at an absolute path to a file OUTSIDE `dep`'s
                        // own directory entirely was compiled in place of `dep/
                        // main.kupl` -- `kupl run`/`kupl pkg tree` both silently used
                        // the out-of-tree file. `kupl new`'s own `valid_project_name`
                        // already rejects this exact class of path-traversal/absolute-
                        // path input for PROJECT NAMES (its own doc comment: "rejects
                        // path traversal (`../evil`, `/abs`, `a/b`)... the `.`/`..`
                        // specials") -- `entry` was the one manifest field the project
                        // hadn't applied that same discipline to.
                        let p = std::path::Path::new(&s);
                        if p.is_absolute() {
                            return Err(format!(
                                "line {}: `entry` must be a path relative to the project directory, not absolute (`{s}`)",
                                i + 1
                            ));
                        }
                        if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                            return Err(format!(
                                "line {}: `entry` must not contain `..` (would escape the project directory): `{s}`",
                                i + 1
                            ));
                        }
                        entry = s;
                    }
                    _ => {} // forward-compatible: ignore unknown project keys
                }
            }
            "dependencies" => {
                let dep = parse_dep(key, value, i + 1)?;
                // A REAL bug found+fixed (production-hardening PR-it747): a
                // duplicate dependency NAME (two separate `mth = { .. }`
                // lines under `[dependencies]`) used to silently resolve
                // "last one wins" -- `deps` is a plain `Vec`, and
                // `loader.rs`'s own `pkg_ctx` builds a `HashMap` from it via
                // a bare `.insert()`, discarding the earlier declaration
                // with zero diagnostic. A plausible copy-paste manifest
                // mistake (e.g. renaming one dependency but forgetting to
                // remove the old line) previously failed silently rather
                // than with a clean, actionable error -- now caught at
                // parse time, before the ambiguity can reach the loader at
                // all. (A narrower, lower-priority residual gap -- a
                // duplicate KEY within a single inline table, e.g.
                // `{ path = "a", path = "b" }` -- was a separate issue
                // inside `parse_dep`'s own field parsing, NOT covered by
                // this check; fixed separately in `parse_dep` itself,
                // production-hardening PR-it752.)
                if deps.iter().any(|d: &Dep| d.name == dep.name) {
                    return Err(format!(
                        "line {}: duplicate dependency `{}` (already declared earlier in [dependencies])",
                        i + 1,
                        dep.name
                    ));
                }
                deps.push(dep);
            }
            "" => return Err(format!("line {}: key `{key}` before any `[section]`", i + 1)),
            _ => {}
        }
    }
    Ok(Manifest { name, version, entry, deps })
}

/// Read and parse a `kupl.toml` at `path`.
pub fn read(path: &std::path::Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text)
}

fn strip_comment(line: &str) -> &str {
    // `#` starts a comment unless inside a string; kupl.toml values are simple,
    // so only honor `#` when it is not preceded by an unclosed quote.
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Split `s` on `delim`, but never while inside a quoted string (the SAME
/// `in_str` toggle-on-`"` technique `strip_comment` above already uses for
/// `#`) -- so `{ path = "my,dir" }`'s comma-containing path VALUE isn't
/// mistaken for a second field. PR-it680.
fn split_outside_quotes(s: &str, delim: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_str = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            c if c == delim && !in_str => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

fn parse_string(v: &str) -> Option<String> {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        // A REAL bug found+fixed (production-hardening PR-it1064, a
        // background close-read survey finding): this format has NO escape
        // mechanism at all (see this file's own top-of-file doc comment:
        // "NOT a general TOML parser") -- `strip_comment`/
        // `split_outside_quotes` (the naive `in_str` toggle-on-every-`"`
        // technique they share) compute field/value boundaries assuming
        // every value contains ZERO embedded `"` characters. When a value
        // DOES contain one (e.g. a user's attempt at `\"` escaping, which
        // this format silently does not support), the toggle desyncs for
        // the REST of the line -- a subsequent field-separating comma gets
        // wrongly swallowed as "inside a string", MERGING two inline-table
        // fields into one and silently DROPPING whichever field never got
        // its own key/value recognized. Live-confirmed BEFORE this fix:
        // `web = { version = "a\"b", path = "c" }` parsed successfully to
        // `Dep { path: None, version: Some("a\\\"b\", path = \"c") }` --
        // `path` silently vanished and `version` ended up holding a
        // corrupted blob containing the literal, unparsed text of the
        // swallowed `path` field. Any value this function would otherwise
        // extract that STILL contains an embedded `"` is therefore always
        // a symptom of this exact desync having already happened earlier
        // on the line -- never a legitimately encodable value -- so reject
        // it here, at the single point every string value in this file
        // flows through, converting the silent corruption into a clean
        // parse error instead.
        if inner.contains('"') {
            return None;
        }
        // A REAL, live-confirmed correctness footgun found+fixed
        // (production-hardening PR-it1133, closing a low-severity
        // observation deferred at PR-it1065): leading/trailing whitespace
        // inside a quoted value used to be preserved verbatim, not trimmed
        // or rejected. For a dependency `version`, this passed
        // `is_safe_relative_path_single_component`'s own check unchanged
        // (a whitespace-padded string is still one "Normal" path component
        // on both Unix and Windows) and flowed straight into
        // `loader.rs::pkg_ctx`'s `registry::cache_dir().join(name).join(v)`
        // -- live-confirmed BEFORE this fix: `version = " 1.2.0"` made `kupl
        // pkg tree` print `widgets @  1.2.0  (registry — not yet supported,
        // unresolved)` (the doubled space the only visible clue), and would
        // have gone on to build a registry-cache directory literally named
        // " 1.2.0" had the fetch itself succeeded, silently splitting what a
        // user thinks of as one version across two on-disk directory names
        // that no longer round-trip through an ordinary string comparison.
        // Not a security bypass (no traversal/absolute-path escape --
        // `is_safe_relative_path_single_component` already rejects those),
        // just a value silently NOT meaning what it visually appears to
        // mean. Rejected here, at the same single point every string value
        // in this file already flows through (mirroring PR-it1064's own
        // embedded-quote rejection immediately above), rather than silently
        // trimming: a leading/trailing space is essentially never
        // intentional in a `name`/`version`/`path`/`entry` value, so a clean
        // parse error is more useful than a silent, invisible normalization
        // a user would have no way to notice.
        if inner != inner.trim() {
            return None;
        }
        Some(inner.to_string())
    } else {
        None
    }
}

fn parse_dep(name: &str, value: &str, line: usize) -> Result<Dep, String> {
    let name = name.to_string();
    // A REAL bug found+fixed (production-hardening PR-it1065, a background
    // close-read survey finding): the name-safety check below (originally
    // added at PR-it919, citing this exact file's own "a module's own
    // stated threat model is worth re-applying to EVERY value that flows
    // into the same dangerous operation" principle) used to sit AFTER the
    // bare-string-shorthand branch's own early `return`, so it only ever
    // gated the inline-table (`{ path = "..", version = ".." }`) form --
    // the bare-string shorthand (`name = "../path"`) skipped it entirely.
    // Live-confirmed BEFORE this fix: `/etc/passwd = "vendor/util"` parsed
    // SUCCESSFULLY (no manifest error at all), while the IDENTICAL name
    // via `../../etc = { path = "vendor/util" }` was correctly REJECTED at
    // parse time with a clean "must be a plain relative name" error -- the
    // exact same inconsistency this file's own PR-it747 already flagged
    // and fixed once for duplicate-name detection across both syntax
    // forms. `dep.name` is not currently filesystem-joined anywhere for a
    // PATH-style dependency (`loader.rs`'s `resolve_deps` only joins
    // `dep.path`, using `dep.name` purely as a `HashMap` key) -- confirmed
    // directly by reading `loader.rs` before this fix, so no CURRENT
    // exploit chain existed through this specific gap -- but the
    // validation itself should not depend on which of two equivalent
    // syntax forms a dependency happens to use. Moved here, before EITHER
    // branch, so both forms are validated identically. Uses the STRICTER
    // single-component variant (production-hardening PR-it1096): a
    // dependency name has no legitimate reason to be nested, unlike a
    // registry file path -- see that function's own doc comment for the
    // destructive cache-corruption bug a multi-component name/version
    // caused.
    if !crate::registry::is_safe_relative_path_single_component(&name) {
        return Err(format!(
            "line {line}: dependency name `{name}` must be a plain relative name, not an absolute path or contain `..`"
        ));
    }
    // bare-string shorthand: `name = "../path"`
    if let Some(s) = parse_string(value) {
        return Ok(Dep { name, path: Some(s), version: None });
    }
    // inline table: `{ path = "..", version = ".." }`
    let inner = value
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| format!("line {line}: expected a string or `{{ … }}` table"))?;
    let mut path = None;
    let mut version = None;
    // A REAL sibling bug to `strip_comment`'s already-fixed `#`-inside-a-
    // string footgun (PR-it654), found by re-checking this SAME file's other
    // naive-delimiter-split for the identical shape (PR-it680): a plain
    // `inner.split(',')` breaks the moment a `path`/`version` VALUE contains
    // a literal comma (e.g. `{ path = "my,dir" }`), since the comma inside
    // the quoted string gets treated as a field separator too. Confirmed
    // live before this fix: `kupl pkg tree` on a manifest with exactly this
    // shape failed with a confusing "expected a string value" instead of
    // parsing the path correctly. `split_outside_quotes` mirrors
    // `strip_comment`'s exact `in_str` toggle-on-`"` technique.
    for field in split_outside_quotes(inner, ',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (k, v) = field
            .split_once('=')
            .ok_or_else(|| format!("line {line}: expected `key = value` in table"))?;
        let val = parse_string(v).ok_or_else(|| format!("line {line}: expected a string value"))?;
        // A REAL bug found+fixed (production-hardening PR-it752): a
        // duplicate KEY within a single inline table (`{ path = "a", path =
        // "b" }`) used to silently resolve "last one wins" -- the SAME
        // shape of gap PR-it747 already fixed for a duplicate dependency
        // NAME across separate `[dependencies]` lines, one level deeper
        // inside a single inline table's own fields (flagged as a
        // deliberately out-of-scope residual gap in that fix's own doc
        // comment above, `deps.iter().any(...)`). Live-confirmed BEFORE
        // this fix: `math = { path = "a", path = "b" }` parsed cleanly to
        // `Dep { name: "math", path: Some("b"), .. }` with ZERO diagnostic.
        match k.trim() {
            "path" => {
                if path.is_some() {
                    return Err(format!(
                        "line {line}: duplicate key `path` in dependency `{name}`'s inline table"
                    ));
                }
                path = Some(val);
            }
            "version" => {
                if version.is_some() {
                    return Err(format!(
                        "line {line}: duplicate key `version` in dependency `{name}`'s inline table"
                    ));
                }
                version = Some(val);
            }
            other => return Err(format!("line {line}: unknown dependency key `{other}`")),
        }
    }
    if path.is_none() && version.is_none() {
        return Err(format!("line {line}: dependency `{name}` needs a `path` or `version`"));
    }
    // A REAL bug found+fixed (production-hardening PR-it919, an Explore
    // survey finding, independently re-verified live before implementing):
    // this is the READ-side counterpart of the exact write-side bug
    // `registry::is_safe_relative_path` was created to close (PR-it683,
    // whose own doc comment already flagged "a module's own stated threat
    // model is worth re-applying to EVERY value that flows into the same
    // dangerous operation" -- a lead never actually followed up here). A
    // version-only dependency (`{ version = ".." }`, no `path`) resolves via
    // `loader.rs::pkg_ctx`'s `registry::cache_dir().join(&dep.name).join(v)`
    // -- but `version` had no path-safety check, and `PathBuf::join`
    // silently DISCARDS its left-hand base entirely when the joined
    // argument is itself absolute (the SAME well-known footgun `entry`'s
    // own PR-it766 fix already treats as security-relevant). Live-
    // confirmed BEFORE this fix: a manifest with `version = "."` (a no-op
    // path segment, ALSO correctly rejected below, not just `..`/absolute
    // paths) made `kupl pkg tree` silently read and report an unrelated
    // directory's own `kupl.toml` as if it were a legitimately cache-
    // resolved dependency. Rejected here, at the SAME "single earliest
    // enforcement point" `is_safe_relative_path`'s own doc comment
    // establishes for `path`/`url` in registry.rs, mirroring `entry`'s own
    // precedent exactly. (The `name` check this comment originally also
    // covered here now lives at the TOP of this function instead, applying
    // uniformly to both the bare-string and inline-table syntax forms --
    // see PR-it1065's own doc comment there for why.)
    if let Some(v) = &version {
        // STRICTER single-component variant (production-hardening PR-it1096):
        // a version has no legitimate reason to be nested, unlike a registry
        // file path -- see is_safe_relative_path_single_component's own doc
        // comment for the destructive cache-corruption bug a multi-component
        // version caused.
        if !crate::registry::is_safe_relative_path_single_component(v) {
            return Err(format!(
                "line {line}: dependency `{name}`'s version `{v}` must be a plain relative value, not an absolute path or contain `..`"
            ));
        }
    }
    Ok(Dep { name, path, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_manifest() {
        let m = parse(
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\nmath = { path = \"../math\" }\nutil = \"vendor/util\"\n\
             web = { version = \"1.2.0\" }\n",
        )
        .unwrap();
        assert_eq!(m.name, "app");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.entry, "main.kupl");
        assert_eq!(m.deps.len(), 3);
        assert_eq!(m.deps[0], Dep { name: "math".into(), path: Some("../math".into()), version: None });
        assert_eq!(m.deps[1], Dep { name: "util".into(), path: Some("vendor/util".into()), version: None });
        assert_eq!(m.deps[2], Dep { name: "web".into(), path: None, version: Some("1.2.0".into()) });
    }

    /// A REAL bug found+fixed (production-hardening PR-it919, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// this is the READ-side counterpart of `entry`'s own PR-it766 fix and
    /// `registry.rs`'s write-side `is_safe_relative_path` (PR-it683) -- a
    /// version-only dependency's `name`/`version` get filesystem-joined onto
    /// `registry::cache_dir()` in `loader.rs::pkg_ctx`, with `PathBuf::join`
    /// silently discarding its base when the joined value is itself
    /// absolute. Live-confirmed BEFORE this fix, with a real multi-directory
    /// repro: a manifest whose `[dependencies]` KEY was an absolute path to
    /// an unrelated local project and `version = "."` (a no-op path segment)
    /// made `kupl pkg tree` silently read and report that ARBITRARY
    /// directory's own `kupl.toml` as a legitimately cache-resolved
    /// dependency -- and since `pkg_ctx`'s `deps` map ALSO feeds ordinary
    /// `use`-resolution/compilation (not just reporting commands), a
    /// sufficiently-crafted manifest could cause an unrelated local
    /// directory's code to be silently compiled and run as a dependency.
    #[test]
    fn a_version_only_dependencys_absolute_or_escaping_name_or_version_is_a_clean_parse_error() {
        let abs_name = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\n/etc/passwd = { version = \".\" }\n",
        );
        assert!(
            abs_name.is_err() && abs_name.unwrap_err().contains("must be a plain relative name"),
            "an absolute dependency NAME must be a clean parse error"
        );

        let dotdot_name = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\n../../etc = { version = \"1.0\" }\n",
        );
        assert!(
            dotdot_name.is_err() && dotdot_name.unwrap_err().contains("must be a plain relative name"),
            "a `..`-containing dependency NAME must be a clean parse error"
        );

        let escaping_version = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\nweb = { version = \"../../etc\" }\n",
        );
        assert!(
            escaping_version.is_err() && escaping_version.unwrap_err().contains("must be a plain relative value"),
            "a `..`-containing dependency VERSION must be a clean parse error"
        );

        let bare_dot_version = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\nweb = { version = \".\" }\n",
        );
        assert!(
            bare_dot_version.is_err() && bare_dot_version.unwrap_err().contains("must be a plain relative value"),
            "a bare `.` dependency VERSION (a no-op path segment, not just `..`) must ALSO be a clean parse error"
        );

        // ordinary, legitimate dependencies of every shape are unaffected
        let ok = parse(
            "[project]\nname = \"app\"\n\n\
             [dependencies]\nmath = { path = \"../math\" }\n\
             abs_path_dep = { path = \"/opt/local/thing\" }\n\
             web = { version = \"1.2.0\" }\n",
        );
        assert!(
            ok.is_ok(),
            "an ordinary relative/absolute PATH dependency and an ordinary VERSION string must still parse cleanly: {ok:?}"
        );
    }

    /// A REAL, live-confirmed DESTRUCTIVE cache-corruption bug found+fixed
    /// (production-hardening PR-it1096, a two-phase self-scoping survey
    /// finding): the test above locks in `..`/absolute rejection, but
    /// `is_safe_relative_path` ALSO accepted a perfectly ordinary-looking
    /// MULTI-COMPONENT name/version like `"beta/preview"` -- no traversal,
    /// no absolute path, just a nested value. `registry.rs::fetch_package_with`'s
    /// `dest = cache_dir.join(name).join(version)` builds an ordinary
    /// intermediate directory for each component, indistinguishable on disk
    /// from a genuine top-level version -- so a LATER, entirely ordinary
    /// fetch of a plain sibling version equal to that ancestor path segment
    /// (`version = "beta"`) silently DESTROYED the entire previously-
    /// fetched, already-hash-verified `"beta/preview"` version's directory
    /// tree via `atomic_replace`'s own unconditional `remove_dir_all`, with
    /// zero diagnostic (see `registry.rs`'s own
    /// `is_safe_relative_path_single_component` doc comment for the full
    /// write-up and a live repro). Now rejected at manifest-PARSE time,
    /// the earliest possible enforcement point, for both `name` and
    /// `version`.
    #[test]
    fn a_multi_component_dependency_name_or_version_is_a_clean_parse_error() {
        let nested_name = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\nns/widgets = { version = \"1.0\" }\n",
        );
        assert!(
            nested_name.is_err() && nested_name.unwrap_err().contains("must be a plain relative name"),
            "a multi-component dependency NAME must be a clean parse error"
        );

        let nested_version = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\nwidgets = { version = \"beta/preview\" }\n",
        );
        assert!(
            nested_version.is_err() && nested_version.unwrap_err().contains("must be a plain relative value"),
            "a multi-component dependency VERSION must be a clean parse error"
        );

        // an ordinary, single-component name/version (including one with
        // dots/dashes, just no `/`) is completely unaffected.
        let ok = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\nwidgets = { version = \"1.0.0-rc\" }\n",
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it1065, a background
    /// close-read survey finding): the test above locks in the NAME check
    /// for the inline-table (`{ version = ".." }`) syntax form -- this test
    /// proves the IDENTICAL check now applies to the bare-string shorthand
    /// form too (`name = "../path"`), which used to skip it entirely via
    /// an early `return` before the check ever ran. Live-confirmed BEFORE
    /// this fix: `/etc/passwd = "vendor/util"` parsed SUCCESSFULLY (no
    /// manifest error), while the identical name via
    /// `../../etc = { path = "vendor/util" }` was already correctly
    /// rejected -- a genuine inconsistency between two equivalent syntax
    /// forms.
    #[test]
    fn a_path_style_dependencys_absolute_or_escaping_name_is_a_clean_parse_error_via_the_bare_string_shorthand_too() {
        let abs_name = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\n/etc/passwd = \"vendor/util\"\n",
        )
        .expect_err("an absolute dependency NAME must be a clean parse error via the bare-string shorthand form too");
        assert!(abs_name.contains("must be a plain relative name"), "{abs_name}");

        let dotdot_name = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\n../../etc = \"vendor/util\"\n",
        )
        .expect_err("a `..`-containing dependency NAME must be a clean parse error via the bare-string shorthand form too");
        assert!(dotdot_name.contains("must be a plain relative name"), "{dotdot_name}");

        // both syntax forms must now reject the SAME unsafe name IDENTICALLY.
        let inline_table = parse(
            "[project]\nname = \"app\"\n\n[dependencies]\n/etc/passwd = { path = \"vendor/util\" }\n",
        )
        .expect_err("the inline-table form must still reject the same unsafe name");
        assert_eq!(
            abs_name, inline_table,
            "the bare-string and inline-table forms must reject the identical unsafe name with the identical error"
        );

        // ordinary, legitimate bare-string-shorthand dependencies are unaffected.
        let ok = parse("[project]\nname = \"app\"\n\n[dependencies]\nutil = \"vendor/util\"\n").unwrap();
        assert_eq!(
            ok.deps,
            vec![Dep { name: "util".into(), path: Some("vendor/util".into()), version: None }]
        );
    }

    #[test]
    fn defaults_and_comments() {
        let m = parse("[project]\nname = \"x\"  # the name\n").unwrap();
        assert_eq!(m.name, "x");
        assert_eq!(m.entry, "main.kupl"); // default
        assert!(m.deps.is_empty());
    }

    /// A REAL, live-confirmed bug found+fixed (production-hardening PR-it766):
    /// `entry` was accepted VERBATIM with zero path validation, then resolved
    /// as `dep_dir.join(&m.entry)` -- but `PathBuf::join` silently DISCARDS
    /// its left-hand base entirely when the argument is itself absolute, and
    /// a leading `..` component walks straight out of the intended directory.
    /// A dependency's OWN `kupl.toml` could point `entry` at ANY file the
    /// process can read anywhere on disk, and that file -- not anything
    /// inside the dependency's own directory -- silently became the
    /// dependency's compiled module content. Live-confirmed BEFORE this fix
    /// via a real multi-package `kupl run`: a dependency whose `entry`
    /// pointed at an absolute path OUTSIDE its own directory was compiled in
    /// place of its real `main.kupl`, with no error at all.
    #[test]
    fn an_absolute_or_parent_escaping_entry_is_a_clean_parse_error() {
        let err = parse("[project]\nname = \"x\"\nentry = \"/etc/passwd\"\n")
            .expect_err("an absolute entry path must be rejected");
        assert!(err.contains("absolute"), "{err}");

        let err2 = parse("[project]\nname = \"x\"\nentry = \"../outside/evil.kupl\"\n")
            .expect_err("an entry path containing `..` must be rejected");
        assert!(err2.contains(".."), "{err2}");

        // an ordinary relative entry, including one nested in a subdirectory,
        // still parses cleanly -- this is a real, legitimate use of `entry`.
        assert_eq!(parse("[project]\nname = \"x\"\nentry = \"src/main.kupl\"\n").unwrap().entry, "src/main.kupl");
    }

    /// A coverage-closing test, per production-hardening PR-it654 (no bug
    /// found -- reasoned through `strip_comment`'s character walk before
    /// writing this: it toggles `in_str` on every `"`, so a `#` encountered
    /// while `in_str` is true is correctly left alone rather than truncating
    /// the line early). This exact edge case -- a literal `#` inside a
    /// quoted string value, as opposed to an actual trailing comment -- is a
    /// classic footgun for a naive "everything after `#` is a comment"
    /// implementation, and had zero prior test coverage despite the
    /// existing `defaults_and_comments` test already covering an ORDINARY
    /// trailing comment.
    #[test]
    fn hash_inside_a_string_value_is_not_treated_as_a_comment() {
        let m = parse("[project]\nname = \"a#b\"\nentry = \"main.kupl\"  # trailing comment\n").unwrap();
        assert_eq!(m.name, "a#b");
        assert_eq!(m.entry, "main.kupl");
    }

    #[test]
    fn malformed_is_error() {
        assert!(parse("[project]\nname \"x\"\n").is_err()); // no `=`
        assert!(parse("[bogus]\n").is_err()); // unknown section
        assert!(parse("[dependencies]\nfoo = { }\n").is_err()); // no path/version
    }

    /// A REAL bug found+fixed (production-hardening PR-it747): a duplicate
    /// dependency NAME under `[dependencies]` used to silently resolve "last
    /// one wins" -- `deps` was a plain `Vec` with no duplicate-name check, and
    /// `loader.rs`'s `pkg_ctx` builds a `HashMap` from it via a bare
    /// `.insert()`, silently discarding the earlier declaration. A plausible
    /// copy-paste mistake (e.g. re-declaring a dependency with a different
    /// path but forgetting to remove the stale line) previously gave zero
    /// signal that anything was wrong.
    #[test]
    fn duplicate_dependency_name_is_a_clean_error_not_silently_last_wins() {
        // two separate inline-table entries for the same name
        let err = parse("[dependencies]\nmth = { path = \"../m1\" }\nmth = { path = \"../m2\" }\n")
            .expect_err("a duplicate dependency name must be a clean error, not silently accepted");
        assert!(err.contains("duplicate") && err.contains("mth"), "{err}");

        // a bare-string shorthand entry followed by an inline-table entry, same name
        let err2 = parse("[dependencies]\nutil = \"vendor/util\"\nutil = { path = \"../util2\" }\n")
            .expect_err("a duplicate name must be caught regardless of which dependency SYNTAX form is used");
        assert!(err2.contains("duplicate") && err2.contains("util"), "{err2}");

        // sanity: two DIFFERENT dependency names still parse fine (not an
        // overly-broad check that rejects every multi-dependency manifest).
        let m = parse("[dependencies]\nmath = { path = \"../math\" }\nutil = { path = \"../util\" }\n")
            .expect("distinct dependency names must still parse cleanly");
        assert_eq!(m.deps.len(), 2);
    }

    #[test]
    fn duplicate_key_inside_an_inline_table_is_a_clean_error_not_silently_last_wins() {
        // A REAL bug found+fixed (production-hardening PR-it752): a
        // duplicate KEY within a SINGLE inline table (`{ path = "a", path =
        // "b" }`) used to silently resolve "last one wins" -- the SAME
        // shape of gap PR-it747 fixed for a duplicate dependency NAME
        // across separate `[dependencies]` lines, one level deeper inside
        // a single inline table's own field parsing. Live-confirmed BEFORE
        // this fix: this exact manifest parsed cleanly to
        // `Dep { name: "math", path: Some("b"), .. }` with ZERO diagnostic.
        let err = parse("[dependencies]\nmath = { path = \"a\", path = \"b\" }\n")
            .expect_err("a duplicate `path` key must be a clean error, not silently last-wins");
        assert!(err.contains("duplicate") && err.contains("path") && err.contains("math"), "{err}");

        // the SAME gap for the OTHER inline-table key, `version`.
        let err2 = parse("[dependencies]\njson2 = { version = \"1.0.0\", version = \"2.0.0\" }\n")
            .expect_err("a duplicate `version` key must be a clean error, not silently last-wins");
        assert!(err2.contains("duplicate") && err2.contains("version") && err2.contains("json2"), "{err2}");

        // sanity: a table with one of EACH distinct key still parses fine
        // (not an overly-broad check that rejects a legitimate 2-key table).
        let m = parse("[dependencies]\nweb = { path = \"../web\", version = \"1.0.0\" }\n")
            .expect("one path key + one version key must still parse cleanly");
        assert_eq!(m.deps[0].path, Some("../web".to_string()));
        assert_eq!(m.deps[0].version, Some("1.0.0".to_string()));
    }

    /// A REAL bug found+fixed (production-hardening PR-it784, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// a SECOND `[project]` section later in the same file used to be
    /// accepted exactly like the first, silently overwriting
    /// `name`/`version`/`entry` -- the SAME "silently last-wins" shape as
    /// `duplicate_dependency_name_...` and `duplicate_key_inside_an_inline_
    /// table_...` above, one level up (a whole SECTION rather than a
    /// dependency name or an inline-table key). Live-confirmed BEFORE this
    /// fix: a dependency's own `kupl.toml` with `entry = "main.kupl"` then
    /// a second `[project]` block with `entry = "other.kupl"` silently
    /// compiled `other.kupl` in `main.kupl`'s place, end-to-end, with ZERO
    /// diagnostic anywhere.
    #[test]
    fn duplicate_project_section_is_a_clean_error_not_silently_last_wins() {
        let err = parse("[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[project]\nentry = \"other.kupl\"\n")
            .expect_err("a second [project] section must be a clean error, not silently accepted");
        assert!(err.contains("duplicate") && err.contains("[project]"), "{err}");

        // sanity: exactly one [project] section still parses fine.
        let m = parse("[project]\nname = \"app\"\nentry = \"main.kupl\"\n").expect("a single [project] section is fine");
        assert_eq!(m.name, "app");
        assert_eq!(m.entry, "main.kupl");
    }

    /// A REAL bug found+fixed (production-hardening PR-it784, the same
    /// survey's second finding, independently re-verified live before
    /// implementing): this module's own doc comment (top of file) promises
    /// "unknown `[project]` keys are ignored" -- but the OLD code called
    /// `parse_string(value)` and propagated its error BEFORE the `match
    /// key` dispatch even ran, so an unrecognized key whose value wasn't a
    /// bare quoted string (a bool/number/array/inline-table -- all
    /// perfectly valid TOML) hard-failed the ENTIRE manifest parse instead
    /// of being ignored, contradicting the doc comment. Live-confirmed
    /// BEFORE this fix: `private = true` failed with "line N: expected a
    /// string", while the SAME key quoted (`private = "true"`) parsed
    /// cleanly and was silently dropped -- same key, only the TOML value
    /// type differed.
    #[test]
    fn unknown_project_keys_are_ignored_regardless_of_their_toml_value_type() {
        for value in ["true", "42", "[1, 2, 3]", "{ x = 1 }"] {
            let src = format!("[project]\nname = \"app\"\nentry = \"main.kupl\"\nprivate = {value}\n");
            let m = parse(&src)
                .unwrap_or_else(|e| panic!("an unknown key with value `{value}` must be ignored, not a parse error: {e}"));
            assert_eq!(m.name, "app", "the rest of the manifest must still parse correctly");
        }
        // sanity: a KNOWN key (`entry`) with a non-string value must still be a clean error --
        // this fix must not accidentally widen ignoring to apply to recognized keys too.
        let err = parse("[project]\nname = \"app\"\nentry = 42\n").expect_err("a known key still needs its declared type");
        assert!(err.contains("expected a string"), "{err}");
    }

    /// A REAL sibling bug to `hash_inside_a_string_value_is_not_treated_as_a_
    /// comment` above (PR-it680, same file, same "naive delimiter split
    /// ignores string-literal boundaries" shape it654 already fixed once for
    /// `#`, just for a DIFFERENT delimiter): `parse_dep`'s inline-table field
    /// parser used a plain `inner.split(',')`, which breaks the moment a
    /// `path`/`version` VALUE contains a literal comma -- the comma inside
    /// the quoted string was mistaken for a field separator, corrupting the
    /// split into two bogus fields. Confirmed live before this fix: `kupl
    /// pkg tree` on a manifest with `{ path = "my,dir" }` failed with a
    /// confusing "expected a string value" instead of parsing the path.
    /// A REAL bug found+fixed (production-hardening PR-it860, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// a duplicate KEY within a single `[project]` block (e.g. `entry`
    /// declared twice) used to silently resolve "last one wins" -- the SAME
    /// shape already fixed for a duplicate dependency NAME (PR-it747), a
    /// duplicate inline-table KEY (PR-it752), and a duplicate `[project]`
    /// SECTION (PR-it784), just one level lower than the section-duplicate
    /// case: a duplicate KEY inside a single section rather than the whole
    /// section repeating. Live-confirmed BEFORE this fix, end-to-end with a
    /// real multi-package `kupl run`: a dependency's own `kupl.toml` with
    /// `entry = "main.kupl"` then a second `entry = "other.kupl"` line
    /// silently compiled `other.kupl` in `main.kupl`'s place, with zero
    /// diagnostic anywhere.
    #[test]
    fn duplicate_key_inside_project_is_a_clean_error_not_silently_last_wins() {
        let err = parse("[project]\nname = \"app\"\nentry = \"main.kupl\"\nentry = \"other.kupl\"\n")
            .expect_err("a duplicate `entry` key must be a clean error, not silently last-wins");
        assert!(err.contains("duplicate") && err.contains("entry"), "{err}");

        let err2 = parse("[project]\nname = \"app\"\nname = \"other\"\n")
            .expect_err("a duplicate `name` key must be a clean error, not silently last-wins");
        assert!(err2.contains("duplicate") && err2.contains("name"), "{err2}");

        let err3 = parse("[project]\nname = \"app\"\nversion = \"1.0.0\"\nversion = \"2.0.0\"\n")
            .expect_err("a duplicate `version` key must be a clean error, not silently last-wins");
        assert!(err3.contains("duplicate") && err3.contains("version"), "{err3}");

        // sanity: one of each key, each declared exactly once, still parses fine.
        let m = parse("[project]\nname = \"app\"\nversion = \"1.0.0\"\nentry = \"main.kupl\"\n")
            .expect("one declaration per key must still parse cleanly");
        assert_eq!(m.name, "app");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.entry, "main.kupl");
    }

    #[test]
    fn comma_inside_a_dependency_string_value_is_not_treated_as_a_field_separator() {
        let m = parse("[dependencies]\nmath = { path = \"my,dir\" }\n").unwrap();
        assert_eq!(m.deps, vec![Dep { name: "math".into(), path: Some("my,dir".into()), version: None }]);
        // BOTH fields present, with the comma-bearing one first, still parses
        // both correctly (the split must resume normally after the string closes).
        let m2 = parse(
            "[dependencies]\nweb = { path = \"a,b\", version = \"1.2.0\" }\n",
        )
        .unwrap();
        assert_eq!(
            m2.deps,
            vec![Dep { name: "web".into(), path: Some("a,b".into()), version: Some("1.2.0".into()) }]
        );
    }

    /// A REAL bug found+fixed (production-hardening PR-it1064, a background
    /// close-read survey finding): see `parse_string`'s own doc comment for
    /// the full writeup. This format has no escape mechanism at all, so a
    /// value containing an embedded `"` (e.g. a user's own attempt at `\"`
    /// escaping) desyncs `split_outside_quotes`'s naive in-string toggle for
    /// the REST of the line -- silently merging two inline-table fields and
    /// dropping one entirely. Live-confirmed BEFORE this fix:
    /// `web = { version = "a\"b", path = "c" }` parsed SUCCESSFULLY to
    /// `Dep { path: None, version: Some("a\\\"b\", path = \"c") }` -- `path`
    /// silently vanished and `version` held a corrupted blob containing the
    /// literal, unparsed text of the swallowed `path` field.
    #[test]
    fn an_embedded_quote_inside_a_dependency_string_value_is_a_clean_error_not_silent_field_corruption() {
        let err = parse("[dependencies]\nweb = { version = \"a\\\"b\", path = \"c\" }\n")
            .expect_err("an embedded quote must be a clean error, not silent field-swallowing");
        assert!(err.contains("expected a string value"), "{err}");

        // the reverse field order must be caught identically.
        let err2 = parse("[dependencies]\nweb = { path = \"a\\\"b\", version = \"1.0\" }\n")
            .expect_err("an embedded quote must be a clean error regardless of field order");
        assert!(err2.contains("expected a string value"), "{err2}");

        // the bare-string shorthand form must be caught too, not just inline tables.
        let err3 = parse("[dependencies]\nweb = \"a\\\"b\"\n")
            .expect_err("an embedded quote in the bare-string shorthand must be a clean error");
        assert!(err3.contains("expected a string or"), "{err3}");

        // sanity: an ordinary, well-formed manifest is completely unaffected.
        let m = parse("[dependencies]\nweb = { version = \"1.2.0\", path = \"c\" }\n").unwrap();
        assert_eq!(
            m.deps,
            vec![Dep { name: "web".into(), path: Some("c".into()), version: Some("1.2.0".into()) }]
        );
    }

    /// A REAL, live-confirmed correctness footgun found+fixed (production-
    /// hardening PR-it1133, closing a low-severity observation deferred at
    /// PR-it1065): leading/trailing whitespace inside a quoted value used to
    /// be preserved verbatim rather than trimmed or rejected. Live-confirmed
    /// BEFORE this fix: `version = " 1.2.0"` passed `parse_dep` cleanly and
    /// made `kupl pkg tree` print `widgets @  1.2.0  (registry — not yet
    /// supported, unresolved)` with a doubled space as the only visible
    /// clue -- see `parse_string`'s own doc comment for the full writeup.
    #[test]
    fn leading_or_trailing_whitespace_inside_a_quoted_value_is_a_clean_error_not_silently_preserved() {
        // a dependency version, inline-table form (the exact live repro).
        let err = parse("[dependencies]\nwidgets = { version = \" 1.2.0\" }\n")
            .expect_err("a leading-space version must be a clean error, not silently preserved");
        assert!(err.contains("expected a string value"), "{err}");

        // trailing whitespace, and a dependency path, inline-table form.
        let err2 = parse("[dependencies]\nwidgets = { path = \"vendor/w \" }\n")
            .expect_err("trailing whitespace in a path must be a clean error too");
        assert!(err2.contains("expected a string value"), "{err2}");

        // the bare-string shorthand path form must be caught too.
        let err3 = parse("[dependencies]\nwidgets = \" vendor/w\"\n")
            .expect_err("leading whitespace in the bare-string shorthand must be a clean error");
        assert!(err3.contains("expected a string or"), "{err3}");

        // [project] fields flow through the SAME parse_string, so name/
        // version/entry are covered by the same single fix point too.
        let err4 = parse("[project]\nname = \"app\"\nversion = \" 0.1.0\"\nentry = \"main.kupl\"\n")
            .expect_err("a leading-space [project] version must be a clean error");
        assert!(err4.contains("expected a string"), "{err4}");

        // sanity: INTERNAL whitespace (not leading/trailing) is untouched --
        // this fix is specifically about the visually-invisible boundary
        // case, not a blanket rejection of any string containing a space.
        let m = parse("[project]\nname = \"my app\"\nversion = \"0.1.0\"\nentry = \"main.kupl\"\n").unwrap();
        assert_eq!(m.name, "my app");

        // sanity: an ordinary, well-formed dependency manifest is unaffected.
        let m2 = parse("[dependencies]\nwidgets = { version = \"1.2.0\" }\n").unwrap();
        assert_eq!(m2.deps, vec![Dep { name: "widgets".into(), path: None, version: Some("1.2.0".into()) }]);
    }
}
