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

    fn fun(&mut self, f: &mut FunDecl) {
        if let Some(m) = self.rename.get(&f.name) {
            f.name = m.clone();
        }
        for p in &mut f.params {
            self.ty(&mut p.ty);
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
        for p in &c.props {
            self.bind(&p.name);
        }
        for s in &c.state {
            self.bind(&s.name);
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
            self.fun(f);
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
                    self.expr(a);
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
            ExprKind::MethodCall { recv, name, args } if is_dep(recv) => {
                let ExprKind::Ident(a) = &recv.kind else { return None };
                let resolved = self.deps.get(a).map(String::as_str).unwrap_or(a);
                Some(ExprKind::Call {
                    callee: Box::new(Expr {
                        kind: ExprKind::Ident(qualify(resolved, name)),
                        span: recv.span,
                    }),
                    args: args.iter().cloned().map(|value| Arg { name: None, value }).collect(),
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
