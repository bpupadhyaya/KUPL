//! Drivers: compile, run apps, run example tests.

use crate::ast::{Item, Program};
use crate::check::{self, Checked};
use crate::diag::{self, Diag, Severity, Span};
use crate::interp::{Flow, Interp, ProgramDb};
use crate::parser;
use crate::value::Value;

pub struct Compiled {
    pub program: Program,
    pub checked: Checked,
    pub warnings: Vec<Diag>,
}

/// The standard prelude: types/constructors available to every program without
/// an import. Currently the built-in `Json` ADT that `json_parse`/
/// `json_stringify` produce and consume.
pub const PRELUDE: &str =
    "type Json = JNull | JBool(b: Bool) | JNum(n: Float) | JStr(s: Str) \
     | JArr(items: List[Json]) | JObj(fields: Map[Str, Json])\n";

/// Prepend the prelude's items to a parsed program so the checker, compiler,
/// and every engine treat prelude types exactly like user declarations.
pub fn inject_prelude(program: &mut Program) {
    let (prelude, diags) = parser::parse(PRELUDE);
    debug_assert!(diags.is_empty(), "prelude must parse cleanly: {diags:?}");
    let mut items = prelude.items;
    items.append(&mut program.items);
    program.items = items;
}

/// Parse + check (types, then effects). Errors are returned; warnings ride
/// along on success.
pub fn compile(src: &str) -> Result<Compiled, Vec<Diag>> {
    let (mut program, mut diags) = parser::parse(src);
    inject_prelude(&mut program);
    // resolve named args + fill default parameters into positional form, so the
    // checker and every engine see plain positional calls
    diags.extend(crate::callargs::resolve_call_args(&mut program));
    let (checked, check_diags) = check::check(&program);
    diags.extend(check_diags);
    // Effects only make sense on a program that parsed; skip if already broken.
    if !diags.iter().any(|d| d.severity == Severity::Error) {
        diags.extend(crate::effects::check_effects(&program));
    }
    let (mut errors, mut warnings): (Vec<_>, Vec<_>) =
        diags.into_iter().partition(|d| d.severity == Severity::Error);
    sort_diags(&mut errors);
    sort_diags(&mut warnings);
    if errors.is_empty() {
        Ok(Compiled { program, checked, warnings })
    } else {
        Err(errors)
    }
}

/// Put diagnostics in a deterministic, top-to-bottom order. Some passes (e.g.
/// effects, which walks a `HashMap` of functions) produce them in an arbitrary
/// order — without this, `kupl run` printed warnings in a different order
/// run-to-run and engine-to-engine. Sort by source position, then code/message
/// to fully pin ties.
pub(crate) fn sort_diags(diags: &mut [Diag]) {
    diags.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.message.cmp(&b.message))
    });
}

/// `kupl context <name>`: emit the item plus the source of everything it
/// directly references — the minimal dependency-closed context for an LLM.
pub fn emit_context(path: &str, name: &str) -> i32 {
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    let file = path;
    let src: &str = &map.concat;
    let items: Vec<(&str, Span)> = compiled
        .program
        .items
        .iter()
        .map(|item| match item {
            Item::Fun(f) => (f.name.as_str(), f.span),
            Item::Type(t) => (t.name.as_str(), t.span),
            Item::Component(c) => (c.name.as_str(), c.span),
            Item::Contract(ct) => (ct.name.as_str(), ct.span),
            Item::Law(l) => (l.name.as_str(), l.span),
        })
        .collect();
    let Some(target) = compiled.program.items.iter().find(|item| match item {
        Item::Fun(f) => f.name == name,
        Item::Type(t) => t.name == name,
        Item::Component(c) => c.name == name,
        Item::Contract(ct) => ct.name == name,
        Item::Law(l) => l.name == name,
    }) else {
        eprintln!("error: no item named `{name}` in {file}");
        return 1;
    };

    // Names the target references, resolved to item names.
    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut note = |n: &str| {
        // constructor names resolve to their owning type
        let owner = compiled
            .checked
            .ctors
            .get(n)
            .map(|(ty, _)| ty.as_str())
            .unwrap_or(n);
        if owner != name && items.iter().any(|(i, _)| *i == owner) {
            referenced.insert(owner.to_string());
        }
    };
    match target {
        Item::Fun(f) => {
            for p in &f.params {
                collect_ty_names(&p.ty, &mut note);
            }
            if let Some(r) = &f.ret {
                collect_ty_names(r, &mut note);
            }
            crate::effects::walk_block(&f.body, &mut |e| collect_expr_names(e, &mut note));
        }
        Item::Type(t) => {
            for v in &t.variants {
                for fld in &v.fields {
                    collect_ty_names(&fld.ty, &mut note);
                }
            }
        }
        Item::Contract(ct) => {
            for s in &ct.sigs {
                for p in &s.params {
                    collect_ty_names(&p.ty, &mut note);
                }
                if let Some(r) = &s.ret {
                    collect_ty_names(r, &mut note);
                }
            }
            for law in &ct.laws {
                crate::effects::walk_block(&law.body, &mut |e| collect_expr_names(e, &mut note));
            }
        }
        Item::Component(c) => {
            for contract in &c.fulfills {
                note(contract);
            }
            for p in &c.ports {
                collect_ty_names(&p.ty, &mut note);
            }
            for p in &c.props {
                collect_ty_names(&p.ty, &mut note);
            }
            for child in &c.children {
                note(&child.component);
            }
            for s in &c.state {
                crate::effects::walk_block(
                    &crate::ast::Block { stmts: vec![crate::ast::Stmt::Expr(s.init.clone())], span: s.span },
                    &mut |e| collect_expr_names(e, &mut note),
                );
            }
            for h in &c.handlers {
                crate::effects::walk_block(&h.body, &mut |e| collect_expr_names(e, &mut note));
            }
            for f in c.exposes.iter().chain(c.funs.iter()) {
                for p in &f.params {
                    collect_ty_names(&p.ty, &mut note);
                }
                if let Some(r) = &f.ret {
                    collect_ty_names(r, &mut note);
                }
                crate::effects::walk_block(&f.body, &mut |e| collect_expr_names(e, &mut note));
            }
        }
        Item::Law(l) => {
            crate::effects::walk_block(&l.body, &mut |e| collect_expr_names(e, &mut note));
        }
    }

    let slice = |span: Span| {
        let s = (span.start as usize).min(src.len());
        let e = (span.end as usize).min(src.len());
        src[s..e].trim_end().to_string()
    };
    let target_span = match target {
        Item::Fun(f) => f.span,
        Item::Type(t) => t.span,
        Item::Component(c) => c.span,
        Item::Contract(ct) => ct.span,
        Item::Law(l) => l.span,
    };
    println!("// kupl context: {name} ({file})");
    println!("{}", slice(target_span));
    if !referenced.is_empty() {
        println!("\n// --- direct dependencies ---");
        for dep in &referenced {
            if let Some((_, span)) = items.iter().find(|(i, _)| i == dep) {
                println!("\n{}", slice(*span));
            }
        }
    }
    0
}

