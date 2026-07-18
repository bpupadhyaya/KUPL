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
use crate::diag::Diag;

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

    let mut temp_counter = 0usize;
    let mut visit = |e: &mut Expr| {
        let callee_name = match &e.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Ident(name) if funs.contains_key(name) => Some(name.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = callee_name {
            let params = funs.get(&name).unwrap().clone();
            resolve_one(&name, &params, e, &mut diags, &mut temp_counter);
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
                // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
                // PR-it839): this arm never walked `c.props[i].default` (a
                // `prop x: Int = EXPR` default value, a fully general
                // expression per the parser) or `c.children[i].args[j].value`
                // (a child's own constructor arguments, `let c =
                // Child(y: EXPR)`) -- so a call relying on named arguments or
                // a default parameter value inside either location was never
                // rewritten to positional form, unlike the IDENTICAL call
                // written anywhere else (top-level, handlers, examples,
                // contract laws). For a prop default this was SILENT VALUE
                // CORRUPTION, not a rejection: `interp.rs::instantiate`
                // evaluates the raw default via `self.eval(d, &env)`, and
                // `compile.rs`'s prop-default-chunk compilation (`fc.expr(d)`)
                // compiles it the same un-rewritten way -- both simply
                // evaluate each argument in SOURCE-WRITTEN order and ignore
                // `a.name`, exactly like an ordinary un-rewritten call. Since
                // this rewrite runs ONCE upstream of check/interp/VM/native,
                // all four engines silently agreed on the WRONG value with
                // zero diagnostics. Live-confirmed: `prop x: Int =
                // sub(b: 3, a: 10)` against `fun sub(a: Int, b: Int) -> Int
                // { a - b }` printed `-7` (the wrong, source-order-positional
                // result) instead of `7` on `kupl run`, `kupl run --vm`, AND
                // `kupl native`, with `kupl check` reporting zero
                // diagnostics. For a child's constructor args the same gap
                // instead produced a misleading K0241 "named arguments only
                // allowed for constructors and props" rejection of an
                // otherwise-legitimate call -- the exact same
                // diagnostic-mismatch shape PR-it769 already fixed for
                // `examples`/`laws`, just two more AST locations that match
                // never covered.
                for p in &mut c.props {
                    if let Some(d) = &mut p.default {
                        walk_expr(d, &mut visit);
                    }
                }
                for child in &mut c.children {
                    for a in &mut child.args {
                        walk_expr(&mut a.value, &mut visit);
                    }
                }
                // A REAL bug found+fixed (production-hardening PR-it769): this
                // arm never walked `c.examples` -- the `example { send ...;
                // expect ... }` blocks `kupl test` runs directly -- so a call
                // relying on a default parameter value or named arguments
                // inside an `expect`/`send` expression was silently rejected
                // by the type checker (a misleading K0242/K0241 arity/named-
                // arg error) even though the IDENTICAL call compiles cleanly
                // everywhere else (top-level, ordinary functions, handlers).
                // Live-confirmed before this fix: `expect result == add(10)`
                // against `fun add(a: Int, b: Int = 5) -> Int`.
                for ex in &mut c.examples {
                    for step in &mut ex.steps {
                        match step {
                            ExampleStep::Send { arg: Some(e), .. } => walk_expr(e, &mut visit),
                            ExampleStep::Expect { expr, .. } => walk_expr(expr, &mut visit),
                            ExampleStep::Send { arg: None, .. } | ExampleStep::Advance { .. } => {}
                        }
                    }
                }
            }
            // A REAL bug found+fixed (production-hardening PR-it769): this
            // whole item kind fell into the `_ => {}` catch-all, so a
            // `contract`'s `law "..." { ... }` bodies (executable properties
            // `kupl test` runs against every component that fulfills the
            // contract) were NEVER visited by this pass -- the exact same
            // silent default-parameter/named-argument rejection as the
            // `examples` gap above, reachable through the SAME root cause
            // (an item kind simply missing from this match) but a genuinely
            // DIFFERENT AST node (`ContractDecl.laws`, not `Component.
            // examples`). Live-confirmed before this fix: `expect add(10) ==
            // 15` inside `contract Adder { law "..." { ... } }`.
            Item::Contract(ct) => {
                for law in &mut ct.laws {
                    walk_block(&mut law.body, &mut visit);
                }
            }
            _ => {}
        }
    }
    diags
}

