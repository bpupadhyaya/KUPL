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

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::Diag;

/// Rewrite every call to a top-level function into positional form, filling
/// defaults and reordering named arguments. Returns any structural diagnostics.
///
/// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it894, an
/// Explore survey finding, agentId a7ba91a6862653340, independently
/// re-verified live before implementing): this pass used to match a call's
/// callee purely by IDENTIFIER TEXT against the flat, whole-program `funs`
/// map above, with no notion of lexical scope at all -- unlike its sibling
/// AST-rewrite pass `resolve.rs::Rewriter`, which threads a `scope:
/// Vec<HashSet<String>>` / `is_local()` through the exact same shape of walk
/// specifically to avoid this class of mistake (see that file's own doc
/// comments). Shadowing a top-level function's name with a local `let`
/// binding or a function-typed parameter is a deliberately supported,
/// ordinarily-working KUPL feature (a plain positional call to the shadowing
/// local already resolved correctly, matching `interp.rs`'s real scoping) --
/// but a call to it using NAMED arguments or omitting a trailing default was
/// silently rewritten using the UNRELATED top-level function's own
/// parameter names/order/defaults instead, with zero diagnostics, because
/// this pass ran upstream of (and blind to) the checker's real scope
/// resolution. Live-confirmed (two distinct failure shapes, both identical
/// on `kupl run` and `kupl run --vm`, with `kupl check` reporting ZERO
/// diagnostics):
///   - silent WRONG VALUE: `fun combine(x: Int, y: Int) -> Int { x - y }`
///     alongside `let combine = fn(m, n) { m * 10 + n }; combine(y: 2, x: 5)`
///     printed `52` (`m=5, n=2` per the LOCAL closure, reached only because
///     this pass rewrote the named args into `combine(5, 2)` using the
///     UNRELATED top-level `combine`'s `(x, y)` parameter order) instead of
///     the correct outcome: calling a local value with named arguments is
///     already rejected elsewhere in this exact file with a clean K0241
///     ("named arguments are only allowed for constructors and props") for
///     any NON-colliding local -- confirmed via a same-shaped control case
///     with no name collision, `let onlyLocal = fn(p, q) { ... };
///     onlyLocal(q: 100, p: 1)`, which correctly K0241s.
///   - spurious REJECTION of valid code: `fun greet(name: Str, punctuation:
///     Str = "!") -> Str { ... }` alongside `let greet = fn(n) { "hi " + n
///     }; greet("world")` -- an ordinary, single-argument call to a
///     one-parameter local closure -- failed with a bogus K0242 ("this
///     function takes 1 argument, 2 given"), because the unrelated
///     top-level `greet`'s own trailing default got appended.
/// Also confirmed to reach a function-TYPED parameter shadowing a top-level
/// function of the same name (an idiomatic pattern this exact codebase uses
/// elsewhere, e.g. `examples/collections.kupl`'s `cmp: fn(T, T) -> Int`),
/// not just a `let`-bound local. Fixed by replacing the old closure-based
/// walk with a `Resolver` struct carrying the SAME `scope: Vec<HashSet
/// <String>>` / `is_local`/`bind`/`push`/`pop` primitives as `resolve.rs`'s
/// `Rewriter`, pushing/popping a scope frame at every binding construct this
/// file's own walk already visits (function/handler/law params, `let`
/// statements, `for`/`forall` loop variables, lambda params, match-arm
/// pattern bindings) and skipping the named/default rewrite entirely
/// whenever the callee identifier is locally bound at that call site --
/// leaving such a call for the checker's own ordinary (already-correct)
/// K0241/K0242 diagnostics, exactly like the pre-existing non-colliding
/// control case above.
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

    let mut r = Resolver { funs: &funs, diags, temp_counter: 0, scope: Vec::new() };
    r.program(program);
    r.diags
}

