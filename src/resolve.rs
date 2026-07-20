//! Package namespace isolation by load-time name mangling.
//!
//! Every dependency package's *definitions* (funs, types + constructors,
//! components, contracts) are renamed to `pkg$name`, and every *reference*
//! inside that package that resolves to one of its own definitions is rewritten
//! to match. The root/entry package is never mangled, so ordinary single- and
//! multi-file programs keep bare names and are completely unaffected.
//!
//! Cross-package access is qualified: after `use math`, a call `math.add(x)`
//! (parsed as a method/field on `Ident("math")`) is rewritten to `math$add(x)`,
//! which matches how the `math` package's `add` was mangled.
//!
//! The mangling sentinel `$` never appears in a source identifier, and these
//! strings only ever become `HashMap<String, _>` keys in the checker/compiler/
//! interpreter, so the engines handle them uniformly — this pass is purely a
//! frontend rewrite and the interp==KVM==native invariant is untouched. A
//! missed rewrite surfaces as a loud unresolved-name error, never silent
//! divergence.

use std::collections::{HashMap, HashSet};

use crate::ast::*;

/// Strip a `pkg$name` mangling prefix for USER-FACING display — never for
/// internal identity/equality, which must keep comparing the full mangled
/// name (that's the entire point of mangling: two same-named types from
/// different packages must stay distinguishable). `$` never appears in a
/// source identifier (this module's own doc comment above), so stripping
/// everything up to and including the LAST `$` is always safe and never
/// mistakes real source text for a mangling artifact.
///
/// A REAL bug found+fixed (production-hardening PR-it628): this module's own
/// doc comment documents the mangling scheme precisely, but nothing ever
/// REVERSED it for display -- so a cross-package type/constructor's mangled
/// name leaked verbatim into `print()` output AND type-checker error
/// messages (`math.origin()` printed as `math$Point(0, 0)` instead of
/// `Point(0, 0)`; a type mismatch reported "expected math$Point" instead of
/// "expected Point"), across ALL THREE engines. Confirmed via a live 3-way
/// repro (interp/vm/native all agreed on the leak) before touching any code.
pub fn demangle_for_display(name: &str) -> &str {
    name.rsplit('$').next().unwrap_or(name)
}

/// Rewrite a program's items so cross-package names are globally unique.
/// `tagged` is `(item, package-prefix)` in load order (prefix "" = root, never
/// mangled); `pkg_deps` maps a package prefix to the ALIASES it may reference
/// with `alias.name`, each pointing to that dependency's OWN resolved
/// mangling prefix -- NOT necessarily the bare alias text itself (production-
/// hardening PR-it698, see `try_qualified` below for why this distinction is
/// load-bearing).
pub fn isolate(
    tagged: Vec<(Item, String)>,
    pkg_deps: &HashMap<String, HashMap<String, String>>,
) -> Vec<Item> {
    // per-package: bare defined name -> mangled name
    let mut renames: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (item, prefix) in &tagged {
        if prefix.is_empty() {
            continue; // the root package keeps bare names
        }
        let map = renames.entry(prefix.clone()).or_default();
        for name in defined_names(item) {
            map.insert(name.clone(), format!("{prefix}${name}"));
        }
    }

    let empty = HashMap::new();
    tagged
        .into_iter()
        .map(|(mut item, prefix)| {
            let rename = renames.get(&prefix).cloned().unwrap_or_default();
            let deps = pkg_deps.get(&prefix).unwrap_or(&empty);
            let mut r = Rewriter { rename: &rename, deps, scope: Vec::new() };
            r.item(&mut item);
            item
        })
        .collect()
}

/// The top-level names an item defines (that other items could reference).
fn defined_names(item: &Item) -> Vec<String> {
    match item {
        Item::Fun(f) => vec![f.name.clone()],
        Item::Type(t) => {
            let mut v = vec![t.name.clone()];
            v.extend(t.variants.iter().map(|va| va.name.clone()));
            v
        }
        Item::Component(c) => vec![c.name.clone()],
        Item::Contract(c) => vec![c.name.clone()],
        Item::Law(_) => vec![],
    }
}

struct Rewriter<'a> {
    rename: &'a HashMap<String, String>,
    /// alias name (as written in THIS package's own `use`) -> that
    /// dependency's OWN resolved mangling prefix.
    deps: &'a HashMap<String, String>,
    scope: Vec<HashSet<String>>,
}

