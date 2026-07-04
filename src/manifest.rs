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
            "dependencies" => deps.push(parse_dep(key, value, i + 1)?),
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
    for field in inner.split(',') {
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

    #[test]
    fn malformed_is_error() {
        assert!(parse("[project]\nname \"x\"\n").is_err()); // no `=`
        assert!(parse("[bogus]\n").is_err()); // unknown section
        assert!(parse("[dependencies]\nfoo = { }\n").is_err()); // no path/version
    }
}
