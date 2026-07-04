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

/// Parse + check (types, then effects). Errors are returned; warnings ride
/// along on success.
pub fn compile(src: &str) -> Result<Compiled, Vec<Diag>> {
    let (program, mut diags) = parser::parse(src);
    let (checked, check_diags) = check::check(&program);
    diags.extend(check_diags);
    // Effects only make sense on a program that parsed; skip if already broken.
    if !diags.iter().any(|d| d.severity == Severity::Error) {
        diags.extend(crate::effects::check_effects(&program));
    }
    let (errors, warnings): (Vec<_>, Vec<_>) =
        diags.into_iter().partition(|d| d.severity == Severity::Error);
    if errors.is_empty() {
        Ok(Compiled { program, checked, warnings })
    } else {
        Err(errors)
    }
}

/// `kupl context <name>`: emit the item plus the source of everything it
/// directly references — the minimal dependency-closed context for an LLM.
pub fn emit_context(src: &str, file: &str, name: &str) -> i32 {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => {
            print_diags(&errors, src, file);
            return 1;
        }
    };
    let items: Vec<(&str, Span)> = compiled
        .program
        .items
        .iter()
        .map(|item| match item {
            Item::Fun(f) => (f.name.as_str(), f.span),
            Item::Type(t) => (t.name.as_str(), t.span),
            Item::Component(c) => (c.name.as_str(), c.span),
            Item::Contract(ct) => (ct.name.as_str(), ct.span),
        })
        .collect();
    let Some(target) = compiled.program.items.iter().find(|item| match item {
        Item::Fun(f) => f.name == name,
        Item::Type(t) => t.name == name,
        Item::Component(c) => c.name == name,
        Item::Contract(ct) => ct.name == name,
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

fn report_panic(msg: &str, span: Span, src: &str, file: &str) {
    let d = Diag::error("K0900", format!("panic: {msg}"), span);
    eprint!("{}", diag::render(&d, src, file));
}

/// `kupl run`: execute the first `app` (or a `fun main()` if there is no app).
pub fn run_program(src: &str, file: &str) -> i32 {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => {
            print_diags(&errors, src, file);
            return 1;
        }
    };
    print_diags(&compiled.warnings, src, file);

    let app = compiled.program.items.iter().find_map(|item| match item {
        Item::Component(c) if c.is_app => Some(c.name.clone()),
        _ => None,
    });
    let db = ProgramDb::build(&compiled.program, &compiled.checked);
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
            report_panic(&msg, span, src, file);
            101
        }
        Err(_) => 0,
    }
}

/// `kupl run --vm`: compile the functional core to KVM bytecode and execute
/// `fun main` on the register VM.
pub fn run_program_vm(src: &str, file: &str) -> i32 {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => {
            print_diags(&errors, src, file);
            return 1;
        }
    };
    print_diags(&compiled.warnings, src, file);
    let module = match crate::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => m,
        Err(diags) => {
            print_diags(&diags, src, file);
            return 1;
        }
    };
    if !module.funs.contains_key("main") {
        eprintln!("error: `kupl run --vm` needs a `fun main()` (components run on the interpreter for now)");
        return 2;
    }
    let mut vm = crate::vm::Vm::new(&module);
    match vm.call_named("main", vec![]) {
        Ok(_) => 0,
        Err(e) => {
            report_panic(&e.msg, e.span, src, file);
            101
        }
    }
}

/// `kupl dis`: disassemble the compiled module.
pub fn disassemble(src: &str, file: &str) -> i32 {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => {
            print_diags(&errors, src, file);
            return 1;
        }
    };
    match crate::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => {
            print!("{}", m.disassemble());
            0
        }
        Err(diags) => {
            print_diags(&diags, src, file);
            1
        }
    }
}

/// `kupl test`: run every `example` block of every component.
pub fn run_tests(src: &str, file: &str) -> i32 {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => {
            print_diags(&errors, src, file);
            return 1;
        }
    };
    print_diags(&compiled.warnings, src, file);

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

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
                    report_panic(&msg, span, src, file);
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
                        let detail = if msg == "expectation failed" {
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
        }
    }
    Ok(None)
}

fn snippet(src: &str, span: Span) -> String {
    let start = (span.start as usize).min(src.len());
    let end = (span.end as usize).min(src.len());
    src[start..end].trim().to_string()
}