/// Resolve one call's args against `params`, rewriting `e` (a `Call` node) in
/// place. Only runs when there are named args or missing trailing args;
/// leaves well-formed positional calls and over-full calls (arity errors) to
/// the checker.
///
/// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it719):
/// reordering named arguments into parameter-declaration order used to move
/// the ARGUMENT EXPRESSIONS THEMSELVES (not just their final call-site slot)
/// -- so `f(b: sideEffectB(), a: sideEffectA())` silently evaluated
/// `sideEffectA()` BEFORE `sideEffectB()`, the REVERSE of how the call reads,
/// identically (and identically WRONG) on all four engines, since this
/// rewrite runs ONCE upstream of interp/KVM/native/`.kx`. Confirmed live:
/// printing inside each side effect showed "A" then "B" for a call written
/// `f(b: sideB(), a: sideA())`. Every mainstream language with named/keyword
/// arguments (Swift, Kotlin, Python, C#, Ruby -- this campaign's own
/// standing "Swift+Kotlin in comparisons" directive) evaluates call
/// arguments in SOURCE-WRITTEN order regardless of parameter declaration
/// order; this was an unconsidered divergence, not a documented design
/// choice. Fixed by evaluating every consumed argument (positional OR
/// named) into a synthetic `let __namedargN = ARGEXPR` IN SOURCE-WRITTEN
/// ORDER, then building the final positional call from `Ident` references
/// to those temporaries in PARAMETER order -- preserving both observable
/// evaluation order (the `let`s run source-first) and the callee's ordinary
/// positional-argument contract (every downstream engine still just sees a
/// plain `Call`, wrapped in a `Block`). Trailing DEFAULT values (no
/// source-written position at all) are left unwrapped, evaluated directly at
/// their PARAMETER-ORDER position in the final call -- after every
/// explicitly-given (now temp-bound) argument, since K0267 already requires
/// defaults to be trailing.
fn resolve_one(fun_name: &str, params: &[Param], e: &mut Expr, diags: &mut Vec<Diag>, temp_counter: &mut usize) {
    let span = e.span;
    let (callee, mut args) = match std::mem::replace(&mut e.kind, ExprKind::Unit) {
        ExprKind::Call { callee, args } => (callee, args),
        other => unreachable!("resolve_one called on a non-Call node: {other:?}"),
    };
    let has_named = args.iter().any(|a| a.name.is_some());
    if !has_named && args.len() == params.len() {
        e.kind = ExprKind::Call { callee, args };
        return;
    }
    if args.len() > params.len() {
        e.kind = ExprKind::Call { callee, args }; // too many — let the arity check report it
        return;
    }
    let mut slots: Vec<Option<Expr>> = (0..params.len()).map(|_| None).collect();
    let mut seen_named = false;
    let mut pos = 0usize;
    // Every consumed argument's expression, wrapped in a fresh `let` IN
    // SOURCE-WRITTEN ORDER (only when the call actually mixes/uses named
    // args -- a purely positional or already-in-order call needs no
    // rewriting, matching the ORIGINAL behavior exactly, since no reordering
    // ever happens for it).
    let mut prelude: Vec<Stmt> = Vec::new();
    let mut bind = |value: Expr| -> Expr {
        if !has_named {
            return value;
        }
        let tmp = format!("__namedarg{temp_counter}");
        *temp_counter += 1;
        let vspan = value.span;
        prelude.push(Stmt::Let { name: tmp.clone(), ty: None, init: value, mutable: false, span: vspan });
        Expr { kind: ExprKind::Ident(tmp), span: vspan }
    };
    for a in args.drain(..) {
        match a.name {
            None => {
                if seen_named {
                    diags.push(Diag::error("K0268", "positional argument after a named argument", span));
                }
                if pos < slots.len() {
                    slots[pos] = Some(bind(a.value));
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
                            slots[i] = Some(bind(a.value));
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
            Some(v) => out.push(Arg { name: None, value: v }),
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
    let call = Expr { kind: ExprKind::Call { callee, args: out }, span };
    if prelude.is_empty() {
        *e = call;
    } else {
        prelude.push(Stmt::Expr(call));
        e.kind = ExprKind::BlockExpr(Block { stmts: prelude, span });
    }
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

    /// A REAL bug found+fixed (production-hardening PR-it769): `resolve_call_args`'s
    /// item walker (the loop right above this test module) never visited a
    /// `contract`'s `law` bodies (fell into the `_ => {}` catch-all -- `Item::
    /// Contract` was simply missing from the match) OR a component's `example`
    /// blocks (the `Item::Component` arm walked handlers/funs/exposes/state
    /// but never `c.examples`) -- so a call relying on a default parameter
    /// value or named arguments, inside either construct, was silently
    /// rejected by the type checker with a misleading K0242 (wrong arity)
    /// error, even though the IDENTICAL call compiles cleanly everywhere else
    /// (top-level, an ordinary function, a handler). Live-confirmed BEFORE
    /// this fix via `kupl test`: `expect add(10) == 15` inside a contract law,
    /// and `expect result == add(10)` inside an example block, both failed
    /// with "this function takes 2 arguments, 1 given" against `fun add(a:
    /// Int, b: Int = 5) -> Int`.
    #[test]
    fn default_params_and_named_args_resolve_inside_contract_laws_and_component_examples() {
        // default parameter, inside a contract law
        let contract_default = "fun add(a: Int, b: Int = 5) -> Int {\n    a + b\n}\n\
                                 contract Adder {\n    law \"with default\" { expect add(10) == 15 }\n}\n";
        assert!(
            errors(contract_default).is_empty(),
            "a default-param call inside a contract law must resolve like it does everywhere else: {:?}",
            errors(contract_default)
        );

        // default parameter, inside a component example block
        let example_default = "fun add(a: Int, b: Int = 5) -> Int {\n    a + b\n}\n\
                                component Adder {\n    intent \"adds\"\n    \
                                in go: Int\n    out result: Int\n    on go(n) { emit result(add(n)) }\n    \
                                example {\n        send go(10)\n        expect result == add(10)\n    }\n}\n";
        assert!(
            errors(example_default).is_empty(),
            "a default-param call inside an example block must resolve like it does everywhere else: {:?}",
            errors(example_default)
        );

        // named arguments, inside a contract law
        let contract_named = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
                               contract Adder {\n    law \"named\" { expect add(b: 2, a: 10) == 12 }\n}\n";
        assert!(
            errors(contract_named).is_empty(),
            "a named-argument call inside a contract law must resolve like it does everywhere else: {:?}",
            errors(contract_named)
        );
    }
}
