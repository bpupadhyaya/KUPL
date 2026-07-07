//! Multi-file loading: resolve `use` declarations to files, merge into one
//! Program, and keep a SourceMap so every diagnostic points into the right
//! file. `use util` -> util.kupl, `use lib.math` -> lib/math.kupl, resolved
//! relative to the entry file's directory. Cycles are fine (loading is
//! idempotent per file); duplicates are loaded once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::ast::Program;
use crate::diag::{self, Diag, Span};
use crate::parser;

/// A loaded package: the directory its files resolve relative to, its declared
/// dependencies (name -> directory), and its mangling prefix (`""` for the root
/// package, which is never mangled).
struct PkgCtx {
    root: PathBuf,
    /// name -> (directory, required version if the manifest pinned one)
    deps: HashMap<String, (PathBuf, Option<String>)>,
    prefix: String,
    /// Set when a `kupl.toml` was found but failed to parse — surfaced as a hard
    /// error rather than silently ignored (which would make the deps vanish).
    err: Option<String>,
}

/// Lexically resolve `.` and `..` in a path without touching the filesystem
/// (so a non-existent dependency path still normalizes correctly).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Build the package context for a package rooted at `dir`. When `walk`, search
/// `dir` and its ancestors for the enclosing `kupl.toml` (used for the *entry*
/// file's project); otherwise the manifest must be at `dir/kupl.toml` (a named
/// *dependency*). With no manifest the package is anonymous (`root = dir`, no
/// deps) — exactly today's behavior, so bare `.kupl` files are unaffected.
fn pkg_ctx(dir: &Path, walk: bool, prefix: &str) -> Rc<PkgCtx> {
    let mut d: Option<&Path> = Some(dir);
    while let Some(cur) = d {
        let toml = cur.join("kupl.toml");
        if toml.is_file() {
            match crate::manifest::read(&toml) {
                Ok(m) => {
                    let mut deps = HashMap::new();
                    for dep in &m.deps {
                        if let Some(p) = &dep.path {
                            deps.insert(dep.name.clone(), (normalize(&cur.join(p)), dep.version.clone()));
                        }
                        // version-only deps resolve via a registry — a later slice
                    }
                    return Rc::new(PkgCtx { root: cur.to_path_buf(), deps, prefix: prefix.to_string(), err: None });
                }
                // The manifest exists but is malformed — stop and report it, rather
                // than walking past it (which would silently drop the project's deps).
                Err(e) => {
                    return Rc::new(PkgCtx {
                        root: cur.to_path_buf(),
                        deps: HashMap::new(),
                        prefix: prefix.to_string(),
                        err: Some(format!("invalid manifest {}: {e}", toml.display())),
                    });
                }
            }
        }
        d = if walk { cur.parent() } else { None };
    }
    Rc::new(PkgCtx { root: dir.to_path_buf(), deps: HashMap::new(), prefix: prefix.to_string(), err: None })
}

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

/// A resolved direct dependency of a project (for `kupl pkg` + the lockfile).
pub struct ResolvedDep {
    pub name: String,
    pub path: String,
    pub version: String,
    /// FNV-1a hash (hex) of the dependency's entry source — for drift detection.
    pub hash: String,
}

/// Resolve the direct dependencies declared in the project owning `entry`.
/// Returns them sorted by name (deterministic). Errors if a dependency's
/// manifest or entry source cannot be read.
pub fn resolve_deps(entry: &str) -> Result<Vec<ResolvedDep>, String> {
    let entry_path = PathBuf::from(entry);
    let dir = entry_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let ctx = pkg_ctx(&dir, true, "");
    if let Some(e) = &ctx.err {
        return Err(e.clone());
    }
    let mut out = Vec::new();
    let mut names: Vec<&String> = ctx.deps.keys().collect();
    names.sort();
    for name in names {
        let (dep_dir, _req) = &ctx.deps[name];
        let m = crate::manifest::read(&dep_dir.join("kupl.toml"))
            .map_err(|e| format!("dependency `{name}`: {e}"))?;
        let entry_file = dep_dir.join(&m.entry);
        let src = std::fs::read_to_string(&entry_file)
            .map_err(|e| format!("dependency `{name}` entry {}: {e}", entry_file.display()))?;
        let hash = crate::encoding::hex_encode(&format!("{}", crate::encoding::hash_fnv(&src)));
        out.push(ResolvedDep {
            name: name.clone(),
            path: dep_dir.display().to_string(),
            version: m.version,
            hash,
        });
    }
    Ok(out)
}

