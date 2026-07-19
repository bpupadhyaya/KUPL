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
    fn item_name(item: &Item) -> &str {
        match item {
            Item::Fun(f) => &f.name,
            Item::Type(t) => &t.name,
            Item::Component(c) => &c.name,
            Item::Contract(ct) => &ct.name,
            Item::Law(l) => &l.name,
        }
    }
    // A REAL usability gap found+fixed (production-hardening PR-it780, the
    // second half of a late-delivered Explore survey finding, agentId
    // aaed1d00a40c9e7b6, independently re-verified live before implementing):
    // a dependency's item is stored under its `isolate()`-mangled name
    // (`dep$Widget`), which `kupl manifest` used to leak verbatim (fixed
    // above) but even after that fix a user/LLM reading `kupl manifest`'s now-
    // clean `"Widget"` had no way to ask `kupl context` about it -- `kupl
    // context main.kupl Widget` failed with "no item named `Widget`", forcing
    // the caller to already know the internal `dep$Widget` mangling syntax,
    // which is never surfaced anywhere else in the CLI. Fix: try an exact
    // match first (covers root-package items, which are never mangled, and a
    // caller who already knows the mangled form), and only if that misses,
    // fall back to a demangled match. Two different dependencies could both
    // declare `Widget`, demangling to the same bare name -- report that
    // explicitly as an ambiguity rather than silently picking one.
    let exact = compiled.program.items.iter().find(|item| item_name(item) == name);
    let target = if let Some(t) = exact {
        t
    } else {
        let matches: Vec<&Item> = compiled
            .program
            .items
            .iter()
            .filter(|item| crate::resolve::demangle_for_display(item_name(item)) == name)
            .collect();
        match matches.as_slice() {
            [] => {
                eprintln!("error: no item named `{name}` in {file}");
                return 1;
            }
            [only] => *only,
            many => {
                let candidates: Vec<&str> = many.iter().map(|item| item_name(item)).collect();
                eprintln!(
                    "error: `{name}` is ambiguous across dependencies — did you mean one of: {}?",
                    candidates.join(", ")
                );
                return 1;
            }
        }
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
            // A REAL usability gap found+fixed (production-hardening
            // PR-it858, an Explore survey finding, independently
            // re-verified live before implementing): a child instance's
            // component TYPE (`child.component`) was noted, but its
            // constructor ARGUMENTS (`child.args[].value`, e.g.
            // `Holder(box: make_box())`) were never walked -- so a function
            // referenced ONLY inside a child-instantiation argument (like
            // `make_box` above) never made it into `kupl context`'s
            // "direct dependencies" section, even though the target
            // item's OWN printed source visibly calls it. Every OTHER
            // expression-bearing field on this arm (`state[].init` just
            // below, handler bodies) already gets this same treatment;
            // `children[].args` was the one omission. Confirmed the SAME
            // gap for both named (`Holder(box: make_box())`) and
            // positional (`Holder(make_box())`) argument forms.
            for child in &c.children {
                note(&child.component);
                for a in &child.args {
                    crate::effects::walk_block(
                        &crate::ast::Block { stmts: vec![crate::ast::Stmt::Expr(a.value.clone())], span: a.value.span },
                        &mut |e| collect_expr_names(e, &mut note),
                    );
                }
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
        // A REAL bug found+fixed (production-hardening PR-it777, an Explore
        // survey finding, agentId ad3c3f6ee2f0cd891, independently re-verified
        // live before implementing): `effects::walk_block`/`walk_expr` (which
        // drives this function's own invocation for every reachable Expr)
        // visits a `Match`'s `scrutinee` and each arm's `guard`/`body`, but
        // NEVER an arm's `pattern` -- so a function that discriminates a type
        // ONLY via `match ... { Circle(_) => ..., Square(_) => ... }`, with
        // that type never appearing in its own signature, had the type
        // SILENTLY OMITTED from `kupl context`'s emitted dependency list.
        // Confirmed live: `fun classify() -> Str { let s = make_shape(); match
        // s { Circle(_) => "circle", Square(_) => "square" } }` -- `kupl
        // context file.kupl classify` included `fun make_shape` (an ordinary
        // call) but completely omitted `type Shape`, even though the type IS
        // the only thing the match structurally depends on. `walk_expr` DOES
        // still call back with the `Match` EXPR itself (its callback fires
        // for every node before recursing on `.kind`), so this arm is reached
        // -- the fix stays entirely within `collect_expr_names`, no change
        // needed to the SHARED `effects.rs` walker (which has no need for
        // pattern-derived names -- patterns can't call functions, so they're
        // irrelevant to ITS purpose, effect-purity inference; extending its
        // shared `FnMut(&Expr)` callback signature to also report patterns
        // would be unnecessary scope creep there). Emitted constructor names
        // reuse the EXACT SAME `note` closure/`compiled.checked.ctors`
        // resolution already used for an ordinary constructor CALL (e.g.
        // `Circle(r: 5)` parses as `ExprKind::Call{callee: Ident("Circle"),
        // ..}`, already handled above) -- no new resolution logic needed.
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                collect_pattern_names(&arm.pattern, f);
            }
        }
        _ => {}
    }
}