fn collect_ty_names(t: &crate::ast::TyExpr, f: &mut impl FnMut(&str)) {
    use crate::ast::TyExprKind;
    match &t.kind {
        TyExprKind::Name(n) => f(n),
        TyExprKind::Generic(n, args) => {
            f(n);
            for a in args {
                collect_ty_names(a, f);
            }
        }
        TyExprKind::Fun(params, ret) => {
            for p in params {
                collect_ty_names(p, f);
            }
            collect_ty_names(ret, f);
        }
    }
}

fn collect_expr_names(e: &crate::ast::Expr, f: &mut impl FnMut(&str)) {
    use crate::ast::ExprKind;
    match &e.kind {
        ExprKind::Ident(n) => f(n),
        ExprKind::Call { callee, .. } => {
            if let ExprKind::Ident(n) = &callee.kind {
                f(n);
            }
        }
        _ => {}
    }
}

pub fn print_diags(diags: &[Diag], src: &str, file: &str) {
    for d in diags {
        eprint!("{}", diag::render(d, src, file));
    }
}

pub fn print_diags_map(diags: &[Diag], map: &crate::loader::SourceMap) {
    for d in diags {
        eprint!("{}", map.render(d));
    }
}

fn report_panic_map(msg: &str, span: Span, map: &crate::loader::SourceMap) {
    let d = Diag::error("K0900", format!("panic: {msg}"), span);
    eprint!("{}", map.render(&d));
}

/// Load (multi-file), type-check, effect-check. Prints errors itself.
pub fn load_compile(path: &str) -> Result<(Compiled, crate::loader::SourceMap), i32> {
    let (mut program, map) = match crate::loader::load(path) {
        Ok(ok) => ok,
        Err((diags, map)) => {
            print_diags_map(&diags, &map);
            return Err(1);
        }
    };
    inject_prelude(&mut program);
    let mut diags = crate::callargs::resolve_call_args(&mut program);
    let (checked, check_diags) = check::check(&program);
    diags.extend(check_diags);
    if !diags.iter().any(|d| d.severity == Severity::Error) {
        diags.extend(crate::effects::check_effects(&program));
    }
    let (mut errors, mut warnings): (Vec<_>, Vec<_>) =
        diags.into_iter().partition(|d| d.severity == Severity::Error);
    sort_diags(&mut errors);
    sort_diags(&mut warnings);
    if !errors.is_empty() {
        print_diags_map(&errors, &map);
        return Err(1);
    }
    print_diags_map(&warnings, &map);
    Ok((Compiled { program, checked, warnings: Vec::new() }, map))
}

