//! Behavioral protocol rules (`docs/design/AGENTS.md` §3, "Behavioral
//! rules: the design that unblocks them"): desugars an agent's own
//! `guards Name` exposed funs into plain `expect` checks at every
//! syntactic exit point, BEFORE type-checking proper -- so this needs
//! zero new interp/vm/cgen work (`expect` already compiles identically
//! on all four engines) and the checker sees ordinary, already-typed
//! KUPL code, not a new construct.
//!
//! Runs from the SAME pipeline position as `callargs::resolve_call_args`
//! (an AST-rewriting pre-check pass) -- deliberately NOT run by
//! `parser::parse` alone, so `kupl fmt`/the LSP still see the ORIGINAL
//! `guards Name` source shape, never the expanded form.
//!
//! ## The rewrite, precisely
//!
//! For a fun with `guards G1, G2, ...`, every syntactic exit point of its
//! body is wrapped so the exit VALUE is bound to `result` (with an
//! explicit `: Type` annotation matching each guard's own declared type,
//! so a genuine return-type mismatch surfaces as an ordinary type error
//! pointing at that annotation, not silently accepted or vaguely
//! reported deep inside the guard's own body) and each guard's own
//! `expect` statements run against it, before the original value is
//! re-yielded unchanged.
//!
//! An exit point is either:
//! 1. The IMPLICIT tail value of the fun's own top-level body (its last
//!    statement, if that's an expression -- Unit if the body doesn't end
//!    in one, or is empty).
//! 2. Every `return expr;` reachable from the body, at ANY nesting depth
//!    through `if`/`match`/`while`/`for`/`receive`/nested blocks -- but
//!    NOT through a `Lambda` body, which has its own separate return
//!    scope (`interp.rs::call_value`'s `Closure` branch independently
//!    catches `Flow::Return` at the closure's own call boundary, verified
//!    by reading it directly, not assumed).
//!
//! `return` is always a STATEMENT (`Stmt::Return`), never an expression,
//! so it can only be reached by walking `Block.stmts`; the only `Expr`
//! kinds that can contain a NESTED block (and thus a nested `return`) are
//! `If`, `Match` (via each arm's own body expression), `BlockExpr`,
//! `Receive` (via each arm's own body block), and `Lambda` (excluded).
//! Every other `Expr` kind is walked purely to reach one of THOSE kinds
//! several levels deeper (e.g. `f(if c { return 1 } else { 2 })`).

use std::collections::HashMap;

use crate::ast::*;
use crate::diag::Diag;

/// Rewrite every `guards`-bearing agent exposed fun in `program` in
/// place. Returns diagnostics for `guards` clauses that couldn't be
/// resolved (K1004: not on an agent's own exposed fun; K1005: unknown
/// guard name) -- an unresolved `guards` clause is left un-rewritten
/// (the fun's body is untouched), matching `resolve_call_args`'s own
/// "diagnose and skip, don't panic the rest of the pipeline" discipline.
pub fn desugar_guards(program: &mut Program) -> Vec<Diag> {
    let mut diags = Vec::new();

    // Owned (not borrowed) keys -- `program.items` is mutably borrowed
    // further down while this table is still in scope.
    let mut protocol_guards: HashMap<String, HashMap<String, GuardDecl>> = HashMap::new();
    for item in &program.items {
        if let Item::Protocol(p) = item {
            let mut m = HashMap::new();
            for g in &p.guards {
                m.insert(g.name.clone(), g.clone());
            }
            protocol_guards.insert(p.name.clone(), m);
        }
    }

    // `guards` used anywhere it's not allowed at all: a top-level `fun`,
    // any component's own private `fun`, or a PLAIN (non-agent)
    // component's `expose fun` -- K1004, mirroring K1003's own "parser
    // accepts broadly, checker narrows" shape for `follows`/`weight`.
    for item in &program.items {
        match item {
            Item::Fun(f) => report_guards_not_allowed(f, "a top-level `fun`", &mut diags),
            Item::Component(c) => {
                for f in &c.funs {
                    report_guards_not_allowed(f, "a component's own private `fun`", &mut diags);
                }
                if !c.is_agent {
                    for f in &c.exposes {
                        report_guards_not_allowed(f, "a plain `component`'s `expose fun`", &mut diags);
                    }
                }
            }
            _ => {}
        }
    }

    for item in &mut program.items {
        let Item::Component(c) = item else { continue };
        if !c.is_agent {
            continue;
        }
        // The union of every guard declared by any protocol this agent
        // follows -- a name collision across two followed protocols
        // (same guard name, different protocols) resolves to whichever
        // is encountered last; genuinely ambiguous `guards` usage across
        // colliding names is a real, deliberately out-of-scope edge case
        // for this v1 (documented, not silently mishandled: both
        // candidates are semantically about the SAME fun's own return
        // value, so applying either is still a real, if possibly
        // surprising, constraint -- not a soundness gap).
        let mut available: HashMap<String, GuardDecl> = HashMap::new();
        for pname in &c.follows {
            if let Some(gs) = protocol_guards.get(pname.as_str()) {
                for (gname, g) in gs {
                    available.insert(gname.clone(), g.clone());
                }
            }
        }
        for f in &mut c.exposes {
            if f.guards.is_empty() {
                continue;
            }
            let mut resolved = Vec::new();
            for (gname, gspan) in &f.guards {
                match available.get(gname.as_str()) {
                    Some(g) => resolved.push(g.clone()),
                    None => {
                        let hint = match crate::check::suggest(gname, available.keys().map(String::as_str)) {
                            Some(s) => format!(" -- did you mean `{s}`?"),
                            None => String::new(),
                        };
                        diags.push(Diag::error(
                            "K1005",
                            format!(
                                "agent `{}`'s `{}` guards unknown guard `{gname}`{hint} -- no protocol it follows declares one",
                                c.name, f.name
                            ),
                            *gspan,
                        ));
                    }
                }
            }
            if resolved.len() != f.guards.len() {
                continue; // at least one name failed to resolve -- diagnosed above, don't rewrite
            }
            apply_guards(f, &resolved);
        }
    }

    diags
}

