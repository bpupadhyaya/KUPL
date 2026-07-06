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

fn print_diags_map(diags: &[Diag], map: &crate::loader::SourceMap) {
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
    let (program, map) = match crate::loader::load(path) {
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
    if deps.is_empty() {
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
    let lock_path = std::path::Path::new(path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("kupl.lock");
    match std::fs::write(&lock_path, crate::loader::lock_text(&deps)) {
        Ok(()) => {
            println!("wrote {} ({} dependencies)", lock_path.display(), deps.len());
            0
        }
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", lock_path.display());
            1
        }
    }
}

pub fn emit_manifest(path: &str) -> i32 {
    use crate::diag::json_escape as esc;
    let Ok((compiled, _map)) = load_compile(path) else { return 1 };
    let mut out = String::from("{\"components\":[");
    let mut first = true;
    for item in &compiled.program.items {
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
        out.push_str(&format!(",\"examples\":{}}}", c.examples.len()));
    }
    out.push_str("]}");
    println!("{out}");
    0
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
    use super::sort_diags;
    use crate::diag::{Diag, Span};

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
}
