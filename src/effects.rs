//! Effect inference and enforcement.
//!
//! Rules (v0.2):
//! - Effects are inferred bottom-up over the call graph (fixpoint, so recursion
//!   and mutual recursion converge).
//! - Builtins carry effects: `print` uses `io`. `panic`/`to_str` are pure.
//! - `pub` functions and `expose` functions MUST declare every effect they use
//!   (boundary explicitness). Private functions and handlers may stay implicit.
//! - A declared effect covers itself and its sub-effects: declaring `db`
//!   covers `db.read`; declaring `db.read` does not cover `db.write`.
//! - Declared-but-unused effects on pub/expose produce a warning.
//!
//! Limitation (documented): effects of calls through closures/variables are not
//! tracked in v0.2 — that needs effect types in `fn(...)`, planned with KIR.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::*;
use crate::diag::Diag;

type EffectSet = BTreeSet<String>;

/// A function's identity: top-level name, or `Component.fun`.
fn fun_key(component: Option<&str>, name: &str) -> String {
    match component {
        Some(c) => format!("{c}.{name}"),
        None => name.to_string(),
    }
}

/// Every component method name (exposed or private) declared ANYWHERE in the
/// program, unqualified -- used by `collect_expr` to tell a genuine (if
/// unresolvable without type info) component-instance method call apart from
/// an ordinary builtin VALUE method (`.len()`, `.push()`, `.map()`, ...),
/// which is never itself a component method name in any realistic program.
/// See `collect_expr`'s doc comment (production-hardening PR-it707).
fn component_method_names(program: &Program) -> HashSet<&str> {
    let mut names = HashSet::new();
    for item in &program.items {
        if let Item::Component(c) = item {
            for f in c.exposes.iter().chain(&c.funs) {
                names.insert(f.name.as_str());
            }
        }
    }
    names
}

/// The names of `decl`'s parameters whose declared type is a function type
/// (`fn(...) -> ...`) -- used by `collect_expr` to scope the K0303 "call
/// through an unverifiable function value" warning EXACTLY to a call through
/// one of the enclosing function's own function-typed parameters, rather
/// than to every plain call this pass cannot otherwise resolve
/// (production-hardening PR-it750 v2). An earlier version of this fix
/// instead EXCLUDED known component names from a much broader "any
/// unresolved plain call" gate (to avoid false-positiving on `Counter()`
/// component-constructor calls, which parse identically to an ordinary
/// plain function call) -- live-confirmed via `cargo test --lib effects::`
/// to STILL false-positive on any PURE builtin called as a plain call this
/// module doesn't separately special-case (e.g. `to_str(x)`): a bare
/// builtin name is not a user function, not a component name, and not
/// resolved by `builtin_effects` either (which lists only EFFECTFUL
/// builtins), so it fell through as "unresolved" just like a genuine
/// function-typed parameter would. Matching ONLY the enclosing function's
/// OWN declared function-typed parameters is both narrower AND simpler: it
/// needs no builtin/component allow-list at all, and it exactly targets the
/// "laundering risk" this warning exists for -- a value of function type
/// invoked without knowing what it points to. A call through a LOCAL
/// variable of function type (`let f = some_fn; f()`) is still not covered
/// -- consistent with this file's own top-of-file "Limitation (documented)"
/// note that closures/variables need real effect types to track safely,
/// not something inferable from syntax alone.
fn fn_typed_param_names(decl: &FunDecl) -> HashSet<&str> {
    decl.params
        .iter()
        .filter(|p| matches!(p.ty.kind, TyExprKind::Fun(..)))
        .map(|p| p.name.as_str())
        .collect()
}