fn report_guards_not_allowed(f: &FunDecl, what: &str, diags: &mut Vec<Diag>) {
    if let Some((_, span)) = f.guards.first() {
        diags.push(Diag::error(
            "K1004",
            format!("`guards` is only valid on an `agent`'s own `expose fun` -- {what} cannot use it"),
            *span,
        ));
    }
}

/// Wrap every syntactic exit point of `f`'s body with `guards`' own
/// checks -- see this module's own top-of-file doc comment for the exact
/// rewrite shape.
fn apply_guards(f: &mut FunDecl, guards: &[GuardDecl]) {
    rewrite_block_returns(&mut f.body, guards);
    match f.body.stmts.last() {
        Some(Stmt::Return(..)) => {
            // every exit through THIS statement is already wrapped by
            // `rewrite_block_returns` above -- no implicit-tail path exists
            // past an unconditional trailing `return`.
        }
        Some(Stmt::Expr(_)) => {
            if let Some(Stmt::Expr(e)) = f.body.stmts.last_mut() {
                let span = e.span;
                let taken = std::mem::replace(e, Expr { kind: ExprKind::Unit, span });
                *e = wrap_with_guards(taken, guards, span);
            }
        }
        _ => {
            // body ends in a non-expression statement (or is empty) --
            // the fun implicitly returns Unit; append a synthetic tail.
            let span = f.body.span;
            let unit = Expr { kind: ExprKind::Unit, span };
            f.body.stmts.push(Stmt::Expr(wrap_with_guards(unit, guards, span)));
        }
    }
}

/// `{ let result: Ty = <value>; <guard1 body stmts>; <guard2 body
/// stmts>; ...; result }` -- a NEW nested scope (`BlockExpr`), so a
/// `result` shadowing the ORIGINAL fun body's own outer scope (if any)
/// is harmless, ordinary shadowing, not a naming conflict. Each guard's
/// own `Type` is used for a SEPARATE `let result: Ty = ...` immediately
/// preceding that guard's own spliced-in body, so multiple guards with
/// DIFFERENT declared types each get their own precisely-typed binding
/// (a genuine mismatch against any ONE of them surfaces as an ordinary
/// type error at that specific `let`, not conflated across guards).
fn wrap_with_guards(value: Expr, guards: &[GuardDecl], span: crate::diag::Span) -> Expr {
    let mut stmts = Vec::with_capacity(1 + guards.len() * 2 + 1);
    stmts.push(Stmt::Let {
        name: "__guard_result".to_string(),
        ty: None,
        init: value,
        mutable: false,
        span,
    });
    for g in guards {
        stmts.push(Stmt::Let {
            name: "result".to_string(),
            ty: Some(g.ty.clone()),
            init: Expr { kind: ExprKind::Ident("__guard_result".to_string()), span },
            mutable: false,
            span,
        });
        for s in &g.body.stmts {
            stmts.push(s.clone());
        }
    }
    stmts.push(Stmt::Expr(Expr { kind: ExprKind::Ident("__guard_result".to_string()), span }));
    Expr { kind: ExprKind::BlockExpr(Block { stmts, span }), span }
}