/// `kupl run`: execute the first `app` (or a `fun main()` if there is no app).
pub fn run_program(path: &str) -> i32 {
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    let file = path;

    let app = compiled.program.items.iter().find_map(|item| match item {
        Item::Component(c) if c.is_app => Some(c.name.clone()),
        _ => None,
    });
    let db = ProgramDb::build(&compiled.program, &compiled.checked);
    if app.is_none() && !db.funs.contains_key("main") {
        eprintln!("error: no `app` or `fun main()` found in {file}");
        return 2;
    }
    let mut interp = Interp::new(db);
    interp.print_unwired = true;

    let outcome: Result<(), Flow> = (|| {
        match app {
            Some(name) => {
                let required: Vec<String> = interp
                    .db
                    .components
                    .get(&name)
                    .map(|c| {
                        c.props
                            .iter()
                            .filter(|p| p.default.is_none())
                            .map(|p| p.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if !required.is_empty() {
                    eprintln!(
                        "error: app `{name}` requires props ({}) — v0.1 apps must be self-contained",
                        required.join(", ")
                    );
                    return Ok(());
                }
                interp.instantiate(&name, &[], Span::default())?;
                interp.start_all()?;
                // fire timers deterministically (bounded, so recurring timers
                // yield finite output under `kupl run`)
                interp.run_timers(100)?;
                Ok(())
            }
            None => {
                if interp.db.funs.contains_key("main") {
                    let f = Value::Fun(std::rc::Rc::new("main".to_string()));
                    interp.call_value(f, vec![], Span::default())?;
                    Ok(())
                } else {
                    eprintln!("error: no `app` or `fun main()` found in {file}");
                    Ok(())
                }
            }
        }
    })();

    match outcome {
        Ok(()) => 0,
        Err(Flow::Panic { msg, span }) => {
            report_panic_map(&msg, span, &map);
            101
        }
        Err(_) => 0,
    }
}

/// `kupl run --vm`: compile to KVM bytecode and execute on the register VM.
pub fn run_program_vm(path: &str) -> i32 {
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    let file = path;
    let module = match crate::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => m,
        Err(diags) => {
            print_diags_map(&diags, &map);
            return 1;
        }
    };
    let app = compiled.program.items.iter().find_map(|item| match item {
        Item::Component(c) if c.is_app => Some(c.name.clone()),
        _ => None,
    });
    let mut vm = crate::vm::Vm::new(&module);
    vm.print_unwired = true;
    // enable the real-thread par_map/par_filter fast path (source run has the AST)
    let db = ProgramDb::build(&compiled.program, &compiled.checked);
    vm.set_image(crate::parallel::ProgramImage::from_db(&db));
    let outcome = match app {
        Some(name) => vm.run_app(&name).map(|_| Value::Unit),
        None if module.funs.contains_key("main") => vm.call_named("main", vec![]),
        None => {
            eprintln!("error: no `app` or `fun main()` found in {file}");
            return 2;
        }
    };
    match outcome {
        Ok(_) => 0,
        Err(e) => {
            report_panic_map(&e.msg, e.span, &map);
            101
        }
    }
}

/// `kupl check [--json]`: load (multi-file), type-check, effect-check.
pub fn check_cmd(path: &str, json: bool) -> i32 {
    let (mut program, map) = match crate::loader::load(path) {
        Ok(ok) => ok,
        Err((mut diags, map)) => {
            sort_diags(&mut diags);
            if json {
                println!("{}", map.to_json(&diags));
            } else {
                print_diags_map(&diags, &map);
            }
            return 1;
        }
    };
    // Inject the prelude (the built-in `Json` ADT etc.) exactly like `compile()` /
    // `load_compile()` — without this, `kupl check` reported false "unknown type
    // `Json`" errors on valid programs that `kupl run`/`build` accept.
    inject_prelude(&mut program);
    let (_, mut diags) = check::check(&program);
    if !diags.iter().any(|d| d.severity == Severity::Error) {
        diags.extend(crate::effects::check_effects(&program));
    }
    // Deterministic, position-ordered output — the effects pass emits in HashMap
    // order (see sort_diags / PR-it78), which made `kupl check` (and `--json`)
    // print warnings in a different order run-to-run.
    sort_diags(&mut diags);
    let has_errors = diags.iter().any(|d| d.severity == Severity::Error);
    if json {
        println!("{}", map.to_json(&diags));
    } else {
        print_diags_map(&diags, &map);
        if !has_errors {
            println!("ok: {path}");
        }
    }
    if has_errors {
        1
    } else {
        0
    }
}

/// `kupl manifest`: emit the component manifests (the visual-tool palette API)
/// as JSON: intent, ports, props, state, exposes, fulfills, wiring.
/// `kupl pkg tree <entry>` — print the resolved dependency graph, flagging
/// drift against an existing kupl.lock.
pub fn pkg_tree(path: &str) -> i32 {
    let deps = match crate::loader::resolve_deps(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    // Registry-only dependencies (`{ version = ".." }`, no `path`) can never
    // resolve without a registry — reported explicitly rather than making
    // the project look like it simply has fewer dependencies than its
    // manifest declares (production-hardening PR-it625).
    let registry_only = crate::loader::registry_only_deps(path).unwrap_or_default();
    if deps.is_empty() && registry_only.is_empty() {
        println!("no dependencies");
        return 0;
    }
    // compare against a lockfile in the project dir, if present
    let lock_path = std::path::Path::new(path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("kupl.lock");
    let locked = std::fs::read_to_string(&lock_path)
        .ok()
        .map(|t| crate::loader::lock_hashes(&t));
    for d in &deps {
        let ver = if d.version.is_empty() { "?".to_string() } else { d.version.clone() };
        let drift = match &locked {
            Some(h) => match h.get(&d.name) {
                Some(old) if old != &d.hash => "  [drift]",
                _ => "",
            },
            None => "",
        };
        println!("{} @ {}  ({}){}", d.name, ver, d.path, drift);
    }
    for (name, version) in &registry_only {
        println!("{name} @ {version}  (registry — not yet supported, unresolved)");
    }
    0
}

/// `kupl pkg lock <entry>` — write/update kupl.lock next to the project.
pub fn pkg_lock(path: &str) -> i32 {
    let deps = match crate::loader::resolve_deps(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    // See pkg_tree above: reported, not silently dropped from the count.
    let registry_only = crate::loader::registry_only_deps(path).unwrap_or_default();
    let lock_path = std::path::Path::new(path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("kupl.lock");
    match std::fs::write(&lock_path, crate::loader::lock_text(&deps)) {
        Ok(()) => {
            println!("wrote {} ({} dependencies)", lock_path.display(), deps.len());
            if !registry_only.is_empty() {
                let names: Vec<&str> = registry_only.iter().map(|(n, _)| n.as_str()).collect();
                println!(
                    "note: {} registry dependenc{} not written to the lockfile (not yet supported): {}",
                    registry_only.len(),
                    if registry_only.len() == 1 { "y" } else { "ies" },
                    names.join(", ")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", lock_path.display());
            1
        }
    }
}

/// `kupl pkg fetch <entry>` — resolve and download every registry-only
/// dependency `<entry>`'s project declares (`{ version = ".." }`, no
/// `path`), populating the local registry cache (`registry_cache_dir`)
/// via `registry::fetch_package`. Uses the SAME `registry_only_deps`
/// (`loader.rs`) that `pkg_tree`/`pkg_lock` already use to REPORT these
/// dependencies as unresolved (production-hardening PR-it625) — this is
/// the first subcommand that actually RESOLVES them.
pub fn pkg_fetch(path: &str) -> i32 {
    pkg_fetch_with(path, crate::registry::DEFAULT_REGISTRY_URL, &crate::registry::cache_dir(), crate::registry::fetch_package)
}

/// `pkg_fetch`, but the registry URL, cache directory, and fetch
/// transport are all injectable — lets a test exercise the real
/// per-dependency iteration/reporting/exit-code logic against a canned
/// fetcher, with no live network access, mirroring `registry.rs`'s own
/// `fetch_package`/`fetch_package_with` split (production-hardening
/// PR-it632). A single dependency's fetch failure is reported and the
/// loop CONTINUES to the rest (not aborted early) — every dependency
/// gets a definitive fetched-or-failed report in one run, matching how a
/// build tool's dependency-install step should behave; the function's
/// own exit code still reflects whether ANY dependency failed.
fn pkg_fetch_with(
    path: &str,
    registry_url: &str,
    cache_dir: &std::path::Path,
    fetch: impl Fn(&str, &str, &str, &std::path::Path) -> Result<std::path::PathBuf, String>,
) -> i32 {
    // Uses `all_registry_deps`, NOT `registry_only_deps` -- the latter drops
    // a dependency once it's already been fetched (so `use`/`pkg
    // tree`/`pkg lock` can treat it as resolved), but `kupl pkg fetch`
    // itself must keep re-fetching and re-verifying every registry
    // dependency on every run, matching `fetch_package`'s own documented
    // no-cache-skip design (production-hardening PR-it641).
    let registry_only = match crate::loader::all_registry_deps(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if registry_only.is_empty() {
        println!("no registry dependencies to fetch");
        return 0;
    }
    let mut ok = true;
    for (name, version) in &registry_only {
        match fetch(registry_url, name, version, cache_dir) {
            Ok(dest) => println!("fetched {name} @ {version} -> {}", dest.display()),
            Err(e) => {
                eprintln!("error: {name} @ {version}: {e}");
                ok = false;
            }
        }
    }
    if ok {
        0
    } else {
        1
    }
}

pub fn emit_manifest(path: &str) -> i32 {
    let Ok((compiled, _map)) = load_compile(path) else { return 1 };
    println!("{}", manifest_json(&compiled.program));
    0
}

/// Serialize a program's components to the visual-tools manifest JSON (intent,
/// ports, props, state, exposes, fulfills, children, wires, supervises,
/// handlers). Every string field goes through `json_escape`, so the result is
/// always valid, parseable JSON.
pub(crate) fn manifest_json(program: &crate::ast::Program) -> String {
    use crate::diag::json_escape as esc;
    let mut out = String::from("{\"components\":[");
    let mut first = true;
    for item in &program.items {
        let Item::Component(c) = item else { continue };
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"kind\":\"{}\",\"intent\":\"{}\"",
            esc(&c.name),
            if c.is_app { "app" } else { "component" },
            esc(c.intent.as_deref().unwrap_or("")),
        ));
        let ports: Vec<String> = c
            .ports
            .iter()
            .map(|p| {
                format!(
                    "{{\"name\":\"{}\",\"dir\":\"{}\",\"type\":\"{}\"}}",
                    esc(&p.name),
                    if p.dir == crate::ast::PortDir::In { "in" } else { "out" },
                    esc(&crate::fmt::ty_str(&p.ty))
                )
            })
            .collect();
        out.push_str(&format!(",\"ports\":[{}]", ports.join(",")));
        let props: Vec<String> = c
            .props
            .iter()
            .map(|p| {
                format!(
                    "{{\"name\":\"{}\",\"type\":\"{}\",\"required\":{}}}",
                    esc(&p.name),
                    esc(&crate::fmt::ty_str(&p.ty)),
                    p.default.is_none()
                )
            })
            .collect();
        out.push_str(&format!(",\"props\":[{}]", props.join(",")));
        let state: Vec<String> = c.state.iter().map(|s| format!("\"{}\"", esc(&s.name))).collect();
        out.push_str(&format!(",\"state\":[{}]", state.join(",")));
        let exposes: Vec<String> = c
            .exposes
            .iter()
            .map(|f| {
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| format!("\"{}: {}\"", esc(&p.name), esc(&crate::fmt::ty_str(&p.ty))))
                    .collect();
                format!(
                    "{{\"name\":\"{}\",\"params\":[{}],\"returns\":\"{}\",\"uses\":[{}]}}",
                    esc(&f.name),
                    params.join(","),
                    esc(&f.ret.as_ref().map(crate::fmt::ty_str).unwrap_or_else(|| "Unit".into())),
                    f.effects.iter().map(|e| format!("\"{}\"", esc(e))).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        out.push_str(&format!(",\"exposes\":[{}]", exposes.join(",")));
        let fulfills: Vec<String> = c.fulfills.iter().map(|f| format!("\"{}\"", esc(f))).collect();
        out.push_str(&format!(",\"fulfills\":[{}]", fulfills.join(",")));
        let children: Vec<String> = c
            .children
            .iter()
            .map(|ch| format!("{{\"name\":\"{}\",\"component\":\"{}\"}}", esc(&ch.name), esc(&ch.component)))
            .collect();
        out.push_str(&format!(",\"children\":[{}]", children.join(",")));
        let wires: Vec<String> = c
            .wires
            .iter()
            .map(|w| {
                format!(
                    "{{\"from\":\"{}.{}\",\"to\":\"{}.{}\"}}",
                    esc(&w.from.0),
                    esc(&w.from.1),
                    esc(&w.to.0),
                    esc(&w.to.1)
                )
            })
            .collect();
        out.push_str(&format!(",\"wires\":[{}]", wires.join(",")));
        // `supervises`/`handlers` were missing entirely -- a genuine completeness
        // gap against this function's own doc comment/design contract ("the
        // component's members must all be present", per this module's own test
        // comment): `children`/`wires` (also purely structural connectivity data)
        // were already included, but a visual tool had NO way to render a
        // supervision tree or see which triggers a component reacts to, since
        // both were silently dropped (production-hardening PR-it647).
        let supervises: Vec<String> = c
            .supervises
            .iter()
            .map(|sv| {
                format!(
                    "{{\"child\":\"{}\",\"policy\":\"{}\"}}",
                    esc(&sv.child),
                    match sv.policy {
                        crate::ast::SupervisePolicy::RestartOnFailure => "restart_on_failure",
                        crate::ast::SupervisePolicy::Never => "never",
                    }
                )
            })
            .collect();
        out.push_str(&format!(",\"supervises\":[{}]", supervises.join(",")));
        let handlers: Vec<String> = c
            .handlers
            .iter()
            .map(|h| {
                let trigger = match &h.trigger {
                    crate::ast::Trigger::Port(p) => format!("port:{}", esc(p)),
                    crate::ast::Trigger::Start => "start".to_string(),
                    crate::ast::Trigger::Stop => "stop".to_string(),
                    crate::ast::Trigger::Every(ms) => format!("every:{ms}"),
                    crate::ast::Trigger::After(ms) => format!("after:{ms}"),
                };
                format!("{{\"trigger\":\"{trigger}\"}}")
            })
            .collect();
        out.push_str(&format!(",\"handlers\":[{}]", handlers.join(",")));
        out.push_str(&format!(",\"examples\":{}}}", c.examples.len()));
    }
    out.push_str("]}");
    out
}

/// `kupl native`: emit C from the bytecode and compile with the system cc.
pub fn native(path: &str, args: &[String]) -> i32 {
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    let module = match crate::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => m,
        Err(diags) => {
            print_diags_map(&diags, &map);
            return 1;
        }
    };
    let c_src = match crate::cgen::emit_c(&module) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let out = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| path.trim_end_matches(".kupl").to_string());
    let c_path = format!("{out}.c");
    if let Err(e) = std::fs::write(&c_path, &c_src) {
        eprintln!("error: cannot write {c_path}: {e}");
        return 1;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(&cc)
        .args(["-O2", "-o", &out, &c_path])
        .status();
    let keep_c = args.iter().any(|a| a == "--keep-c");
    match status {
        Ok(s) if s.success() => {
            if !keep_c {
                let _ = std::fs::remove_file(&c_path);
            }
            println!(
                "native executable: {out}{}",
                if keep_c { format!(" (C source: {c_path})") } else { String::new() }
            );
            0
        }
        Ok(s) => {
            eprintln!("error: {cc} failed with {s} (C source kept at {c_path})");
            1
        }
        Err(e) => {
            eprintln!("error: cannot run {cc}: {e} (C source kept at {c_path})");
            1
        }
    }
}

/// Execute an already-compiled module: the first `app`, else `fun main`.
pub fn run_module(module: &crate::bytecode::Module, origin: &str) -> i32 {
    let mut vm = crate::vm::Vm::new(module);
    vm.print_unwired = true;
    let app = module.components.iter().find(|c| c.is_app).map(|c| c.name.clone());
    let outcome = match app {
        Some(name) => vm.run_app(&name).map(|_| Value::Unit),
        None if module.funs.contains_key("main") => vm.call_named("main", vec![]),
        None => {
            eprintln!("error: no `app` or `fun main()` in {origin}");
            return 2;
        }
    };
    match outcome {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("panic: {} (in {origin})", e.msg);
            101
        }
    }
}

/// `kupl dis`: disassemble the compiled module.
pub fn disassemble(path: &str) -> i32 {
    // A compiled `.kx` module is already bytecode — decode and disassemble it directly
    // rather than trying to read it as UTF-8 source (which gave a confusing error).
    if path.ends_with(".kx") {
        return match std::fs::read(path) {
            Ok(bytes) => match crate::kx::decode(&bytes) {
                Ok(module) => {
                    print!("{}", module.disassemble());
                    0
                }
                Err(e) => {
                    eprintln!("error: cannot decode {path}: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                1
            }
        };
    }
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    match crate::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => {
            print!("{}", m.disassemble());
            0
        }
        Err(diags) => {
            print_diags_map(&diags, &map);
            1
        }
    }
}

/// `kupl test`: run every `example` block of every component.
pub fn run_tests(path: &str) -> i32 {
    let Ok((compiled, map)) = load_compile(path) else { return 1 };
    let src: &str = &map.concat;

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    // top-level laws (free-standing tests, incl. `forall` properties)
    for item in &compiled.program.items {
        let Item::Law(law) = item else { continue };
        let label = format!("law \"{}\"", law.name);
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        let env = interp.globals.child();
        match interp.exec_block(&law.body, &env) {
            Ok(_) | Err(Flow::Return(_)) => {
                println!("ok    {label}");
                passed += 1;
            }
            Err(Flow::Panic { msg, span }) => {
                let detail = if msg.starts_with("expectation failed") {
                    format!("`{}` was not satisfied", snippet(src, span))
                } else {
                    msg
                };
                println!("FAIL  {label}: {detail}");
                failed += 1;
            }
            Err(_) => {
                println!("FAIL  {label}: unexpected control flow");
                failed += 1;
            }
        }
    }

    for item in &compiled.program.items {
        let Item::Component(c) = item else { continue };
        if c.examples.is_empty() {
            continue;
        }
        // v0.1: examples run components with all props defaulted
        if c.props.iter().any(|p| p.default.is_none()) {
            println!("skip  {} (component requires props)", c.name);
            skipped += c.examples.len();
            continue;
        }
        for (i, example) in c.examples.iter().enumerate() {
            let label = if c.examples.len() > 1 {
                format!("{} example #{}", c.name, i + 1)
            } else {
                format!("{} example", c.name)
            };
            let db = ProgramDb::build(&compiled.program, &compiled.checked);
            let mut interp = Interp::new(db);
            let result = run_example(&mut interp, &c.name, example, src);
            match result {
                Ok(None) => {
                    println!("ok    {label}");
                    passed += 1;
                }
                Ok(Some(failure)) => {
                    println!("FAIL  {label}: {failure}");
                    failed += 1;
                }
                Err(Flow::Panic { msg, span }) => {
                    println!("FAIL  {label}: panic: {msg}");
                    report_panic_map(&msg, span, &map);
                    failed += 1;
                }
                Err(_) => {
                    println!("FAIL  {label}: unexpected control flow");
                    failed += 1;
                }
            }
        }
    }

    // contract laws: every law runs against every fulfilling component
    for item in &compiled.program.items {
        let Item::Component(c) = item else { continue };
        if c.fulfills.is_empty() {
            continue;
        }
        if c.props.iter().any(|p| p.default.is_none()) {
            println!("skip  {} laws (component requires props)", c.name);
            // Matches the sibling `example` skip branch above (`skipped +=
            // c.examples.len()`): tally how many laws this skip actually
            // covers -- every law of every contract this component fulfills
            // -- rather than leaving the final summary silently undercounting
            // (PR-it583, found via a multi-file/generics investigation that
            // otherwise confirmed cross-file law-running is fully correct).
            let n: usize = c
                .fulfills
                .iter()
                .filter_map(|contract_name| {
                    compiled.program.items.iter().find_map(|i| match i {
                        Item::Contract(ct) if &ct.name == contract_name => Some(ct.laws.len()),
                        _ => None,
                    })
                })
                .sum();
            skipped += n;
            continue;
        }
        for contract_name in &c.fulfills {
            let Some(contract) = compiled.program.items.iter().find_map(|i| match i {
                Item::Contract(ct) if &ct.name == contract_name => Some(ct),
                _ => None,
            }) else {
                continue;
            };
            for law in &contract.laws {
                let label = format!("{} law \"{}\"", c.name, law.name);
                let db = ProgramDb::build(&compiled.program, &compiled.checked);
                let mut interp = Interp::new(db);
                let outcome: Result<(), Flow> = (|| {
                    let v = interp.instantiate(&c.name, &[], Span::default())?;
                    let Value::Component(id) = v else {
                        return Err(Flow::Panic { msg: "instantiation failed".into(), span: law.span });
                    };
                    interp.start_all()?;
                    let env = interp.globals.child();
                    for sig in &contract.sigs {
                        env.define(&sig.name, Value::Bound(id, std::rc::Rc::new(sig.name.clone())));
                    }
                    match interp.exec_block(&law.body, &env) {
                        Ok(_) | Err(Flow::Return(_)) => Ok(()),
                        Err(other) => Err(other),
                    }
                })();
                match outcome {
                    Ok(()) => {
                        println!("ok    {label}");
                        passed += 1;
                    }
                    Err(Flow::Panic { msg, span }) => {
                        let detail = if msg.starts_with("expectation failed") {
                            format!("`{}` was not satisfied", snippet(src, span))
                        } else {
                            format!("panic: {msg}")
                        };
                        println!("FAIL  {label}: {detail}");
                        failed += 1;
                    }
                    Err(_) => {
                        println!("FAIL  {label}: unexpected control flow");
                        failed += 1;
                    }
                }
            }
        }
    }

    println!("\n{passed} passed, {failed} failed, {skipped} skipped");
    if failed > 0 {
        1
    } else {
        0
    }
}

fn run_example(
    interp: &mut Interp,
    comp_name: &str,
    example: &crate::ast::Example,
    src: &str,
) -> Result<Option<String>, Flow> {
    use crate::ast::ExampleStep;

    let v = interp.instantiate(comp_name, &[], Span::default())?;
    let Value::Component(id) = v else {
        return Ok(Some("could not instantiate component".into()));
    };
    interp.start_all()?;

    for step in &example.steps {
        match step {
            ExampleStep::Send { port, arg, .. } => {
                let payload = match arg {
                    Some(e) => {
                        let env = interp.globals.child();
                        interp.eval(e, &env)?
                    }
                    None => Value::Unit,
                };
                interp.send(id, port, payload)?;
            }
            ExampleStep::Expect { expr, span } => {
                // out ports are visible by name, bound to their last emitted value
                let env = interp.instances[id].env.child();
                let ports: Vec<String> = interp.instances[id]
                    .comp
                    .ports
                    .iter()
                    .filter(|p| p.dir == crate::ast::PortDir::Out)
                    .map(|p| p.name.clone())
                    .collect();
                for port in ports {
                    let v = interp.instances[id]
                        .last_emit
                        .get(&port)
                        .cloned()
                        .unwrap_or(Value::Unit);
                    env.define(&port, v);
                }
                let result = interp.eval(expr, &env)?;
                if result != Value::Bool(true) {
                    let text = snippet(src, *span);
                    return Ok(Some(format!("`{text}` was not satisfied")));
                }
            }
            ExampleStep::Advance { ms, .. } => {
                interp.advance(*ms)?;
            }
        }
    }
    Ok(None)
}

fn snippet(src: &str, span: Span) -> String {
    let start = (span.start as usize).min(src.len());
    let end = (span.end as usize).min(src.len());
    src[start..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{compile, sort_diags};
    use crate::diag::{Diag, Span};

    #[test]
    fn manifest_json_is_valid_and_escaped() {
        // `kupl manifest` feeds visual tools — its output must be parseable JSON with
        // every string field escaped, and the component's members must all be present.
        let src = "component Counter {\n    intent \"Counts \\\"clicks\\\"\\nand \\\\slashes\\\\\\ttabs — é\"\n    \
                   prop label: Str\n    in click: Int\n    out value: Int\n    state count: Int = 0\n    \
                   on click(n) { count = count + n\n        emit value(count) }\n    \
                   expose fun current() -> Int { count }\n}\n";
        let compiled = compile(src).expect("compiles");
        let json = super::manifest_json(&compiled.program);
        // parses as JSON (equivalent to a real visual-tool consumer)
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let arr_len = |j: Option<&crate::lsp::Json>| match j {
            Some(crate::lsp::Json::Arr(a)) => Some(a.len()),
            _ => None,
        };
        assert_eq!(arr_len(v.get("components")), Some(1));
        let c = v.get("components").and_then(|c| c.index(0)).expect("component 0");
        // the tricky intent round-trips with its quotes/newline/tab decoded
        let intent = c.get("intent").and_then(|i| i.str()).expect("intent");
        assert!(intent.contains("\"clicks\"") && intent.contains('\n') && intent.contains('\t'),
                "escaped chars must decode back: {intent:?}");
        // members present + counted
        assert_eq!(arr_len(c.get("ports")), Some(2));
        assert_eq!(arr_len(c.get("props")), Some(1));
        assert_eq!(arr_len(c.get("state")), Some(1));
        assert_eq!(arr_len(c.get("exposes")), Some(1));
        // `on click(n) { ... }` above is a `handlers` entry -- was silently
        // dropped entirely before PR-it647 (see `manifest_reports_supervises_
        // and_handlers` below for the full regression coverage).
        assert_eq!(arr_len(c.get("handlers")), Some(1));
        let trigger = c.get("handlers").and_then(|h| h.index(0)).and_then(|h| h.get("trigger")).and_then(|t| t.str());
        assert_eq!(trigger, Some("port:click"));
        // a program with no components is still valid JSON with an empty array
        let empty = super::manifest_json(&compile("fun main() {}\n").unwrap().program);
        let ev = crate::lsp::parse_json(&empty).expect("empty manifest is valid JSON");
        assert_eq!(arr_len(ev.get("components")), Some(0));
    }

    /// A REAL BUG found+fixed (production-hardening PR-it647): `manifest_json`'s
    /// own doc comment (and this module's OTHER manifest test's comment: "the
    /// component's members must all be present") claims completeness, but
    /// `supervises`/`handlers` were entirely absent from the emitted JSON --
    /// `children`/`wires` (also purely structural connectivity data a visual
    /// tool needs) were already included, making the omission of `supervises`
    /// (a supervision-tree edge) and `handlers` (which triggers a component
    /// reacts to) an inconsistent, silent gap rather than a deliberate design
    /// choice like `state`'s name-only serialization (state's `init` is an
    /// arbitrary expression, not simple manifest data, unlike `SuperviseDecl`/
    /// `Trigger` which are both small, fully-serializable structures).
    #[test]
    fn manifest_reports_supervises_and_handlers() {
        let src = "component Child {\n    intent \"c\"\n}\n\
                   component Parent {\n    intent \"p\"\n    \
                   let kid = Child()\n    supervise kid restart on_failure\n    \
                   on start { }\n    on stop { }\n    on every 5s { }\n    on after 2s { }\n}\n";
        let compiled = compile(src).expect("compiles");
        let json = super::manifest_json(&compiled.program);
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let arr_len = |j: Option<&crate::lsp::Json>| match j {
            Some(crate::lsp::Json::Arr(a)) => Some(a.len()),
            _ => None,
        };
        let parent = v
            .get("components")
            .and_then(|c| c.index(1))
            .expect("component 1 (Parent)");
        assert_eq!(
            parent.get("name").and_then(|n| n.str()),
            Some("Parent"),
            "components are emitted in declaration order"
        );
        assert_eq!(arr_len(parent.get("supervises")), Some(1));
        let sv = parent.get("supervises").and_then(|s| s.index(0)).expect("supervise entry");
        assert_eq!(sv.get("child").and_then(|c| c.str()), Some("kid"));
        assert_eq!(sv.get("policy").and_then(|p| p.str()), Some("restart_on_failure"));

        assert_eq!(arr_len(parent.get("handlers")), Some(4));
        let triggers: Vec<&str> = match parent.get("handlers") {
            Some(crate::lsp::Json::Arr(hs)) => {
                hs.iter().filter_map(|h| h.get("trigger")).filter_map(|t| t.str()).collect()
            }
            _ => Vec::new(),
        };
        assert_eq!(triggers, vec!["start", "stop", "every:5000", "after:2000"]);

        // a component with neither must still emit empty arrays, not omit the keys.
        let child = v.get("components").and_then(|c| c.index(0)).expect("component 0 (Child)");
        assert_eq!(arr_len(child.get("supervises")), Some(0));
        assert_eq!(arr_len(child.get("handlers")), Some(0));
    }

    #[test]
    fn disassemble_handles_source_and_compiled_modules() {
        // `kupl dis` disassembles a .kupl source (compile -> disassemble)...
        let dir = std::env::temp_dir().join(format!("kupl-dis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "fun fib(n: Int) -> Int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\nfun main() uses io { print(fib(5)) }\n";
        let sp = dir.join("m.kupl");
        std::fs::write(&sp, src).unwrap();
        assert_eq!(super::disassemble(sp.to_str().unwrap()), 0, "source disassembles");
        // ...and a compiled .kx module directly (PR-it121 — previously a confusing UTF-8
        // error). Truncated bytecode is a clean decode error, not a crash.
        let compiled = compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).expect("module");
        let bytes = crate::kx::encode(&module);
        let kx = dir.join("m.kx");
        std::fs::write(&kx, &bytes).unwrap();
        assert_eq!(super::disassemble(kx.to_str().unwrap()), 0, "a .kx module disassembles");
        let bad = dir.join("bad.kx");
        std::fs::write(&bad, &bytes[..8]).unwrap();
        assert_eq!(super::disassemble(bad.to_str().unwrap()), 1, "a truncated .kx is a clean error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_tests_reports_pass_fail_and_catches_panics() {
        // `kupl test` runs `law` blocks: a satisfied law exits 0, a violated one exits 1,
        // and a law that PANICS at runtime is caught and reported as a failure (exit 1),
        // never crashing the runner. (PR-it118 certified; forall counterexamples are
        // deterministic — verified end-to-end via the CLI.)
        let dir = std::env::temp_dir().join(format!("kupl-runtests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str| -> String {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p.to_str().unwrap().to_string()
        };
        let pass = write("pass.kupl", "fun add(a: Int, b: Int) -> Int { a + b }\nlaw \"ok\" {\n    expect add(2, 3) == 5\n}\n");
        assert_eq!(super::run_tests(&pass), 0, "a satisfied law exits 0");
        let fail = write("fail.kupl", "fun add(a: Int, b: Int) -> Int { a + b }\nlaw \"bad\" {\n    expect add(2, 3) == 6\n}\n");
        assert_eq!(super::run_tests(&fail), 1, "a violated law exits 1");
        let panic = write("panic.kupl", "fun bad(n: Int) -> Int { n / 0 }\nlaw \"boom\" {\n    expect bad(5) == 0\n}\n");
        assert_eq!(super::run_tests(&panic), 1, "a panicking law is a caught failure, not a crash");
        // a file with no laws is not an error.
        let none = write("none.kupl", "fun add(a: Int, b: Int) -> Int { a + b }\n");
        assert_eq!(super::run_tests(&none), 0, "no tests is a clean pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_tests_tallies_skipped_contract_laws() {
        // A REAL BUG found+fixed (PR-it583): the contract-laws loop's "component
        // requires props, skip its laws" branch printed a "skip ..." line but never
        // incremented the `skipped` counter (unlike the sibling `example`-skip branch
        // right above it, which correctly does `skipped += c.examples.len()`) -- the
        // final "N passed, N failed, N skipped" summary always undercounted by the
        // number of laws belonging to every prop-requiring fulfilling component,
        // regardless of exit code (still 0, since only `failed` gates it) -- a silent
        // wrong-VALUE bug in `kupl test`'s own reporting, not its execution.
        let dir = std::env::temp_dir().join(format!("kupl-runtests-skiplaw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "contract Counter {\n    intent \"a counter\"\n    expose fun get() -> Int\n    expose fun inc()\n    \
                   law \"law one\" {\n        expect get() >= 0\n    }\n    \
                   law \"law two\" {\n        let before = get()\n        inc()\n        expect get() == before + 1\n    }\n}\n\
                   component B fulfills Counter {\n    intent \"requires a prop\"\n    prop start: Int\n    \
                   state n: Int = start\n    expose fun get() -> Int { n }\n    expose fun inc() { n = n + 1 }\n}\n\
                   fun main() { print(\"hi\") }\n";
        let path = dir.join("skiplaw.kupl");
        std::fs::write(&path, src).unwrap();
        // `CARGO_BIN_EXE_kupl` is only set for integration tests/benches, not for
        // unit tests embedded in the lib crate -- fall back to the standard debug
        // build path `cargo test` itself just produced.
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet (e.g. a lib-only build) -- nothing to test
        }
        let out = std::process::Command::new(&bin)
            .args(["test", path.to_str().unwrap()])
            .output()
            .expect("kupl test runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("0 passed, 0 failed, 2 skipped"),
            "both of B's contract laws must count as skipped, not zero: {stdout:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emit_context_resolves_item_and_errors_on_missing() {
        // `kupl context <file> <item>` emits the item + its direct-dependency closure
        // for an LLM. A present item resolves (rc 0); a name that doesn't exist is a
        // clean error (rc 1), not a crash. (Closure correctness — direct deps in,
        // unrelated items out, ctor -> owning type, recursion/cycles terminate — is
        // exercised end-to-end via the CLI.)
        let dir = std::env::temp_dir().join(format!("kupl-ctx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.kupl");
        std::fs::write(&file, "fun helper(n: Int) -> Int {\n    n * 2\n}\nfun target() -> Int {\n    helper(1)\n}\n").unwrap();
        let p = file.to_str().unwrap();
        assert_eq!(super::emit_context(p, "target"), 0, "a present item resolves");
        assert_eq!(super::emit_context(p, "does_not_exist"), 1, "a missing item is a clean error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frontend_accepts_valid_and_rejects_invalid_across_features() {
        // Cross-command validity consistency (PR-it90): every command (check, run,
        // build, native) reaches the SAME frontend — compile() and check_cmd both
        // parse + inject the prelude + type-check + effect-check. This pins that the
        // shared frontend accepts a representative valid program per feature and
        // rejects the ill-typed ones, so no command can diverge on validity. (native
        // and run additionally require an entry point, which is a separate, intended
        // requirement — a valid library with no `main` still type-checks here.)
        let valid = [
            "fun f(j: Json) -> Str { match j { JStr(s) => s\n _ => \"x\" } }\n", // prelude ADT
            "fun id[T](x: T) -> T { x }\nfun main() { let _ = id(5) }\n",        // generics
            "fun add(a: Int, b: Int) -> Int { a + b }\nai fun s(q: Str) -> Str tools [add] { intent \"{q}\" }\n", // ai fun
            "component C {\n intent \"x\"\n in tick: Int\n state n: Int = 0\n on tick(v) { n = n + v } }\n", // component
        ];
        for src in valid {
            assert!(compile(src).is_ok(), "frontend wrongly rejected a valid program:\n{src}");
        }
        let invalid = [
            "fun main() { let x = nope }\n",                               // undefined name
            "fun f() -> Int { \"s\" }\n",                                  // type mismatch
            "fun main() { let x = Nope(1) }\n",                            // undefined ctor
            "type C = Red | Green\nfun f(c: C) -> Int { match c { Red => 1 } }\n", // non-exhaustive
        ];
        for src in invalid {
            assert!(compile(src).is_err(), "frontend wrongly accepted an ill-typed program:\n{src}");
        }
    }

    #[test]
    fn check_cmd_injects_the_prelude() {
        // `kupl check` must accept a program that uses a prelude type (the built-in
        // `Json` ADT) — before PR-it89 it reported false "unknown type `Json`"
        // errors because check_cmd skipped inject_prelude (unlike run/build).
        let ex = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/json.kupl");
        assert_eq!(
            super::check_cmd(ex.to_str().unwrap(), false),
            0,
            "`kupl check` must accept a valid Json-using program"
        );
    }

    #[test]
    fn ai_fun_intent_interpolation_is_checked() {
        // An `ai fun`'s `intent` string is type-checked like any string: an undefined
        // interpolation `{var}` is a clean compile error (K0240), not a runtime panic
        // that diverges interp (K0900) vs KVM (K0240). PR-it88.
        assert!(
            compile("ai fun greet(name: Str) -> Str { intent \"Hello {nombre}\" }\n").is_err(),
            "undefined intent interpolation var must be a compile error"
        );
        // a valid intent referencing a real param checks clean.
        assert!(compile("ai fun greet(name: Str) -> Str { intent \"Hello {name}\" }\n").is_ok());
    }

    #[test]
    fn type_checker_rejects_ill_typed_programs() {
        // Soundness: the checker must REJECT programs that would otherwise crash or
        // misbehave at runtime — no silent hole hands an ill-typed program to the
        // engines. compile() returns Err when there is any Severity::Error.
        let rejected = [
            "fun f(o: Option[Int]) -> Int { o + 1 }\n", // Option used as its inner type
            "fun main() { let x: Int = [1, 2, 3].get(0) }\n", // get returns Option, no implicit unwrap
            "fun main() { let xs = [1, 2].push(\"s\") }\n", // wrong element type
            "fun f() -> Int { \"s\" }\n",               // return type mismatch
            "fun main() { let xs = [1, \"s\"] }\n",     // heterogeneous list
            "fun main() { let x = (5).to_upper() }\n",  // method on wrong receiver type
            "fun g(a: Int, b: Int) -> Int { a + b }\nfun main() { let x = g(1) }\n", // arity
            "type C = Red | Green | Blue\nfun f(c: C) -> Int { match c {\n Red => 1\n Green => 2 } }\n", // non-exhaustive
            "fun main() { let x = nope }\n",            // undefined name
            "fun id[T](x: T) -> T { x }\nfun main() { let a: Int = id(\"s\") }\n", // generic misuse
        ];
        for src in rejected {
            assert!(compile(src).is_err(), "checker WRONGLY ACCEPTED ill-typed program:\n{src}");
        }
        // …and ACCEPT valid programs (no false positives): shadowing that changes a
        // binding's type, a recursive ADT with match, and a generic function.
        let accepted = [
            "fun main() uses io { let x = 1\n    let x = \"s\"\n    print(x.to_upper()) }\n",
            "type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)\nfun sum(t: Tree) -> Int { match t {\n Leaf(v) => v\n Node(l, r) => sum(l) + sum(r) } }\n",
            "fun first[T](xs: List[T]) -> Option[T] { xs.first() }\nfun main() { let x = first([1, 2, 3]) }\n",
        ];
        for src in accepted {
            assert!(compile(src).is_ok(), "checker WRONGLY REJECTED a valid program:\n{src}");
        }
    }

    #[test]
    fn sort_diags_is_deterministic_and_position_ordered() {
        // Diagnostics arriving in arbitrary (e.g. HashMap-iteration) order must come
        // out ordered by source position, then code, then message — so `kupl check`/
        // `run` output is byte-identical every invocation (PR-it78/79).
        let mk = |start: u32, code: &'static str, msg: &str| Diag::warning(code, msg.to_string(), Span::new(start, start + 1));
        let ordered = vec![
            mk(0, "K0302", "a"),
            mk(5, "K0100", "b"),
            mk(5, "K0101", "c"), // same span, later code
            mk(20, "K0302", "d"),
        ];
        // feed several scrambles; each must sort to the same canonical order
        for perm in [[3, 1, 0, 2], [2, 0, 3, 1], [1, 3, 2, 0], [0, 1, 2, 3]] {
            let mut v: Vec<Diag> = perm.iter().map(|&i| ordered[i].clone()).collect();
            sort_diags(&mut v);
            let keys: Vec<(u32, &str)> = v.iter().map(|d| (d.span.start, d.code)).collect();
            assert_eq!(keys, vec![(0, "K0302"), (5, "K0100"), (5, "K0101"), (20, "K0302")]);
        }
    }

    /// A follow-up to the loader.rs fix (production-hardening PR-it625): a
    /// project whose ONLY declared dependency is registry-only (`{ version =
    /// ".." }`, no `path`) used to make `kupl pkg tree`/`kupl pkg lock` look
    /// completely dependency-free ("no dependencies" / a 0-dependency
    /// lockfile), even though the manifest DOES declare one -- silently
    /// indistinguishable from a project with no `[dependencies]` section at
    /// all. `pkg_tree`/`pkg_lock` don't panic on this (never did), but their
    /// EXIT CODE and LOCKFILE CONTENT are the only things a test can assert
    /// on without capturing stdout — confirms both still succeed cleanly
    /// (exit 0) and the lockfile correctly has 0 RESOLVED entries (nothing
    /// to lock for an unresolvable registry dep), while the loader.rs test
    /// (`version_only_dependency_reports_a_clear_registry_error_not_a_confusing_file_not_found`)
    /// covers the actual user-facing improvement: `registry_only_deps`
    /// surfacing the name/version so the CLI's printed note (verified
    /// manually, not capturable here) can report it instead of staying
    /// silent.
    #[test]
    fn pkg_tree_and_lock_do_not_crash_on_a_registry_only_dependency() {
        let dir = std::env::temp_dir().join(format!("kupl-pkgcli-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\njson2 = { version = \"1.2.0\" }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "fun main() {}\n").unwrap();
        let entry = dir.join("main.kupl");
        let entry_str = entry.to_str().unwrap();

        assert_eq!(super::pkg_tree(entry_str), 0, "pkg tree must not error on an unresolvable registry dep");
        assert_eq!(super::pkg_lock(entry_str), 0, "pkg lock must not error on an unresolvable registry dep");
        let lock_text = std::fs::read_to_string(dir.join("kupl.lock")).expect("lockfile written");
        let hashes = crate::loader::lock_hashes(&lock_text);
        assert!(hashes.is_empty(), "nothing resolvable, so nothing should be locked: {lock_text:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project with NO registry-only dependencies must report that
    /// plainly (mirroring `pkg_tree`/`pkg_lock`'s existing "no
    /// dependencies" messaging) and exit 0 — without ever invoking a
    /// fetch at all, so this exercises the REAL `pkg_fetch` (not the
    /// injectable `_with` variant) with zero live network access, since
    /// the fetch closure is provably never called on this path.
    #[test]
    fn pkg_fetch_reports_no_registry_dependencies_cleanly() {
        let dir = std::env::temp_dir().join(format!("kupl-pkgfetch-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "fun main() {}\n").unwrap();
        assert_eq!(super::pkg_fetch(dir.join("main.kupl").to_str().unwrap()), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing/unreadable entry file is a clean error (matching
    /// `pkg_tree`/`pkg_lock`'s existing behavior for the same condition),
    /// not a panic.
    #[test]
    fn pkg_fetch_reports_a_missing_entry_as_a_clean_error() {
        assert_eq!(super::pkg_fetch("/nonexistent/path/does-not-exist/main.kupl"), 1);
    }

    /// The real work: every registry-only dependency gets fetched via the
    /// injected transport (no live network access — the SAME
    /// dependency-injection pattern `registry.rs`'s `fetch_package_with`
    /// already uses, production-hardening PR-it632), and the printed
    /// destination + exit code both reflect success.
    #[test]
    fn pkg_fetch_with_downloads_every_registry_only_dependency_via_the_injected_fetcher() {
        let dir = std::env::temp_dir().join(format!("kupl-pkgfetch-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\njson2 = { version = \"1.2.0\" }\ncsvlib = { version = \"2.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "fun main() {}\n").unwrap();
        let cache = dir.join("cache");
        let fetched = std::cell::RefCell::new(Vec::new());
        let exit = super::pkg_fetch_with(
            dir.join("main.kupl").to_str().unwrap(),
            "https://registry.example.com",
            &cache,
            |registry_url, name, version, cache_dir| {
                fetched.borrow_mut().push((registry_url.to_string(), name.to_string(), version.to_string()));
                Ok(cache_dir.join(name).join(version))
            },
        );
        assert_eq!(exit, 0);
        let calls = fetched.into_inner();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls.contains(&(
            "https://registry.example.com".to_string(),
            "csvlib".to_string(),
            "2.0.0".to_string()
        )));
        assert!(calls.contains(&(
            "https://registry.example.com".to_string(),
            "json2".to_string(),
            "1.2.0".to_string()
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One dependency's fetch failure must not abort the rest — a project
    /// with several registry dependencies should still attempt (and
    /// report) EVERY one, not stop at the first failure, while the
    /// function's own exit code still reflects that something failed.
    #[test]
    fn pkg_fetch_with_reports_a_per_package_failure_without_aborting_the_rest() {
        let dir = std::env::temp_dir().join(format!("kupl-pkgfetch-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\njson2 = { version = \"1.2.0\" }\ncsvlib = { version = \"2.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.kupl"), "fun main() {}\n").unwrap();
        let attempted = std::cell::RefCell::new(Vec::new());
        let exit = super::pkg_fetch_with(
            dir.join("main.kupl").to_str().unwrap(),
            "https://registry.example.com",
            &dir.join("cache"),
            |_registry_url, name, _version, cache_dir| {
                attempted.borrow_mut().push(name.to_string());
                if name == "json2" {
                    Err("simulated network failure".to_string())
                } else {
                    Ok(cache_dir.join(name))
                }
            },
        );
        assert_eq!(exit, 1, "a failed dependency must make the overall exit code non-zero");
        assert_eq!(
            attempted.into_inner().len(),
            2,
            "the OTHER dependency must still be attempted, not skipped after the first failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

}