pub fn check_effects(program: &Program) -> Vec<Diag> {
    let mut diags = Vec::new();

    // ---- collect every function body with its scope ----
    struct FunInfo<'a> {
        decl: &'a FunDecl,
        component: Option<&'a str>,
        must_declare: bool,
    }
    let mut funs: HashMap<String, FunInfo> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Fun(f) => {
                funs.insert(
                    fun_key(None, &f.name),
                    FunInfo { decl: f, component: None, must_declare: f.is_pub },
                );
            }
            Item::Component(c) => {
                for f in &c.exposes {
                    funs.insert(
                        fun_key(Some(&c.name), &f.name),
                        FunInfo { decl: f, component: Some(&c.name), must_declare: true },
                    );
                }
                for f in &c.funs {
                    funs.insert(
                        fun_key(Some(&c.name), &f.name),
                        FunInfo { decl: f, component: Some(&c.name), must_declare: f.is_pub },
                    );
                }
            }
            Item::Type(_) | Item::Contract(_) | Item::Law(_) => {}
        }
    }

    let method_names = component_method_names(program);

    // ---- direct effects + call edges per function ----
    let mut direct: HashMap<String, EffectSet> = HashMap::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    // Which functions make a PLAIN call through a value this pass cannot
    // resolve to a builtin/component-local/top-level function -- i.e. a call
    // through a function-typed parameter or local (production-hardening
    // PR-it750, closing a real soundness gap: see `collect_expr`'s doc
    // comment for why this is tracked SEPARATELY from the broader
    // `unresolved` flag `pure_funs()` uses, rather than reusing it here).
    let mut plain_call_unresolved: HashMap<String, bool> = HashMap::new();
    for (key, info) in &funs {
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        // K0301/K0302 deliberately don't act on the BROADER "unresolved
        // call" flag (see `collect_expr`'s doc comment) -- discarded, not
        // accumulated. The NARROWER `plain_call` flag below is tracked and
        // DOES feed K0303/K0302 (see the enforcement loop).
        let mut unresolved = false;
        let mut plain_call = false;
        let fn_params = fn_typed_param_names(info.decl);
        // `ai fun` performs the `ai` effect; the keyword itself declares it.
        if info.decl.ai.is_some() {
            eff.insert("ai".to_string());
        }
        // A REAL bug found+fixed (production-hardening PR-it629), the SAME
        // missed-traversal-site shape and severity as it569 (a function
        // referenced by name, not called directly) and it584 (a match arm's
        // guard expression): a parameter's DEFAULT VALUE (`x: Int = EXPR`,
        // `Param::default`) is evaluated on every call that omits that
        // argument -- a REAL, observable execution path -- but was never
        // walked here, only `decl.body` was. Confirmed via a live repro
        // BEFORE this fix: `pub fun greet(x: Int = noisy())` (where `noisy`
        // calls `print`) was accepted with NO `uses io` requirement at all,
        // and calling `greet()` with the argument omitted genuinely printed
        // the undeclared side effect at runtime -- a real boundary-
        // explicitness violation, not just a missing diagnostic.
        for p in &info.decl.params {
            if let Some(d) = &p.default {
                walk_expr(d, &mut |expr| {
                    collect_expr(
                        expr,
                        info.component,
                        &funs,
                        &method_names,
                        &fn_params,
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call,
                    );
                });
            }
        }
        // A REAL bug found+fixed (production-hardening PR-it689), the SAME
        // missed-traversal-site shape as it569/it584/it629 in this same
        // file: an `ai fun`'s `tools [f, g]` clause names top-level
        // functions the MODEL may genuinely invoke mid-conversation (a real
        // execution path -- `ai.rs`'s tool loop actually calls them) -- but
        // was never walked here, only `decl.body` was (and an `ai fun`'s
        // body can ONLY ever be `intent "..."` / `model "..."`, per K0119's
        // grammar restriction, so `tools` is the ONLY way an `ai fun` can
        // indirectly perform an effect beyond `ai` itself). Confirmed via a
        // live repro BEFORE this fix: `pub ai fun summarize(text: Str) -> Str
        // tools [do_write]` (where `do_write` calls `print`, `uses io`)
        // compiled with NO `uses io` requirement on `summarize` at all --
        // the general mechanism DOES correctly require it for an ordinary
        // function that calls `do_write` directly, confirming this was
        // specifically the `tools` traversal that was missing, not the
        // enforcement mechanism itself. Tool names are always TOP-LEVEL
        // functions (`check.rs::resolve_ai_tools`'s own scope), so this
        // looks up `fun_key(None, tool)` directly rather than trying the
        // component-local lookup `collect_expr` does for an ordinary call.
        if let Some(ai) = &info.decl.ai {
            for tool in &ai.tools {
                let top_level = fun_key(None, tool);
                if funs.contains_key(&top_level) {
                    calls.push(top_level);
                }
            }
        }
        walk_block(&info.decl.body, &mut |expr| {
            collect_expr(
                expr,
                info.component,
                &funs,
                &method_names,
                &fn_params,
                &mut eff,
                &mut calls,
                &mut unresolved,
                &mut plain_call,
            );
        });
        let _ = unresolved; // deliberately unused here -- see comment above
        plain_call_unresolved.insert(key.clone(), plain_call);
        direct.insert(key.clone(), eff);
        edges.insert(key.clone(), calls);
    }

    // ---- fixpoint: propagate effects along call edges ----
    let mut inferred: HashMap<String, EffectSet> = direct.clone();
    loop {
        let mut changed = false;
        for (key, callees) in &edges {
            let mut acc = inferred.get(key).cloned().unwrap_or_default();
            let before = acc.len();
            for callee in callees {
                if let Some(ce) = inferred.get(callee) {
                    acc.extend(ce.iter().cloned());
                }
            }
            if acc.len() != before {
                inferred.insert(key.clone(), acc);
                changed = true;
            } else {
                inferred.insert(key.clone(), acc);
            }
        }
        if !changed {
            break;
        }
    }

    // ---- enforce boundary explicitness ----
    for (key, info) in &funs {
        let used = inferred.get(key).cloned().unwrap_or_default();
        let mut declared: Vec<&str> = info.decl.effects.iter().map(String::as_str).collect();
        // the `ai` keyword on the signature IS the boundary declaration
        if info.decl.ai.is_some() && !declared.contains(&"ai") {
            declared.push("ai");
        }
        if info.must_declare {
            let missing: Vec<&String> = used
                .iter()
                .filter(|u| !declared.iter().any(|d| covers(d, u)))
                .collect();
            if !missing.is_empty() {
                let names: Vec<String> = missing.iter().map(|m| m.to_string()).collect();
                diags.push(Diag::error(
                    "K0301",
                    format!(
                        "`{}` is public but does not declare its effects — add `uses {}`",
                        info.decl.name,
                        names.join(", ")
                    ),
                    info.decl.span,
                ));
            }
        }
        // Gated by `must_declare`, EXACTLY like K0301 above -- this
        // module's own top-of-file doc comment is explicit that "Private
        // functions and handlers may stay implicit", i.e. a private
        // function is under NO obligation to declare its effects at all,
        // so a warning telling it to `declare uses` for an unverifiable
        // call makes no sense there. Confirmed as a REAL regression in an
        // early version of this very fix via the mandatory examples/*.kupl
        // sweep (production-hardening PR-it750): `examples/collections.kupl`
        // (`bst_insert`/`bst_contains`, private, taking a `cmp: fn(T, T) ->
        // Int` comparator) and `examples/generics.kupl` (`swap_apply`,
        // private, taking `f: fn(T) -> U`) both newly warned K0303 despite
        // being ordinary, idiomatic private higher-order helpers with no
        // boundary-explicitness obligation whatsoever.
        let has_unresolved_plain_call =
            info.must_declare && plain_call_unresolved.get(key).copied().unwrap_or(false);
        // A REAL, live-confirmed HIGH-severity soundness gap found+fixed
        // (production-hardening PR-it750): a PUBLIC/EXPOSED function that
        // plain-calls a value this pass cannot resolve to any known
        // function -- i.e. a call through a FUNCTION-TYPED PARAMETER, the
        // same "laundering risk" `collect_expr`'s own doc comment already
        // named but declined to act on for K0301/K0302 (to avoid also
        // flagging the much more common, unrelated "construct a component,
        // call an exposed method" pattern -- see that comment). Confirmed
        // live BEFORE this fix: `pub fun outer(f: fn() -> Int) -> Int {
        // f() }` compiled with ZERO diagnostics even though `outer(noisy)`
        // (where `noisy` calls `print`) genuinely executed the undeclared
        // `io` effect on `kupl run`/`kupl run --vm`/a compiled `.kx`
        // module -- and, worse, a caller who responsibly wrote `uses io`
        // up front to cover the callback was PUNISHED with a spurious
        // K0302 "declared but unused" warning. This does NOT attempt full
        // HOF effect soundness (that needs effect-typed `fn(...)`
        // signatures, a genuinely bigger language feature -- see this
        // file's own top-of-file "Limitation (documented)" note) --
        // instead, mirroring the EXACT precedent `check.rs`'s K0279 already
        // established for the narrower "closure stored in a component
        // state field" case: a WARNING (not a hard K0301 error, to avoid
        // turning every legitimate, already-widely-used callback-accepting
        // function into a fresh compile error), scoped to ONLY the
        // boundary (`must_declare`) functions K0301/K0302 already cover,
        // plus suppressing K0302 for its own declared effects (a
        // declaration this pass cannot prove is truly unused).
        if has_unresolved_plain_call {
            diags.push(Diag::warning(
                "K0303",
                format!(
                    "`{}` calls a value of function type -- its effects cannot be verified; \
                     declare `uses` for any effect it may perform",
                    info.decl.name
                ),
                info.decl.span,
            ));
        }
        // declared-but-unused (any fun that declares)
        if !has_unresolved_plain_call {
            for d in &declared {
                if !used.iter().any(|u| covers(d, u)) {
                    diags.push(Diag::warning(
                        "K0302",
                        format!("`{}` declares `uses {d}` but never uses it", info.decl.name),
                        info.decl.span,
                    ));
                }
            }
        }
    }

    diags
}

