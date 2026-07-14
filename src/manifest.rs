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

    for (i, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match sec.trim() {
                "project" => "project",
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
                let s = parse_string(value).ok_or_else(|| format!("line {}: expected a string", i + 1))?;
                match key {
                    "name" => name = s,
                    "version" => version = s,
                    "entry" => entry = s,
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
                // `{ path = "a", path = "b" }` -- is a separate,
                // deliberately out-of-scope issue inside `parse_dep`'s own
                // field parsing, not covered by this check.)
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
        match k.trim() {
            "path" => path = Some(val),
            "version" => version = Some(val),
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