fn rewrite_block_returns(block: &mut Block, guards: &[GuardDecl]) {
    for s in &mut block.stmts {
        rewrite_stmt_returns(s, guards);
    }
}

fn rewrite_stmt_returns(s: &mut Stmt, guards: &[GuardDecl]) {
    match s {
        Stmt::Let { init, .. } => rewrite_expr_returns(init, guards),
        Stmt::Assign { target, value, .. } => {
            rewrite_expr_returns(target, guards);
            rewrite_expr_returns(value, guards);
        }
        Stmt::Expr(e) => rewrite_expr_returns(e, guards),
        Stmt::Return(opt_e, span) => {
            if let Some(e) = opt_e {
                rewrite_expr_returns(e, guards);
            }
            let span = *span;
            let value = opt_e.take().unwrap_or(Expr { kind: ExprKind::Unit, span });
            *opt_e = Some(wrap_with_guards(value, guards, span));
        }
        Stmt::While { cond, body, .. } => {
            rewrite_expr_returns(cond, guards);
            rewrite_block_returns(body, guards);
        }
        Stmt::For { iter, body, .. } => {
            rewrite_expr_returns(iter, guards);
            rewrite_block_returns(body, guards);
        }
        Stmt::Emit { arg: Some(e), .. } => rewrite_expr_returns(e, guards),
        Stmt::Emit { arg: None, .. } => {}
        Stmt::Expect(e, _) => rewrite_expr_returns(e, guards),
        Stmt::Forall { body, .. } => rewrite_block_returns(body, guards),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn rewrite_expr_returns(e: &mut Expr, guards: &[GuardDecl]) {
    match &mut e.kind {
        // A `Lambda` has its OWN separate return-catching boundary
        // (`interp.rs::call_value`'s `Closure` branch) -- a `return`
        // inside it exits the LAMBDA, never the enclosing named fun, so
        // it must never be rewritten by an OUTER fun's own `guards`.
        ExprKind::Lambda { .. } => {}
        ExprKind::If { cond, then_block, else_block } => {
            rewrite_expr_returns(cond, guards);
            rewrite_block_returns(then_block, guards);
            if let Some(eb) = else_block {
                rewrite_expr_returns(eb, guards);
            }
        }
        ExprKind::BlockExpr(b) => rewrite_block_returns(b, guards),
        ExprKind::Match { scrutinee, arms } => {
            rewrite_expr_returns(scrutinee, guards);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_expr_returns(g, guards);
                }
                rewrite_expr_returns(&mut arm.body, guards);
            }
        }
        ExprKind::Receive { arms } => {
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_expr_returns(g, guards);
                }
                rewrite_block_returns(&mut arm.body, guards);
            }
        }
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(inner) = p {
                    rewrite_expr_returns(inner, guards);
                }
            }
        }
        ExprKind::List(xs) | ExprKind::Par(xs) => xs.iter_mut().for_each(|x| rewrite_expr_returns(x, guards)),
        ExprKind::Call { callee, args } => {
            rewrite_expr_returns(callee, guards);
            args.iter_mut().for_each(|a| rewrite_expr_returns(&mut a.value, guards));
        }
        ExprKind::MethodCall { recv, args, .. } => {
            rewrite_expr_returns(recv, guards);
            args.iter_mut().for_each(|a| rewrite_expr_returns(&mut a.value, guards));
        }
        ExprKind::Field { recv, .. } => rewrite_expr_returns(recv, guards),
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_expr_returns(lhs, guards);
            rewrite_expr_returns(rhs, guards);
        }
        ExprKind::Unary { operand, .. } => rewrite_expr_returns(operand, guards),
        ExprKind::Range { lo, hi, .. } => {
            rewrite_expr_returns(lo, guards);
            rewrite_expr_returns(hi, guards);
        }
        ExprKind::With { recv, updates } => {
            rewrite_expr_returns(recv, guards);
            for (_, v) in updates {
                rewrite_expr_returns(v, guards);
            }
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => rewrite_expr_returns(inner, guards),
        ExprKind::CallWithTimeout { call, .. } => rewrite_expr_returns(call, guards),
        // No sub-expressions, so no nested Block/Stmt reachable at all.
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Unit
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::SizedInt(..)
        | ExprKind::F32(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{Flow, Interp, ProgramDb};
    use crate::value::Value;

    /// Parses `src` (expected to declare exactly one `protocol` with
    /// exactly one `guard`, and a top-level `fun probe() -> T { .. }`),
    /// applies `apply_guards` DIRECTLY to `probe` -- bypassing the whole
    /// protocol/`follows`/agent ceremony `desugar_guards` itself normally
    /// requires, so each control-flow shape can be tested in isolation --
    /// then actually EXECUTES the rewritten `probe()` via the real
    /// interpreter (the oracle for correctness, not just an inspection of
    /// the rewritten AST's own text shape) and returns its outcome as
    /// `"ok(value)"` or `"panic: msg"`.
    fn apply_and_run(src: &str) -> String {
        let (mut program, diags) = crate::parser::parse(src);
        assert!(diags.is_empty(), "parse diags: {diags:?}");
        crate::run::inject_prelude(&mut program);

        let guard = program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Protocol(p) => p.guards.first().cloned(),
                _ => None,
            })
            .expect("fixture must declare a protocol with exactly one guard");

        let mut found = false;
        for item in &mut program.items {
            if let Item::Fun(f) = item {
                if f.name == "probe" {
                    apply_guards(f, std::slice::from_ref(&guard));
                    found = true;
                }
            }
        }
        assert!(found, "fixture must declare `fun probe`");

        let (checked, check_diags) = crate::check::check(&program);
        let hard_errors: Vec<&Diag> =
            check_diags.iter().filter(|d| d.severity == crate::diag::Severity::Error).collect();
        assert!(hard_errors.is_empty(), "rewritten program must still check clean: {hard_errors:?}\n{}", crate::fmt::format_program(&program));

        let db = ProgramDb::build(&program, &checked);
        let mut interp = Interp::new(db);
        let f = Value::Fun(std::rc::Rc::new("probe".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(v) => format!("ok({v})"),
            Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
            Err(_) => "control-flow error".into(),
        }
    }

    const PROTO: &str = "protocol P {\n    intent \"p\"\n    guard G: Int {\n        expect result < 10000\n    }\n}\n";

    /// The plain, un-nested tail-value exit point -- the case a naive
    /// "just wrap the last expression" implementation would ALSO get
    /// right, establishing the baseline before the harder cases below.
    #[test]
    fn tail_value_respecting_guard_runs_clean() {
        let src = format!("{PROTO}fun probe() -> Int {{\n    5000\n}}\n");
        assert_eq!(apply_and_run(&src), "ok(5000)");
    }

    #[test]
    fn tail_value_violating_guard_panics() {
        let src = format!("{PROTO}fun probe() -> Int {{\n    50000\n}}\n");
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    /// THE correctness-critical case this whole rewrite pass exists for:
    /// a naive "wrap only the tail expression" implementation would
    /// silently miss this early `return`, letting a genuinely-violating
    /// value escape unchecked.
    #[test]
    fn early_return_before_the_tail_is_also_checked() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    if true {{\n        return 50000\n    }}\n    5000\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    /// The companion positive control: when the early-return branch is
    /// NOT taken, the (also-checked, also-passing) tail value still runs
    /// clean -- confirms the rewrite doesn't spuriously trigger on the
    /// untaken branch.
    #[test]
    fn early_return_branch_not_taken_falls_through_to_a_checked_clean_tail() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    if false {{\n        return 50000\n    }}\n    5000\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "ok(5000)");
    }

    #[test]
    fn return_inside_a_while_loop_is_checked() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    var i = 0\n    while i < 3 {{\n        if i == 1 {{\n            return 50000\n        }}\n        i = i + 1\n    }}\n    5000\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    #[test]
    fn return_inside_a_for_loop_is_checked() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    for x in [1, 2, 3] {{\n        if x == 2 {{\n            return 50000\n        }}\n    }}\n    5000\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    #[test]
    fn return_inside_a_match_arm_is_checked() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    let x = 2\n    match x {{\n        1 => 100,\n        _ => {{\n            return 50000\n        }},\n    }}\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    /// A `return` nested several levels deep (match arm -> if -> return) --
    /// confirms the walker recurses through EVERY intermediate layer, not
    /// just one level of nesting.
    #[test]
    fn return_nested_inside_if_inside_match_arm_is_checked() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    let x = 2\n    match x {{\n        1 => 100,\n        _ => {{\n            if true {{\n                return 50000\n            }}\n            0\n        }},\n    }}\n}}\n"
        );
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    /// The tail value of an `if`/`match` used as the FUN'S OWN top-level
    /// tail (not inside an explicit `return`) is still a single exit
    /// point, checked once as a whole -- not per-branch.
    #[test]
    fn if_expression_as_the_funs_own_tail_is_checked_as_one_exit_point() {
        let violating = format!("{PROTO}fun probe() -> Int {{\n    if true {{ 50000 }} else {{ 0 }}\n}}\n");
        assert_eq!(apply_and_run(&violating), "panic: expectation failed: result < 10000");
        let respecting = format!("{PROTO}fun probe() -> Int {{\n    if true {{ 5000 }} else {{ 0 }}\n}}\n");
        assert_eq!(apply_and_run(&respecting), "ok(5000)");
    }

    /// A `return` INSIDE a lambda must NOT be treated as an exit point of
    /// the ENCLOSING fun -- `interp.rs::call_value`'s `Closure` branch
    /// independently catches `Flow::Return` at the closure's own call
    /// boundary, so wrapping it here would be both unnecessary and, if
    /// the lambda's own return type differs from the guard's `Type`,
    /// a spurious type error on code that never even reaches the guard.
    #[test]
    fn return_inside_a_lambda_is_not_touched_by_the_enclosing_funs_guard() {
        let src = format!(
            "{PROTO}fun probe() -> Int {{\n    let f = fn(x: Bool) {{\n        if x {{\n            return \"early\"\n        }}\n        \"late\"\n    }}\n    let _ = f(true)\n    5000\n}}\n"
        );
        // if the lambda's `return \"early\"` were wrongly rewritten against
        // `G: Int`, this would fail to TYPE-CHECK at all (Str vs Int) --
        // reaching a clean numeric result at all is itself the proof.
        assert_eq!(apply_and_run(&src), "ok(5000)");
    }

    /// A body that doesn't end in an expression (its own last statement
    /// is `Stmt::Let`) implicitly returns `Unit` -- the rewrite must
    /// still append a checked synthetic tail rather than silently
    /// skipping the guard entirely.
    #[test]
    fn body_with_no_trailing_expression_implicitly_returns_unit_and_is_still_checked() {
        let src = "protocol P {\n    intent \"p\"\n    guard G: Unit {\n        expect true\n    }\n}\nfun probe() -> Unit {\n    let x = 1\n}\n";
        assert_eq!(apply_and_run(src), "ok(())");
    }

    /// A bare `return` (no expression) is Unit, same as the case above,
    /// but reached via an explicit early exit instead of falling through.
    #[test]
    fn bare_return_with_no_expression_is_unit_and_is_still_checked() {
        let src = "protocol P {\n    intent \"p\"\n    guard G: Unit {\n        expect true\n    }\n}\nfun probe() -> Unit {\n    if true {\n        return\n    }\n}\n";
        assert_eq!(apply_and_run(src), "ok(())");
    }

    /// A trailing `return` as the body's OWN last statement must not be
    /// double-wrapped by BOTH the explicit-return rewrite AND the
    /// implicit-tail rewrite -- `apply_guards`'s own `Some(Stmt::Return(..))`
    /// arm exists specifically to skip the tail-wrapping step here.
    #[test]
    fn trailing_return_as_the_last_statement_is_wrapped_exactly_once() {
        let src = format!("{PROTO}fun probe() -> Int {{\n    return 50000\n}}\n");
        assert_eq!(apply_and_run(&src), "panic: expectation failed: result < 10000");
    }

    /// Multiple guards on the same fun: EACH runs independently against
    /// the SAME exit value, at every exit point.
    #[test]
    fn multiple_guards_each_run_independently() {
        let (mut program, diags) = crate::parser::parse(
            "protocol P {\n    intent \"p\"\n    guard Positive: Int {\n        expect result > 0\n    }\n    guard UnderLimit: Int {\n        expect result < 10000\n    }\n}\nfun probe() -> Int {\n    -1\n}\n",
        );
        assert!(diags.is_empty(), "{diags:?}");
        crate::run::inject_prelude(&mut program);
        let guards: Vec<GuardDecl> = program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Protocol(p) => Some(p.guards.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(guards.len(), 2);
        for item in &mut program.items {
            if let Item::Fun(f) = item {
                if f.name == "probe" {
                    apply_guards(f, &guards);
                }
            }
        }
        let (checked, check_diags) = crate::check::check(&program);
        assert!(
            !check_diags.iter().any(|d| d.severity == crate::diag::Severity::Error),
            "{check_diags:?}"
        );
        let db = ProgramDb::build(&program, &checked);
        let mut interp = Interp::new(db);
        let f = Value::Fun(std::rc::Rc::new("probe".to_string()));
        match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Err(Flow::Panic { msg, .. }) => {
                assert!(msg.contains("result > 0"), "the FIRST-declared guard should fail first: {msg:?}");
            }
            other => panic!("expected a panic from the `Positive` guard, got {}", match &other {
                Ok(v) => format!("Ok({v})"),
                Err(_) => "Err(<non-Panic Flow>)".to_string(),
            }),
        }
    }
}