/// Infer the transitive effect set of every function (keyed as in
/// `check_effects`: top-level name, or `Component.fun`), paired with whether
/// its call graph reaches a call this module couldn't resolve to a builtin,
/// component-local method, or top-level function (see `collect_expr`'s doc
/// comment). Exposed so other passes (e.g. the parallel scheduler) can ask
/// which functions are pure.
pub fn infer_effects(program: &Program) -> HashMap<String, (EffectSet, bool)> {
    // key -> (decl body, owning component)
    let mut funs: HashMap<String, (&FunDecl, Option<&str>)> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Fun(f) => {
                funs.insert(fun_key(None, &f.name), (f, None));
            }
            Item::Component(c) => {
                for f in &c.exposes {
                    funs.insert(fun_key(Some(&c.name), &f.name), (f, Some(c.name.as_str())));
                }
                for f in &c.funs {
                    funs.insert(fun_key(Some(&c.name), &f.name), (f, Some(c.name.as_str())));
                }
            }
            Item::Type(_) | Item::Contract(_) | Item::Law(_) => {}
        }
    }

    let method_names = component_method_names(program);

    let mut direct: HashMap<String, EffectSet> = HashMap::new();
    let mut direct_unresolved: HashMap<String, bool> = HashMap::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for (key, (decl, component)) in &funs {
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        let mut unresolved = false;
        // `pure_funs()` only needs the broader `unresolved` flag (already
        // tracked above); this local sink is discarded, matching
        // `check_effects`'s OWN pre-PR-it750 treatment of `unresolved`.
        let mut plain_call_unresolved = false;
        let fn_params = fn_typed_param_names(decl);
        if decl.ai.is_some() {
            eff.insert("ai".to_string());
        }
        // See the identical fix + comment in check_effects above
        // (production-hardening PR-it629): a parameter default value is a
        // real execution path (evaluated whenever that argument is
        // omitted), and `pure_funs()` (built on this function) gates the
        // real-OS-thread par_map/par_filter fast path -- so missing this
        // here isn't just a missing diagnostic, it's the SAME severity
        // class as it569/it584: a function whose body is genuinely pure but
        // whose ONLY impurity lives in a default value used to be wrongly
        // classified pure, letting it run unsynchronized on that fast path.
        for p in &decl.params {
            if let Some(d) = &p.default {
                walk_expr(d, &mut |expr| {
                    collect_expr(
                        expr,
                        *component,
                        &funs,
                        &method_names,
                        &fn_params,
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call_unresolved,
                    );
                });
            }
        }
        walk_block(&decl.body, &mut |expr| {
            collect_expr(
                expr,
                *component,
                &funs,
                &method_names,
                &fn_params,
                &mut eff,
                &mut calls,
                &mut unresolved,
                &mut plain_call_unresolved,
            );
        });
        direct.insert(key.clone(), eff);
        direct_unresolved.insert(key.clone(), unresolved);
        edges.insert(key.clone(), calls);
    }

    let mut inferred = direct;
    let mut inferred_unresolved = direct_unresolved;
    loop {
        let mut changed = false;
        for (key, callees) in &edges {
            let mut acc = inferred.get(key).cloned().unwrap_or_default();
            let before = acc.len();
            let mut unresolved = inferred_unresolved.get(key).copied().unwrap_or(false);
            let unresolved_before = unresolved;
            for callee in callees {
                if let Some(ce) = inferred.get(callee) {
                    acc.extend(ce.iter().cloned());
                }
                if inferred_unresolved.get(callee).copied().unwrap_or(false) {
                    unresolved = true;
                }
            }
            if acc.len() != before || unresolved != unresolved_before {
                changed = true;
            }
            inferred.insert(key.clone(), acc);
            inferred_unresolved.insert(key.clone(), unresolved);
        }
        if !changed {
            break;
        }
    }
    inferred
        .into_iter()
        .map(|(k, eff)| {
            let unresolved = inferred_unresolved.remove(&k).unwrap_or(false);
            (k, (eff, unresolved))
        })
        .collect()
}

