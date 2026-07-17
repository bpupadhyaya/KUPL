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
                    "name" => name = parse_str(value)?,
                    "version" => version = parse_str(value)?,
                    "entry" => {
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
        Some(v[1..v.len() - 1].to_string())
    } else {
        None
    }
}

fn parse_dep(name: &str, value: &str, line: usize) -> Result<Dep, String> {
    let name = name.to_string();
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
}
