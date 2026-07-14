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
    /// name -> version, for a dependency declared with ONLY `{ version = ".." }`
    /// (no `path`) -- the manifest's own doc comment documents this as valid
    /// syntax ("registry (resolved later)"), but no registry exists yet. Kept
    /// separate from `deps` (rather than silently dropped, which it was
    /// before) so a `use` of one of these names can report a clear,
    /// accurate "registry dependencies aren't supported yet" error instead
    /// of the confusing "cannot read module file <name>.kupl" a silently-
    /// dropped dependency used to produce -- indistinguishable from simply
    /// forgetting to write the file, even though the manifest correctly
    /// declared the dependency (production-hardening PR-it625).
    registry_only: HashMap<String, String>,
    /// name -> version for EVERY version-only dependency the manifest
    /// declares, regardless of whether it has already been fetched into the
    /// registry cache (unlike `registry_only`, which only holds the
    /// still-unresolved ones once a dependency's cache directory exists —
    /// production-hardening PR-it641). `kupl pkg fetch` iterates this one:
    /// `registry.rs`'s `fetch_package` doc comment is explicit that v1
    /// deliberately never cache-skips a re-fetch, and that design decision
    /// must keep holding even though `registry_only`/`deps` now do
    /// distinguish fetched from unfetched for every OTHER purpose (`use`
    /// resolution, `pkg tree`/`pkg lock`, ordinary loading).
    all_registry: HashMap<String, String>,
    prefix: String,
    /// Set when a `kupl.toml` was found but failed to parse — surfaced as a hard
    /// error rather than silently ignored (which would make the deps vanish).
    err: Option<String>,
}