/// Top-level functions with NO inferred effects AND no call this module
/// couldn't resolve (see `collect_expr`'s doc comment) — referentially
/// transparent, so safe to evaluate on a worker thread. Component methods
/// are excluded (they can touch instance state); only bare `fun` names are
/// returned.
///
/// A REAL, LIVE-CRASHING bug (production-hardening PR-it707): before the
/// `unresolved` flag existed, this filtered on `eff.is_empty()` alone -- so a
/// bare `fun` that constructs a component and calls an effectful exposed
/// method on it (`let s = SomeComponent()  s.method()`, entirely invisible to
/// `collect_expr`'s call-graph traversal, which has no type information for
/// `s`) was wrongly classified PURE. Dispatched to `parallel.rs`'s real-OS-
/// thread fast path for `xs.par_map(wrapper)`/`xs.par_filter(wrapper)`, where
/// `ProgramImage::worker_db()` deliberately builds each worker with an EMPTY
/// `components` map ("workers stay sequential, no nested threads") -- so the
/// exact same correct program panicked with `unknown name 'X'` the instant
/// the list crossed `THRESHOLD` (256) elements, while succeeding identically
/// via `.map()` or a shorter list. Confirmed live on both `kupl run` and
/// `kupl run --vm` before this fix.
pub fn pure_funs(program: &Program) -> std::collections::HashSet<String> {
    infer_effects(program)
        .into_iter()
        .filter(|(key, (eff, unresolved))| eff.is_empty() && !unresolved && !key.contains('.'))
        .map(|(key, _)| key)
        .collect()
}

/// `db` covers `db` and `db.read`; `db.read` covers only `db.read`.
fn covers(declared: &str, used: &str) -> bool {
    used == declared || used.starts_with(&format!("{declared}."))
}

fn builtin_effects(name: &str) -> Option<&'static str> {
    match name {
        "print" => Some("io"),
        // file I/O — a sub-effect of `io`, so `uses io` covers it and
        // `uses io.fs` is the precise capability
        "read_file" | "write_file" | "append_file" | "delete_file" | "file_exists"
        | "list_dir" | "make_dir" | "remove_dir" => {
            Some("io.fs")
        }
        // reading the environment / command line — another `io` sub-effect
        "env_var" | "args" | "read_line" | "read_all" => Some("io.env"),
        // network access — another `io` sub-effect
        "http_get" | "http_post" | "http_serve" => Some("io.net"),
        "exec" => Some("io.proc"),
        // reading the wall clock — another `io` sub-effect (format_time and the
        // extractors are pure; only `now` observes ambient time)
        "now" => Some("io.time"),
        // stderr output is ordinary `io` (`exit` diverges like `panic`: no effect)
        "eprint" => Some("io"),
        _ => None,
    }
}

fn collect_expr(
    expr: &Expr,
    component: Option<&str>,
    funs: &HashMap<String, impl Sized>,
    component_method_names: &HashSet<&str>,
    fn_typed_params: &HashSet<&str>,
    eff: &mut EffectSet,
    calls: &mut Vec<String>,
    unresolved: &mut bool,
    plain_call_unresolved: &mut bool,
) {
    // A method-call name may be a UFCS call to a top-level function; a plain
    // call names the function directly. Both attribute that function's effects
    // (conservatively — over-attribution is sound). A bare reference to a
    // known function's NAME (passed as a plain value -- e.g. `xs.map(log)`,
    // or stored in a local and passed on) is ALSO treated as a potential
    // call: `funs.contains_key` below only matches genuine function names,
    // and the callee could invoke it at any time, so conservatively
    // attributing the referenced function's effects here is sound (and, for
    // a name that happens to shadow a function with an unrelated local
    // variable, only over-attributes -- never silently drops a real edge).
    // This matters for more than the `uses` diagnostic: `pure_funs()` (which
    // gates the interp/KVM real-thread `par_map`/`par_filter` fast path)
    // used to misclassify a wrapper like `fun w(x) { [x].map(log)... }` as
    // PURE whenever it only referenced `log` by name instead of calling it
    // directly -- letting a genuinely impure function run unsynchronized
    // across real OS threads, producing observable nondeterminism (PR-it569).
    let (call_name, is_plain_call, is_method_call) = match &expr.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Ident(name) => (Some(name.as_str()), true, false),
            _ => (None, false, false),
        },
        ExprKind::MethodCall { name, .. } => (Some(name.as_str()), false, true),
        // A bare name reference is NEVER itself flagged `unresolved` below --
        // see the comment above: it's a deliberately loose, ALWAYS-harmless
        // over-attribution heuristic (an ordinary local/parameter reference,
        // like `x` in `x * 2`, matches here too, and marking every such
        // reference "unresolved" would make `pure_funs()` reject nearly
        // everything).
        ExprKind::Ident(name) => (Some(name.as_str()), false, false),
        _ => (None, false, false),
    };
    let Some(name) = call_name else { return };
    let builtin = builtin_effects(name);
    let mut resolved = builtin.is_some();
    if let Some(e) = builtin {
        eff.insert(e.to_string());
    }
    // component-local fun first, then top-level
    if let Some(c) = component {
        let local = fun_key(Some(c), name);
        if funs.contains_key(&local) {
            calls.push(local);
            return;
        }
    }
    let global = fun_key(None, name);
    if funs.contains_key(&global) {
        calls.push(global);
        resolved = true;
    }
    if resolved {
        return;
    }
    // A REAL, LIVE-CRASHING bug (production-hardening PR-it707): a call this
    // module genuinely cannot resolve to a builtin, a component-local
    // method, or a top-level function used to be silently dropped here with
    // NO signal at all -- the exact same root cause as K0279's closure-field
    // gap (it706), generalized to a THIRD vector K0279 didn't cover:
    // constructing a component and calling an effectful exposed method on it
    // from a bare `fun` (`let s = SomeComponent()  s.method()`), entirely
    // invisible to this module since it has no type information for `s`.
    // `check_effects` (K0301/K0302) intentionally IGNORES this flag (see its
    // call site) -- injecting it into the shared `EffectSet` would make
    // every ordinary function that legitimately constructs a component (an
    // extremely common pattern) newly fail K0301, trading one crash for a
    // much bigger diagnostic-noise regression. `pure_funs()` (see its own
    // doc comment) is the one consumer that needs the STRICT "provably safe"
    // bar this flag provides -- a wrongly-pure classification there doesn't
    // just miss a diagnostic, it panics a live program purely because a list
    // crossed a size threshold (confirmed live, both interp and KVM).
    //
    // A PLAIN call (`f()`) always counts -- there is no ambiguity: a plain
    // call unresolved at this point is either a call through a function-
    // typed local/parameter value (a real, if narrower, laundering risk of
    // its own) or a genuinely undefined name the type checker would have
    // already rejected (so it can't reach `pure_funs()` at all). A METHOD
    // call only counts when its name is ALSO a real component method
    // somewhere in the program -- otherwise it's almost certainly an
    // ordinary builtin VALUE method (`.len()`, `.push()`, `.map()`, ...),
    // which is never itself a component method name in any realistic
    // program; flagging every builtin value-method call too would make
    // `pure_funs()` reject nearly every function that touches a list/string/
    // map at all.
    if is_plain_call || (is_method_call && component_method_names.contains(name)) {
        *unresolved = true;
        // `plain_call_unresolved` is DELIBERATELY narrower than `unresolved`
        // above -- set ONLY for a plain call whose name is one of the
        // ENCLOSING function's own declared function-typed parameters
        // (`fun outer(f: fn() -> Int) { f() }`), NEVER for the method-call/
        // component-instance case, and NEVER for an unresolved plain call to
        // anything else (production-hardening PR-it750 v2). This is exactly
        // the distinction `check_effects`'s own K0301/K0302 pass needs:
        // feeding the BROADER `unresolved` flag straight into K0301/K0302
        // would reintroduce the diagnostic-noise regression PR-it707
        // explicitly avoided for the "construct a component, call an exposed
        // method" pattern (`Counter()` parses identically to an ordinary
        // plain function call). An earlier version of this fix instead
        // excluded known component names from the broader "any unresolved
        // plain call" gate -- live-confirmed via `cargo test --lib effects::`
        // to still false-positive on any PURE builtin called as a plain call
        // (e.g. `to_str(x)`: not a user function, not a component name, and
        // not in `builtin_effects`'s EFFECTFUL-only table either, so it fell
        // through as "unresolved" too). Matching only `fn_typed_params`
        // sidesteps both false positives at once with no allow-list to
        // maintain: neither a component constructor nor any builtin name
        // (pure or effectful) can ever collide with a parameter name the
        // enclosing function itself declared as `fn(...)`-typed.
        // `pure_funs()` deliberately keeps NO such narrowing for the
        // BROADER `unresolved` flag it still consumes unchanged (it wants
        // the strictest possible bar for the real-OS-thread fast path it
        // gates, per its own doc comment) -- this narrowing applies ONLY to
        // the new, K0301/K0302-facing `plain_call_unresolved` flag.
        if is_plain_call && fn_typed_params.contains(name) {
            *plain_call_unresolved = true;
        }
    }
}