struct Resolver<'a> {
    funs: &'a HashMap<String, Vec<Param>>,
    diags: Vec<Diag>,
    temp_counter: usize,
    scope: Vec<HashSet<String>>,
}

impl Resolver<'_> {
    fn is_local(&self, n: &str) -> bool {
        self.scope.iter().any(|f| f.contains(n))
    }
    fn bind(&mut self, n: &str) {
        if let Some(f) = self.scope.last_mut() {
            f.insert(n.to_string());
        }
    }
    fn push(&mut self) {
        self.scope.push(HashSet::new());
    }
    fn pop(&mut self) {
        self.scope.pop();
    }

    fn bind_pattern(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Bind(n) => self.bind(n),
            PatternKind::Ctor { args, .. } => {
                for a in args {
                    self.bind_pattern(a);
                }
            }
            PatternKind::Or(alts) => {
                for a in alts {
                    self.bind_pattern(a);
                }
            }
            PatternKind::At { name, inner } => {
                self.bind(name);
                self.bind_pattern(inner);
            }
            _ => {}
        }
    }

    fn program(&mut self, program: &mut Program) {
        for item in &mut program.items {
            match item {
            // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
            // PR-it840): an `ai fun`'s body is always a parser-synthesized
            // EMPTY block (see parser.rs's `parse_ai_fun`) -- the actual
            // content is `f.ai`'s `intent_expr`, a fully general expression
            // (the `intent "..."` string's interpolation pieces, parsed via
            // `str_parts_expr`) that `check.rs::check_fun` DOES type-check
            // (`self.infer_expr(&ai.intent_expr, &mut ctx)`) and that
            // `interp.rs::call_fun_body`/`compile.rs::compile_ai_fun` DO
            // evaluate/compile directly -- but this arm only ever walked the
            // (always-empty) `f.body`, never `f.ai`, so a named-argument or
            // trailing-default-relying call inside an `intent` interpolation
            // was silently rejected, the FOURTH instance of this file's
            // "an Expr-bearing field simply missing from the item walker"
            // gap class (after PR-it769's `examples`/`laws`, PR-it839's
            // `props[i].default`/`children[i].args`). Because `check.rs`
            // DOES independently visit `intent_expr` (unlike PR-it839's prop
            // defaults, which it skips entirely), this manifests as a LOUD
            // false-rejection, not silent corruption -- live-confirmed:
            // `intent "value: {sub(b: panic("B"), a: panic("A"))}"` against
            // `fun sub(a: Int, b: Int) -> Int { a - b }` failed K0241 on
            // `kupl check`/`run`/`run --vm` even though the identical call
            // compiles cleanly everywhere else; `intent "value: {add(10)}"`
            // against `fun add(a: Int, b: Int = 5) -> Int` failed K0242.
            Item::Fun(f) => self.fun_body(f),
            Item::Law(l) => {
                self.push();
                self.block(&mut l.body);
                self.pop();
            }
            Item::Component(c) => {
                for s in &mut c.state {
                    self.expr(&mut s.init);
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
                        self.expr(d);
                    }
                }
                // A REAL, LIVE-CONFIRMED bug found+fixed (production-
                // hardening PR-it894, same sweep as this file's own top doc
                // comment): props/state were never bound into scope here,
                // so a handler/method/child/example referencing a prop or
                // state field that happens to share a name with an
                // unrelated top-level function would ALSO hit the exact
                // same scope-blind rewrite this fix closes above -- one
                // push/pop frame, opened here (after prop defaults/state
                // inits, which can't reference the component's own
                // not-yet-constructed state, mirroring `resolve.rs::
                // component`'s identical ordering) and closed at the end of
                // this arm, covers every location below that can reference
                // a prop or state field by bare name.
                self.push();
                for p in &c.props {
                    self.bind(&p.name);
                }
                for s in &c.state {
                    self.bind(&s.name);
                }
                for child in &mut c.children {
                    for a in &mut child.args {
                        self.expr(&mut a.value);
                    }
                }
                for h in &mut c.handlers {
                    self.push();
                    if let Some(p) = &h.param {
                        self.bind(p);
                    }
                    self.block(&mut h.body);
                    self.pop();
                }
                for f in c.funs.iter_mut().chain(c.exposes.iter_mut()) {
                    self.fun_body(f);
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
                            ExampleStep::Send { arg: Some(e), .. } => self.expr(e),
                            ExampleStep::Expect { expr, .. } => self.expr(expr),
                            ExampleStep::Send { arg: None, .. } | ExampleStep::Advance { .. } => {}
                        }
                    }
                }
                self.pop(); // the props/state frame opened above
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
                    self.push();
                    self.block(&mut law.body);
                    self.pop();
                }
            }
            _ => {}
        }
        }
    }

    /// A top-level `fun` or a component method (`c.funs`/`c.exposes`, the
    /// same `FunDecl` shape): push a fresh scope frame binding this
    /// function's own parameters, then walk its body and (for an `ai fun`)
    /// its `intent` expression -- both see the SAME parameter scope, since
    /// `intent` functionally replaces `body` for an ai fun (production-
    /// hardening PR-it840's own finding: `f.body` is always a
    /// parser-synthesized empty block for one).
    ///
    /// A REAL bug found+fixed (production-hardening PR-it1068): a
    /// parameter's OWN default value (`Param.default`, only legal on a
    /// top-level `fun` -- component-method/ai-fun params and
    /// constructor/variant fields all reject a default outright via
    /// check.rs's K0275) was never walked here, the same missing-`Expr`-
    /// field gap as the `PropDecl.default` case in the `Item::Component`
    /// arm above, just on a different AST node. Live-confirmed before this
    /// fix: `fun f(a: Int, b: Int = g(y: 1, x: 10))` against `fun g(x: Int,
    /// y: Int)` spuriously rejected the named call inside the default with
    /// K0241, even though `f` was never called anywhere. Resolved BEFORE
    /// this function's own params are bound, mirroring `PropDecl.default`'s
    /// identical before-the-frame placement above -- a default is spliced
    /// into the CALLER's own scope at each call site (never this
    /// function's own param scope, per this file's own top doc comment), so
    /// a call target here must never be treated as shadowed by one of this
    /// function's OWN sibling parameter names.
    fn fun_body(&mut self, f: &mut FunDecl) {
        for p in &mut f.params {
            if let Some(d) = &mut p.default {
                self.expr(d);
            }
        }
        self.push();
        for p in &f.params {
            self.bind(&p.name);
        }
        self.block(&mut f.body);
        if let Some(ai) = &mut f.ai {
            self.expr(&mut ai.intent_expr);
        }
        self.pop();
    }

    fn block(&mut self, b: &mut Block) {
        self.push();
        for s in &mut b.stmts {
            self.stmt(s);
        }
        self.pop();
    }

    fn stmt(&mut self, s: &mut Stmt) {
        match s {
            Stmt::Let { name, init, .. } => {
                self.expr(init);
                self.bind(name); // in scope for later statements
            }
            Stmt::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            Stmt::Expr(e) => self.expr(e),
            Stmt::Return(Some(e), _) => self.expr(e),
            Stmt::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            Stmt::For { var, iter, body, .. } => {
                self.expr(iter);
                self.push();
                self.bind(var);
                self.block(body);
                self.pop();
            }
            Stmt::Emit { arg: Some(e), .. } => self.expr(e),
            Stmt::Expect(e, _) => self.expr(e),
            Stmt::Forall { vars, body, .. } => {
                self.push();
                for (v, _) in vars.iter() {
                    self.bind(v);
                }
                self.block(body);
                self.pop();
            }
            Stmt::Return(None, _) | Stmt::Emit { arg: None, .. } | Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        let callee_name = match &e.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Ident(name) if self.funs.contains_key(name) && !self.is_local(name) => Some(name.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = callee_name {
            let params = self.funs.get(&name).unwrap().clone();
            resolve_one(&name, &params, e, &mut self.diags, &mut self.temp_counter);
        }
        match &mut e.kind {
            ExprKind::Str(pieces) => {
                for p in pieces {
                    if let StrPiece::Expr(inner) = p {
                        self.expr(inner);
                    }
                }
            }
            ExprKind::List(items) | ExprKind::Par(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for a in args {
                    self.expr(&mut a.value);
                }
            }
            ExprKind::MethodCall { recv, args, .. } => {
                self.expr(recv);
                for a in args {
                    self.expr(&mut a.value);
                }
            }
            ExprKind::Field { recv, .. } => self.expr(recv),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::If { cond, then_block, else_block } => {
                self.expr(cond);
                self.block(then_block);
                if let Some(e) = else_block {
                    self.expr(e);
                }
            }
            ExprKind::BlockExpr(b) => self.block(b),
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.push();
                    self.bind_pattern(&arm.pattern);
                    if let Some(g) = &mut arm.guard {
                        self.expr(g);
                    }
                    self.expr(&mut arm.body);
                    self.pop();
                }
            }
            ExprKind::Lambda { params, body } => {
                self.push();
                for p in params.iter() {
                    self.bind(&p.name);
                }
                self.block(body);
                self.pop();
            }
            ExprKind::Range { lo, hi, .. } => {
                self.expr(lo);
                self.expr(hi);
            }
            ExprKind::With { recv, updates } => {
                self.expr(recv);
                for (_, v) in updates {
                    self.expr(v);
                }
            }
            ExprKind::Try(e) | ExprKind::Await(e) => self.expr(e),
            _ => {}
        }
    }
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
                    // A REAL diagnostic-wording gap found+fixed (production-
                    // hardening PR-it972): unlike check.rs's K0242 (which
                    // genuinely can't name the callee -- that path handles
                    // ANY callable expression, e.g. `get_fn()(1, 2)`, not
                    // just a named function), `resolve_one` -- and this
                    // K0274 site specifically -- only ever runs for a DIRECT
                    // call to a top-level named `fun` (this file's own
                    // top-of-file doc comment), so `fun_name` is ALWAYS a
                    // real, known, meaningful name here, exactly like its
                    // sibling K0273 just above (`` `{fun_name}` has no
                    // parameter named `{n}` ``) -- which already includes
                    // it. This message used to omit it entirely.
                    diags.push(Diag::error(
                        "K0274",
                        format!("`{fun_name}` is missing an argument for parameter `{}`", p.name),
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

    #[test]
    fn k0274_missing_argument_names_the_function() {
        // A REAL diagnostic-wording gap found+fixed (production-hardening
        // PR-it972): K0274 ("missing argument for parameter") used to omit
        // the function name entirely, unlike its sibling K0273 (unknown
        // named argument) just above, which already names it -- both fire
        // from the exact same named/default-argument resolution pass, which
        // ONLY ever runs for a direct call to a top-level named `fun` (this
        // file's own top-of-file doc comment), so the function name is
        // ALWAYS known and meaningful at this site (unlike check.rs's
        // separate, differently-scoped K0242, which genuinely can't name an
        // arbitrary callable expression).
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(\"{add(b: 5)}\")\n}\n";
        let errs = errors(src);
        assert!(
            errs.iter().any(|d| d.code == "K0274" && d.message.contains("`add`") && d.message.contains("parameter `a`")),
            "a missing named argument must name BOTH the function and the missing parameter: {errs:?}"
        );
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

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it840):
    /// `Item::Fun`'s arm only ever walked `f.body` -- always a parser-
    /// synthesized EMPTY block for an `ai fun` (see `parser.rs::
    /// parse_ai_fun`) -- never `f.ai.intent_expr`, the `intent "..."` string's
    /// actual interpolated content, which `check.rs` DOES type-check. So a
    /// named-argument or trailing-default-relying call inside an `intent`
    /// interpolation was silently rejected with a misleading K0241/K0242,
    /// even though the identical call compiles cleanly everywhere else. The
    /// FOURTH instance of this file's "an Expr-bearing field missing from the
    /// item walker" class (after PR-it769's `examples`/`laws`, PR-it839's
    /// `props[i].default`/`children[i].args`), found via a deliberate,
    /// exhaustive audit of every Expr-bearing AST field prompted by it839
    /// being the third such gap.
    #[test]
    fn default_params_and_named_args_resolve_inside_an_ai_funs_intent() {
        // named arguments, inside an `intent` interpolation.
        let named = "fun sub(a: Int, b: Int) -> Int {\n    a - b\n}\n\
                      ai fun helper(x: Int) -> Str {\n    intent \"{sub(b: 3, a: 10)}\"\n}\n";
        assert!(
            errors(named).is_empty(),
            "a named-argument call inside an ai fun's intent must resolve like it does everywhere else: {:?}",
            errors(named)
        );

        // a trailing default, inside an `intent` interpolation.
        let default = "fun add(a: Int, b: Int = 5) -> Int {\n    a + b\n}\n\
                        ai fun helper(x: Int) -> Str {\n    intent \"{add(10)}\"\n}\n";
        assert!(
            errors(default).is_empty(),
            "a default-param call inside an ai fun's intent must resolve like it does everywhere else: {:?}",
            errors(default)
        );
    }

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
    /// PR-it1068, an Explore survey finding, agentId a4c21ee677f4e8991,
    /// independently re-verified live before implementing): `fun_body`
    /// (used for every top-level `fun` and component method) bound each
    /// parameter's NAME into scope but never walked that parameter's own
    /// `default` expression -- unlike the sibling `PropDecl.default` case
    /// (`Item::Component`'s arm, fixed at PR-it839), which already does.
    /// So a named-argument or trailing-default-relying call written INSIDE
    /// a `fun` parameter's own default value was silently rejected with a
    /// misleading K0241/K0242, even though the identical call compiles
    /// cleanly everywhere else -- and this misfires purely from the
    /// function's OWN DECLARATION, with no call to it required anywhere in
    /// the program. The FIFTH instance of this file's "an Expr-bearing
    /// field missing from the item walker" class (after PR-it769's
    /// `examples`/`laws`, PR-it839's `props[i].default`/`children[i].args`,
    /// PR-it840's `ai.intent_expr`). Component-method/`ai fun` params and
    /// constructor/variant fields all reject a default outright via
    /// check.rs's K0275, so a plain top-level `fun`'s own parameter default
    /// is the ONLY reachable location for this specific gap.
    #[test]
    fn default_params_and_named_args_resolve_inside_another_funs_own_parameter_default() {
        // named arguments, inside a parameter's own default value.
        let named = "fun g(x: Int, y: Int) -> Int {\n    x - y\n}\n\
                      fun f(a: Int, b: Int = g(y: 1, x: 10)) -> Int {\n    a + b\n}\n\
                      fun main() uses io {\n    print(\"{f(0)}\")\n}\n";
        assert!(
            errors(named).is_empty(),
            "a named-argument call inside a fun parameter's own default must resolve like it does everywhere else: {:?}",
            errors(named)
        );

        // a trailing default, inside a parameter's own default value.
        let default = "fun add(a: Int, b: Int = 5) -> Int {\n    a + b\n}\n\
                        fun f(a: Int, b: Int = add(10)) -> Int {\n    a + b\n}\n\
                        fun main() uses io {\n    print(\"{f(1)}\")\n}\n";
        assert!(
            errors(default).is_empty(),
            "a default-param call inside a fun parameter's own default must resolve like it does everywhere else: {:?}",
            errors(default)
        );
    }
}