/// A pattern's constructor names, resolved to their owning types via the SAME
/// `note` closure every other name in this module goes through. Recurses into
/// `Ctor`'s nested args and `Or`/`At`'s sub-patterns -- mirroring the EXACT
/// same Or/At recursion gap `resolve.rs`'s `Rewriter::pattern` had (PR-it775,
/// a different bug, same missing-case shape) so this doesn't reintroduce it
/// here. `Wildcard`/`Bind`/`Int`/`Bool`/`Str`/`Range` are leaf patterns with
/// no name to report.
fn collect_pattern_names(p: &crate::ast::Pattern, f: &mut impl FnMut(&str)) {
    use crate::ast::PatternKind;
    match &p.kind {
        PatternKind::Ctor { name, args } => {
            f(name);
            for a in args {
                collect_pattern_names(a, f);
            }
        }
        PatternKind::Or(alts) => {
            for a in alts {
                collect_pattern_names(a, f);
            }
        }
        PatternKind::At { inner, .. } => collect_pattern_names(inner, f),
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
        // A REAL bug found+fixed (production-hardening PR-it763, the second
        // finding from the SAME survey that produced PR-it762's lockfile
        // field-escaping fix): the drift check below only ever asked "is
        // THIS dependency's hash different from what the lockfile
        // recorded" -- `h.get(&d.name)` returning `None` (this dependency
        // was never locked at all) and `Some(hash) if hash == &d.hash`
        // (locked AND unchanged) both fell through to the SAME "" (no
        // marker) branch, so a BRAND-NEW dependency just added to
        // `kupl.toml` looked indistinguishable from one that's already
        // locked and unchanged. Live-confirmed before this fix: adding a
        // never-locked dependency produced no `[new]`/any marker at all in
        // `kupl pkg tree`'s output. Fixed by splitting the `None` case out
        // from the "unchanged" case with its own `[new, not yet locked]`
        // marker.
        // A REAL bug found+fixed (production-hardening PR-it763, the second
        // finding from the SAME survey that produced PR-it762's lockfile
        // field-escaping fix): the drift check below only ever asked "is
        // THIS dependency's hash different from what the lockfile
        // recorded" -- `h.get(&d.name)` returning `None` (this dependency
        // was never locked at all) and `Some(hash) if hash == &d.hash`
        // (locked AND unchanged) both fell through to the SAME "" (no
        // marker) branch, so a BRAND-NEW dependency just added to
        // `kupl.toml` looked indistinguishable from one that's already
        // locked and unchanged. Live-confirmed before this fix: adding a
        // never-locked dependency produced no `[new]`/any marker at all in
        // `kupl pkg tree`'s output. Fixed by splitting the `None` case out
        // from the "unchanged" case with its own `[new, not yet locked]`
        // marker.
        let marker = match &locked {
            Some(h) => match h.get(&d.name) {
                Some(old) if old != &d.hash => "  [drift]",
                Some(_) => "",
                None => "  [new, not yet locked]",
            },
            None => "",
        };
        println!("{} @ {}  ({}){}", d.name, ver, d.path, marker);
    }
    for (name, version) in &registry_only {
        println!("{name} @ {version}  (registry — not yet supported, unresolved)");
    }
    // A SECOND, independently-real half of the SAME bug (PR-it763): the loop
    // above only ever iterates the CURRENT manifest's dependencies, looking
    // each one up in the old lock map -- it never iterates the LOCK FILE'S
    // own names to find entries that no longer exist in the current
    // manifest at all. Removing a dependency from `[dependencies]` without
    // re-running `kupl pkg lock` made it silently VANISH from this
    // command's output with no indication the lockfile itself was now
    // stale for that entry -- live-confirmed before this fix. `kupl.lock`
    // only ever locks path-resolvable dependencies (never `registry_only`
    // ones, which can't resolve without a registry), so orphans are found
    // by diffing `locked`'s names against `deps`'s names alone.
    if let Some(h) = &locked {
        let current: std::collections::HashSet<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        let mut orphaned: Vec<&String> = h.keys().filter(|name| !current.contains(name.as_str())).collect();
        orphaned.sort();
        for name in orphaned {
            println!("{name}  [in kupl.lock but no longer in kupl.toml — stale, re-run `kupl pkg lock`]");
        }
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
        // `c.name`/`fulfills`/`children[].component` all name a top-level item
        // that may live in a dependency package, so `isolate()` may have
        // mangled it to `pkg$Name` -- demangle for display here, matching
        // `ty_str`'s own PR-it780 fix just above (and PR-it628's precedent in
        // check.rs/types.rs/value.rs). `ports`/`props`/`state`/`exposes`/
        // `children[].name`/`supervises[].child` are all LOCAL names, never
        // mangled, and need no such treatment.
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"kind\":\"{}\",\"intent\":\"{}\"",
            esc(crate::resolve::demangle_for_display(&c.name)),
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
        // A REAL schema-consistency bug found+fixed (production-hardening
        // PR-it778, an Explore survey finding, agentId ad3c3f6ee2f0cd891,
        // independently re-verified live before implementing): `state` was
        // emitted as a BARE ARRAY OF NAME STRINGS (`["count"]`) while `ports`/
        // `props` are arrays of STRUCTURED OBJECTS carrying a `type` field --
        // a visual-tool consumer got zero type info for state fields
        // specifically. DELIBERATELY NARROWER than the survey's own framing,
        // per this file's OWN prior documented reasoning (see
        // `manifest_reports_supervises_and_handlers`'s doc comment, PR-it647):
        // `state`'s name-only serialization was NOT an oversight of the SAME
        // shape as PR-it647's `supervises`/`handlers` gap -- it was a
        // DELIBERATE choice, because `init` is "an arbitrary expression, not
        // simple manifest data." That reasoning still holds and is NOT
        // re-litigated here: `init` stays OUT of the manifest, matching
        // `props`'s OWN identical precedent (a prop's default value is
        // likewise never rendered as expression text -- only a derived
        // `required: bool`). What genuinely WAS missing, and is NOT an
        // arbitrary-expression concern, is `type` -- a `TyExpr`, exactly as
        // simple and structurally serializable as `ports`/`props`'s OWN
        // `type` fields (rendered via the SAME `ty_str`). `ty` is optional in
        // the grammar (`state count = 0` infers its type at check-time,
        // unavailable to this function given only the raw, unchecked
        // `Program`) -- falls back to `""`, matching this SAME function's
        // own existing convention for `intent` (`Option<String>` ->
        // `unwrap_or("")`).
        let state: Vec<String> = c
            .state
            .iter()
            .map(|s| {
                format!(
                    "{{\"name\":\"{}\",\"type\":\"{}\"}}",
                    esc(&s.name),
                    esc(&s.ty.as_ref().map(crate::fmt::ty_str).unwrap_or_default()),
                )
            })
            .collect();
        out.push_str(&format!(",\"state\":[{}]", state.join(",")));
        // A REAL schema-consistency bug found+fixed (production-hardening
        // PR-it856, an Explore survey finding, independently re-verified live
        // before implementing): `exposes[].params` was a bare array of
        // PRE-FORMATTED STRINGS (`["n: Int", "tag: Str"]`) while `ports`/
        // `props`/`state` are all arrays of STRUCTURED OBJECTS carrying a
        // separate `name`/`type` field -- the EXACT same shape of gap
        // PR-it778 just above fixed for `state`, missed for this sibling
        // field in that same sweep. A consumer had to re-parse each string
        // (splitting on `": "`) instead of reading two JSON fields directly.
        // No `init`-style "arbitrary expression" concern applies here (a
        // param's type is a `TyExpr`, exactly as simple/serializable as
        // `props`'s own `type` field, rendered via the SAME `ty_str`).
        let exposes: Vec<String> = c
            .exposes
            .iter()
            .map(|f| {
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| {
                        format!(
                            "{{\"name\":\"{}\",\"type\":\"{}\"}}",
                            esc(&p.name),
                            esc(&crate::fmt::ty_str(&p.ty))
                        )
                    })
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
        let fulfills: Vec<String> = c
            .fulfills
            .iter()
            .map(|f| format!("\"{}\"", esc(crate::resolve::demangle_for_display(f))))
            .collect();
        out.push_str(&format!(",\"fulfills\":[{}]", fulfills.join(",")));
        let children: Vec<String> = c
            .children
            .iter()
            .map(|ch| {
                format!(
                    "{{\"name\":\"{}\",\"component\":\"{}\"}}",
                    esc(&ch.name),
                    esc(crate::resolve::demangle_for_display(&ch.component))
                )
            })
            .collect();
        out.push_str(&format!(",\"children\":[{}]", children.join(",")));
        // A REAL schema-consistency bug found+fixed (production-hardening
        // PR-it857, the SAME sweep that just fixed `exposes[].params` at
        // it856): `from`/`to` were each flattened into a SINGLE dot-joined
        // string (`"feed.numbers"`) instead of a structured `{"component":
        // ..., "port":...}` object like `ports`/`props`/`state`/`children`/
        // `exposes.params` -- forcing a consumer to re-parse by splitting on
        // `.` instead of reading two JSON fields directly.
        let wires: Vec<String> = c
            .wires
            .iter()
            .map(|w| {
                format!(
                    "{{\"from\":{{\"component\":\"{}\",\"port\":\"{}\"}},\"to\":{{\"component\":\"{}\",\"port\":\"{}\"}}}}",
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
        // A REAL schema-consistency bug found+fixed (production-hardening
        // PR-it857, the SAME sweep that just fixed `exposes[].params` at
        // it856, and `wires`'s own analogous fix just above): `trigger` was
        // a SINGLE colon-joined string (`"port:input"`, `"every:5000"`)
        // rather than a structured `{"kind":..., ...}` object -- the `Port`/
        // `Every`/`After` variants each carry a genuine sub-value (a port
        // name, or a millisecond count) a consumer had to re-parse out of
        // the string instead of reading a direct field. `Start`/`Stop`
        // (unit variants) get just `{"kind":"start"}`/`{"kind":"stop"}`,
        // matching this SAME function's own established convention (e.g.
        // `props`'s `required` boolean) of omitting a field entirely rather
        // than emitting an empty/null placeholder when there's nothing to say.
        let handlers: Vec<String> = c
            .handlers
            .iter()
            .map(|h| {
                let trigger = match &h.trigger {
                    crate::ast::Trigger::Port(p) => format!("{{\"kind\":\"port\",\"port\":\"{}\"}}", esc(p)),
                    crate::ast::Trigger::Start => "{\"kind\":\"start\"}".to_string(),
                    crate::ast::Trigger::Stop => "{\"kind\":\"stop\"}".to_string(),
                    crate::ast::Trigger::Every(ms) => format!("{{\"kind\":\"every\",\"ms\":{ms}}}"),
                    crate::ast::Trigger::After(ms) => format!("{{\"kind\":\"after\",\"ms\":{ms}}}"),
                };
                format!("{{\"trigger\":{trigger}}}")
            })
            .collect();
        out.push_str(&format!(",\"handlers\":[{}]", handlers.join(",")));
        out.push_str(&format!(",\"examples\":{}}}", c.examples.len()));
    }
    out.push_str("]}");
    out
}

/// A CRITICAL data-loss bug found+fixed (production-hardening PR-it781, an
/// Explore survey finding, independently re-verified live before
/// implementing): `kupl bundle`/`kupl native`'s default output path is the
/// input path with a trailing `.kupl` trimmed off -- a no-op if the source
/// file doesn't literally end in `.kupl` (KUPL never requires that
/// extension), so the computed output SILENTLY COLLIDED WITH THE SOURCE
/// FILE and overwrote it with a compiled binary -- no warning, no
/// confirmation, permanently (for `native`, the intermediate `.c` is also
/// deleted on success unless `--keep-c`, leaving nothing recoverable at
/// all). An explicit `-o <source-path>` hits the identical collision
/// regardless of naming. Confirmed live before this fix: `kupl bundle foo`
/// (source file literally named `foo`, no `.kupl` suffix) destroyed `foo`,
/// replacing it with a Mach-O executable, exit code 0, a "success" message
/// giving no indication anything unusual happened. Canonicalizes both sides
/// (falling back to `loader::normalize`'s lexical `.`/`..` resolution when
/// the output doesn't exist yet, since `canonicalize()` requires the path
/// to already exist -- mirrors `loader.rs`'s own `dep_identity` convention
/// exactly, see that function's doc comment) so `./foo` vs `foo` and a
/// symlinked path still compare correctly, not just a literal string match.
pub fn output_would_overwrite_source(out: &str, source: &str) -> bool {
    let identity = |p: &str| -> std::path::PathBuf {
        let p = std::path::Path::new(p);
        p.canonicalize().unwrap_or_else(|_| crate::loader::normalize(p))
    };
    identity(out) == identity(source)
}

/// `kupl native`: emit C from the bytecode and compile with the system cc.
pub fn native(path: &str, args: &[String]) -> i32 {
    // A REAL usability bug found+fixed (production-hardening PR-it782, an
    // Explore survey finding, independently re-verified live before
    // implementing): `native`/`build`/`bundle` all try to PARSE their input
    // as `.kupl` source with no `.kx`-extension check, unlike `run`/`dis`,
    // which already special-case a compiled `.kx` module (`disassemble`'s
    // own `path.ends_with(".kx")` guard right above, mirrored here exactly,
    // and `main.rs`'s `run` dispatch arm, PR-it594's original precedent).
    // Feeding a `.kx` file to any of these instead walked the lexer over
    // raw bytecode BYTE-BY-BYTE, emitting one `K0001: unexpected character`
    // diagnostic per non-token byte -- confirmed live before this fix:
    // `kupl native qux.kx` printed 1455 lines of garbage instead of one
    // clear, actionable error.
    if path.ends_with(".kx") {
        eprintln!(
            "error: {path} is already compiled bytecode (.kx) -- `kupl native` compiles `.kupl` \
             source, not an existing module"
        );
        return 1;
    }
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
    if output_would_overwrite_source(&out, path) {
        eprintln!(
            "error: refusing to overwrite the source file {path} -- the output path resolves to the \
             same file (use -o to choose a different output path)"
        );
        return 1;
    }
    let c_path = format!("{out}.c");
    if let Err(e) = std::fs::write(&c_path, &c_src) {
        eprintln!("error: cannot write {c_path}: {e}");
        return 1;
    }
    // `-ffp-contract=off` (production-hardening PR-it813): a REAL, live-confirmed
    // silent value-corruption bug -- `-O2` alone leaves fused-multiply-add
    // CONTRACTION enabled (clang/gcc's default at -O2 is `fast`/`on`, both of
    // which permit fusing `a * b + c`-shaped expressions into a single hardware
    // `fmadd`/`fmla` with ONE rounding, vs. two separate IEEE-754 roundings).
    // `Tensor.dot`'s accumulator loop (`s += t->data[i] * args[0].as.ten->data[i]`,
    // cgen.rs) is exactly that shape, and generated C has no `#pragma STDC
    // FP_CONTRACT OFF` guarding it -- Rust's `*`/`+` NEVER auto-fuse (only
    // `f64::mul_add`'s EXPLICIT fusion does, already mirrored via cgen.rs's own
    // explicit `fma()` call for `Float.mul_add`), so interp.rs/vm.rs always
    // compute two roundings while native's C compiler was free to compute one --
    // CONFIRMED LIVE on this (AArch64, where FMA is a baseline instruction, not
    // gated behind a march flag) machine: `tensor([-15.885545904716025234,
    // -821.03283107768288573]).dot(tensor([1.0, -830.29967831256601585]))`
    // printed `681687.4099819507` on `kupl run`/`kupl run --vm` but
    // `681687.4099819508` on `kupl native` -- a genuine last-bit divergence, no
    // crash, no panic, a silent WRONG ANSWER in ordinary floating-point Tensor
    // code. This flag is a portable, deterministic fix (disables the compiler's
    // discretion outright, rather than depending on any particular
    // architecture's or compiler's default), so post-fix all three engines are
    // BIT-IDENTICAL regardless of the build machine's ISA.
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(&cc)
        .args(["-O2", "-ffp-contract=off", "-o", &out, &c_path])
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
///
/// `args_override`: when `Some`, `args()` returns this list directly instead
/// of going through `program_args()`'s `--`-separator convention (correct
/// ONLY for the `kupl run`/`kupl run --vm` CLI-wrapper invocation shape) --
/// production-hardening PR-it798. `run_module` has two callers with
/// genuinely different correct `args()` semantics: a `kupl bundle`-produced
/// self-contained executable IS the whole running process (invoked directly,
/// `./myapp a b c`, no wrapper, no `--` needed, exactly like `kupl native`'s
/// own `argv[1..]`), so it passes `Some(argv[1..])`; `kupl run some.kx --
/// a b c` (a precompiled `.kx` run through the ordinary `kupl` CLI wrapper)
/// passes `None`, preserving the SAME `--`-required convention as running a
/// `.kupl` source file the normal way.
pub fn run_module(module: &crate::bytecode::Module, origin: &str, args_override: Option<Vec<String>>) -> i32 {
    let mut vm = crate::vm::Vm::new(module);
    vm.print_unwired = true;
    vm.args_override = args_override;
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
            // A REAL text-consistency bug found+fixed (production-hardening
            // PR-it783, an Explore survey finding, independently re-verified
            // live before implementing): the OLD `snippet(src, span)` call
            // re-sliced raw SOURCE using `span`, which `parser.rs` builds by
            // merging the `expect` KEYWORD's own span with the condition
            // expression's span -- so the rendered text was `` `expect
            // doubled >= -50` `` (keyword included), unlike `run_forall`'s
            // OWN failure text (PR-it771), which is built from the ALREADY-
            // COMPUTED `msg` (interp.rs's `Stmt::Expect` panic message,
            // itself rendered via `fmt::expr_str`, which never includes the
            // keyword) and so correctly shows just `` `doubled >= -50` ``.
            // PR-it771's own doc comment explicitly says the two are
            // MEANT to match -- they didn't. Confirmed live: `law "plain" {
            // expect 1 == 2 }` printed `` `expect 1 == 2` was not satisfied
            // `` while an equivalent `forall`-wrapped `expect` printed just
            // `` `1 == 2` was not satisfied ``. Fixed by reusing `msg`'s
            // already-clean text here too, mirroring `run_forall`'s EXACT
            // pattern -- `snippet()` is no longer called anywhere after this
            // fix (see its sibling fixes below), so it's removed as dead
            // code rather than left orphaned.
            Err(Flow::Panic { msg, span }) => {
                let detail = if let Some(cond) = msg.strip_prefix("expectation failed: ") {
                    format!("`{cond}` was not satisfied")
                } else if msg.starts_with("property failed for ") {
                    // A `forall` counterexample (interp.rs's `run_forall`) is
                    // ALREADY a complete, self-descriptive test-failure
                    // message in its own right (PR-it771) -- not a genuine
                    // interpreter panic needing a source-pointing diagnostic,
                    // even when the inner cause it names WAS a genuine panic
                    // (`run_forall`'s own message already wraps that case as
                    // `"... (panic: {msg})"`). Left OUT of scope for this
                    // iteration's `report_panic_map`/`"panic: "` fix below --
                    // an EARLIER draft of this fix wrapped this case too,
                    // caught live: it made an ORDINARY property-test failure
                    // (`forall x: Int { expect x == 999999 }`, deterministically
                    // false at x=0) print a spurious `error[K0900]` block, as
                    // if the interpreter itself had crashed.
                    msg
                } else {
                    // A REAL reporting-consistency bug found+fixed (same
                    // PR-it783 finding): a genuine runtime panic (as opposed
                    // to an ordinary `expect` failure OR a `forall` failure,
                    // both handled above) in a top-level law printed a
                    // stdout FAIL line but NOTHING on stderr -- unlike the
                    // structurally identical component-example panic case
                    // just below, which ALREADY calls `report_panic_map`
                    // for a rich, source-pointing diagnostic. Confirmed
                    // live: a `law` whose body divides by zero left stderr
                    // completely empty, while the identical panic inside a
                    // component `example` produced a full `error[K0900]`
                    // block with a caret. Both branches now call the SAME
                    // `report_panic_map`, and both now use the SAME
                    // "panic: {msg}" stdout wording, closing this gap for
                    // every one of `kupl test`'s three test-item categories
                    // uniformly (the contract-law loop below gets the
                    // identical fix).
                    report_panic_map(&msg, span, &map);
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
            let result = run_example(&mut interp, &c.name, example);
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
                    // Same three-way PR-it783 fix as the top-level law loop
                    // above: reuse `msg`'s already-clean, keyword-free
                    // condition text instead of re-slicing source with a
                    // keyword-inclusive span; leave a `forall` counterexample's
                    // own already-complete message alone (NOT a genuine
                    // panic, see the sibling comment above); and call
                    // `report_panic_map` for a genuine panic so contract-law
                    // failures get the SAME rich stderr diagnostic as a
                    // component-example panic.
                    Err(Flow::Panic { msg, span }) => {
                        let detail = if let Some(cond) = msg.strip_prefix("expectation failed: ") {
                            format!("`{cond}` was not satisfied")
                        } else if msg.starts_with("property failed for ") {
                            msg
                        } else {
                            report_panic_map(&msg, span, &map);
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
            ExampleStep::Expect { expr, .. } => {
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
                    // A REAL text-consistency bug found+fixed (production-
                    // hardening PR-it783, matching this file's own law-loop
                    // fix above): `snippet(src, *span)` re-sliced raw source
                    // using a span that includes the `expect` KEYWORD itself
                    // (parser.rs merges the keyword's span with the
                    // condition's), so an example's failed `expect` showed
                    // `` `expect ok == 0` was not satisfied `` instead of
                    // just `` `ok == 0` was not satisfied ``. Unlike the law
                    // loops, this path has no panic `msg` to reuse -- render
                    // the condition via `fmt::expr_str` directly instead,
                    // the SAME renderer `Stmt::Expect`'s own panic message
                    // uses (interp.rs), so this now matches every OTHER
                    // "was not satisfied" site in the codebase.
                    let text = crate::fmt::expr_str(expr, 0);
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

#[cfg(test)]
mod tests {
    use super::{compile, sort_diags};
    use crate::diag::{Diag, Span};

    #[test]
    fn output_would_overwrite_source_handles_identical_different_and_not_yet_existing_paths() {
        let dir = std::env::temp_dir().join(format!("kupl-owos-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("foo.kupl");
        std::fs::write(&source, "fun main() {}\n").unwrap();
        let s = source.to_str().unwrap();

        assert!(super::output_would_overwrite_source(s, s), "the exact same path is a collision");
        // a differently-SPELLED but identical path (`./` prefix vs bare) must
        // still be detected -- a literal string comparison alone would miss this.
        let dotted = format!("{}/./foo.kupl", dir.to_str().unwrap());
        assert!(
            super::output_would_overwrite_source(&dotted, s),
            "a lexically different but identical real path must still collide"
        );

        let other = dir.join("bar.kx");
        assert!(
            !super::output_would_overwrite_source(other.to_str().unwrap(), s),
            "a genuinely different path is not a collision"
        );

        // the output path doesn't exist YET (the normal case, before writing) --
        // `canonicalize()` fails for it, so this must fall back to lexical
        // normalization rather than panicking or false-positiving.
        let not_yet = dir.join("does_not_exist_yet.kx");
        assert!(
            !super::output_would_overwrite_source(not_yet.to_str().unwrap(), s),
            "a not-yet-existing distinct output path is not a collision"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A CRITICAL data-loss bug found+fixed (production-hardening PR-it781, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `native`'s default output path (`path` with `.kupl`
    /// trimmed off) is a no-op when the source doesn't literally end in
    /// `.kupl`, so it silently collided with and overwrote the source file.
    /// Confirmed live before this fix: `kupl native bar` (source `bar`, no
    /// extension) destroyed `bar`, replacing it with a Mach-O executable --
    /// with NOTHING recoverable afterward, since the intermediate `.c` file
    /// is also deleted on success unless `--keep-c` is passed.
    #[test]
    fn native_refuses_to_overwrite_an_extensionless_source_file() {
        let dir = std::env::temp_dir().join(format!("kupl-native-owos-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "fun main() uses io {\n    print(\"hi\")\n}\n";
        let bare = dir.join("bar");
        std::fs::write(&bare, src).unwrap();
        let p = bare.to_str().unwrap().to_string();
        let code = super::native(&p, &[]);
        assert_eq!(code, 1, "must refuse rather than overwrite the source");
        let after = std::fs::read_to_string(&bare).unwrap();
        assert_eq!(after, src, "the source file's content must be completely untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // `trigger` is a structured `{"kind":...}` object (PR-it857), not a
        // bare colon-joined string -- see `manifest_reports_handler_
        // triggers_and_wires_as_structured_objects` for full coverage.
        let trigger = c.get("handlers").and_then(|h| h.index(0)).and_then(|h| h.get("trigger"));
        assert_eq!(trigger.and_then(|t| t.get("kind")).and_then(|k| k.str()), Some("port"));
        assert_eq!(trigger.and_then(|t| t.get("port")).and_then(|p| p.str()), Some("click"));
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
    /// choice like `state`'s continued exclusion of `init` (still true after
    /// PR-it778 added `state`'s own `type` field: `init` is an arbitrary
    /// expression, not simple manifest data, unlike `SuperviseDecl`/
    /// `Trigger` which are both small, fully-serializable structures --
    /// `type`, unlike `init`, IS exactly that kind of simple structure).
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
        // `trigger` is a structured `{"kind":...}` object (PR-it857), not a
        // bare colon-joined string -- see `manifest_reports_handler_triggers_
        // and_wires_as_structured_objects` below for the full regression
        // coverage of that fix; this test only asserts the COUNT here, which
        // predates and is orthogonal to the trigger-shape fix.
        let kinds: Vec<&str> = match parent.get("handlers") {
            Some(crate::lsp::Json::Arr(hs)) => hs
                .iter()
                .filter_map(|h| h.get("trigger"))
                .filter_map(|t| t.get("kind"))
                .filter_map(|k| k.str())
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(kinds, vec!["start", "stop", "every", "after"]);

        // a component with neither must still emit empty arrays, not omit the keys.
        let child = v.get("components").and_then(|c| c.index(0)).expect("component 0 (Child)");
        assert_eq!(arr_len(child.get("supervises")), Some(0));
        assert_eq!(arr_len(child.get("handlers")), Some(0));
    }

    /// A REAL schema-consistency bug found+fixed (production-hardening
    /// PR-it778, an Explore survey finding, agentId ad3c3f6ee2f0cd891,
    /// independently re-verified live before implementing): `state` was
    /// emitted as a bare array of NAME STRINGS (`["count"]`) while `ports`/
    /// `props` are arrays of structured objects carrying a `type` field --
    /// a visual-tool consumer got zero type info for state fields
    /// specifically. Fixed by adding `state`'s own `type` field (falling
    /// back to `""` when the state declaration has no explicit type
    /// annotation, since that's inferred at check-time and unavailable to
    /// this function). Deliberately does NOT add `init` -- see
    /// `manifest_reports_supervises_and_handlers`'s own doc comment for why
    /// that specific exclusion is a pre-existing, still-valid design choice,
    /// not re-litigated by this fix.
    #[test]
    fn manifest_reports_states_own_type() {
        let src = "component Counter778 {\n    intent \"c\"\n    \
                   state count: Int = 0\n    state untyped = \"x\"\n    on start { }\n}\n";
        let compiled = compile(src).expect("compiles");
        let json = super::manifest_json(&compiled.program);
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let comp = v.get("components").and_then(|c| c.index(0)).expect("component 0");
        let state = match comp.get("state") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("state must be an array of objects: {other:?}"),
        };
        assert_eq!(state.len(), 2);
        assert_eq!(state[0].get("name").and_then(|n| n.str()), Some("count"));
        assert_eq!(state[0].get("type").and_then(|t| t.str()), Some("Int"), "an explicitly-typed state field reports its type");
        assert_eq!(state[1].get("name").and_then(|n| n.str()), Some("untyped"));
        assert_eq!(
            state[1].get("type").and_then(|t| t.str()),
            Some(""),
            "an untyped state field (type inferred at check-time) falls back to empty, not omitted or a crash"
        );
        // `init` is deliberately NOT present -- see this test's own doc comment.
        assert!(state[0].get("init").is_none(), "init is deliberately excluded, not just empty");
    }

    /// A REAL schema-consistency bug found+fixed (production-hardening
    /// PR-it856, an Explore survey finding, independently re-verified live
    /// before implementing): `exposes[].params` was a bare array of
    /// pre-formatted strings (`["n: Int", "tag: Str"]`) instead of an array
    /// of structured `{"name":..., "type":...}` objects like `ports`/
    /// `props`/`state` -- the EXACT gap this same sweep already fixed for
    /// `state` (see `manifest_reports_states_own_type` above), missed for
    /// this sibling field.
    #[test]
    fn manifest_reports_exposes_params_as_structured_objects() {
        let src = "component Counter856 {\n    intent \"c\"\n    \
                   expose fun add(n: Int, tag: Str) -> Int { n }\n    \
                   expose fun noop() -> Unit { }\n}\n";
        let compiled = compile(src).expect("compiles");
        let json = super::manifest_json(&compiled.program);
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let comp = v.get("components").and_then(|c| c.index(0)).expect("component 0");
        let exposes = match comp.get("exposes") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("exposes must be an array of objects: {other:?}"),
        };
        assert_eq!(exposes.len(), 2);
        let params = match exposes[0].get("params") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("params must be an array of objects, not pre-formatted strings: {other:?}"),
        };
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].get("name").and_then(|n| n.str()), Some("n"));
        assert_eq!(params[0].get("type").and_then(|t| t.str()), Some("Int"));
        assert_eq!(params[1].get("name").and_then(|n| n.str()), Some("tag"));
        assert_eq!(params[1].get("type").and_then(|t| t.str()), Some("Str"));
        // a zero-param expose still reports a valid, empty (not omitted/null) array.
        let noop_params = match exposes[1].get("params") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("params must be an array even with zero params: {other:?}"),
        };
        assert!(noop_params.is_empty());
    }

    /// A REAL schema-consistency bug found+fixed (production-hardening
    /// PR-it857, the SAME sweep that just fixed `exposes[].params` at
    /// it856 -- found by re-reading `manifest_json`'s remaining fields for
    /// any OTHER sibling instance of the identical shape): `wires[].from`/
    /// `.to` were each a single dot-joined string (`"feed.numbers"`)
    /// instead of a structured `{"component":..., "port":...}` object, and
    /// `handlers[].trigger` was a single colon-joined string
    /// (`"port:input"`, `"every:5000"`) instead of a structured
    /// `{"kind":..., ...}` object -- both forced a consumer to re-parse a
    /// compound string instead of reading direct JSON fields, the same gap
    /// already fixed for `state`/`exposes.params`.
    #[test]
    fn manifest_reports_handler_triggers_and_wires_as_structured_objects() {
        let src = "component Child857 {\n    intent \"c\"\n    in input: Int\n    out output: Int\n}\n\
                   component Parent857 {\n    intent \"p\"\n    in tick: Int\n    \
                   let a = Child857()\n    let b = Child857()\n    \
                   on tick(n) { }\n    on start { }\n    on stop { }\n    \
                   on every 5s { }\n    on after 2s { }\n    wire a.output -> b.input\n}\n";
        let compiled = compile(src).expect("compiles");
        let json = super::manifest_json(&compiled.program);
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let parent = v.get("components").and_then(|c| c.index(1)).expect("component 1 (Parent857)");

        let handlers = match parent.get("handlers") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("handlers must be an array: {other:?}"),
        };
        assert_eq!(handlers.len(), 5);
        let trigger = |i: usize| handlers[i].get("trigger").expect("trigger present");
        assert_eq!(trigger(0).get("kind").and_then(|k| k.str()), Some("port"));
        assert_eq!(trigger(0).get("port").and_then(|p| p.str()), Some("tick"));
        assert_eq!(trigger(1).get("kind").and_then(|k| k.str()), Some("start"));
        assert!(trigger(1).get("port").is_none(), "start has no port field, not an empty/null one");
        assert_eq!(trigger(2).get("kind").and_then(|k| k.str()), Some("stop"));
        assert_eq!(trigger(3).get("kind").and_then(|k| k.str()), Some("every"));
        assert_eq!(trigger(3).get("ms").and_then(|m| m.as_usize()), Some(5000));
        assert_eq!(trigger(4).get("kind").and_then(|k| k.str()), Some("after"));
        assert_eq!(trigger(4).get("ms").and_then(|m| m.as_usize()), Some(2000));

        let wires = match parent.get("wires") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("wires must be an array: {other:?}"),
        };
        assert_eq!(wires.len(), 1);
        let from = wires[0].get("from").expect("from present");
        assert_eq!(from.get("component").and_then(|c| c.str()), Some("a"));
        assert_eq!(from.get("port").and_then(|p| p.str()), Some("output"));
        let to = wires[0].get("to").expect("to present");
        assert_eq!(to.get("component").and_then(|c| c.str()), Some("b"));
        assert_eq!(to.get("port").and_then(|p| p.str()), Some("input"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it780, the first half
    /// of a late-delivered Explore survey finding, agentId aaed1d00a40c9e7b6,
    /// independently re-verified live before implementing): `manifest_json`
    /// walks `compiled.program`, which has already been through
    /// `resolve::isolate()`'s load-time name mangling -- so a dependency's
    /// own component came out as `"name":"dep$Widget"`, not `"Widget"`, and a
    /// prop typed with one of that dependency's OWN types leaked the same
    /// way (`"type":"dep$Color"` instead of `"Color"`, via `fmt::ty_str`,
    /// which had never demangled at all). Confirmed live before this fix.
    /// Fixed by demangling `name`/`fulfills`/`children[].component` at their
    /// call sites in this function, plus `ty_str` itself (so `ports`/`props`/
    /// `state`/`exposes` types are covered for every caller, not just this
    /// one). Uses `loader.rs`'s established two-package temp-dir convention
    /// since mangling only exists once a real dependency graph is loaded.
    #[test]
    fn manifest_demangles_a_dependencys_component_name_and_type_references() {
        let base = std::env::temp_dir().join(format!("kupl-manifest-demangle-test-{}", std::process::id()));
        let dep = base.join("dep");
        let app = base.join("app");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep.join("kupl.toml"), "[project]\nname = \"dep\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(
            dep.join("main.kupl"),
            "pub type Color = Red | Green | Blue\n\n\
             pub component Widget {\n    intent \"a widget\"\n    prop shade: Color\n\
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
             let w = dep.Widget(shade: dep.Red)\n    print(w)\n}\n",
        )
        .unwrap();

        let (program, _map) = crate::loader::load(app.join("main.kupl").to_str().unwrap())
            .map_err(|(d, _)| format!("{d:?}"))
            .expect("app loads with its dep dependency");
        let json = super::manifest_json(&program);
        assert!(
            !json.contains('$'),
            "no `pkg$Name` mangling artifact should ever reach the manifest's JSON output: {json}"
        );
        let v = crate::lsp::parse_json(&json).expect("manifest must be valid JSON");
        let comp = v.get("components").and_then(|c| c.index(0)).expect("component 0 (Widget)");
        assert_eq!(comp.get("name").and_then(|n| n.str()), Some("Widget"));
        let props = match comp.get("props") {
            Some(crate::lsp::Json::Arr(a)) => a.clone(),
            other => panic!("props must be an array: {other:?}"),
        };
        assert_eq!(props[0].get("name").and_then(|n| n.str()), Some("shade"));
        assert_eq!(
            props[0].get("type").and_then(|t| t.str()),
            Some("Color"),
            "a prop typed with the dependency's OWN type must report the bare name, not `dep$Color`"
        );

        let _ = std::fs::remove_dir_all(&base);
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

    /// A REAL usability bug found+fixed (production-hardening PR-it783, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `native`/`build`/`bundle` already reject a compiled
    /// `.kx` file cleanly (PR-it782), but `kupl test`/`check`/`context`/
    /// `manifest` had no equivalent guard -- all four route through
    /// `loader::load`/`load_with` (via `load_compile` or, for `check`,
    /// directly), which had no `.kx`-extension check, so any of them fed a
    /// `.kx` file tried to LEX the raw bytecode as source, one `K0001:
    /// unexpected character` diagnostic per non-token byte. Confirmed live
    /// before this fix: `kupl test sample.kx` printed 1290 lines of
    /// garbage; identical for `check`/`manifest`/`context`. Fixed ONCE in
    /// the shared `loader::load_with`, not four separate call sites.
    #[test]
    fn kx_input_is_rejected_cleanly_by_test_check_context_and_manifest() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-kx-input-guard-shared-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sample.kupl");
        std::fs::write(&src, "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n").unwrap();
        let kx = dir.join("sample.kx");
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let built = run(&["build", src.to_str().unwrap(), "-o", kx.to_str().unwrap()]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");

        for args in [
            vec!["test", kx.to_str().unwrap()],
            vec!["check", kx.to_str().unwrap()],
            vec!["manifest", kx.to_str().unwrap()],
            vec!["context", kx.to_str().unwrap(), "add"],
        ] {
            let out = run(&args);
            assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                stderr.lines().count(),
                1,
                "{args:?} on a .kx file must report ONE clean line, not a lexer-error dump: {stderr:?}"
            );
            assert!(stderr.contains("already compiled bytecode"), "{args:?}: {stderr:?}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL text-consistency bug found+fixed (production-hardening
    /// PR-it783, the same survey's finding 2, independently re-verified
    /// live before implementing): a PLAIN (non-`forall`) failed `expect`'s
    /// "was not satisfied" text was built by re-slicing raw SOURCE with a
    /// span that (per `parser.rs`) includes the `expect` KEYWORD itself, so
    /// it read `` `expect 1 == 2` was not satisfied ``, while the
    /// STRUCTURALLY IDENTICAL failure inside a `forall` (built from the
    /// already-clean `fmt::expr_str`-rendered condition, PR-it771) read
    /// just `` `1 == 2` was not satisfied `` -- PR-it771's own doc comment
    /// explicitly says the two are MEANT to match. Confirmed live before
    /// this fix, exactly as here. Fixed by reusing the already-clean
    /// `msg`-derived text for the plain case too (law and contract-law
    /// loops), and by rendering via `fmt::expr_str` instead of source-
    /// slicing for a component example's own `expect` (which has no `msg`
    /// to reuse). This test ALSO guards the regression caught mid-
    /// implementation: an early draft made this fix ALSO wrap a `forall`
    /// counterexample's OWN already-complete message as a genuine panic
    /// (spurious `error[K0900]`) -- asserts stderr stays clean for the
    /// `forall` case specifically.
    #[test]
    fn plain_and_forall_expect_failures_render_the_condition_identically() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-expect-text-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("expecttext.kupl");
        std::fs::write(
            &file,
            "law \"plain\" {\n    expect 1 == 2\n}\n\n\
             law \"quantified\" {\n    forall x: Int {\n        expect x == 999999\n    }\n}\n",
        )
        .unwrap();
        let out = std::process::Command::new(&bin)
            .args(["test", file.to_str().unwrap()])
            .output()
            .expect("kupl runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("`1 == 2` was not satisfied"),
            "the plain law must not leak the `expect` keyword into the quoted condition: {stdout:?}"
        );
        assert!(!stdout.contains("`expect 1 == 2`"), "the `expect` keyword must not leak: {stdout:?}");
        assert!(
            stdout.contains("`x == 999999` was not satisfied"),
            "the forall-wrapped condition must still render cleanly too: {stdout:?}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.is_empty(),
            "a forall property-test failure (an EXPECTED test outcome, not a genuine \
             interpreter panic) must not produce a report_panic_map diagnostic: {stderr:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL text-consistency bug found+fixed (production-hardening
    /// PR-it783, the same finding as `plain_and_forall_expect_failures_...`
    /// above, but at a DIFFERENT site with no shared code path: a component
    /// `example`'s own failed `expect` step (`run_example`'s
    /// `ExampleStep::Expect` handling) has NO panic `msg` to reuse (unlike
    /// a `law`'s `expect`, which raises a `Flow::Panic`) -- it directly
    /// re-sliced raw source with a keyword-inclusive span instead, the
    /// SAME underlying bug as the law loops but requiring a SEPARATE fix
    /// (render via `fmt::expr_str` instead). Caught mid-implementation as a
    /// GENUINE COVERAGE GAP: the sibling law-focused test above still
    /// PASSED when `run_example`'s own fix was reverted in isolation (it
    /// only exercises `law`/`forall`, never a component `example`), which
    /// is exactly this campaign's own "a revert-and-verify test that
    /// unexpectedly still passes is a MANDATORY red flag" rule -- added
    /// THIS test specifically to close that gap, confirmed it fails when
    /// `run_example`'s fix is reverted, restored the fix.
    #[test]
    fn a_failed_expect_inside_a_component_example_also_renders_without_the_keyword() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-example-expect-text-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exampleexpect.kupl");
        std::fs::write(
            &file,
            "component Widget {\n    intent \"w\"\n    in click: Int\n    out ok: Int\n    \
             on click(n) { emit ok(n) }\n    example {\n        send click(5)\n        \
             expect ok == 999999\n    }\n}\n",
        )
        .unwrap();
        let out = std::process::Command::new(&bin)
            .args(["test", file.to_str().unwrap()])
            .output()
            .expect("kupl runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("`ok == 999999` was not satisfied"),
            "a component example's failed expect must not leak the `expect` keyword: {stdout:?}"
        );
        assert!(!stdout.contains("`expect ok == 999999`"), "the `expect` keyword must not leak: {stdout:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL reporting-consistency bug found+fixed (production-hardening
    /// PR-it783, the same survey's finding 3, independently re-verified
    /// live before implementing): a genuine runtime panic (e.g. division by
    /// zero) inside a top-level `law` or a contract `law` produced a stdout
    /// FAIL line but NOTHING on stderr, while the STRUCTURALLY IDENTICAL
    /// panic inside a component `example` already got a full `error[K0900]`
    /// source-pointing diagnostic via `report_panic_map` -- a purely
    /// accidental omission (only one of `kupl test`'s three symmetric
    /// test-item categories was wired up). Confirmed live before this fix:
    /// a law dividing by zero left stderr completely empty. Now all three
    /// categories call the same `report_panic_map` for a genuine panic.
    #[test]
    fn law_and_contract_law_panics_get_the_same_stderr_diagnostic_as_component_examples() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-law-panic-diag-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let law_file = dir.join("lawpanic.kupl");
        std::fs::write(&law_file, "fun bad(n: Int) -> Int {\n    n / 0\n}\n\nlaw \"boom\" {\n    expect bad(5) == 0\n}\n")
            .unwrap();
        let law_out = std::process::Command::new(&bin)
            .args(["test", law_file.to_str().unwrap()])
            .output()
            .expect("kupl runs");
        let law_stderr = String::from_utf8_lossy(&law_out.stderr);
        assert!(law_stderr.contains("K0900"), "a genuine panic in a law must get a report_panic_map diagnostic: {law_stderr:?}");
        let law_stdout = String::from_utf8_lossy(&law_out.stdout);
        assert!(law_stdout.contains("panic: division by zero"), "{law_stdout:?}");

        let contract_file = dir.join("contractlaw.kupl");
        std::fs::write(
            &contract_file,
            "contract Doubler {\n    intent \"d\"\n    expose fun value() -> Int\n    \
             law \"doubled\" {\n        expect value() / 0 == 999999\n    }\n}\n\
             component Impl fulfills Doubler {\n    intent \"impl\"\n    \
             expose fun value() -> Int {\n        21\n    }\n}\n",
        )
        .unwrap();
        let contract_out = std::process::Command::new(&bin)
            .args(["test", contract_file.to_str().unwrap()])
            .output()
            .expect("kupl runs");
        let contract_stderr = String::from_utf8_lossy(&contract_out.stderr);
        assert!(
            contract_stderr.contains("K0900"),
            "a genuine panic in a contract law must ALSO get a report_panic_map diagnostic: {contract_stderr:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL quality bug in `prop::shrink` (PR-it694): the generic `Value::Ctor` arm
    /// only shrunk fields IN PLACE, never trying a smaller SIBLING variant of the same
    /// recursive type -- so shrinking a self-referential ADT (a `Tree`, a linked-list-
    /// as-ADT, an expression AST, ...) could get permanently stuck at whatever depth the
    /// first failing generated case happened to land on, reporting a needlessly deep,
    /// non-minimal counterexample instead of the genuinely minimal one. Confirmed live
    /// before this fix: `type Chain = Base | Rec1(child: Chain) | Rec2(child: Chain) |
    /// Rec3(child: Chain)` with `forall c: Chain { expect false }` (unconditionally
    /// false, so the true minimal counterexample is simply `Base`) reported
    /// `Rec2(Rec3(Base))` instead. `forall`/`shrink` is interp-only (`kupl test` always
    /// runs laws via `Interp`, never the KVM or native) -- sweeps not applicable, stated
    /// explicitly.
    #[test]
    fn forall_shrinks_a_recursive_adt_counterexample_to_its_minimal_sibling_variant() {
        use crate::ast::Item;
        use crate::interp::{Flow, Interp, ProgramDb};

        let law_panic_msg = |src: &str| -> String {
            let compiled = super::compile(src).expect("compiles");
            let Item::Law(law) = compiled
                .program
                .items
                .iter()
                .find(|i| matches!(i, Item::Law(_)))
                .expect("has a law")
            else {
                unreachable!()
            };
            let db = ProgramDb::build(&compiled.program, &compiled.checked);
            let mut interp = Interp::new(db);
            let env = interp.globals.child();
            match interp.exec_block(&law.body, &env) {
                Err(Flow::Panic { msg, .. }) => msg,
                Ok(_) => panic!("expected the law to fail, but it passed"),
                Err(_) => panic!("expected a panic, got other unexpected control flow"),
            }
        };
        // A single-recursive-field enum: the minimal counterexample is the nullary
        // variant `Base`, reachable only by promoting a same-typed field up a level.
        let chain = "type Chain = Base | Rec1(child: Chain) | Rec2(child: Chain) | Rec3(child: Chain)\n\
                     law \"x\" { forall c: Chain { expect false } }\n";
        // PRODUCTION-HARDENING (PR-it771): the trailing "(`false` was not
        // satisfied)" detail is new -- run_forall used to discard the specific
        // failing `expect` condition entirely for this message shape; see that
        // fix's own comment for why.
        assert_eq!(law_panic_msg(chain), "property failed for c = Base (`false` was not satisfied)");
        // A two-recursive-field (binary tree) shape shrinks to its own nullary variant
        // too -- promotion must work regardless of WHICH field position holds the
        // same-typed value, and regardless of how many recursive fields a variant has.
        let tree = "type Tree = Leaf | Node(l: Tree, r: Tree)\n\
                    law \"x\" { forall t: Tree { expect false } }\n";
        assert_eq!(law_panic_msg(tree), "property failed for t = Leaf (`false` was not satisfied)");
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

    /// A coverage-closing test (production-hardening PR-it705, no bug found --
    /// a seventeenth research-subagent dispatch investigated `example`-block
    /// execution semantics end-to-end (`Send`/`Expect`/`Advance`) and
    /// confirmed every mechanism routes through the SAME shared interp.rs
    /// primitives a real running program uses -- `interp.send`, `interp.
    /// advance`, `interp.eval` -- never a parallel reimplementation. But
    /// `run_example`, one of run.rs's core user-facing mechanisms, had ZERO
    /// test coverage in this file's own `mod tests` before this (confirmed:
    /// zero "example" hits in any `#[test]` fn name here). Worse, NOTHING in
    /// the whole test suite actually RUNS `examples/timers.kupl`'s own
    /// `example` blocks through `kupl test` -- `fmt.rs` sweeps `examples/*.
    /// kupl` for formatting idempotence only, never execution -- so the
    /// canonical, human-reviewed `Ticker`/`Delayed` fixtures documenting
    /// multi-fire `advance` semantics were unverified by CI. This test
    /// reuses `Ticker` verbatim from `examples/timers.kupl` and locks in the
    /// single most fragile property confirmed live: `advance` correctly
    /// fires a REPEATING timer MULTIPLE times within one `advance` step
    /// (5s and 10s within `advance 12s`), not just once.
    #[test]
    fn example_advance_fires_a_repeating_timer_multiple_times_in_one_step() {
        let dir = std::env::temp_dir().join(format!("kupl-example-advance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "component Ticker {\n    intent \"Emits a rising tick count on a recurring timer.\"\n\n    out tick: Int\n    state n: Int = 0\n\n    on every 5s {\n        n += 1\n        emit tick(n)\n    }\n\n    example {\n        advance 12s\n        expect tick == 2\n        advance 3s\n        expect tick == 3\n    }\n}\nfun main() {}\n";
        let path = dir.join("advance.kupl");
        std::fs::write(&path, src).unwrap();
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let out = std::process::Command::new(&bin)
            .args(["test", path.to_str().unwrap()])
            .output()
            .expect("kupl test runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("1 passed, 0 failed, 0 skipped"),
            "advance 12s must fire `on every 5s` exactly twice (at 5s, 10s), then advance 3s once more \
             (at 15s), satisfying both `expect tick == 2` and `expect tick == 3`: {stdout:?}"
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

    /// A REAL usability gap found+fixed (production-hardening PR-it780, the
    /// second half of a late-delivered Explore survey finding, agentId
    /// aaed1d00a40c9e7b6, independently re-verified live before
    /// implementing): a dependency's item is stored under its
    /// `isolate()`-mangled name, so `kupl context app.kupl Widget` used to
    /// fail with "no item named `Widget`" even though `kupl manifest`
    /// reports the SAME component (post-PR-it780's OTHER fix, above) as
    /// plain `Widget` -- forcing a caller to already know the internal
    /// `dep$Widget` mangling syntax, never surfaced anywhere else. Fixed by
    /// falling back to a demangled-name match when an exact match misses.
    /// Subprocess test (matching this file's own established pattern,
    /// PR-it777) since the point is to inspect PRINTED content, not just the
    /// exit code.
    #[test]
    fn emit_context_resolves_a_dependencys_item_by_its_demangled_name() {
        let base = std::env::temp_dir().join(format!("kupl-ctx-demangle-test-{}", std::process::id()));
        let dep = base.join("dep");
        let app = base.join("app");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep.join("kupl.toml"), "[project]\nname = \"dep\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(
            dep.join("main.kupl"),
            "pub component Widget {\n    intent \"a widget\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use dep\n\nfun main() uses io {\n    let w = dep.Widget()\n    print(w)\n}\n",
        )
        .unwrap();
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            let _ = std::fs::remove_dir_all(&base);
            return; // no debug binary built yet -- nothing to test
        }
        let main = app.join("main.kupl");
        let out = std::process::Command::new(&bin)
            .args(["context", main.to_str().unwrap(), "Widget"])
            .output()
            .expect("kupl context runs");
        assert!(out.status.success(), "the bare demangled name must resolve: {:?}", out);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("component Widget"), "must print the resolved component: {stdout:?}");
        assert!(!stdout.contains("dep$Widget"), "the mangled name must never leak into the output: {stdout:?}");

        // the mangled form itself must keep working too (an exact match wins
        // over the demangled fallback).
        let exact = std::process::Command::new(&bin)
            .args(["context", main.to_str().unwrap(), "dep$Widget"])
            .output()
            .expect("kupl context runs");
        assert!(exact.status.success(), "the exact mangled name must still resolve: {:?}", exact);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two dependencies that both declare a same-named public item demangle
    /// to the SAME bare name -- resolving `kupl context app.kupl Widget`
    /// arbitrarily between them would be a real, silent correctness trap for
    /// an LLM caller (production-hardening PR-it780, designed alongside the
    /// demangled-lookup fix above, per the survey's own suggested fix
    /// direction). Must report an explicit ambiguity error instead.
    #[test]
    fn emit_context_reports_ambiguity_when_two_dependencies_demangle_to_the_same_name() {
        let base = std::env::temp_dir().join(format!("kupl-ctx-ambiguous-test-{}", std::process::id()));
        let dep_a = base.join("dep_a");
        let dep_b = base.join("dep_b");
        let app = base.join("app");
        std::fs::create_dir_all(&dep_a).unwrap();
        std::fs::create_dir_all(&dep_b).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dep_a.join("kupl.toml"), "[project]\nname = \"dep_a\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(dep_a.join("main.kupl"), "pub component Widget {\n    intent \"a\"\n}\n").unwrap();
        std::fs::write(dep_b.join("kupl.toml"), "[project]\nname = \"dep_b\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(dep_b.join("main.kupl"), "pub component Widget {\n    intent \"b\"\n}\n").unwrap();
        std::fs::write(
            app.join("kupl.toml"),
            "[project]\nname = \"app\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\ndep_a = { path = \"../dep_a\" }\ndep_b = { path = \"../dep_b\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.kupl"),
            "use dep_a\nuse dep_b\n\nfun main() uses io {\n    \
             let a = dep_a.Widget()\n    let b = dep_b.Widget()\n    print(a)\n    print(b)\n}\n",
        )
        .unwrap();
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            let _ = std::fs::remove_dir_all(&base);
            return; // no debug binary built yet -- nothing to test
        }
        let main = app.join("main.kupl");
        let out = std::process::Command::new(&bin)
            .args(["context", main.to_str().unwrap(), "Widget"])
            .output()
            .expect("kupl context runs");
        assert!(!out.status.success(), "an ambiguous bare name must be a clean error, not an arbitrary pick");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("ambiguous"), "must explain the ambiguity, not just say \"no item named\": {stderr:?}");
        assert!(
            stderr.contains("dep_a$Widget") && stderr.contains("dep_b$Widget"),
            "must list both candidates so the caller can pick the exact mangled form: {stderr:?}"
        );

        // each exact mangled form must still resolve unambiguously.
        let a = std::process::Command::new(&bin)
            .args(["context", main.to_str().unwrap(), "dep_a$Widget"])
            .output()
            .expect("kupl context runs");
        assert!(a.status.success(), "the exact mangled name must resolve despite the ambiguous bare name: {:?}", a);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL bug found+fixed (production-hardening PR-it777, an Explore
    /// survey finding, agentId ad3c3f6ee2f0cd891, independently re-verified
    /// live before implementing): `collect_expr_names` (this file) is driven
    /// by `effects::walk_block`/`walk_expr`, which visits a `Match`'s
    /// `scrutinee` and each arm's `guard`/`body` but NEVER an arm's
    /// `pattern` -- so a function that discriminates a type ONLY via
    /// `match ... { Circle(_) => ..., Square(_) => ... }`, with that type
    /// never appearing in its own signature, had the type SILENTLY OMITTED
    /// from `kupl context`'s emitted dependency closure. Confirmed live
    /// before fixing: `kupl context file.kupl classify` included
    /// `fun make_shape` (an ordinary call) but completely omitted
    /// `type Shape`, even though the type is the only thing the match
    /// structurally depends on. `emit_context` prints directly to stdout
    /// (no return value to inspect in-process) -- this is a real subprocess
    /// test, spawning `target/debug/kupl context` end-to-end, matching this
    /// file's OWN established pattern and its sibling test's own comment
    /// ("closure correctness ... is exercised end-to-end via the CLI").
    #[test]
    fn emit_context_includes_a_type_referenced_only_via_a_match_pattern() {
        let dir = std::env::temp_dir().join(format!("kupl-ctx-pattern-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.kupl");
        std::fs::write(
            &file,
            "type Shape = Circle(r: Int) | Square(s: Int)\n\n\
             fun make_shape() -> Shape {\n    Circle(r: 5)\n}\n\n\
             fun classify() -> Str {\n    \
             let s = make_shape()\n    \
             match s {\n        \
             Circle(_) => \"circle\"\n        \
             Square(_) => \"square\"\n    \
             }\n\
             }\n",
        )
        .unwrap();
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no debug binary built yet -- nothing to test
        }
        let out = std::process::Command::new(&bin)
            .args(["context", file.to_str().unwrap(), "classify"])
            .output()
            .expect("kupl context runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("type Shape"),
            "a type referenced ONLY via a match pattern (not the function's own signature) must still \
             appear in the dependency closure: {stdout:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL usability gap found+fixed (production-hardening PR-it858, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): a child instance's component TYPE (`child.component`)
    /// was noted, but its constructor ARGUMENTS (`child.args[].value`) were
    /// NEVER walked -- so a function referenced ONLY inside a
    /// child-instantiation argument (e.g. `Holder(box: make_box())`) never
    /// appeared in `kupl context`'s "direct dependencies", even though the
    /// target item's own printed source visibly calls it. Confirmed live
    /// BEFORE this fix (both named and positional argument forms) via the
    /// real `kupl context` CLI. Same subprocess-test pattern as this file's
    /// own sibling test above (this command prints directly to stdout, no
    /// in-process return value to inspect).
    #[test]
    fn emit_context_includes_a_function_referenced_only_in_a_child_instantiation_argument() {
        let dir = std::env::temp_dir().join(format!("kupl-ctx-childargs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.kupl");
        std::fs::write(
            &file,
            "fun compute_named() -> Int { 42 }\n\
             fun compute_positional() -> Int { 7 }\n\
             component Holder {\n    intent \"wraps a value\"\n    prop n: Int\n}\n\
             component Main {\n    intent \"demo\"\n    \
             let named = Holder(n: compute_named())\n    \
             let positional = Holder(compute_positional())\n\
             }\n",
        )
        .unwrap();
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no debug binary built yet -- nothing to test
        }
        let out = std::process::Command::new(&bin)
            .args(["context", file.to_str().unwrap(), "Main"])
            .output()
            .expect("kupl context runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("fun compute_named"),
            "a function referenced ONLY in a NAMED child-instantiation argument must appear \
             in the dependency closure: {stdout:?}"
        );
        assert!(
            stdout.contains("fun compute_positional"),
            "a function referenced ONLY in a POSITIONAL child-instantiation argument must appear \
             in the dependency closure: {stdout:?}"
        );
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
