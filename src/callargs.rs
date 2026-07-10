//! Call-argument resolution: a pre-check pass that rewrites calls to top-level
//! functions into plain positional form, so the checker, interpreter, VM, and
//! native backend all see ordinary positional calls (byte-identical for free).
//!
//! - **Named arguments** — `f(b: 2, a: 1)` are reordered into parameter order.
//! - **Default parameter values** — `fun f(a, b = EXPR)` called as `f(x)` gets
//!   the missing trailing argument filled with a clone of `EXPR` (evaluated per
//!   call at the call site).
//!
//! Named/default resolution applies only to direct calls of top-level `fun`s
//! (not constructors, methods, or UFCS). Defaults must be trailing.

use std::collections::HashMap;

use crate::ast::*;
use crate::diag::{Diag, Span};

/// Rewrite every call to a top-level function into positional form, filling
/// defaults and reordering named arguments. Returns any structural diagnostics.
pub fn resolve_call_args(program: &mut Program) -> Vec<Diag> {
    let mut diags = Vec::new();
    let mut funs: HashMap<String, Vec<Param>> = HashMap::new();
    for item in &program.items {
        if let Item::Fun(f) = item {
            if f.ai.is_some() {
                continue; // ai funs are prompt templates, not ordinary calls
            }
            // defaults must be trailing: no required param after a defaulted one
            let mut seen_default = false;
            for p in &f.params {
                if p.default.is_some() {
                    seen_default = true;
                } else if seen_default {
                    diags.push(Diag::error(
                        "K0267",
                        format!("parameter `{}` has no default but follows one that does", p.name),
                        p.span,
                    ));
                }
            }
            funs.insert(f.name.clone(), f.params.clone());
        }
    }

    let mut visit = |e: &mut Expr| {
        let span = e.span;
        if let ExprKind::Call { callee, args } = &mut e.kind {
            if let ExprKind::Ident(name) = &callee.kind {
                if let Some(params) = funs.get(name) {
                    resolve_one(name, params, args, span, &mut diags);
                }
            }
        }
    };

    for item in &mut program.items {
        match item {
            Item::Fun(f) => walk_block(&mut f.body, &mut visit),
            Item::Law(l) => walk_block(&mut l.body, &mut visit),
            Item::Component(c) => {
                for h in &mut c.handlers {
                    walk_block(&mut h.body, &mut visit);
                }
                for f in c.funs.iter_mut().chain(c.exposes.iter_mut()) {
                    walk_block(&mut f.body, &mut visit);
                }
                for s in &mut c.state {
                    walk_expr(&mut s.init, &mut visit);
                }
            }
            _ => {}
        }
    }
    diags
}

/// Resolve one call's args against `params`, in place. Only runs when there are
/// named args or missing trailing args; leaves well-formed positional calls and
/// over-full calls (arity errors) to the checker.
fn resolve_one(fun_name: &str, params: &[Param], args: &mut Vec<Arg>, span: Span, diags: &mut Vec<Diag>) {
    let has_named = args.iter().any(|a| a.name.is_some());
    if !has_named && args.len() == params.len() {
        return;
    }
    if args.len() > params.len() {
        return; // too many — let the arity check report it
    }
    let mut slots: Vec<Option<Expr>> = (0..params.len()).map(|_| None).collect();
    let mut seen_named = false;
    let mut pos = 0usize;
    for a in std::mem::take(args) {
        match a.name {
            None => {
                if seen_named {
                    diags.push(Diag::error("K0268", "positional argument after a named argument", span));
                }
                if pos < slots.len() {
                    slots[pos] = Some(a.value);
                }
                pos += 1;
            }
            Some(n) => {
                seen_named = true;
                match params.iter().position(|p| p.name == n) {
                    Some(i) => {
                        if slots[i].is_some() {
                            diags.push(Diag::error("K0269", format!("argument `{n}` given more than once"), span));
                        } else {
                            slots[i] = Some(a.value);
                        }
                    }
                    None => {
                        let mut msg = format!("`{fun_name}` has no parameter named `{n}`");
                        // Only suggest a parameter that isn't ALREADY filled -- e.g. `add(a: 1,
                        // c: 2)` suggesting `a` (already given) would be a red herring; the
                        // useful suggestion is the remaining unfilled one, `b`.
                        let unfilled = params.iter().enumerate().filter(|(i, _)| slots[*i].is_none()).map(|(_, p)| p.name.as_str());
                        if let Some(s) = crate::check::suggest(&n, unfilled) {
                            msg.push_str(&format!(" — did you mean `{s}`?"));
                        }
                        diags.push(Diag::error("K0273", msg, span));
                    }
                }
            }
        }
    }
    let mut out = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        match slots[i].take() {
            Some(e) => out.push(Arg { name: None, value: e }),
            None => match &p.default {
                Some(d) => out.push(Arg { name: None, value: d.clone() }),
                None => {
                    diags.push(Diag::error(
                        "K0274",
                        format!("missing argument for parameter `{}`", p.name),
                        span,
                    ));
                    // placeholder so the arg list stays the right length
                    out.push(Arg { name: None, value: Expr { kind: ExprKind::Unit, span } });
                }
            },
        }
    }
    *args = out;
}