/// Walk every expression in a block (including nested blocks/handlers).
pub fn walk_block(block: &Block, f: &mut impl FnMut(&Expr)) {
    for stmt in &block.stmts {
        walk_stmt(stmt, f);
    }
}

fn walk_stmt(stmt: &Stmt, f: &mut impl FnMut(&Expr)) {
    match stmt {
        Stmt::Let { init, .. } => walk_expr(init, f),
        Stmt::Assign { target, value, .. } => {
            walk_expr(target, f);
            walk_expr(value, f);
        }
        Stmt::Expr(e) => walk_expr(e, f),
        Stmt::Return(Some(e), _) => walk_expr(e, f),
        Stmt::Return(None, _) => {}
        Stmt::While { cond, body, .. } => {
            walk_expr(cond, f);
            walk_block(body, f);
        }
        Stmt::For { iter, body, .. } => {
            walk_expr(iter, f);
            walk_block(body, f);
        }
        Stmt::Emit { arg: Some(e), .. } => walk_expr(e, f),
        Stmt::Emit { arg: None, .. } => {}
        Stmt::Expect(e, _) => walk_expr(e, f),
        Stmt::Forall { body, .. } => walk_block(body, f),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn walk_expr(expr: &Expr, f: &mut impl FnMut(&Expr)) {
    f(expr);
    match &expr.kind {
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(e) = p {
                    walk_expr(e, f);
                }
            }
        }
        ExprKind::List(items) => {
            for i in items {
                walk_expr(i, f);
            }
        }
        ExprKind::Call { callee, args } => {
            walk_expr(callee, f);
            for a in args {
                walk_expr(&a.value, f);
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
                // A match arm's `if COND` guard is an arbitrary expression (parsed via
                // the full `parse_expr`) and can contain any call, including an impure
                // one -- but it was never walked here, so an impure call reachable ONLY
                // through a guard was completely invisible to effect inference, in BOTH
                // directions (a caller could omit `uses io` with no K0301, and one that
                // correctly declared it got a spurious K0302 "declared but unused").
                // Since `pure_funs()` shares this exact walk, an impure guard-only call
                // was also misclassified PURE for the par_map/par_filter real-OS-thread
                // fast path -- the same severity class as it569's bug (PR-it584).
                if let Some(g) = &arm.guard {
                    walk_expr(g, f);
                }
                walk_expr(&arm.body, f);
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
        ExprKind::Par(branches) => {
            for b in branches {
                walk_expr(b, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::parser;

    fn diags_for(src: &str) -> Vec<crate::diag::Diag> {
        let (p, d) = parser::parse(src);
        assert!(d.is_empty(), "parse diags: {d:?}");
        super::check_effects(&p)
    }

    #[test]
    fn pub_fun_must_declare_io() {
        let d = diags_for("pub fun greet() {\n    print(\"hi\")\n}\n");
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
    }

    #[test]
    fn declared_io_is_fine() {
        let d = diags_for("pub fun greet() uses io {\n    print(\"hi\")\n}\n");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn effects_propagate_through_private_helpers() {
        let d = diags_for(
            "fun helper() {\n    print(\"hi\")\n}\npub fun outer() {\n    helper()\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
    }

    #[test]
    fn unused_effect_warns() {
        let d = diags_for("pub fun quiet() uses io -> Int {\n    42\n}\n");
        assert!(d.iter().any(|d| d.code == "K0302"), "{d:?}");
    }

    #[test]
    fn warnings_are_deterministic_and_position_sorted() {
        // The effects pass walks a HashMap, so it emits K0302s in an arbitrary
        // order; run::compile must sort them by source position so `kupl run`
        // output is reproducible (and interp==KVM). Compile many times: the
        // warning positions must be identical every time and strictly ascending.
        let src = "fun aa() uses io -> Int { 1 }\nfun bb() uses io -> Int { 2 }\n\
                   fun cc() uses io -> Int { 3 }\nfun main() { let _ = aa() + bb() + cc() }\n";
        let positions = |()| -> Vec<u32> {
            crate::run::compile(src)
                .unwrap()
                .warnings
                .iter()
                .map(|w| w.span.start)
                .collect()
        };
        let first = positions(());
        assert_eq!(first.len(), 3, "expected three K0302 warnings");
        assert!(first.windows(2).all(|p| p[0] < p[1]), "warnings must be position-sorted: {first:?}");
        for _ in 0..25 {
            assert_eq!(positions(()), first, "warning order must be deterministic");
        }
    }

    #[test]
    fn private_funs_stay_implicit() {
        let d = diags_for("fun helper() {\n    print(\"hi\")\n}\n");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn effect_propagates_through_a_function_passed_by_name_to_a_hof() {
        // `collect_expr` used to attribute a function's effects only when its
        // name was the DIRECT callee of a Call/MethodCall node -- a function
        // referenced as a plain VALUE (e.g. `xs.map(log)`, passing `log` by
        // name rather than calling it) was invisible to effect inference
        // entirely, so a `pub fun` that only ever referenced an impure
        // function this way was never required to declare it (PR-it569).
        let d = diags_for(
            "fun log(x: Int) -> Int {\n    print(to_str(x))\n    x\n}\n\
             pub fun outer(xs: List[Int]) -> List[Int] {\n    xs.map(log)\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
        // and the corresponding declaration is accepted with no spurious
        // "declared but unused" K0302 once correctly attributed.
        let ok = diags_for(
            "fun log(x: Int) -> Int {\n    print(to_str(x))\n    x\n}\n\
             pub fun outer(xs: List[Int]) uses io -> List[Int] {\n    xs.map(log)\n}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn pure_funs_excludes_a_function_only_referenced_by_name() {
        // The SAME root cause as the test above has a much higher-severity
        // consequence: `pure_funs()` gates the interp/KVM real-thread
        // `par_map`/`par_filter` fast path (src/parallel.rs), which assumes a
        // "pure" function is safe to run unsynchronized across OS threads. A
        // wrapper that only references an impure function BY NAME (instead
        // of calling it directly) used to be wrongly classified pure,
        // letting genuinely impure work (e.g. `print`) run concurrently and
        // unsynchronized -- observable as run-to-run nondeterministic output
        // interleaving, a real safety violation, not just a missing
        // diagnostic (PR-it569).
        let (p, d) = crate::parser::parse(
            "fun log(x: Int) -> Int {\n    print(to_str(x))\n    x\n}\n\
             fun wrapper(x: Int) -> Int {\n    [x].map(log).get(0).unwrap_or(0)\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(!pure.contains("wrapper"), "wrapper must NOT be classified pure: {pure:?}");
        assert!(pure.contains("pure_double"), "a genuinely pure function must stay pure: {pure:?}");
    }

    #[test]
    fn effect_propagates_through_a_match_arm_guard() {
        // A REAL BUG found+fixed (PR-it584), the SAME severity class as it569's
        // by-name-reference bug: `walk_expr`'s `Match` arm walked each arm's `body` but
        // never its optional `if COND` guard (`MatchArm.guard: Option<Expr>`) -- an
        // arbitrary expression that can contain any call, including an impure one. An
        // impure call reachable ONLY through a guard was completely invisible to effect
        // inference in BOTH directions: a caller could omit `uses io` with no K0301, and
        // one that correctly declared it got a spurious K0302 "declared but unused".
        let d = diags_for(
            "fun noisy(x: Int) -> Bool {\n    print(to_str(x))\n    x > 0\n}\n\
             pub fun outer(x: Int) -> Str {\n    match x {\n        n if noisy(n) => \"pos\"\n        \
             _ => \"other\"\n    }\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
        let ok = diags_for(
            "fun noisy(x: Int) -> Bool {\n    print(to_str(x))\n    x > 0\n}\n\
             pub fun outer(x: Int) uses io -> Str {\n    match x {\n        n if noisy(n) => \"pos\"\n        \
             _ => \"other\"\n    }\n}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn pure_funs_excludes_a_function_only_referenced_in_a_match_guard() {
        // The SAME root cause as the test above, with the SAME higher-severity
        // consequence as it569: `pure_funs()` shares the identical walk, so a wrapper
        // whose only impure call was hidden inside a match guard used to be wrongly
        // classified pure -- letting it run unsynchronized on the real-thread
        // par_map/par_filter fast path (PR-it584).
        let (p, d) = crate::parser::parse(
            "fun noisy(x: Int) -> Bool {\n    print(to_str(x))\n    x > 0\n}\n\
             fun wrapper(x: Int) -> Str {\n    match x {\n        n if noisy(n) => \"pos\"\n        \
             _ => \"other\"\n    }\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(!pure.contains("wrapper"), "wrapper must NOT be classified pure: {pure:?}");
        assert!(pure.contains("pure_double"), "a genuinely pure function must stay pure: {pure:?}");
    }

    #[test]
    fn effect_propagates_through_a_parameter_default_value() {
        // A REAL bug found+fixed (production-hardening PR-it629), the SAME
        // missed-traversal-site shape as it569 (a function referenced by
        // name) and it584 (a match arm's guard): a parameter DEFAULT VALUE
        // (`x: Int = EXPR`) is evaluated on every call that omits that
        // argument -- a real, observable execution path -- but was never
        // walked, only the function BODY was. Confirmed via a live repro
        // BEFORE this fix: `pub fun greet(x: Int = noisy())` (calling
        // `print`) was accepted with NO `uses io` requirement at all, and
        // calling `greet()` with the argument omitted genuinely printed the
        // undeclared side effect at runtime.
        let d = diags_for(
            "fun noisy() -> Int {\n    print(\"hi\")\n    42\n}\n\
             pub fun greet(x: Int = noisy()) -> Int {\n    x\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
        // and the corresponding declaration is accepted with no spurious
        // "declared but unused" K0302 once correctly attributed.
        let ok = diags_for(
            "fun noisy() -> Int {\n    print(\"hi\")\n    42\n}\n\
             pub fun greet(x: Int = noisy()) uses io -> Int {\n    x\n}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn pure_funs_excludes_a_function_whose_only_impurity_is_a_parameter_default() {
        // The SAME root cause as the test above has the SAME higher-severity
        // consequence as it569/it584: `pure_funs()` gates the interp/KVM
        // real-thread `par_map`/`par_filter` fast path (src/parallel.rs). A
        // function whose BODY is genuinely pure but whose ONLY impurity
        // lives in a parameter's default value used to be wrongly
        // classified pure, letting it run unsynchronized on that fast path
        // -- a real safety violation, not just a missing diagnostic.
        let (p, d) = crate::parser::parse(
            "fun noisy() -> Int {\n    print(\"hi\")\n    42\n}\n\
             fun wrapper(x: Int = noisy()) -> Int {\n    x * 2\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(!pure.contains("wrapper"), "wrapper must NOT be classified pure: {pure:?}");
        assert!(pure.contains("pure_double"), "a genuinely pure function must stay pure: {pure:?}");
    }

    /// A REAL, LIVE-CRASHING bug (production-hardening PR-it707): a bare
    /// `fun` that constructs a component and calls an effectful exposed
    /// method on it is entirely invisible to `collect_expr`'s call-graph
    /// traversal (no type information for the constructed value), so
    /// `pure_funs()` used to wrongly classify it PURE -- letting it be
    /// dispatched to `parallel.rs`'s real-OS-thread `par_map`/`par_filter`
    /// fast path, where `ProgramImage::worker_db()` deliberately builds each
    /// worker with an EMPTY `components` map. Confirmed live before this fix:
    /// the exact same correct program panicked with `unknown name 'Sink'`
    /// the instant a list crossed THRESHOLD (256) elements, while succeeding
    /// identically via `.map()` or a shorter list -- found via a nineteenth
    /// research-subagent dispatch instructed to adversarially try to
    /// disprove the orchestrator's own reasoning that this specific vector
    /// was unreachable (the closure-laundering vectors K0279 (it706)
    /// targeted ARE correctly unreachable here, since `pure_funs()` excludes
    /// component methods entirely and a portable-list-of-closures can never
    /// reach the fast path -- but this THIRD, distinct vector was missed).
    #[test]
    fn pure_funs_excludes_a_function_that_constructs_a_component_and_calls_an_effectful_method() {
        let (p, d) = crate::parser::parse(
            "component Sink {\n    intent \"an effectful exposed method\"\n    \
             expose fun boom(x: Int) uses io -> Int {\n        print(\"side effect {x}\")\n        x\n    }\n}\n\
             fun wrapper(x: Int) -> Int {\n    let s = Sink()\n    s.boom(x)\n    x * 2\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(!pure.contains("wrapper"), "wrapper must NOT be classified pure: {pure:?}");
        assert!(pure.contains("pure_double"), "a genuinely pure function must stay pure: {pure:?}");
    }

    /// A coverage-closing test (production-hardening PR-it708, no bug found).
    /// it707's fix was designed to ALSO close a THIRD closure-laundering
    /// vector alongside the two it actually live-reproduced (K0279's
    /// closure-field vector, it707's own component-construction vector): a
    /// function-typed PARAMETER invoked directly (`fun apply(f: fn() -> Int)
    /// -> Int { f() }`). That vector was only ever verified "by design" --
    /// the `is_plain_call` branch in `collect_expr`'s `unresolved` logic
    /// unconditionally marks ANY unresolved plain call, which structurally
    /// covers this case too -- but it707's own memory entry explicitly
    /// flagged it as not yet given its own dedicated live test. This closes
    /// that gap. (Note: this vector was ALREADY unreachable via `par_map`/
    /// `par_filter`'s specific calling convention even before it707 -- a
    /// function-typed list element always fails `to_portable`'s portability
    /// check -- so `pure_funs()` being conservative here costs nothing; this
    /// test locks in the classification itself, not a live crash fix.)
    #[test]
    fn pure_funs_excludes_a_function_that_invokes_its_own_function_typed_parameter() {
        let (p, d) = crate::parser::parse(
            "fun apply(f: fn() -> Int) -> Int {\n    f()\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(!pure.contains("apply"), "apply must NOT be classified pure: {pure:?}");
        assert!(pure.contains("pure_double"), "a genuinely pure function must stay pure: {pure:?}");
    }

    #[test]
    fn effect_propagates_through_an_ai_funs_tools_list() {
        // A REAL bug found+fixed (production-hardening PR-it689), the SAME
        // missed-traversal-site shape as it569/it584/it629 above: an
        // `ai fun`'s `tools [f, g]` clause names top-level functions the
        // MODEL may genuinely invoke mid-conversation -- a real execution
        // path (`ai.rs`'s tool loop actually calls them) -- but was never
        // walked, only `decl.body` was (and an `ai fun`'s body can ONLY
        // ever be `intent "..."` / `model "..."`, so `tools` is the ONLY
        // way an `ai fun` can indirectly perform an effect beyond `ai`
        // itself). Confirmed via a live repro BEFORE this fix: `pub ai fun
        // summarize(text: Str) -> Str tools [do_write]` (where `do_write`
        // calls `print`, `uses io`) was accepted with NO `uses io`
        // requirement on `summarize` at all.
        let d = diags_for(
            "fun do_write(msg: Str) uses io -> Str {\n    print(msg)\n    \"done\"\n}\n\
             pub ai fun summarize(text: Str) -> Str tools [do_write] {\n    \
             intent \"Summarize: {text}\"\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
        // and the corresponding declaration is accepted with no spurious
        // "declared but unused" K0302 once correctly attributed.
        let ok = diags_for(
            "fun do_write(msg: Str) uses io -> Str {\n    print(msg)\n    \"done\"\n}\n\
             pub ai fun summarize(text: Str) uses io -> Str tools [do_write] {\n    \
             intent \"Summarize: {text}\"\n}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        // an `ai fun` with a genuinely PURE tool (or no tools at all) stays
        // unaffected -- this isn't a blanket "ai funs must declare uses"
        // rule, only a correct ATTRIBUTION of what the tool itself does.
        let pure_tool = diags_for(
            "fun square(x: Int) -> Int {\n    x * x\n}\n\
             pub ai fun mathy(text: Str) -> Str tools [square] {\n    intent \"Math: {text}\"\n}\n",
        );
        assert!(pure_tool.is_empty(), "{pure_tool:?}");
        let no_tools = diags_for(
            "pub ai fun classify(text: Str) -> Str {\n    intent \"Classify: {text}\"\n}\n",
        );
        assert!(no_tools.is_empty(), "{no_tools:?}");
    }

    #[test]
    fn a_call_through_a_function_typed_parameter_warns_k0303_and_is_not_silently_pure() {
        // A REAL, live-confirmed HIGH-severity soundness bypass found+fixed
        // (production-hardening PR-it750): a `pub fun` that plain-calls a
        // FUNCTION-TYPED PARAMETER could perform arbitrary effects (I/O,
        // network, ...) with zero required `uses` and zero diagnostic --
        // and a caller who responsibly declared `uses io` up front to cover
        // the callback was actively PUNISHED with a spurious K0302
        // "declared but unused" warning. Confirmed live BEFORE this fix:
        // `pub fun outer(f: fn() -> Int) -> Int { f() }` compiled with ZERO
        // diagnostics even though `outer(noisy)` (where `noisy` performs
        // `print`) genuinely executed the undeclared `io` effect at runtime.
        let bypass = diags_for("pub fun outer(f: fn() -> Int) -> Int {\n    f()\n}\n");
        assert!(
            bypass.iter().any(|d| d.code == "K0303"),
            "a call through a function-typed parameter must warn K0303: {bypass:?}"
        );
        assert!(
            !bypass.iter().any(|d| d.code == "K0301"),
            "K0303 is a warning, not a hard K0301 error: {bypass:?}"
        );

        // Declaring `uses io` to cover the callback must NOT also draw a
        // spurious K0302 "declared but unused" warning -- this pass cannot
        // prove the declaration unused, since it cannot see what `f` does.
        let declared = diags_for("pub fun outer(f: fn() -> Int) uses io -> Int {\n    f()\n}\n");
        assert!(declared.iter().any(|d| d.code == "K0303"), "{declared:?}");
        assert!(
            !declared.iter().any(|d| d.code == "K0302"),
            "a declared effect covering an unverifiable call must not warn K0302: {declared:?}"
        );

        // A component CONSTRUCTOR call (`Counter()`, parsed identically to
        // an ordinary plain function call) must NOT trigger K0303 -- the
        // exact diagnostic-noise regression PR-it707 already avoided for
        // the broader `unresolved` flag, reintroduced here if not excluded.
        let component_ctor = diags_for(
            "component Counter {\n    intent \"c\"\n    state n: Int = 0\n    \
             expose fun bump() -> Int { n }\n}\n\
             pub fun make_and_bump() -> Int {\n    let c = Counter()\n    c.bump()\n}\n",
        );
        assert!(
            !component_ctor.iter().any(|d| d.code == "K0303"),
            "constructing a component must not warn K0303: {component_ctor:?}"
        );

        // A PURE builtin called as a plain call (e.g. `to_str`) must also
        // stay clean -- confirmed as a real regression introduced by an
        // earlier, broader version of this fix (a component-name-only
        // exclusion), which still misclassified any plain call this module
        // doesn't otherwise resolve, including ordinary pure builtins.
        let pure_builtin = diags_for(
            "pub fun describe(x: Int) -> Str {\n    to_str(x)\n}\n",
        );
        assert!(
            !pure_builtin.iter().any(|d| d.code == "K0303"),
            "a plain call to a pure builtin must not warn K0303: {pure_builtin:?}"
        );

        // A PRIVATE function calling its own function-typed parameter must
        // also stay clean -- this module's own top-of-file doc comment is
        // explicit that "Private functions and handlers may stay implicit",
        // i.e. a private function has no boundary-explicitness obligation
        // at all, so K0303 telling it to `declare uses` makes no sense.
        // Confirmed as a REAL regression in an early version of this fix
        // via the mandatory examples/*.kupl sweep (production-hardening
        // PR-it750): `examples/collections.kupl`'s private `bst_insert`/
        // `bst_contains` (taking a `cmp: fn(T, T) -> Int` comparator) and
        // `examples/generics.kupl`'s private `swap_apply` (taking `f:
        // fn(T) -> U`) both newly warned K0303 despite being ordinary,
        // idiomatic private higher-order helpers.
        let private_hof = diags_for(
            "fun apply_cmp[T](a: T, b: T, cmp: fn(T, T) -> Int) -> Int {\n    cmp(a, b)\n}\n",
        );
        assert!(
            !private_hof.iter().any(|d| d.code == "K0303"),
            "a private function calling its own function-typed parameter must not warn K0303: {private_hof:?}"
        );
    }
}
