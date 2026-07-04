//! Multi-file loading: resolve `use` declarations to files, merge into one
//! Program, and keep a SourceMap so every diagnostic points into the right
//! file. `use util` -> util.kupl, `use lib.math` -> lib/math.kupl, resolved
//! relative to the entry file's directory. Cycles are fine (loading is
//! idempotent per file); duplicates are loaded once.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::Program;
use crate::diag::{self, Diag, Span};
use crate::parser;

pub struct SourceFile {
    pub path: String,
    pub src: String,
    /// Offset of this file's first byte in the virtual concatenation.
    pub base: u32,
}

pub struct SourceMap {
    pub files: Vec<SourceFile>,
    /// All sources concatenated (each file's spans index into this).
    pub concat: String,
}

impl SourceMap {
    fn locate(&self, span: Span) -> Option<(&SourceFile, Span)> {
        let file = self
            .files
            .iter()
            .rev()
            .find(|f| span.start >= f.base)?;
        Some((file, Span::new(span.start - file.base, span.end - file.base)))
    }

    /// Render a diagnostic against the owning file.
    pub fn render(&self, d: &Diag) -> String {
        match self.locate(d.span) {
            Some((file, local)) => {
                let mut local_diag = d.clone();
                local_diag.span = local;
                diag::render(&local_diag, &file.src, &file.path)
            }
            None => format!("{}[{}]: {}\n", severity_str(d), d.code, d.message),
        }
    }

    /// Machine-readable diagnostics with per-file locations.
    pub fn to_json(&self, diags: &[Diag]) -> String {
        let mut out = String::from("{\"diagnostics\":[");
        for (i, d) in diags.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let (file, local) = match self.locate(d.span) {
                Some((f, l)) => (f.path.as_str(), l),
                None => ("<unknown>", d.span),
            };
            let src = self
                .files
                .iter()
                .find(|f| f.path == file)
                .map(|f| f.src.as_str())
                .unwrap_or("");
            let (line, col) = diag::line_col(src, local.start);
            let sev = match d.severity {
                crate::diag::Severity::Error => "error",
                crate::diag::Severity::Warning => "warning",
            };
            out.push_str(&format!(
                "{{\"severity\":\"{sev}\",\"code\":\"{}\",\"message\":\"{}\",\"file\":\"{}\",\"span\":{{\"start\":{},\"end\":{},\"line\":{line},\"col\":{col}}}}}",
                d.code,
                diag::json_escape(&d.message),
                diag::json_escape(file),
                local.start,
                local.end,
            ));
        }
        out.push_str("]}");
        out
    }

    /// Slice the virtual source (for snippets in test output etc.).
    pub fn snippet(&self, span: Span) -> String {
        let start = (span.start as usize).min(self.concat.len());
        let end = (span.end as usize).min(self.concat.len());
        self.concat[start..end].trim().to_string()
    }
}

fn severity_str(d: &Diag) -> &'static str {
    match d.severity {
        crate::diag::Severity::Error => "error",
        crate::diag::Severity::Warning => "warning",
    }
}

/// Load the entry file plus everything reachable through `use`.
pub fn load(entry: &str) -> Result<(Program, SourceMap), (Vec<Diag>, SourceMap)> {
    load_with(entry, &std::collections::HashMap::new())
}

/// Like `load`, but file contents can be overridden (unsaved editor buffers).
pub fn load_with(
    entry: &str,
    overrides: &std::collections::HashMap<PathBuf, String>,
) -> Result<(Program, SourceMap), (Vec<Diag>, SourceMap)> {
    let mut map = SourceMap { files: Vec::new(), concat: String::new() };
    let mut program = Program::default();
    let mut diags: Vec<Diag> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let entry_path = PathBuf::from(entry);
    let root = entry_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut queue: Vec<(PathBuf, Option<Span>)> = vec![(entry_path, None)];

    while let Some((path, use_span)) = queue.pop() {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen.insert(canonical) {
            continue;
        }
        let override_src = overrides
            .get(&path)
            .cloned()
            .or_else(|| path.canonicalize().ok().and_then(|c| overrides.get(&c).cloned()));
        let src = match override_src.map(Ok).unwrap_or_else(|| std::fs::read_to_string(&path)) {
            Ok(s) => s,
            Err(e) => {
                diags.push(Diag::error(
                    "K0400",
                    format!("cannot read module file {}: {e}", path.display()),
                    use_span.unwrap_or_default(),
                ));
                continue;
            }
        };
        let base = map.concat.len() as u32;
        map.concat.push_str(&src);
        map.concat.push('\n');
        let (file_program, file_diags) = parser::parse_with_base(&src, base);
        map.files.push(SourceFile {
            path: path.display().to_string(),
            src,
            base,
        });
        diags.extend(file_diags);
        for (use_path, span) in &file_program.uses {
            let rel: PathBuf = use_path.split('.').collect();
            let mut fs_path = root.join(rel);
            fs_path.set_extension("kupl");
            queue.push((fs_path, Some(*span)));
        }
        program.items.extend(file_program.items);
    }

    let has_errors = diags.iter().any(|d| d.severity == crate::diag::Severity::Error);
    if has_errors {
        Err((diags, map))
    } else {
        // warnings ride along via the caller re-running check; parse warnings
        // are rare, so we simply drop-through with the merged program
        for d in diags {
            eprint!("{}", map.render(&d));
        }
        Ok((program, map))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn multi_file_load_and_diag_mapping() {
        let dir = std::env::temp_dir().join(format!("kupl-loader-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(
            dir.join("main.kupl"),
            "use util\nuse lib.math\n\nfun main() {\n    print(\"{double(add(20, 1))}\")\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("util.kupl"), "fun double(n: Int) -> Int {\n    n * 2\n}\n").unwrap();
        std::fs::write(
            dir.join("lib/math.kupl"),
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n",
        )
        .unwrap();

        let (program, map) = super::load(dir.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("loads");
        assert_eq!(map.files.len(), 3);
        assert_eq!(program.items.len(), 3);

        // full pipeline over the merged program
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(interp.call_value(f, vec![], crate::diag::Span::default()).is_ok());

        // an error in a dep maps back to the dep's file
        std::fs::write(dir.join("util.kupl"), "fun double(n: Int) -> Int {\n    n * true\n}\n").unwrap();
        let (program2, map2) = super::load(dir.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("parses");
        let (_, diags2) = crate::check::check(&program2);
        let err = diags2
            .iter()
            .find(|d| d.severity == crate::diag::Severity::Error)
            .expect("type error found");
        let rendered = map2.render(err);
        assert!(rendered.contains("util.kupl"), "diag should point at util.kupl:\n{rendered}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