// ---- mutable AST walkers (mirror the immutable ones in effects.rs) ----

fn walk_block(block: &mut Block, f: &mut impl FnMut(&mut Expr)) {
    for stmt in &mut block.stmts {
        walk_stmt(stmt, f);
    }
}

fn walk_stmt(stmt: &mut Stmt, f: &mut impl FnMut(&mut Expr)) {
    match stmt {
        Stmt::Let { init, .. } => walk_expr(init, f),
        Stmt::Assign { target, value, .. } => {
            walk_expr(target, f);
            walk_expr(value, f);
        }
        Stmt::Expr(e) => walk_expr(e, f),
        Stmt::Return(Some(e), _) => walk_expr(e, f),
        Stmt::While { cond, body, .. } => {
            walk_expr(cond, f);
            walk_block(body, f);
        }
        Stmt::For { iter, body, .. } => {
            walk_expr(iter, f);
            walk_block(body, f);
        }
        Stmt::Emit { arg: Some(e), .. } => walk_expr(e, f),
        Stmt::Expect(e, _) => walk_expr(e, f),
        Stmt::Forall { body, .. } => walk_block(body, f),
        Stmt::Return(None, _) | Stmt::Emit { arg: None, .. } | Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn walk_expr(expr: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    f(expr);
    match &mut expr.kind {
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(e) = p {
                    walk_expr(e, f);
                }
            }
        }
        ExprKind::List(items) | ExprKind::Par(items) => {
            for i in items {
                walk_expr(i, f);
            }
        }
        ExprKind::Call { callee, args } => {
            walk_expr(callee, f);
            for a in args {
                walk_expr(&mut a.value, f);
            }
        }
        ExprKind::MethodCall { recv, args, .. } => {
            walk_expr(recv, f);
            for a in args {
                walk_expr(a, f);
            }
        }
        ExprKind::Field { recv, .. } => walk_expr(recv, f),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, f),
        ExprKind::If { cond, then_block, else_block } => {
            walk_expr(cond, f);
            walk_block(then_block, f);
            if let Some(e) = else_block {
                walk_expr(e, f);
            }
        }
        ExprKind::BlockExpr(b) => walk_block(b, f),
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, f);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    walk_expr(g, f);
                }
                walk_expr(&mut arm.body, f);
            }
        }
        ExprKind::Lambda { body, .. } => walk_block(body, f),
        ExprKind::Range { lo, hi, .. } => {
            walk_expr(lo, f);
            walk_expr(hi, f);
        }
        ExprKind::With { recv, updates } => {
            walk_expr(recv, f);
            for (_, v) in updates {
                walk_expr(v, f);
            }
        }
        ExprKind::Try(e) | ExprKind::Await(e) => walk_expr(e, f),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    // `resolve_call_args` only runs as part of the real `kupl check`/`run`
    // pipeline (`crate::run::compile`) -- check.rs's own bare `errors()` test
    // harness does NOT call it (the same discrepancy K0241's it520 fix hit),
    // so these tests go through the full pipeline instead.
    fn errors(src: &str) -> Vec<crate::diag::Diag> {
        crate::run::compile(src).err().unwrap_or_default()
    }

    #[test]
    fn k0273_unknown_named_argument_names_the_function_and_suggests_closest_unfilled_param() {
        // Error-message round 43 (PR-it536): `add(a: 1, c: 2)` (typo'd named
        // argument) was flat "no parameter named `c`" -- named neither the
        // function being called nor the fix. Found by widening the err-msg scan
        // beyond check.rs (confirmed exhausted it535) into callargs.rs, a
        // pre-check pass that resolves named/default arguments before the
        // checker ever sees the call.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(\"{add(a: 1, c: 2)}\")\n}\n";
        let typo = errors(src);
        assert!(
            typo.iter().any(|d| d.code == "K0273" && d.message.contains("`add` has no parameter named `c`") && d.message.contains("did you mean `b`?")),
            "typo'd named argument should name the function and suggest the closest UNFILLED param: {typo:?}"
        );
        // The already-given `a` must NOT be suggested (a red herring: the user
        // already provided it, the useful fix is the remaining unfilled one).
        assert!(
            !typo.iter().any(|d| d.code == "K0273" && d.message.contains("did you mean `a`?")),
            "must not suggest a parameter that's already been filled: {typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean).
        let none_src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(\"{add(a: 1, zqxwbly: 2)}\")\n}\n";
        let none = errors(none_src);
        assert!(
            none.iter().any(|d| d.code == "K0273" && !d.message.contains("did you mean")),
            "unrelated name should stay bare: {none:?}"
        );
        // A correct named call still type-checks cleanly.
        let ok_src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(\"{add(a: 1, b: 2)}\")\n}\n";
        assert!(errors(ok_src).is_empty());
    }
}