/// Filesystem-identity key for `ctx_cache`, matching what `seen` (the
/// file-content dedup loop, below) already uses -- production-hardening
/// PR-it761: a REAL bug found+fixed where `ctx_cache` used to be keyed
/// directly by a dependency's `normalize()`d path (LEXICAL-only, no
/// filesystem access), while `seen`'s file-content dedup used TRUE
/// canonical (symlink-resolved) identity. A dependency directory reached
/// via two lexically-DIFFERENT paths that are the SAME real directory (most
/// realistically: two aliases where one goes through a symlinked
/// dependency directory and one doesn't) got TWO different `PkgCtx`s / two
/// different mangling prefixes here, but the file itself was only ever
/// parsed ONCE under `seen`'s stricter identity -- whichever alias's queue
/// entry happened to be popped SECOND (an accident of the loader's LIFO
/// traversal order) ended up with a mangling prefix that had ZERO items
/// registered under it, so any reference through it failed with a
/// spurious `K0240: unknown name`, for a perfectly valid, unambiguous
/// dependency graph. Live-confirmed before this fix: `b` and `c` both
/// depending on the same physical directory `d` (one directly, one via a
/// symlinked alias) made `kupl run`/`kupl check`/`kupl native` all reject
/// `d.greet(n)` inside `b` with `unknown name \`b.d$greet\` (did you mean
/// \`c.d$greet\`?)`, even though both dependency declarations are
/// individually correct. Falls back to `normalize()` when the directory
/// doesn't exist yet (`canonicalize()` requires the path to actually
/// exist), so a still-missing dependency continues to surface as today's
/// clean K0400, not a panic or a behavior change.
fn dep_identity(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| normalize(p))
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
                    let mut registry_only = HashMap::new();
                    let mut all_registry = HashMap::new();
                    for dep in &m.deps {
                        if let Some(p) = &dep.path {
                            deps.insert(dep.name.clone(), (normalize(&cur.join(p)), dep.version.clone()));
                        } else if let Some(v) = &dep.version {
                            // version-only deps resolve via the registry cache `kupl pkg
                            // fetch` populates (`registry::cache_dir()/name/version`,
                            // exactly where `registry::fetch_package` materializes them
                            // — a plain local directory, matching `registry.rs`'s own
                            // central design claim that a materialized package is
                            // indistinguishable downstream from a hand-written `{ path =
                            // ".." }` dependency). If that directory already exists,
                            // treat this dependency exactly like a local one from here
                            // on — `use`, `resolve_deps`, `pkg tree`/`pkg lock`, and
                            // ordinary loading all pick it up transparently, with no
                            // separate "run kupl pkg fetch first, then re-run" step
                            // once the fetch has actually happened. Only still-
                            // unfetched dependencies fall into `registry_only` below
                            // (production-hardening PR-it641 — unifies the
                            // resolve_deps/registry_only_deps split PR-it633 deferred).
                            all_registry.insert(dep.name.clone(), v.clone());
                            let cached = crate::registry::cache_dir().join(&dep.name).join(v);
                            if cached.join("kupl.toml").is_file() {
                                deps.insert(dep.name.clone(), (cached, Some(v.clone())));
                            } else {
                                registry_only.insert(dep.name.clone(), v.clone());
                            }
                        }
                    }
                    return Rc::new(PkgCtx {
                        root: cur.to_path_buf(),
                        deps,
                        registry_only,
                        all_registry,
                        prefix: prefix.to_string(),
                        err: None,
                    });
                }
                // The manifest exists but is malformed — stop and report it, rather
                // than walking past it (which would silently drop the project's deps).
                Err(e) => {
                    return Rc::new(PkgCtx {
                        root: cur.to_path_buf(),
                        deps: HashMap::new(),
                        registry_only: HashMap::new(),
                        all_registry: HashMap::new(),
                        prefix: prefix.to_string(),
                        err: Some(format!("invalid manifest {}: {e}", toml.display())),
                    });
                }
            }
        }
        d = if walk { cur.parent() } else { None };
    }
    Rc::new(PkgCtx {
        root: dir.to_path_buf(),
        deps: HashMap::new(),
        registry_only: HashMap::new(),
        all_registry: HashMap::new(),
        prefix: prefix.to_string(),
        err: None,
    })
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
/// manifest or entry source cannot be read -- and, just as much, if `entry`
/// ITSELF cannot be read. A REAL bug (PR-it593): this function already holds
/// every DEPENDENCY's entry to that standard (below), but never checked its
/// OWN entry the same way, since it only ever touches `entry`'s PARENT
/// directory (to find the enclosing `kupl.toml`) and otherwise never reads
/// `entry` at all -- so `kupl pkg tree`/`kupl pkg lock` on a typo'd or
/// missing entry path silently reported "no dependencies" / wrote an empty
/// `kupl.lock` with exit 0, instead of the same "cannot read" error every
/// other subcommand (`run`/`check`/`native`/`test`/`build`, all routed
/// through `load`/`load_compile`) gives for a bad entry path.
pub fn resolve_deps(entry: &str) -> Result<Vec<ResolvedDep>, String> {
    let entry_path = PathBuf::from(entry);
    if let Err(e) = std::fs::read_to_string(&entry_path) {
        return Err(format!("entry {}: {e}", entry_path.display()));
    }
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

/// The direct dependencies declared with ONLY a `version` (no `path`) in the
/// project owning `entry` — registry dependencies, which cannot resolve until
/// a registry exists. Returns (name, version) pairs, sorted by name. Kept
/// separate from `resolve_deps` (rather than folded into `ResolvedDep`, which
/// always carries a `path`/`hash` these can never have) so `kupl pkg
/// tree`/`kupl pkg lock` can report them explicitly instead of the project
/// simply looking like it has fewer dependencies than its manifest actually
/// declares (production-hardening PR-it625 — the same silent-drop this
/// module's `use`-resolution path was ALSO fixed to report clearly).
pub fn registry_only_deps(entry: &str) -> Result<Vec<(String, String)>, String> {
    let entry_path = PathBuf::from(entry);
    if let Err(e) = std::fs::read_to_string(&entry_path) {
        return Err(format!("entry {}: {e}", entry_path.display()));
    }
    let dir = entry_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let ctx = pkg_ctx(&dir, true, "");
    if let Some(e) = &ctx.err {
        return Err(e.clone());
    }
    let mut out: Vec<(String, String)> = ctx.registry_only.iter().map(|(n, v)| (n.clone(), v.clone())).collect();
    out.sort();
    Ok(out)
}

/// EVERY version-only (`{ version = ".." }`, no `path`) direct dependency the
/// project owning `entry` declares — including ones already fetched into the
/// registry cache, unlike `registry_only_deps` above (which drops those once
/// resolved). `kupl pkg fetch` uses this one, not `registry_only_deps`, so
/// that re-running it still re-fetches and re-verifies every registry
/// dependency fresh even after a prior successful fetch — `registry.rs`'s
/// `fetch_package` doc comment is explicit that v1 deliberately never
/// cache-skips (production-hardening PR-it641).
pub fn all_registry_deps(entry: &str) -> Result<Vec<(String, String)>, String> {
    let entry_path = PathBuf::from(entry);
    if let Err(e) = std::fs::read_to_string(&entry_path) {
        return Err(format!("entry {}: {e}", entry_path.display()));
    }
    let dir = entry_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let ctx = pkg_ctx(&dir, true, "");
    if let Some(e) = &ctx.err {
        return Err(e.clone());
    }
    let mut out: Vec<(String, String)> = ctx.all_registry.iter().map(|(n, v)| (n.clone(), v.clone())).collect();
    out.sort();
    Ok(out)
}

/// Escape a `kupl.lock` field so it can never be mistaken for the `\t`
/// column delimiter or a `\n` line break -- production-hardening PR-it762:
/// a REAL bug found+fixed where a dependency NAME containing a literal tab
/// byte (`manifest.rs` places NO identifier-syntax restriction on a
/// `[dependencies]` key -- it's just `key.trim()` from a raw
/// `split_once('=')`) produced a lock line with 5 tab-separated columns
/// instead of 4, silently dropped by `lock_hashes`'s exact `cols.len() ==
/// 4` check -- with no error, no warning, and no indication in `kupl pkg
/// tree`'s output that drift detection had gone dark for that ONE
/// dependency (confirmed live: a genuine source change to the affected
/// dependency produced NO `[drift]` marker, while a sibling dependency in
/// the SAME lockfile continued to correctly show `[drift]`). The identical
/// corruption is reachable through a dependency's `version` string too
/// (`manifest.rs`'s `parse_string` does no escape processing on quoted
/// string content), so the fix lives in the lock format's OWN
/// serialization rather than trying to reject every field independently at
/// every point data enters it.
fn escape_lock_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Reverse `escape_lock_field`.
fn unescape_lock_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                // an unrecognized escape (or a dangling trailing backslash) is
                // preserved verbatim rather than silently eaten -- this can
                // only arise from hand-editing a lockfile against its own
                // "do not edit by hand" header, so surfacing the raw text
                // (which then simply won't match any real hash) is safer
                // than guessing.
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Serialize resolved deps to the `kupl.lock` line format:
/// `name<TAB>path<TAB>version<TAB>hash` (one per line, name-sorted). Each
/// field is escaped (`escape_lock_field`) so an embedded `\t`/`\n`/`\\` in
/// any field can never be mistaken for the column/line delimiters.
pub fn lock_text(deps: &[ResolvedDep]) -> String {
    let mut s = String::from("# kupl.lock — resolved dependencies (do not edit by hand)\n");
    for d in deps {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            escape_lock_field(&d.name),
            escape_lock_field(&d.path),
            escape_lock_field(&d.version),
            escape_lock_field(&d.hash)
        ));
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
            m.insert(unescape_lock_field(cols[0]), unescape_lock_field(cols[3]));
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
    // Seed the cache with the ROOT package itself, keyed under the same
    // `normalize()`d form every dependency's own `path` entry is stored under
    // (see `pkg_ctx` above) -- production-hardening PR-it746, closing a real
    // bug: without this, a dependency (however many hops away) that declares
    // a `path` dependency BACK to the root project's own directory (a
    // circular or self-dependency through the root) missed this cache
    // entirely and fabricated a BOGUS second `PkgCtx` for root with a
    // synthesized non-empty mangling prefix, even though root's own items
    // were already tagged with the real, empty prefix (`resolve.rs` never
    // mangles the root package). Any cross-package reference back into root
    // from inside such a cycle then failed to resolve -- e.g. `dep.root$compute`
    // (an internal mangling artifact leaking into a user-facing "unknown name"
    // diagnostic) for a perfectly legitimate, public root function.
    ctx_cache.insert(dep_identity(&root_dir), root_ctx.clone());
    let mut queue: Vec<(PathBuf, Rc<PkgCtx>, Option<Span>)> = vec![(entry_path, root_ctx, None)];
    // items tagged with their owning package's mangling prefix, plus each
    // package's own alias table (alias name -> that dependency's resolved
    // prefix) — fed to the namespace-isolation pass.
    let mut tagged: Vec<(crate::ast::Item, String)> = Vec::new();
    let mut pkg_deps: HashMap<String, HashMap<String, String>> = HashMap::new();

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
                // package is mangled with a prefix unique to ITS OWN position
                // in the dependency graph (this package's own prefix, chained
                // with the alias), NOT the bare alias text. A REAL namespace-
                // isolation-bypass bug found+fixed (production-hardening
                // PR-it698): two UNRELATED dependencies can each independently
                // alias a DIFFERENT sub-dependency as the SAME name (e.g. both
                // `depA` and `depB` `use shared`, pointing at two entirely
                // different physical packages) — mangling both under the bare
                // alias `shared` made their definitions collide (or, worse,
                // with no name collision at all, silently invoke whichever
                // definition happened to load last, since `try_qualified` in
                // resolve.rs used to build the qualified reference from the
                // same bare alias text too). `ctx_cache` dedupes by PHYSICAL
                // directory (`dep_identity`, PR-it761 -- see its own doc
                // comment: this used to be keyed by lexical `normalize()`
                // alone, which could still assign TWO prefixes to one real
                // directory), so a genuinely SHARED dependency (the exact
                // same path reached via two different alias chains) still
                // gets ONE mangled namespace, as intended — only the prefix
                // CHOSEN for a not-yet-cached directory changes.
                let dep_ctx = ctx_cache
                    .entry(dep_identity(dep_dir))
                    .or_insert_with(|| {
                        let dep_prefix = if ctx.prefix.is_empty() {
                            first.to_string()
                        } else {
                            format!("{}.{first}", ctx.prefix)
                        };
                        pkg_ctx(dep_dir, false, &dep_prefix)
                    })
                    .clone();
                // A REAL, live-confirmed bug found+fixed (production-hardening
                // PR-it766, discovered while fixing the `entry`-field path-escape
                // bug right below): unlike `root_ctx.err` (checked once, right at
                // the very start of this function), `dep_ctx.err` -- set by
                // `pkg_ctx` above when a DEPENDENCY's OWN `kupl.toml` fails to
                // parse for ANY reason (missing `[project]`, an unknown section, a
                // malformed line, or -- as of this same iteration -- an unsafe
                // `entry` value) -- was NEVER checked anywhere in this `use`-
                // resolution loop. The redundant manifest re-read two lines below
                // (`crate::manifest::read(...).map(|m| m.entry).unwrap_or_else(|_|
                // "main.kupl".to_string())`) silently swallowed that SAME error and
                // fell back to trying "main.kupl" directly -- so a dependency with
                // a genuinely broken manifest either compiled successfully with the
                // WRONG entry file (if a "main.kupl" happened to also exist) or
                // failed later with a confusing, unrelated "cannot read module
                // file" error that never mentions the actual broken manifest.
                // `kupl pkg tree`/`kupl pkg lock` (a separate code path,
                // `resolve_deps`, which propagates manifest errors via `?`) already
                // reported this correctly -- only the primary `run`/`check`/
                // `build`/`native`/`test` compile path had this gap.
                if let Some(e) = &dep_ctx.err {
                    diags.push(Diag::error("K0401", e.clone(), *span));
                    continue;
                }
                pkg_deps
                    .entry(ctx.prefix.clone())
                    .or_default()
                    .insert(first.to_string(), dep_ctx.prefix.clone());
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
            } else if let Some(version) = ctx.registry_only.get(first) {
                // A registry-only dependency (declared `{ version = ".." }` with no
                // `path`) — no registry exists yet, so this can never resolve. Report
                // that PLAINLY rather than falling through to the local-file lookup
                // below, which would otherwise report "cannot read module file
                // <name>.kupl" — indistinguishable from simply forgetting to write
                // the file, even though the manifest correctly declared the
                // dependency (production-hardening PR-it625).
                diags.push(Diag::error(
                    "K0401",
                    format!(
                        "dependency `{first}` (version {version}) has no `path` — registry \
                         dependencies are not supported yet; declare a local `{{ path = \"...\" }}` \
                         dependency instead"
                    ),
                    *span,
                ));
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
    //
    // A REAL bug found+fixed (production-hardening PR-it746): the original
    // `tagged.iter().any(|(_, p)| !p.is_empty())` check assumed "no item has
    // a non-empty prefix" means "isolate() has nothing to do" -- true for an
    // ordinary single-package program, but FALSE for a project with a SELF
    // dependency (`kupl.toml` declaring `me = { path = "." }`): every item
    // still has the root's empty prefix (root is never mangled), yet
    // `pkg_deps[""]` can still hold a real `alias -> resolved-prefix`
    // mapping (`"me" -> ""`) that `try_qualified` needs to rewrite a
    // `me.compute()` REFERENCE into a bare `compute()` call. Skipping
    // `isolate()` entirely left such references as unrewritten
    // `MethodCall{recv: Ident("me"), ..}` nodes, which the checker then
    // reported as `unknown name \`me\`` -- `me` was never meant to exist as
    // a real identifier, only as a package alias. `pkg_deps` is only ever
    // populated when a genuine cross-package `use` was resolved (regardless
    // of whether that resolves back to root or to a distinct package), so
    // checking it directly covers both cases.
    program.items = if tagged.iter().any(|(_, p)| !p.is_empty()) || !pkg_deps.is_empty() {
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

    /// A REAL, live-confirmed bug found+fixed (production-hardening PR-it766,
    /// discovered while fixing the `entry`-field path-escape bug in the SAME
    /// iteration): unlike `root_ctx.err` (checked once, right at the start of
    /// `load_with`, by `malformed_manifest_is_a_clean_error_not_silently_ignored`
    /// above), `dep_ctx.err` -- set by `pkg_ctx` when a DEPENDENCY's OWN
    /// `kupl.toml` fails to parse for ANY reason -- was NEVER checked anywhere
    /// in the `use`-resolution loop. A redundant manifest re-read on the very
    /// next line silently swallowed that SAME error via
    /// `.unwrap_or_else(|_| "main.kupl".to_string())` and fell back to trying
    /// "main.kupl" directly -- so a dependency with a genuinely broken
    /// manifest either compiled with the WRONG entry file (if a "main.kupl"
    /// happened to also exist, as it usually does) or failed later with a
    /// confusing, unrelated error that never mentions the actual broken
    /// manifest. `kupl pkg tree`/`kupl pkg lock` (`resolve_deps`, a separate
    /// code path that propagates manifest errors via `?`) already reported
    /// this correctly -- only the primary `run`/`check`/`build` compile path
    /// had this gap.
    #[test]
    fn a_dependencys_own_malformed_manifest_is_a_clean_error_not_silently_ignored() {
        let base = std::env::temp_dir().join(format!("kupl-dep-badtoml-it766-{}", std::process::id()));
        let app = base.join("app");
        let dep = base.join("dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.kupl"), "use dep\nfun main() {}\n").unwrap();
        // dep's OWN manifest is malformed -- a genuinely broken kupl.toml, not
        // just an unsafe entry value (that narrower case is covered live in
        // manifest.rs's own `an_absolute_or_parent_escaping_entry_is_a_clean_parse_error`).
        std::fs::write(dep.join("kupl.toml"), "this is not toml at all {{{\n").unwrap();
        std::fs::write(dep.join("main.kupl"), "pub fun helper() -> Int {\n    1\n}\n").unwrap();

        let (diags, _) = match super::load(app.join("main.kupl").to_str().unwrap()) {
            Ok(_) => panic!("a dependency with a malformed manifest must be a load error, not silently ignored"),
            Err(e) => e,
        };
        assert!(
            diags.iter().any(|d| d.severity == crate::diag::Severity::Error
                && d.message.contains("invalid manifest")),
            "should report the dependency's actual invalid manifest, not a generic downstream error: {diags:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
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

    /// A REAL bug found+fixed (production-hardening PR-it746): this module's
    /// own doc comment claims "cycles are fine" -- true for a cycle among
    /// non-root packages (unaffected by this fix, still verified below), but
    /// FALSE for a cycle that loops back through the ROOT project's own
    /// directory. `ctx_cache` (keyed by normalized directory) never contained
    /// an entry for root itself, so a dependency declaring a `path` back to
    /// root's directory missed the cache and fabricated a BOGUS second
    /// `PkgCtx` for root with a synthesized non-empty mangling prefix (e.g.
    /// `dep2.root`), even though root's real items were tagged with the
    /// correct, empty prefix. A legitimate, public root function referenced
    /// from inside the cycle (`dep2.root.compute()`) then failed to resolve,
    /// reporting `unknown name \`dep2.root$compute\`` -- an internal `$`
    /// mangling artifact leaking verbatim into a user-facing diagnostic.
    #[test]
    fn a_dependency_cycle_that_loops_back_through_the_root_package_resolves_cleanly() {
        let base = std::env::temp_dir().join(format!("kupl-pkg-cycle-test-{}", std::process::id()));
        let root = base.join("root");
        let dep2 = base.join("dep2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&dep2).unwrap();
        std::fs::write(
            root.join("kupl.toml"),
            "[project]\nname = \"root\"\nentry = \"main.kupl\"\n\n[dependencies]\ndep2 = { path = \"../dep2\" }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.kupl"),
            "use dep2\n\npub fun compute() -> Int {\n    41\n}\n\nfun main() uses io {\n    print(dep2.helper())\n}\n",
        )
        .unwrap();
        std::fs::write(
            dep2.join("kupl.toml"),
            "[project]\nname = \"dep2\"\nentry = \"main.kupl\"\n\n[dependencies]\nroot = { path = \"../root\" }\n",
        )
        .unwrap();
        std::fs::write(
            dep2.join("main.kupl"),
            "use root\n\npub fun helper() -> Int {\n    root.compute() + 1\n}\n",
        )
        .unwrap();

        let (program, _map) = super::load(root.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("a root-involving dependency cycle must resolve, not error");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "root.compute() must resolve cleanly from inside the cycle, no mangled-name leak: {diags:?}"
        );
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(interp.call_value(f, vec![], crate::diag::Span::default()).is_ok(), "dep2.helper() -> root.compute() + 1 = 42");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A SECOND, independently-real bug found+fixed in the SAME investigation as
    /// the test above (production-hardening PR-it746): a project declaring a
    /// SELF dependency (`kupl.toml`'s `[dependencies]` pointing `path = "."` back
    /// at itself) hit a DIFFERENT failure than the two-package cycle case --
    /// `program.items`'s own `isolate()`-skip fast path assumed "no item has a
    /// non-empty prefix" means "nothing to isolate," but a self dependency keeps
    /// every item at root's own empty prefix (root is never mangled) while STILL
    /// needing `pkg_deps[""]`'s `"me" -> ""` alias entry rewritten via
    /// `try_qualified` -- skipping `isolate()` entirely left `me.compute()`
    /// completely unrewritten, reported as `unknown name \`me\`` (worse than the
    /// cycle case's leaked mangling artifact: here `me` isn't recognized as a
    /// dependency reference AT ALL).
    #[test]
    fn a_self_dependency_on_the_root_package_resolves_cleanly() {
        let base = std::env::temp_dir().join(format!("kupl-pkg-selfdep-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nme = { path = \".\" }\n",
        )
        .unwrap();
        std::fs::write(
            base.join("main.kupl"),
            "use me\n\npub fun compute() -> Int {\n    41\n}\n\nfun main() uses io {\n    print(me.compute() + 1)\n}\n",
        )
        .unwrap();

        let (program, _map) = super::load(base.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("a self-dependency on root must resolve, not error");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "`me` must resolve as the self-dependency alias it is, not an unknown name: {diags:?}"
        );
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(interp.call_value(f, vec![], crate::diag::Span::default()).is_ok(), "me.compute() + 1 = 42");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL bug found+fixed (production-hardening PR-it628): a cross-
    /// package type/constructor's mangled name (`resolve.rs`'s own `pkg$name`
    /// scheme) used to leak verbatim into user-facing `Display` output AND
    /// type-checker error messages — `math.origin()`'s `Point` value printed
    /// as `math$Point(0, 0)` instead of `Point(0, 0)`, and a type mismatch
    /// reported "expected math$Point" instead of "expected Point". Confirmed
    /// via a live 3-engine repro (interp/vm/native all agreed on the leak)
    /// before this fix. Covers BOTH the runtime `Display` leak (via the
    /// returned `Value`'s `to_string()`, which is exactly what `print()`
    /// uses internally) and the compile-time error-message leak (via a
    /// deliberate type mismatch), in one fixture.
    #[test]
    fn cross_package_type_names_are_demangled_for_display() {
        let base = std::env::temp_dir().join(format!("kupl-demangle-test-{}", std::process::id()));
        let math = base.join("math");
        let app = base.join("app");
        std::fs::create_dir_all(&math).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(math.join("kupl.toml"), "[project]\nname = \"math\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(
            math.join("main.kupl"),
            "pub type Point = Point(x: Int, y: Int)\npub fun origin() -> Point {\n    Point(x: 0, y: 0)\n}\n",
        )
        .unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nmath = { path = \"../math\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use math\n\nfun get_point() {\n    math.origin()\n}\n",
        )
        .unwrap();

        // the runtime Display leak: the returned value's `to_string()` (what
        // `print()` uses internally) must show the ORIGINAL name, not the
        // internal `math$Point` mangled one.
        let (program, _map) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its math dependency");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("get_point".to_string()));
        let v = match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(v) => v,
            Err(_) => panic!("get_point should evaluate"),
        };
        assert_eq!(v.to_string(), "Point(0, 0)", "the mangled `math$` prefix must not leak into Display");
        assert_eq!(v.type_name(), "Point", "the mangled `math$` prefix must not leak into type_name either");

        // the compile-time error-message leak: an intentional type mismatch
        // involving the cross-package type must name it as `Point`, not
        // `math$Point`.
        std::fs::write(
            app.join("main.kupl"),
            "use math\n\nfun main() uses io {\n    let p = math.origin()\n    print(p + 1)\n}\n",
        )
        .unwrap();
        let (program2, _map2) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads and parses even though it has a type error");
        let (_, diags2) = crate::check::check(&program2);
        let err = diags2
            .iter()
            .find(|d| d.severity == crate::diag::Severity::Error)
            .expect("a type mismatch should be reported");
        assert!(err.message.contains("Point"), "error should name the type as Point: {}", err.message);
        assert!(
            !err.message.contains("math$Point"),
            "the mangled name must not leak into the error message: {}",
            err.message
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL bug found+fixed (production-hardening PR-it684): `resolve.rs`'s
    /// `Rewriter::component` visited a component's `props`/`state` field
    /// DECLARATIONS (types, default/init expressions) but never bound their
    /// NAMES into its own scope tracking -- so a bare reference to a state or
    /// prop field inside a handler/exposed-method body was treated as an
    /// ordinary global reference, not a component-local one. If the SAME
    /// dependency package also happened to define a top-level `fun` with the
    /// identical bare name (legal: different namespaces), the mangling pass
    /// incorrectly rewrote the state/prop reference to the package's mangled
    /// TOP-LEVEL name. Confirmed live before this fix: `state counter: Int`
    /// alongside a top-level `fun counter()` in the same package made
    /// `counter += 1` inside a handler fail with `K0220: unknown variable
    /// dep$counter` (mangled, no longer matching the un-mangled state
    /// field) -- and a bare `counter` READ elsewhere didn't even fail
    /// cleanly, it silently resolved to the mangled TOP-LEVEL FUN instead, a
    /// genuine wrong-value substitution surfaced only as a confusing
    /// downstream type mismatch ("expected Int, found fn() -> Int").
    #[test]
    fn component_state_field_name_colliding_with_a_top_level_fun_is_not_mangled() {
        let base = std::env::temp_dir().join(format!("kupl-state-collision-test-{}", std::process::id()));
        let dep = base.join("dep");
        let app = base.join("app");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep.join("kupl.toml"), "[project]\nname = \"dep\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(
            dep.join("main.kupl"),
            "pub fun counter() -> Int {\n    42\n}\n\n\
             pub component Widget {\n    \
             intent \"collides with a top-level fun by name\"\n    \
             state counter: Int = 0\n    \
             expose fun bump() {\n        counter += 1\n    }\n    \
             expose fun value() -> Int {\n        counter\n    }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use dep\n\nfun main() uses io {\n    \
             let w = dep.Widget()\n    w.bump()\n    print(w.value())\n}\n",
        )
        .unwrap();

        let (program, _map) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its dep dependency");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "state field reference must not be mistaken for the same-named top-level fun: {diags:?}"
        );
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(
            interp.call_value(f, vec![], crate::diag::Span::default()).is_ok(),
            "main() should run cleanly"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL bug found+fixed (production-hardening PR-it775, an Explore
    /// survey finding, agentId ad3c3f6ee2f0cd891, independently re-verified
    /// live before implementing): `resolve.rs`'s `Rewriter::pattern` only
    /// recursed into `PatternKind::Bind`/`Ctor` -- `Or(alts)` (`A | B`) and
    /// `At { name, inner }` (`name @ SUBPATTERN`) fell into the catch-all `_
    /// => {}` and were never recursed into. Since `isolate()` mangles a
    /// dependency package's OWN constructor definitions to `pkg$Name`, a
    /// package matching its OWN type via `A | B` or `name @ pat` kept the
    /// BARE constructor name in the pattern while the constructor itself got
    /// mangled -- a guaranteed mismatch. Confirmed live before this fix: the
    /// identical `match s { Circle | Square => "known" }` logic compiled and
    /// ran fine as a plain single-file program, but failed with K0257
    /// (non-exhaustive match) and two K0254 (unknown constructor) errors when
    /// loaded as a dependency package -- a real language feature broken
    /// specifically by the package-isolation pass. `At`'s own `name` binding
    /// was ALSO never registered via `self.bind()`, a second, adjacent gap
    /// fixed in the same arm.
    #[test]
    fn or_and_at_patterns_against_a_dependencys_own_types_resolve_correctly() {
        let base = std::env::temp_dir().join(format!("kupl-orat-pattern-test-{}", std::process::id()));
        let dep = base.join("shapes");
        let app = base.join("app");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep.join("kupl.toml"), "[project]\nname = \"shapes\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(
            dep.join("main.kupl"),
            "pub type Shape = Circle | Square\n\n\
             pub fun classify(s: Shape) -> Str {\n    \
             match s {\n        \
             Circle | Square => \"known\"\n    \
             }\n\
             }\n\n\
             pub fun describe(s: Shape) -> Str {\n    \
             match s {\n        \
             whole @ Circle => \"circle: {whole}\"\n        \
             whole @ Square => \"square: {whole}\"\n    \
             }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\nshapes = { path = \"../shapes\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use shapes\n\nfun main() uses io {\n    \
             print(shapes.classify(shapes.Circle))\n    \
             print(shapes.describe(shapes.Square))\n\
             }\n",
        )
        .unwrap();

        let (program, _map) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its shapes dependency");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "Or/At patterns against the dependency's OWN type must not produce unknown-constructor/non-exhaustive errors: {diags:?}"
        );
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(
            interp.call_value(f, vec![], crate::diag::Span::default()).is_ok(),
            "main() should run cleanly"
        );

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

    /// A REAL, previously-silent namespace-isolation BYPASS (production-hardening
    /// PR-it698): `depA` and `depB` are UNRELATED packages that each independently
    /// alias a DIFFERENT sub-dependency as `shared` (their own local choice, no
    /// coordination possible between them). Before this fix, both got mangled under
    /// the bare alias text `shared` (not a unique-per-dependency-graph-edge prefix),
    /// AND cross-package references (`shared.addA(...)`) were rewritten using that
    /// SAME bare alias text too -- so `depB.calcB`'s call to `shared.addA` (which
    /// `utilB`, `depB`'s OWN `shared`, does NOT define) silently resolved to
    /// `utilA`'s `addA` instead of failing, purely because `depA` (an unrelated
    /// sibling) happened to alias ITS OWN unrelated dependency `shared` too.
    /// Confirmed live before this fix: `kupl check`/`kupl run` on this EXACT
    /// scenario reported clean/`7 7` instead of an unknown-name error. Fixed by
    /// mangling each dependency under a prefix chained from its OWN position in the
    /// dependency graph (`{parent_prefix}.{alias}`) and threading each package's
    /// alias->resolved-prefix table through to reference-rewriting too (not just
    /// definition-mangling) -- so `depA`'s `shared` and `depB`'s `shared` are two
    /// entirely distinct namespaces, byte-identical on interp/KVM/native.
    /// Scaffolds the diamond-alias fixture used by both regression tests below:
    /// `depA`/`depB` are UNRELATED packages that each independently alias a
    /// DIFFERENT sub-dependency as `shared` (their own local choice, no
    /// coordination possible between them). `util_b_addr` controls whether
    /// `utilB` (depB's OWN `shared`) also defines an `addA` -- the two tests
    /// exercise the "both sides otherwise valid" and "depB's shared genuinely
    /// lacks addA" scenarios respectively (`check()` validates the WHOLE merged
    /// program regardless of what `probe()` itself calls, so these must be
    /// separate fixtures, not two phases of one program).
    fn diamond_alias_fixture(tag: &str, util_b_body: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("kupl-diamond-alias-{tag}-{}", std::process::id()));
        let util_a = base.join("utilA");
        let util_b = base.join("utilB");
        let dep_a = base.join("depA");
        let dep_b = base.join("depB");
        let app = base.join("app");
        for d in [&util_a, &util_b, &dep_a, &dep_b, &app] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(util_a.join("kupl.toml"), "[project]\nname = \"utilA\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(util_a.join("main.kupl"), "pub fun addA(a: Int, b: Int) -> Int {\n    a + b\n}\n").unwrap();
        std::fs::write(util_b.join("kupl.toml"), "[project]\nname = \"utilB\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(util_b.join("main.kupl"), util_b_body).unwrap();
        std::fs::write(
            dep_a.join("kupl.toml"),
            "[project]\nname = \"depA\"\nentry = \"main.kupl\"\n\n[dependencies]\nshared = { path = \"../utilA\" }\n",
        )
        .unwrap();
        std::fs::write(dep_a.join("main.kupl"), "use shared\npub fun calcA(x: Int, y: Int) -> Int {\n    shared.addA(x, y)\n}\n").unwrap();
        std::fs::write(
            dep_b.join("kupl.toml"),
            "[project]\nname = \"depB\"\nentry = \"main.kupl\"\n\n[dependencies]\nshared = { path = \"../utilB\" }\n",
        )
        .unwrap();
        std::fs::write(dep_b.join("main.kupl"), "use shared\npub fun calcB(x: Int, y: Int) -> Int {\n    shared.addA(x, y)\n}\n").unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\ndepA = { path = \"../depA\" }\ndepB = { path = \"../depB\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use depA\nuse depB\nfun probe() -> Int {\n    depA.calcA(3, 4) + depB.calcB(3, 4)\n}\n",
        )
        .unwrap();
        app.join("main.kupl")
    }

    /// A REAL, previously-silent namespace-isolation BYPASS (production-hardening
    /// PR-it698). Before this fix, `depA`'s and `depB`'s (unrelated, independently-
    /// chosen) `shared` aliases were BOTH mangled under the bare alias text
    /// `shared` (not a prefix unique to each's OWN position in the dependency
    /// graph), AND cross-package references (`shared.addA(...)`) were rewritten
    /// using that SAME bare alias text too -- so even when BOTH `utilA` and
    /// `utilB` legitimately define their OWN `addA`, `depB.calcB`'s call could
    /// silently resolve to `utilA`'s `addA` instead of `utilB`'s, purely because
    /// `depA` happened to alias its own unrelated dependency `shared` too.
    /// Confirmed live before this fix: both calls returned utilA's `7`, never
    /// reaching utilB's `12`. Fixed by mangling each dependency under a prefix
    /// chained from its OWN position in the dependency graph
    /// (`{parent_prefix}.{alias}`) and threading each package's own
    /// alias->resolved-prefix table through reference-rewriting too (not just
    /// definition-mangling) -- so `depA`'s `shared` and `depB`'s `shared` are two
    /// entirely distinct namespaces, byte-identical on interp/KVM/native.
    #[test]
    fn diamond_dependency_with_same_alias_resolves_each_package_to_its_own_util() {
        let entry = diamond_alias_fixture(
            "ok",
            "pub fun addA(a: Int, b: Int) -> Int {\n    a * b\n}\n",
        );
        let (program, _) = super::load(entry.to_str().unwrap()).map_err(|(d, _)| format!("{d:?}")).expect("loads");
        let (checked, diags) = crate::check::check(&program);
        assert!(diags.iter().all(|d| d.severity != crate::diag::Severity::Error), "{diags:?}");
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("probe".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            // depA.calcA = utilA.addA(3,4) = 7; depB.calcB = utilB.addA(3,4) = 12
            Ok(v) => assert_eq!(v.to_string(), "19", "each dep must resolve `shared` to its OWN util, not cross-contaminate"),
            Err(_) => panic!("probe should evaluate"),
        }
        let _ = std::fs::remove_dir_all(entry.parent().unwrap().parent().unwrap());
    }

    /// Sibling to the test above: `utilB` (depB's OWN `shared`) genuinely does
    /// NOT define `addA` at all. Before this fix, `depB.calcB`'s `shared.addA`
    /// call silently resolved against `utilA`'s `addA` (a package `depB` never
    /// declared as a dependency) instead of failing -- confirmed live: `kupl
    /// check` reported `ok`. This must be a clean, hard unknown-name error.
    #[test]
    fn diamond_dependency_where_one_side_genuinely_lacks_the_function_is_a_clean_error() {
        let entry = diamond_alias_fixture(
            "bad",
            "pub fun subB(a: Int, b: Int) -> Int {\n    a - b\n}\n",
        );
        let (program, _) = super::load(entry.to_str().unwrap()).map_err(|(d, _)| format!("{d:?}")).expect("loads");
        let (_, diags) = crate::check::check(&program);
        assert!(
            diags.iter().any(|d| d.severity == crate::diag::Severity::Error
                && d.message.contains("unknown name")
                && d.message.contains("addA")),
            "depB's `shared.addA` (utilB has no addA) must be an unknown-name error, \
             not silently resolved against utilA's addA: {diags:?}"
        );
        let _ = std::fs::remove_dir_all(entry.parent().unwrap().parent().unwrap());
    }

    /// A companion fix (production-hardening PR-it698): a GENUINE cross-package
    /// name collision (two files inside the SAME mangled dependency package both
    /// defining `helper`) used to report `K0203: function \`dep$helper\` is
    /// defined more than once` -- leaking the internal `pkg$name` mangling
    /// artifact into a user-facing diagnostic (the user never wrote `dep$helper`
    /// anywhere). `demangle_for_display` was already used for print()/type-
    /// mismatch messages (PR-it628) but never wired into this specific
    /// duplicate-definition diagnostic (nor its K0201/K0202/K0260 siblings for
    /// types/constructors/contracts).
    #[test]
    fn cross_package_duplicate_definition_message_is_demangled() {
        let base = std::env::temp_dir().join(format!("kupl-dup-demangle-test-{}", std::process::id()));
        let dep = base.join("dep");
        let app = base.join("app");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep.join("kupl.toml"), "[project]\nname = \"dep\"\nentry = \"a.kupl\"\n").unwrap();
        std::fs::write(dep.join("a.kupl"), "use b\npub fun helper() -> Int {\n    1\n}\n").unwrap();
        std::fs::write(dep.join("b.kupl"), "pub fun helper() -> Int {\n    2\n}\n").unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.kupl"), "use dep\nfun probe() -> Int {\n    dep.helper()\n}\n").unwrap();

        let (program, _) = super::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("loads");
        let (_, diags) = crate::check::check(&program);
        let dup = diags
            .iter()
            .find(|d| d.code == "K0203")
            .unwrap_or_else(|| panic!("expected a K0203 duplicate-function diagnostic: {diags:?}"));
        assert!(dup.message.contains("`helper`"), "message must name the bare, user-written identifier: {}", dup.message);
        assert!(!dup.message.contains('$'), "message must NOT leak the internal `pkg$name` mangling artifact: {}", dup.message);
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

    #[test]
    fn resolve_deps_errors_on_a_missing_entry_instead_of_silently_reporting_none() {
        // A REAL bug found+fixed (PR-it593): `resolve_deps` (which backs `kupl pkg
        // tree`/`kupl pkg lock`) already validates every DEPENDENCY's entry file is
        // readable, but never validated its OWN `entry` the same way -- since it only
        // ever reads `entry`'s PARENT directory (to find `kupl.toml`), a typo'd or
        // missing entry path used to silently resolve to "no dependencies" instead of
        // the same "cannot read" error every other subcommand gives.
        let missing = "/definitely/does/not/exist/kupl-it593-repro.kupl";
        match super::resolve_deps(missing) {
            Ok(deps) => panic!("a missing entry file must error, not resolve to {} deps", deps.len()),
            Err(e) => assert!(e.contains(missing), "{e}"),
        }

        // a project WITH real dependencies still resolves correctly -- the new
        // entry-readability check doesn't regress the happy path.
        let base = std::env::temp_dir().join(format!("kupl-pkg-missing-entry-{}", std::process::id()));
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
        std::fs::write(app.join("main.kupl"), "use math\nfun main() {}\n").unwrap();
        let deps = super::resolve_deps(app.join("main.kupl").to_str().unwrap()).expect("resolves fine");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "math");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL usability bug found+fixed (production-hardening PR-it625):
    /// `kupl.toml`'s own doc comment (top of this file) documents
    /// `foo = { version = "1.2.0" }` (no `path`) as valid syntax for a
    /// "registry (resolved later)" dependency -- but `pkg_ctx` silently
    /// DROPPED any dependency with no `path`, so a `use` of that name fell
    /// through to the SAME local-file lookup as an undeclared name, giving
    /// "cannot read module file foo.kupl: No such file or directory" --
    /// indistinguishable from simply forgetting to write the file, even
    /// though the manifest correctly declared the dependency and a registry
    /// simply doesn't exist yet. Fixed by tracking registry-only deps
    /// separately (`PkgCtx::registry_only`) and reporting them with a clear,
    /// specific K0401 naming the actual cause (no registry support yet)
    /// instead of falling through to the generic file-not-found path.
    #[test]
    fn version_only_dependency_reports_a_clear_registry_error_not_a_confusing_file_not_found() {
        let dir = std::env::temp_dir().join(format!("kupl-registry-dep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\njson2 = { version = \"1.2.0\" }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "use json2\nfun main() {}\n").unwrap();
        let (diags, _) = match super::load(dir.join("main.kupl").to_str().unwrap()) {
            Ok(_) => panic!("a use of an unresolvable registry dependency must be an error"),
            Err(e) => e,
        };
        let err = diags
            .iter()
            .find(|d| d.severity == crate::diag::Severity::Error)
            .unwrap_or_else(|| panic!("expected an error, got {diags:?}"));
        assert_eq!(err.code, "K0401", "{diags:?}");
        assert!(err.message.contains("json2"), "should name the dependency: {}", err.message);
        assert!(
            err.message.contains("registry") && err.message.contains("not supported"),
            "should explain the ACTUAL cause (no registry support), not a generic file-not-found: {}",
            err.message
        );
        assert!(
            !err.message.contains("No such file or directory"),
            "must not fall through to the misleading local-file error: {}",
            err.message
        );

        // a project that ALSO has a real path dependency alongside the
        // registry-only one still resolves the path one correctly -- the fix
        // doesn't regress the mixed case.
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\njson2 = { version = \"1.2.0\" }\nmath = { path = \"./mathlib\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("mathlib")).unwrap();
        std::fs::write(
            dir.join("mathlib/kupl.toml"),
            "[project]\nname = \"math\"\nentry = \"main.kupl\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("mathlib/main.kupl"),
            "pub fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "use math\nfun main() {\n    let _ = math.add(1, 2)\n}\n").unwrap();
        assert!(
            super::load(dir.join("main.kupl").to_str().unwrap()).is_ok(),
            "a real path dependency still resolves when a registry-only one is ALSO declared"
        );

        // `resolve_deps` (kupl pkg tree/lock) omits the registry-only dep
        // from its resolved list (nothing to resolve), but `registry_only_deps`
        // surfaces it explicitly rather than the project looking dep-free.
        let deps = super::resolve_deps(dir.join("main.kupl").to_str().unwrap()).expect("resolves the path dep");
        assert_eq!(deps.len(), 1, "{:?}", deps.iter().map(|d| &d.name).collect::<Vec<_>>());
        assert_eq!(deps[0].name, "math");
        let registry_only =
            super::registry_only_deps(dir.join("main.kupl").to_str().unwrap()).expect("registry_only_deps works");
        assert_eq!(registry_only, vec![("json2".to_string(), "1.2.0".to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL usability gap found+fixed (production-hardening PR-it641): once
    /// `kupl pkg fetch` has actually populated a registry dependency's cache
    /// directory (`registry::cache_dir()/name/version`), a `use` of that
    /// name still hit the SAME "registry dependencies aren't supported yet"
    /// K0401 error `version_only_dependency_reports_a_clear_registry_error...`
    /// above proves for the unfetched case -- `resolve_deps`/`registry_only`
    /// checked only whether the manifest declared a `path`, never whether the
    /// dependency had ALREADY been resolved into the cache, even though
    /// `registry.rs`'s own design (proven by
    /// `a_materialized_package_loads_and_runs_exactly_like_a_local_dependency`)
    /// is that a materialized package is an ordinary local directory. Fixed
    /// by having `pkg_ctx` check for the cache directory and, if present,
    /// resolve the dependency exactly like a `{ path = ".." }` one from then
    /// on -- `use`, `resolve_deps` (`kupl pkg tree`/`kupl pkg lock`), and
    /// ordinary program loading all pick it up transparently now, with no
    /// separate "already fetched, re-run to pick it up" step.
    #[test]
    fn a_registry_dependency_already_fetched_into_the_cache_resolves_like_a_local_one() {
        let name = "kuplforgeit641testdep";
        let version = "9.9.9";
        let cache_pkg = crate::registry::cache_dir().join(name).join(version);
        std::fs::create_dir_all(&cache_pkg).unwrap();
        std::fs::write(
            cache_pkg.join("kupl.toml"),
            format!("[project]\nname = \"{name}\"\nentry = \"main.kupl\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        std::fs::write(
            cache_pkg.join("main.kupl"),
            "pub fun double(x: Int) -> Int {\n    x * 2\n}\n",
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!("kupl-registry-dep-fetched-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            format!(
                "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\n{name} = {{ version = \"{version}\" }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("main.kupl"),
            format!("use {name}\nfun main() {{\n    let _ = {name}.double(3)\n}}\n"),
        )
        .unwrap();

        assert!(
            super::load(dir.join("main.kupl").to_str().unwrap()).is_ok(),
            "an already-fetched registry dependency must load like a local one"
        );

        let deps = super::resolve_deps(dir.join("main.kupl").to_str().unwrap()).expect("resolves the fetched dep");
        assert_eq!(deps.len(), 1, "{:?}", deps.iter().map(|d| &d.name).collect::<Vec<_>>());
        assert_eq!(deps[0].name, name);

        // no longer reported as unresolved...
        let registry_only =
            super::registry_only_deps(dir.join("main.kupl").to_str().unwrap()).expect("registry_only_deps works");
        assert!(registry_only.is_empty(), "{registry_only:?}");
        // ...but `kupl pkg fetch` must still see it, so re-running it
        // re-fetches/re-verifies rather than silently skipping.
        let all = super::all_registry_deps(dir.join("main.kupl").to_str().unwrap()).expect("all_registry_deps works");
        assert_eq!(all, vec![(name.to_string(), version.to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(crate::registry::cache_dir().join(name));
    }

    /// A REAL bug found+fixed (production-hardening PR-it761, from a fresh
    /// Explore survey of resolve.rs/loader.rs's module-resolution edge
    /// cases): a genuine DIAMOND dependency -- two sibling packages `b` and
    /// `c` that both depend on the SAME physical directory `d`, reached
    /// through two lexically-DIFFERENT paths (here: directly, and through a
    /// symlinked alias `d2 -> d`) -- got assigned TWO different mangling
    /// prefixes, even though the file itself was only ever parsed and
    /// tagged ONCE (the file-content dedup loop already used TRUE
    /// canonical/symlink-resolved identity; `ctx_cache`'s prefix-assignment
    /// dedup only used lexical `normalize()`, which can't see through a
    /// symlink). Whichever alias's queue entry was popped SECOND ended up
    /// with a mangling prefix that had zero items registered under it, so
    /// its own reference to the shared dependency failed with a spurious
    /// `K0240: unknown name`, for a perfectly valid, unambiguous dependency
    /// graph -- directly contradicting this exact code's own doc comment,
    /// which claimed `ctx_cache` "still dedupes by PHYSICAL directory."
    /// Fixed by keying `ctx_cache` with `dep_identity()` (canonicalize,
    /// falling back to `normalize()` for a not-yet-existing path), matching
    /// the identity notion the file-content `seen` dedup already used.
    #[test]
    fn a_diamond_dependency_reached_through_a_symlinked_alias_resolves_to_one_shared_package() {
        let base = std::env::temp_dir().join(format!("kupl-pkg-diamond-symlink-test-{}", std::process::id()));
        let root = base.join("root");
        let b = root.join("b");
        let c = root.join("c");
        let d = root.join("d");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            root.join("kupl.toml"),
            "[project]\nname = \"root\"\nentry = \"main.kupl\"\n\n[dependencies]\nb = { path = \"b\" }\nc = { path = \"c\" }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.kupl"),
            "use b\nuse c\n\nfun main() uses io {\n    print(b.from_b(1))\n    print(c.from_c(1))\n}\n",
        )
        .unwrap();
        std::fs::write(d.join("kupl.toml"), "[project]\nname = \"d\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(d.join("main.kupl"), "pub fun greet(n: Int) -> Int {\n    n + 1000\n}\n").unwrap();
        std::fs::write(
            b.join("kupl.toml"),
            "[project]\nname = \"b\"\nentry = \"main.kupl\"\n\n[dependencies]\nd = { path = \"../d\" }\n",
        )
        .unwrap();
        std::fs::write(b.join("main.kupl"), "use d\n\npub fun from_b(n: Int) -> Int {\n    d.greet(n)\n}\n").unwrap();
        // c reaches the SAME physical directory `d` through a symlinked alias
        // `d2`, a lexically different path that `normalize()` alone cannot
        // see is the identical real directory `canonicalize()` resolves it
        // (and `seen`'s file-content dedup) to.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&d, root.join("d2")).unwrap();
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_dir(&d, root.join("d2"));
        std::fs::write(
            c.join("kupl.toml"),
            "[project]\nname = \"c\"\nentry = \"main.kupl\"\n\n[dependencies]\nd = { path = \"../d2\" }\n",
        )
        .unwrap();
        std::fs::write(c.join("main.kupl"), "use d\n\npub fun from_c(n: Int) -> Int {\n    d.greet(n)\n}\n").unwrap();

        let (program, _map) = super::load(root.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("a diamond dependency reached via a symlinked alias must resolve, not error");
        let (checked, diags) = crate::check::check(&program);
        assert!(
            diags.iter().all(|d| d.severity != crate::diag::Severity::Error),
            "both b.from_b and c.from_c must resolve cleanly to the ONE shared `d` package: {diags:?}"
        );
        let db = crate::interp::ProgramDb::build(&program, &checked);
        let mut interp = crate::interp::Interp::new(db);
        let f = crate::value::Value::Fun(std::rc::Rc::new("main".to_string()));
        assert!(interp.call_value(f, vec![], crate::diag::Span::default()).is_ok(), "main() must run to completion (1001 printed twice)");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL bug found+fixed (production-hardening PR-it762, from a fresh
    /// Explore survey of loader.rs's `lock_hashes`/lockfile-drift-detection
    /// mechanism): a dependency NAME (or `path`/`version`) containing a
    /// literal tab byte -- `manifest.rs` places NO identifier-syntax
    /// restriction on a `[dependencies]` key, it's just `key.trim()` -- got
    /// serialized into a `kupl.lock` line with 5 tab-separated columns
    /// instead of the expected 4, silently DROPPED by `lock_hashes`'s exact
    /// `cols.len() == 4` check, with no error and no indication anything
    /// was wrong. Live-confirmed before this fix: `kupl pkg tree` on a
    /// project with a tab-containing dependency name showed NO `[drift]`
    /// marker even after the dependency's real source content changed,
    /// while a sibling dependency with a normal name in the SAME lockfile
    /// correctly showed `[drift]` -- drift detection silently went dark for
    /// exactly one dependency with zero warning. This test exercises the
    /// pure `lock_text`/`lock_hashes` round-trip directly (deterministic,
    /// no filesystem/process needed) rather than the full `kupl pkg
    /// tree`/`pkg lock` CLI path, since the bug is entirely in this
    /// serialization pair.
    #[test]
    fn a_lockfile_field_containing_a_tab_or_newline_round_trips_instead_of_corrupting_the_column_count() {
        let deps = vec![
            super::ResolvedDep {
                name: "foo\tbar".to_string(),
                path: "/some/path".to_string(),
                version: "1.0.0".to_string(),
                hash: "deadbeef".to_string(),
            },
            super::ResolvedDep {
                name: "normal".to_string(),
                path: "/other/path".to_string(),
                version: "2.0.0\nwith-newline".to_string(),
                hash: "cafef00d".to_string(),
            },
        ];
        let text = super::lock_text(&deps);
        // exactly 2 real dependency lines (plus the leading comment) -- an
        // embedded `\n` in the version field must NOT be mistaken for a
        // line break, and an embedded `\t` in the name field must NOT be
        // mistaken for an extra column.
        assert_eq!(
            text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count(),
            2,
            "an embedded tab/newline must not fragment one dependency into multiple lock lines: {text:?}"
        );
        let hashes = super::lock_hashes(&text);
        assert_eq!(
            hashes.get("foo\tbar"),
            Some(&"deadbeef".to_string()),
            "a tab-containing dependency name must still round-trip to its own hash, not be silently dropped: {hashes:?}"
        );
        assert_eq!(hashes.get("normal"), Some(&"cafef00d".to_string()), "{hashes:?}");
        assert_eq!(hashes.len(), 2, "no dependency should be silently dropped from drift tracking: {hashes:?}");
    }
}