impl Rewriter<'_> {
    fn is_local(&self, n: &str) -> bool {
        self.scope.iter().any(|f| f.contains(n))
    }
    /// Rewrite a bare reference name (leave locals, builtins, and root names).
    fn name(&self, n: &str) -> Option<String> {
        if self.is_local(n) {
            None
        } else {
            self.rename.get(n).cloned()
        }
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

    fn item(&mut self, item: &mut Item) {
        match item {
            Item::Fun(f) => self.fun(f),
            Item::Type(t) => {
                if let Some(m) = self.rename.get(&t.name) {
                    t.name = m.clone();
                }
                for va in &mut t.variants {
                    if let Some(m) = self.rename.get(&va.name) {
                        va.name = m.clone();
                    }
                    for field in &mut va.fields {
                        self.ty(&mut field.ty);
                    }
                }
            }
            Item::Component(c) => self.component(c),
            Item::Contract(c) => self.contract(c),
            Item::Law(l) => {
                self.push();
                self.block(&mut l.body);
                self.pop();
            }
        }
    }

    /// Mangle a TOP-LEVEL `fun`'s own declared name (so its definition site
    /// matches how OTHER items in this package reference it), then walk the
    /// rest via `fun_body`.
    fn fun(&mut self, f: &mut FunDecl) {
        if let Some(m) = self.rename.get(&f.name) {
            f.name = m.clone();
        }
        self.fun_body(f);
    }

    /// A component's OWN exposed/private method (`c.exposes`/`c.funs`) --
    /// same `FunDecl` shape as a top-level `fun`, but its bare name is never
    /// looked up through the package-level rename map at all (a method is
    /// resolved relative to its OWN component, e.g. `dep$C`'s `greet`, not
    /// through the flat `pkg$name` function namespace `defined_names`
    /// populates), so it must NOT be renamed like one.
    ///
    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it895,
    /// an Explore survey finding, agentId a7ba91a6862653340, independently
    /// re-verified live before implementing): `component()` below used to
    /// call the SAME `fun()` (rename-then-walk) for a component's own
    /// methods as for a top-level `fun` -- but `defined_names` (this file's
    /// own top-of-`isolate` helper) never adds a component's METHOD names to
    /// the per-package rename map, only the component's OWN top-level name.
    /// So `self.rename.get(&f.name)` on a method was only ever a HIT by pure
    /// COINCIDENCE: whenever this SAME package also happened to define an
    /// UNRELATED top-level `fun` sharing the method's bare name, that
    /// entry -- meant for the top-level fun's own definition site -- got
    /// applied to the method too, silently renaming e.g. `expose fun
    /// greet()` to `dep$greet` even though every CALLER still looks up the
    /// method by its bare, un-mangled name on the component (`dep.C().
    /// greet()`), guaranteed to no longer match. Live-confirmed: a `dep`
    /// package with a top-level `pub fun greet() -> Str { "top-level" }`
    /// ALONGSIDE `pub component C { expose fun greet() -> Str { "method" } }`
    /// -- loaded as a dependency and called as `dep.C().greet()` -- failed
    /// to compile with `K0247: component \`dep$C\` does not expose a
    /// function named \`greet\`` -- even though the IDENTICAL component,
    /// with the same-named top-level fun simply deleted (a same-shaped
    /// control case with no collision), compiles and runs fine (`method`).
    /// Since this is a LOUD false-rejection (`resolve.rs`'s own top-of-file
    /// doc comment: "a missed rewrite surfaces as a loud unresolved-name
    /// error, never silent divergence"), not silent corruption, this
    /// matches the file's own documented failure mode for this pass -- but
    /// is still a genuine correctness bug: legitimate code using an exposed/
    /// private method whose bare name happens to collide with an unrelated
    /// top-level fun in the SAME package cannot compile as a dependency at
    /// all. Fixed by giving component methods their own entry point that
    /// skips the rename step entirely and only calls the shared `fun_body`
    /// walk (params/ret/ai/body), matching how a method's bare name is
    /// ACTUALLY resolved everywhere else in the pipeline.
    fn method(&mut self, f: &mut FunDecl) {
        self.fun_body(f);
    }

    fn fun_body(&mut self, f: &mut FunDecl) {
        for p in &mut f.params {
            self.ty(&mut p.ty);
            // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
            // PR-it842, a targeted completeness sweep of this file's OTHER
            // `Expr`-bearing fields prompted by PR-it775/PR-it841 both
            // finding a gap here): `p.default` (a function parameter's
            // default value, `fun f(a, b: Int = EXPR)`) was never walked --
            // this loop only ever mangled `p.ty`. `callargs.rs`'s
            // `resolve_one` clones this raw `p.default` expression DIRECTLY
            // into whichever call site omits that trailing argument, so an
            // unmangled reference inside it travels along unrewritten to
            // check.rs/interp.rs/compile.rs, exactly like PR-it841's
            // match-guard gap and PR-it684's prop-default gap before it --
            // a THIRD instance of the SAME "unwalked AST field lets a
            // package's own reference silently collide with an unrelated
            // same-named definition" root cause in this one file.
            // Live-confirmed: `dep`'s private `fun default_flag() -> Bool {
            // true }`, referenced only as `pub fun classify(valid: Bool =
            // default_flag())`'s default, collided with an UNRELATED
            // root-level `fun default_flag() -> Bool { false }` --
            // `dep.classify()` (omitting the default) printed "dep-invalid"
            // instead of the correct "dep-valid" (dep's OWN default_flag()
            // returns true), with `kupl check` reporting ZERO diagnostics,
            // identically wrong on interp/KVM/native. Walked HERE, BEFORE
            // `self.push()` binds this function's own params into scope
            // below -- a default is evaluated at the CALL SITE, in the
            // CALLER's scope (K0280 already rejects a default referencing
            // a SIBLING parameter of the same function for exactly this
            // reason), so mangling it must NOT treat this function's own
            // params as locally in scope.
            if let Some(d) = &mut p.default {
                self.expr(d);
            }
        }
        if let Some(r) = &mut f.ret {
            self.ty(r);
        }
        // ai fun tool names refer to top-level funs
        if let Some(ai) = &mut f.ai {
            for t in &mut ai.tools {
                if let Some(m) = self.name(t) {
                    *t = m;
                }
            }
            self.expr(&mut ai.intent_expr);
        }
        self.push();
        for p in &f.params {
            self.bind(&p.name);
        }
        self.block(&mut f.body);
        self.pop();
    }

    fn component(&mut self, c: &mut ComponentDecl) {
        if let Some(m) = self.rename.get(&c.name) {
            c.name = m.clone();
        }
        for fc in &mut c.fulfills {
            if let Some(m) = self.name(fc) {
                *fc = m;
            }
        }
        for p in &mut c.ports {
            self.ty(&mut p.ty);
        }
        for p in &mut c.props {
            self.ty(&mut p.ty);
            if let Some(d) = &mut p.default {
                self.expr(d);
            }
        }
        for s in &mut c.state {
            if let Some(t) = &mut s.ty {
                self.ty(t);
            }
            self.expr(&mut s.init);
        }
        // A prop/state field is referenced by BARE name inside handler
        // bodies, exposed/private-method bodies, child-instantiation args,
        // and example blocks below -- but until this fix (PR-it684) those
        // names were never bound into `self.scope`, so `is_local` never saw
        // them. If this SAME package also happens to define a top-level
        // `fun`/`type`/`component`/`contract` with the identical bare name
        // (legal: different namespaces), `self.name(n)` fell through to the
        // rename map and incorrectly mangled the reference. Confirmed live:
        // a component `state counter: Int` alongside a top-level
        // `fun counter()` in the same package made `counter += 1` inside a
        // handler fail with "unknown variable `pkg$counter`" (mangled, no
        // longer matching the un-mangled state field), and a bare `counter`
        // read elsewhere silently resolved to the mangled TOP-LEVEL FUN
        // instead -- a genuine wrong-value substitution, not just a clean
        // "unknown name," surfaced downstream as a confusing type mismatch.
        // Prop DEFAULTS and state INIT expressions above are evaluated
        // BEFORE this scope opens (a default/init can't reference the
        // component's own not-yet-constructed state), matching how a
        // constructor's own field defaults can't reference sibling fields.
        self.push();
        // A REAL bug found+fixed (production-hardening PR-it961, survey
        // #111's close-read of resolve.rs, the SAME "unbound name shadowed
        // by an in-scope binding this pass doesn't know about" bug family
        // as the props/state fix immediately above (PR-it684): an OUT
        // port's bare name is ALSO read as an ordinary local variable
        // inside an `example { ... expect PORT == ... }` block --
        // `run.rs::run_example`/`check.rs::check_example` both bind it to
        // the port's last-emitted value, a real, documented language
        // feature -- but `c.ports` was never bound into `self.scope` here,
        // only its TYPE was walked (`self.ty(&mut p.ty)` above). If this
        // SAME package also happens to define a top-level `fun`/`type`/
        // constructor/`component`/`contract` with the identical bare name
        // as a port (legal: ports are a different namespace), `self.name`
        // fell through to the rename map and silently mangled the port
        // reference to `pkg$name` instead -- confirmed live via a
        // `component Gauge { out Go: Signal ... example { expect Go ==
        // Stop } }` alongside a colliding `type Signal = Go | Stop` in the
        // SAME dependency package: `kupl check` reported zero diagnostics,
        // but `kupl test` on a consuming package showed `FAIL dep$Gauge
        // example: \`dep$Go == dep$Stop\` was not satisfied` -- the
        // diagnostic text itself proving the port reference was wrongly
        // rewritten to the colliding constructor, permanently severing the
        // assertion from the port's actual emitted value (silent value
        // corruption, not the "loud unresolved-name error" this file's own
        // top-of-file doc comment claims is the worst case). The
        // byte-identical component compiled standalone (no package
        // involved, so `isolate` never mangles it) passes cleanly,
        // isolating the bug to cross-package mangling specifically. Bound
        // unconditionally for BOTH `in` and `out` ports, matching props/
        // state's own unconditional-binding style, even though only an
        // OUT port's name is currently known to be read as a bare
        // expression (an `in` port's name in `send NAME(...)` is parsed as
        // a plain string field, never an `Expr` this pass walks at all) --
        // binding a name that happens to go unread is always harmless.
        for p in &c.ports {
            self.bind(&p.name);
        }
        for p in &c.props {
            self.bind(&p.name);
        }
        for s in &c.state {
            self.bind(&s.name);
        }
        // A SIBLING instance of the SAME PR-it961 bug shape as ports above,
        // found by auditing every other component-local binding source per
        // this campaign's own "audit every analogous site" convention: a
        // CHILD's own instance name (`let helper = Widget()`) is likewise
        // read as a bare local identifier -- `helper.value()` inside a
        // handler/method/example body -- but was never bound into
        // `self.scope` either, only `ch.component` (the TYPE being
        // constructed) got rewritten. Confirmed live via a child instance
        // named `helper` alongside a colliding top-level `fun helper()` in
        // the SAME dependency package: `helper.value()` resolved to the
        // MANGLED top-level function instead of the child instance,
        // failing with `K0249: fn() -> Int has no method 'value'` on a
        // consuming package (a LOUD error in this specific repro, since a
        // function value has no such method -- but the SAME underlying
        // mis-binding could just as easily manifest as SILENT corruption
        // if the colliding top-level entity happened to have a
        // method-compatible shape, exactly like the ports case above).
        // Bound in its own loop, before children's constructor `args` are
        // walked, matching props/state's own "bind everything first, walk
        // usages after" ordering (harmless even for a child referencing
        // itself in its own args, a case check.rs's real type-checker
        // rejects anyway -- this pass only decides local-vs-mangled).
        for ch in &c.children {
            self.bind(&ch.name);
        }
        for ch in &mut c.children {
            if let Some(m) = self.name(&ch.component) {
                ch.component = m;
            }
            for a in &mut ch.args {
                self.expr(&mut a.value);
            }
        }
        // wire/supervise endpoints are instance names (component-local), not
        // package-level, so they are not rewritten.
        for h in &mut c.handlers {
            self.push();
            if let Some(p) = &h.param {
                self.bind(p);
            }
            self.block(&mut h.body);
            self.pop();
        }
        for f in c.exposes.iter_mut().chain(c.funs.iter_mut()) {
            self.method(f);
        }
        for ex in &mut c.examples {
            self.push();
            for step in &mut ex.steps {
                match step {
                    ExampleStep::Send { arg: Some(a), .. } => self.expr(a),
                    ExampleStep::Expect { expr, .. } => self.expr(expr),
                    _ => {}
                }
            }
            self.pop();
        }
        self.pop();
    }

    fn contract(&mut self, c: &mut ContractDecl) {
        if let Some(m) = self.rename.get(&c.name) {
            c.name = m.clone();
        }
        for sig in &mut c.sigs {
            for p in &mut sig.params {
                self.ty(&mut p.ty);
            }
            if let Some(r) = &mut sig.ret {
                self.ty(r);
            }
        }
        for law in &mut c.laws {
            self.push();
            self.block(&mut law.body);
            self.pop();
        }
    }

    fn ty(&mut self, t: &mut TyExpr) {
        match &mut t.kind {
            TyExprKind::Name(n) => {
                if let Some(m) = self.rename.get(n) {
                    *n = m.clone();
                }
            }
            TyExprKind::Generic(n, args) => {
                if let Some(m) = self.rename.get(n) {
                    *n = m.clone();
                }
                for a in args {
                    self.ty(a);
                }
            }
            TyExprKind::Fun(ps, r) => {
                for p in ps {
                    self.ty(p);
                }
                self.ty(r);
            }
        }
    }

    fn block(&mut self, b: &mut Block) {
        // a block introduces its own binding frame (sequential lets)
        self.push();
        for s in &mut b.stmts {
            self.stmt(s);
        }
        self.pop();
    }

    fn stmt(&mut self, s: &mut Stmt) {
        match s {
            Stmt::Let { name, ty, init, .. } => {
                if let Some(t) = ty {
                    self.ty(t);
                }
                self.expr(init);
                self.bind(name); // in scope for later statements
            }
            Stmt::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            Stmt::Expr(e) => self.expr(e),
            Stmt::Return(Some(e), _) => self.expr(e),
            Stmt::Return(None, _) => {}
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
            Stmt::Emit { arg: None, .. } => {}
            Stmt::Expect(e, _) => self.expr(e),
            Stmt::Forall { vars, body, .. } => {
                self.push();
                for (v, t) in vars.iter_mut() {
                    self.ty(t);
                    self.bind(v);
                }
                self.block(body);
                self.pop();
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        // handle the package-qualified forms first (they replace the whole kind)
        if let Some(new_kind) = self.try_qualified(e) {
            e.kind = new_kind;
        }
        match &mut e.kind {
            ExprKind::Ident(n) => {
                if let Some(m) = self.name(n) {
                    *n = m;
                }
            }
            ExprKind::Str(pieces) => {
                for p in pieces {
                    if let StrPiece::Expr(inner) = p {
                        self.expr(inner);
                    }
                }
            }
            ExprKind::List(xs) => xs.iter_mut().for_each(|x| self.expr(x)),
            ExprKind::Par(xs) => xs.iter_mut().for_each(|x| self.expr(x)),
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
                    self.pattern(&mut arm.pattern);
                    // A REAL, LIVE-CONFIRMED bug found+fixed (production-
                    // hardening PR-it841): this loop never visited
                    // `arm.guard` (the `if COND` clause of `x if COND =>
                    // body`, a fully general `Option<Expr>` -- parsed,
                    // type-checked, compiled, and interpreted throughout the
                    // pipeline), so a reference inside a match guard to one
                    // of THIS package's own definitions was never mangled to
                    // `pkg$name` -- staying bare while every OTHER reference
                    // in the same function got rewritten. Since a mangled
                    // package's definitions and the (never-mangled) root
                    // package's definitions share ONE flat namespace in
                    // check.rs/interp.rs's function maps (this module's own
                    // top-of-file doc comment), a leftover bare name inside a
                    // guard could silently resolve to a DIFFERENT, same-named
                    // function elsewhere in the program instead of erroring
                    // -- directly contradicting this module's own documented
                    // invariant ("A missed rewrite surfaces as a loud
                    // unresolved-name error, never silent divergence") and
                    // matching PR-it698's severity class exactly (silent
                    // cross-package function invocation), just via a
                    // different root cause (an unwalked AST field, not the
                    // alias-resolution logic PR-it698 fixed). Live-confirmed:
                    // a `dep` package's private `fun is_valid(x: Int) -> Bool
                    // { x > 0 }`, referenced only inside `pub fun
                    // classify(x)`'s match guard (`n if is_valid(n) => ...`),
                    // collided with an UNRELATED root-level `fun
                    // is_valid(x: Int) -> Bool { x < 0 }` of opposite
                    // meaning -- `dep.classify(5)` printed "dep-invalid"
                    // instead of the correct "dep-valid" (5 > 0 per dep's OWN
                    // is_valid), with `kupl check` reporting ZERO
                    // diagnostics, identically wrong on interp/KVM/native
                    // since `resolve::isolate()` runs once upstream of all
                    // three. Pattern bindings are pushed into scope by
                    // `self.pattern` just above, so the guard sees them (a
                    // guard can legitimately reference pattern-bound
                    // variables, e.g. `Some(n) if n > 0 => ...`), matching
                    // `interp.rs`'s own pattern-then-guard evaluation order.
                    if let Some(g) = &mut arm.guard {
                        self.expr(g);
                    }
                    self.expr(&mut arm.body);
                    self.pop();
                }
            }
            ExprKind::Lambda { params, body } => {
                self.push();
                for p in params.iter_mut() {
                    if let Some(t) = &mut p.ty {
                        self.ty(t);
                    }
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
            ExprKind::Try(inner) | ExprKind::Await(inner) => self.expr(inner),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::SizedInt(..)
            | ExprKind::F32(_) => {}
        }
    }

    /// If `e` is a package-qualified access `alias.name` (`alias` a dependency,
    /// not shadowed by a local), rewrite it to a bare `resolved$name`
    /// reference -- `resolved` is `alias`'s DEPENDENCY's own resolved
    /// mangling prefix, NOT the bare `alias` text itself. This distinction is
    /// load-bearing (production-hardening PR-it698, a REAL namespace-
    /// isolation-bypass bug found+fixed): two UNRELATED nested dependencies
    /// can each independently choose the SAME local alias for their own
    /// (different) sub-dependency (e.g. both `depA` and `depB` `use shared`,
    /// pointing at two entirely different physical packages) -- using the
    /// bare alias text here, as this used to, would mangle both packages'
    /// references to the SAME `shared$name` regardless of prefix uniqueness
    /// upstream, silently invoking whichever definition happened to load
    /// last. `self.deps` maps THIS package's own alias table to each
    /// dependency's actual (now-unique) resolved prefix, so the reference
    /// always lands on the SAME definition the DEFINING side was mangled
    /// under.
    fn try_qualified(&self, e: &Expr) -> Option<ExprKind> {
        let is_dep = |recv: &Expr| matches!(&recv.kind, ExprKind::Ident(a) if self.deps.contains_key(a) && !self.is_local(a));
        // A REAL bug found+fixed (production-hardening PR-it746): `resolved`
        // is EMPTY exactly when `alias` refers back to the ROOT package (a
        // dependency cycle looping through root, now resolvable at all since
        // the loader's own PR-it746 companion fix) -- `isolate()` above never
        // mangles root's own items (`if prefix.is_empty() { continue; }`), so
        // they keep bare, unmangled names. Unconditionally formatting
        // `"{resolved}${name}"` produced `"$name"` (a literal leading `$`
        // with an EMPTY prefix) in that case, which matches NO defined item
        // anywhere -- root's own `compute` is registered as `"compute"`, not
        // `"$compute"`. Mirror `isolate()`'s own empty-prefix special case.
        let qualify = |resolved: &str, name: &str| {
            if resolved.is_empty() { name.to_string() } else { format!("{resolved}${name}") }
        };
        match &e.kind {
            // `alias.method(args)` -> `resolved$method(args)` (or bare `method(args)`
            // when `resolved` is root's empty prefix).
            //
            // A REAL, latent bug found+fixed ALONGSIDE the main PR-it915 fix
            // (production-hardening PR-it915, survey #71): this rewrite used
            // to unconditionally discard each argument's own name
            // (`Arg { name: None, value }`) when reconstructing the `Call`
            // node -- `dep.Widget(shade: 1)` is a cross-package QUALIFIED
            // CONSTRUCTOR call (this rewrite exists specifically to turn it
            // into `dep$Widget(...)`, an ordinary constructor call), which
            // legitimately supports named args exactly like a same-package
            // `Widget(shade: 1)` does -- but named-arg info never survived
            // this far even BEFORE `MethodCall.args` widened to `Vec<Arg>`
            // (PR-it915), since the parser itself used to discard it first.
            // Now that `args` already carries names correctly, simply
            // preserving them here restores full named-arg support for
            // cross-package constructor calls, matching the same-package
            // case.
            ExprKind::MethodCall { recv, name, args } if is_dep(recv) => {
                let ExprKind::Ident(a) = &recv.kind else { return None };
                let resolved = self.deps.get(a).map(String::as_str).unwrap_or(a);
                Some(ExprKind::Call {
                    callee: Box::new(Expr {
                        kind: ExprKind::Ident(qualify(resolved, name)),
                        span: recv.span,
                    }),
                    args: args.clone(),
                })
            }
            // `alias.name` used as a value / callee -> `resolved$name` (or bare `name`).
            ExprKind::Field { recv, name } if is_dep(recv) => {
                let ExprKind::Ident(a) = &recv.kind else { return None };
                let resolved = self.deps.get(a).map(String::as_str).unwrap_or(a);
                Some(ExprKind::Ident(qualify(resolved, name)))
            }
            _ => None,
        }
    }

    fn pattern(&mut self, p: &mut Pattern) {
        match &mut p.kind {
            PatternKind::Bind(n) => self.bind(n),
            PatternKind::Ctor { name, args } => {
                if let Some(m) = self.rename.get(name) {
                    *name = m.clone();
                }
                for a in args {
                    self.pattern(a);
                }
            }
            // A REAL bug found+fixed (production-hardening PR-it775, an
            // Explore survey finding, agentId ad3c3f6ee2f0cd891, independently
            // re-verified live before implementing): `Or`/`At` fell into the
            // catch-all `_ => {}` below, so a `Ctor` pattern NESTED inside
            // either -- `A | B` (each alternative), or `name @ SUBPATTERN`
            // (the inner pattern) -- never got its constructor name mangled,
            // unlike an identical `Ctor` pattern one level up. Since
            // `isolate()` (loader.rs, called before check/compile/interp)
            // mangles a non-root package's OWN constructor definitions to
            // `pkg$Name`, a dependency package matching its OWN type via `A |
            // B` or `name @ pat` kept the BARE pattern name while the
            // constructor itself got mangled -- a guaranteed mismatch.
            // Confirmed live: `type Shape = Circle | Square; fun classify(s:
            // Shape) -> Str { match s { Circle | Square => "known" } }`
            // compiled and ran fine (`known`) as a plain single-file program,
            // but failed with K0257 (non-exhaustive match: missing
            // `shapes$Circle`, `shapes$Square`) and TWO K0254 (unknown
            // constructor) errors when the IDENTICAL source was loaded as a
            // dependency package -- a real language feature broken
            // specifically by the package-isolation pass, not a silent
            // divergence (matching this module's own stated invariant, "a
            // missed rewrite surfaces as a loud unresolved-name error"), but
            // still a genuine correctness bug: legitimate code using `|`/`@`
            // patterns against a package's own types cannot compile as a
            // dependency at all. `At`'s `name` is a binding exactly like
            // `PatternKind::Bind` above (`name @ SUBPATTERN` binds `name` to
            // the whole matched value) and was ALSO never registered via
            // `self.bind()` -- a second, adjacent gap sharing the same
            // root cause, fixed in the same arm.
            PatternKind::Or(alts) => {
                for a in alts {
                    self.pattern(a);
                }
            }
            PatternKind::At { name, inner } => {
                self.bind(name);
                self.pattern(inner);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL bug found+fixed (production-hardening PR-it628): confirms
    /// `demangle_for_display` correctly reverses this module's OWN mangling
    /// scheme (documented at the top of this file) for user-facing display,
    /// without needing to touch the internal (still-mangled) representation
    /// anywhere else. Also confirms a name with NO mangling prefix (the
    /// common case — most types live in the root/never-mangled package, or
    /// are builtins) passes through completely unchanged.
    #[test]
    fn demangle_for_display_strips_only_the_mangling_prefix() {
        assert_eq!(demangle_for_display("math$Point"), "Point");
        // a name with no `$` at all (the common case) is unchanged
        assert_eq!(demangle_for_display("Point"), "Point");
        assert_eq!(demangle_for_display(""), "");
        // only the LAST `$`-delimited segment is kept, in case some future
        // extension ever produces more than one level of prefixing
        assert_eq!(demangle_for_display("outer$inner$Point"), "Point");
    }
}