/// Serialize resolved deps to the `kupl.lock` line format:
/// `name<TAB>path<TAB>version<TAB>hash` (one per line, name-sorted).
pub fn lock_text(deps: &[ResolvedDep]) -> String {
    let mut s = String::from("# kupl.lock — resolved dependencies (do not edit by hand)\n");
    for d in deps {
        s.push_str(&format!("{}\t{}\t{}\t{}\n", d.name, d.path, d.version, d.hash));
    }
    s
}

/// Parse a `kupl.lock` into (name -> hash) for drift comparison.
pub fn lock_hashes(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() == 4 {
            m.insert(cols[0].to_string(), cols[3].to_string());
        }
    }
    m
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
    let root_dir = entry_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let root_ctx = pkg_ctx(&root_dir, true, "");
    // A malformed project manifest is a hard error — otherwise its dependencies
    // would silently vanish and the build would fail later with confusing
    // "unknown name" errors that never mention the broken kupl.toml.
    if let Some(e) = &root_ctx.err {
        diags.push(Diag::error("K0401", e.clone(), Span::default()));
        return Err((diags, map));
    }
    let mut ctx_cache: HashMap<PathBuf, Rc<PkgCtx>> = HashMap::new();
    let mut queue: Vec<(PathBuf, Rc<PkgCtx>, Option<Span>)> = vec![(entry_path, root_ctx, None)];
    // items tagged with their owning package's mangling prefix, plus the deps
    // each package may reference — fed to the namespace-isolation pass.
    let mut tagged: Vec<(crate::ast::Item, String)> = Vec::new();
    let mut pkg_deps: HashMap<String, HashSet<String>> = HashMap::new();

    while let Some((path, ctx, use_span)) = queue.pop() {
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
        pkg_deps
            .entry(ctx.prefix.clone())
            .or_default()
            .extend(ctx.deps.keys().cloned());
        for (use_path, span) in &file_program.uses {
            let first = use_path.split('.').next().unwrap_or(use_path);
            if let Some((dep_dir, req_version)) = ctx.deps.get(first) {
                // version assertion: a pinned version must match the dependency's
                // own manifest (exact match in v1; ranges are a future addition)
                if let Some(req) = req_version {
                    if let Ok(dm) = crate::manifest::read(&dep_dir.join("kupl.toml")) {
                        if !dm.version.is_empty() && &dm.version != req {
                            diags.push(Diag::error(
                                "K0401",
                                format!(
                                    "dependency `{first}` requires version {req} but found {}",
                                    dm.version
                                ),
                                *span,
                            ));
                        }
                    }
                }
                // cross-package `use <dep>` (or `<dep>.sub`) — the dependency's
                // package is mangled with its import alias as the prefix
                let dep_ctx = ctx_cache
                    .entry(dep_dir.clone())
                    .or_insert_with(|| pkg_ctx(dep_dir, false, first))
                    .clone();
                let target = if let Some(tail) =
                    use_path.strip_prefix(first).and_then(|t| t.strip_prefix('.'))
                {
                    let mut p = dep_ctx.root.join(tail.split('.').collect::<PathBuf>());
                    p.set_extension("kupl");
                    p
                } else {
                    let entry = crate::manifest::read(&dep_dir.join("kupl.toml"))
                        .map(|m| m.entry)
                        .unwrap_or_else(|_| "main.kupl".to_string());
                    dep_ctx.root.join(entry)
                };
                queue.push((target, dep_ctx, Some(*span)));
            } else {
                let rel: PathBuf = use_path.split('.').collect();
                let mut fs_path = ctx.root.join(rel);
                fs_path.set_extension("kupl");
                queue.push((fs_path, ctx.clone(), Some(*span)));
            }
        }
        for item in file_program.items {
            tagged.push((item, ctx.prefix.clone()));
        }
    }

    // Isolate package namespaces (mangle dependency names). When there are no
    // dependency packages, keep items verbatim so ordinary programs are
    // byte-identical to before.
    program.items = if tagged.iter().any(|(_, p)| !p.is_empty()) {
        crate::resolve::isolate(tagged, &pkg_deps)
    } else {
        tagged.into_iter().map(|(i, _)| i).collect()
    };

    // resolve named args + default parameters into positional form on the
    // merged program, so every downstream phase sees plain positional calls
    diags.extend(crate::callargs::resolve_call_args(&mut program));

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

    #[test]
    fn cross_module_types_funs_and_transitive_deps_resolve() {
        // A cross-module program: util defines a TYPE (Point) + funs (add, manhattan calling
        // add), geo `use util` and adds origin_dist calling manhattan, main `use util`/`use geo`
        // -> the full TRANSITIVE chain (main -> geo -> util) resolves into one merged program
        // and evaluates correctly (PR-it174).
        let dir = std::env::temp_dir().join(format!("kupl-xmod-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("util.kupl"),
            "type Point = Point(x: Int, y: Int)\nfun add(a: Int, b: Int) -> Int { a + b }\nfun manhattan(p: Point) -> Int { match p { Point(x, y) => add(x, y) } }\n",
        )
        .unwrap();
        std::fs::write(dir.join("geo.kupl"), "use util\nfun origin_dist(p: Point) -> Int { manhattan(p) }\n").unwrap();
        std::fs::write(
            dir.join("main.kupl"),
            "use util\nuse geo\nfun compute() -> Int { origin_dist(Point(x: 3, y: 4)) }\nfun main() { }\n",
        )
        .unwrap();

        let (program, _map) = super::load(dir.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("loads");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("compute".to_string()));
        let r = match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(v) => v,
            Err(_) => panic!("compute() should run across the merged modules"),
        };
        // origin_dist(Point(3,4)) = manhattan = add(3,4) = 7, threading across all three modules.
        assert_eq!(format!("{r}"), "7");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_manifest_is_a_clean_error_not_silently_ignored() {
        let dir = std::env::temp_dir().join(format!("kupl-loader-badtoml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.kupl"), "fun main() {}\n").unwrap();
        // a kupl.toml that exists but doesn't parse must not be silently walked past
        // (which would drop the project's dependencies and fail later, confusingly).
        std::fs::write(dir.join("kupl.toml"), "this is not toml at all {{{\n").unwrap();
        let (diags, _) = match super::load(dir.join("main.kupl").to_str().unwrap()) {
            Ok(_) => panic!("a malformed manifest must be an error"),
            Err(e) => e,
        };
        assert!(
            diags.iter().any(|d| d.severity == crate::diag::Severity::Error
                && d.message.contains("invalid manifest")),
            "should report an invalid manifest: {diags:?}"
        );
        // a valid manifest (and an empty one — a valid dependency-free project) load fine
        std::fs::write(dir.join("kupl.toml"), "[project]\nname = \"app\"\nentry = \"main.kupl\"\n").unwrap();
        assert!(super::load(dir.join("main.kupl").to_str().unwrap()).is_ok(), "valid manifest loads");
        std::fs::write(dir.join("kupl.toml"), "").unwrap();
        assert!(super::load(dir.join("main.kupl").to_str().unwrap()).is_ok(), "empty manifest loads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn circular_use_loads_once_and_missing_module_errors_cleanly() {
        let dir = std::env::temp_dir().join(format!("kupl-loader-circ-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // a `use` b and b `use` a — a cycle. The loader dedups via its `seen` set, so
        // each file is merged once (no infinite loop / stack overflow) and every
        // definition is available across the cycle.
        std::fs::write(dir.join("a.kupl"), "use b\nfun fa() -> Int {\n    fb() + 1\n}\n").unwrap();
        std::fs::write(dir.join("b.kupl"), "use a\nfun fb() -> Int {\n    10\n}\n").unwrap();
        let (program, _) = super::load(dir.join("a.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("circular use loads");
        assert_eq!(program.items.len(), 2, "both fa and fb are present exactly once");

        // a `use` of a nonexistent module is a clean diagnostic (not a panic), and it
        // names the missing file.
        std::fs::write(dir.join("bad.kupl"), "use does_not_exist\nfun main() {}\n").unwrap();
        let (diags, _) = match super::load(dir.join("bad.kupl").to_str().unwrap()) {
            Ok(_) => panic!("missing module must be an error"),
            Err(e) => e,
        };
        assert!(
            diags.iter().any(|d| d.severity == crate::diag::Severity::Error
                && d.message.contains("does_not_exist")),
            "missing-module error should name the file: {diags:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_path_dependency() {
        let base = std::env::temp_dir().join(format!("kupl-pkg-test-{}", std::process::id()));
        let math = base.join("math");
        let app = base.join("app");
        std::fs::create_dir_all(&math).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(math.join("kupl.toml"), "[project]\nname = \"math\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(math.join("main.kupl"), "pub fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n").unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../math\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use math\n\nfun main() uses io {\n    print(math.add(1, 2))\n}\n",
        )
        .unwrap();

        // resolves across packages and runs
        let (program, _map) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its math dependency");
        assert_eq!(program.items.len(), 2, "app main + math add");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(interp.call_value(f, vec![], crate::diag::Span::default()).is_ok());

        // a missing dependency path is a clear K0400 at the `use` span
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../gone\" }\n",
        )
        .unwrap();
        let err = super::load(app.join("main.kupl").to_str().unwrap());
        match err {
            Err((diags, _)) => assert!(diags.iter().any(|d| d.code == "K0400"), "{diags:?}"),
            Ok(_) => panic!("missing dependency should fail to load"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn two_packages_same_name_dont_collide() {
        // two dependencies BOTH define `helper` — namespace isolation keeps them
        // distinct, and each dep's internal calls bind to its own definitions.
        let base = std::env::temp_dir().join(format!("kupl-ns-test-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        let app = base.join("app");
        for d in [&a, &b, &app] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(a.join("kupl.toml"), "[project]\nname = \"a\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(a.join("main.kupl"), "pub fun helper() -> Int {\n    1\n}\npub fun via() -> Int {\n    helper() + 10\n}\n").unwrap();
        std::fs::write(b.join("kupl.toml"), "[project]\nname = \"b\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(b.join("main.kupl"), "pub fun helper() -> Int {\n    2\n}\n").unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use a\nuse b\n\nfun probe() -> Int {\n    a.helper() + b.helper() * 10 + a.via() * 100\n}\n",
        )
        .unwrap();

        let (program, _) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("two same-named deps load without collision");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "no collision expected, got {diags:?}"
        );
        // 1 + 2*10 + 11*100 = 1121
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("probe".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(v) => assert_eq!(v.to_string(), "1121"),
            Err(_) => panic!("probe should evaluate"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dependency_version_assertion_and_lock() {
        let base = std::env::temp_dir().join(format!("kupl-ver-test-{}", std::process::id()));
        let math = base.join("math");
        let app = base.join("app");
        std::fs::create_dir_all(&math).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(math.join("kupl.toml"), "[project]\nname = \"math\"\nversion = \"1.0.0\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(math.join("main.kupl"), "pub fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n").unwrap();
        // matching version loads clean
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../math\", version = \"1.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.kupl"), "use math\n\nfun main() {\n    let _ = math.add(1, 2)\n}\n").unwrap();
        assert!(super::load(app.join("main.kupl").to_str().unwrap()).is_ok(), "matching version loads");

        // mismatched version -> K0401
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../math\", version = \"2.0.0\" }\n",
        )
        .unwrap();
        match super::load(app.join("main.kupl").to_str().unwrap()) {
            Err((diags, _)) => assert!(diags.iter().any(|d| d.code == "K0401"), "{diags:?}"),
            Ok(_) => panic!("version mismatch should fail"),
        }

        // lockfile round-trips and detects drift
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../math\", version = \"1.0.0\" }\n",
        )
        .unwrap();
        let deps = super::resolve_deps(app.join("main.kupl").to_str().unwrap()).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "math");
        let lock = super::lock_text(&deps);
        let hashes = super::lock_hashes(&lock);
        assert_eq!(hashes.get("math"), Some(&deps[0].hash)); // no drift when unchanged
        // edit the dependency source → its hash changes → drift vs the old lock
        std::fs::write(math.join("main.kupl"), "pub fun add(a: Int, b: Int) -> Int {\n    a + b + 1\n}\n").unwrap();
        let deps2 = super::resolve_deps(app.join("main.kupl").to_str().unwrap()).unwrap();
        assert_ne!(deps2[0].hash, deps[0].hash, "editing the dep changes its hash");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bare_file_has_no_deps() {
        // a single file with no kupl.toml loads exactly as before (backward compat)
        let dir = std::env::temp_dir().join(format!("kupl-bare-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.kupl"), "fun main() -> Int {\n    42\n}\n").unwrap();
        let (program, _) = super::load(dir.join("solo.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("bare file loads");
        assert_eq!(program.items.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
