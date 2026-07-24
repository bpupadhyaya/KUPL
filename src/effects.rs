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
//! The SAME limitation applies, NARROWER since production-hardening
//! PR-it1129, to a component INSTANCE method call (`s.method()`, as
//! opposed to `s.method()`'s own DECLARATION, which is checked normally):
//! this pass has no per-local-variable TYPE information in general, but
//! `local_component_bindings` (a separate, additive pre-pass) DOES resolve
//! the common, direct `let x = ComponentName(...)` shape precisely, so a
//! `pub`/`expose` function whose only use of an effect flows through THAT
//! shape IS now required to declare it. The gap remains for anything more
//! indirect — a component behind a typed function PARAMETER, stored in a
//! record field/prop, returned from another function, or reassigned mid-
//! function — a DELIBERATE tradeoff for those remaining cases, not an
//! oversight (see `collect_expr`'s own doc comment, production-hardening
//! PR-it707, and `local_component_bindings`'s own doc comment, PR-it1129,
//! for the full conservative-by-construction design; the regression tests
//! beginning with `check_effects_now_catches_a_pub_fun_whose_only_io_flows_through_a_directly_constructed_component_instance`
//! lock in both what is now caught and what still isn't).

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

/// A REAL bug found+fixed (production-hardening PR-it951, found via a
/// breadth-first fuzzing-style survey): a component's `state`/`prop` field
/// initializers (`state n: Int = EXPR`, `prop n: Int = EXPR`) are evaluated
/// on EVERY construction (`Component()`) -- a real, unconditional execution
/// path -- but were never walked by `check_effects`/`infer_effects` at all
/// (unlike a plain function's parameter defaults, PR-it629's own fix).
/// Confirmed live BEFORE this fix: `component Sink { state n: Int =
/// noisy() }` (where `noisy` calls `print`) let a `pub fun` that only
/// CONSTRUCTS a `Sink` (`let s = Sink()`, no method call at all) compile
/// with NO `uses io` requirement, and `kupl run`/`kupl run --vm`/native all
/// genuinely printed the undeclared side effect identically. This is
/// DIFFERENT from the already-documented, deliberately-out-of-scope
/// "component-instance method call" limitation (`collect_expr`'s own doc
/// comment below, PR-it707): a bare `Sink()` construction call names the
/// component directly, by a LEXICALLY RESOLVABLE identifier right there in
/// the AST -- no type information about a variable's instance type is
/// needed, unlike `s.method()`. Fixed by giving each component a synthetic
/// "constructor" node in the SAME call-graph `direct`/`edges` maps every
/// real function already uses, keyed by this function (guaranteed to never
/// collide with a real `fun_key` -- a bare function key is always a plain
/// identifier with no `#`, and a component-method key always contains a
/// literal `.`), and having `collect_expr` resolve a plain call to a known
/// component name into a call edge to that synthetic node, mirroring how it
/// already resolves a plain call to a component-local/top-level function.
fn construct_key(component: &str) -> String {
    format!("{component}#new")
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

/// Every type/ADT constructor name declared ANYWHERE in the program
/// (including the prelude's `Json` variants, since `run.rs::inject_prelude`
/// prepends prelude items to `program.items` before effects analysis runs),
/// PLUS `Option`/`Result`'s own `Some`/`None`/`Ok`/`Err` -- unlike `Json`,
/// these are compiler-intrinsic type names (see `check.rs`'s own hardcoded
/// `["Some", "None", "Ok", "Err"]` list, e.g. around its `builtins`/pattern-
/// exhaustiveness handling) with NO corresponding `Item::Type` AST node at
/// all, so a plain scan of `program.items` alone would miss the single most
/// common constructor pair in idiomatic KUPL code. Used by `collect_expr` to
/// recognize a plain call to a constructor as fully resolved with NO effect
/// and NO call-graph edge, rather than falling through to "unresolved"
/// (production-hardening PR-it953). Unlike `construct_key`'s synthetic per-
/// component node, a constructor needs no such node: `check.rs`'s K0275
/// rejects a default value on ANY constructor field ("defaults only apply
/// to `fun` parameters, not `{Type}`'s fields"), confirmed live -- so every
/// field value at a constructor call site is always an explicit, ordinary
/// argument expression, already walked as an ordinary sub-expression of the
/// same `Call` node by `walk_expr`/`walk_block`, with nothing this function
/// needs to chase down separately (unlike a component's state/prop
/// initializers, which CAN be omitted and filled from a default, per
/// `construct_key`'s own doc comment). Constructing a value is also,
/// unconditionally, side-effect-free in KUPL -- it never touches the
/// runtime's shared instance registry the way constructing a COMPONENT
/// does (`construct_key`'s own `*unresolved = true` requirement) -- so this
/// resolution is unconditionally safe for `pure_funs()` too.
fn type_ctor_names(program: &Program) -> HashSet<&str> {
    let mut names: HashSet<&str> = ["Some", "None", "Ok", "Err"].into_iter().collect();
    for item in &program.items {
        if let Item::Type(t) = item {
            for v in &t.variants {
                names.insert(v.name.as_str());
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
    let component_names: HashSet<&str> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Component(c) => Some(c.name.as_str()),
            _ => None,
        })
        .collect();
    let ctor_names = type_ctor_names(program);

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
                        &component_names,
                        &ctor_names,
                        &fn_params,
                        &HashMap::new(),
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
            // A REAL bug found+fixed (production-hardening PR-it866), the
            // SAME missed-traversal-site shape as it569/it584/it629/it689 in
            // this same file: an `ai fun`'s `intent_expr` (the interpolated
            // `intent "...{expr}..."` string) is evaluated on EVERY call --
            // a real execution path, both `interp.rs::eval` and
            // `compile.rs` genuinely evaluate it -- but was never walked
            // here, only `ai.tools` (it689's own fix) and `decl.body` were.
            // The it689 fix's OWN doc comment above claims "`tools` is the
            // ONLY way an `ai fun` can indirectly perform an effect beyond
            // `ai` itself" -- that reasoning was incomplete: a function
            // called from INSIDE the intent string's own `{...}`
            // interpolation is an equally real, unconditional call site.
            // Confirmed via a live repro BEFORE this fix: `pub ai fun
            // summarize(text: Str) -> Str { intent "...{noisy()}" }` (where
            // `noisy` calls `print`, `uses io`) checked clean with NO `uses
            // io` requirement on `summarize` at all, and `kupl run`
            // genuinely printed the undeclared side effect. A positive
            // control -- the identical `noisy()` call routed through
            // `tools [noisy]` instead of `intent_expr` -- correctly
            // triggered K0301, confirming this was specifically the
            // `intent_expr` traversal that was missing.
            walk_expr(&ai.intent_expr, &mut |expr| {
                collect_expr(
                    expr,
                    info.component,
                    &funs,
                    &method_names,
                    &component_names,
                    &ctor_names,
                    &fn_params,
                    &HashMap::new(),
                    &mut eff,
                    &mut calls,
                    &mut unresolved,
                    &mut plain_call,
                );
            });
        }
        let resolved_methods =
            local_component_bindings(&info.decl.body, &info.decl.params, &component_names);
        walk_block(&info.decl.body, &mut |expr| {
            collect_expr(
                expr,
                info.component,
                &funs,
                &method_names,
                &component_names,
                &ctor_names,
                &fn_params,
                &resolved_methods,
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

    // A synthetic "constructor" node per component, walking `state`/`prop`
    // initializers -- see `construct_key`'s own doc comment (production-
    // hardening PR-it951) for the full rationale. Fed into the SAME
    // `direct`/`edges` maps and fixpoint below as any real function.
    for item in &program.items {
        let Item::Component(c) = item else { continue };
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        let mut unresolved = false;
        let mut plain_call = false;
        for s in &c.state {
            walk_expr(&s.init, &mut |expr| {
                collect_expr(
                    expr,
                    Some(c.name.as_str()),
                    &funs,
                    &method_names,
                    &component_names,
                    &ctor_names,
                    &HashSet::new(),
                    &HashMap::new(),
                    &mut eff,
                    &mut calls,
                    &mut unresolved,
                    &mut plain_call,
                );
            });
        }
        for p in &c.props {
            if let Some(d) = &p.default {
                walk_expr(d, &mut |expr| {
                    collect_expr(
                        expr,
                        Some(c.name.as_str()),
                        &funs,
                        &method_names,
                        &component_names,
                        &ctor_names,
                        &HashSet::new(),
                        &HashMap::new(),
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call,
                    );
                });
            }
        }
        // A REAL, LIVE-CONFIRMED soundness hole found+fixed (production-
        // hardening PR-it1058, found via a background close-read survey of
        // this whole file): this synthetic constructor node used to walk
        // ONLY `state`/`prop` initializers -- `c.children`'s own
        // construction-argument expressions (`let child = Component(args)`)
        // were never walked at all, even though `interp.rs`'s own
        // `instantiate` (the SAME real, unconditional execution path
        // PR-it951's own doc comment establishes as the correct standard
        // for this synthetic node) evaluates every `child.args[i].value`
        // on EVERY construction of the parent, and `compile.rs` compiles
        // them into the component's own init chunk identically. Live-
        // confirmed BEFORE this fix: `component Wrapper { let s =
        // Sink(noisy()) }` where `noisy()` calls `print(...)` -- a `pub
        // fun make_wrapper() { let w = Wrapper() }` (NO `uses io`
        // declared) passed `kupl check` cleanly, and `kupl run` genuinely
        // printed the output -- an undeclared `io` effect crossing a
        // function boundary the checker exists to police. The SAME gap
        // also silently let `pure_funs()` (used by `parallel.rs`'s real-
        // OS-thread `par_map`/`par_filter` fast path) wrongly classify
        // such a function as pure, and produced a false-positive "declares
        // `uses io` but never uses it" (K0302) warning for a developer who
        // responsibly DID declare it -- punishing correct code, the same
        // pattern PR-it750's own comment documents for a different vector.
        // Fixed by walking `c.children`'s own argument expressions too,
        // mirroring the EXACT `state`/`props` pattern above.
        for ch in &c.children {
            for a in &ch.args {
                walk_expr(&a.value, &mut |expr| {
                    collect_expr(
                        expr,
                        Some(c.name.as_str()),
                        &funs,
                        &method_names,
                        &component_names,
                        &ctor_names,
                        &HashSet::new(),
                        &HashMap::new(),
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call,
                    );
                });
            }
        }
        let key = construct_key(&c.name);
        direct.insert(key.clone(), eff);
        edges.insert(key, calls);
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
    let component_names: HashSet<&str> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Component(c) => Some(c.name.as_str()),
            _ => None,
        })
        .collect();
    let ctor_names = type_ctor_names(program);

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
                        &component_names,
                        &ctor_names,
                        &fn_params,
                        &HashMap::new(),
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call_unresolved,
                    );
                });
            }
        }
        let resolved_methods = local_component_bindings(&decl.body, &decl.params, &component_names);
        walk_block(&decl.body, &mut |expr| {
            collect_expr(
                expr,
                *component,
                &funs,
                &method_names,
                &component_names,
                &ctor_names,
                &fn_params,
                &resolved_methods,
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

    // Same synthetic "constructor" node per component as `check_effects` --
    // see `construct_key`'s own doc comment (production-hardening PR-it951).
    for item in &program.items {
        let Item::Component(c) = item else { continue };
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        let mut unresolved = false;
        let mut plain_call_unresolved = false;
        for s in &c.state {
            walk_expr(&s.init, &mut |expr| {
                collect_expr(
                    expr,
                    Some(c.name.as_str()),
                    &funs,
                    &method_names,
                    &component_names,
                    &ctor_names,
                    &HashSet::new(),
                    &HashMap::new(),
                    &mut eff,
                    &mut calls,
                    &mut unresolved,
                    &mut plain_call_unresolved,
                );
            });
        }
        for p in &c.props {
            if let Some(d) = &p.default {
                walk_expr(d, &mut |expr| {
                    collect_expr(
                        expr,
                        Some(c.name.as_str()),
                        &funs,
                        &method_names,
                        &component_names,
                        &ctor_names,
                        &HashSet::new(),
                        &HashMap::new(),
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call_unresolved,
                    );
                });
            }
        }
        // Same `children` construction-argument gap as `check_effects`'s own
        // mirror above -- see that fix's own doc comment for the full
        // writeup (production-hardening PR-it1058).
        for ch in &c.children {
            for a in &ch.args {
                walk_expr(&a.value, &mut |expr| {
                    collect_expr(
                        expr,
                        Some(c.name.as_str()),
                        &funs,
                        &method_names,
                        &component_names,
                        &ctor_names,
                        &HashSet::new(),
                        &HashMap::new(),
                        &mut eff,
                        &mut calls,
                        &mut unresolved,
                        &mut plain_call_unresolved,
                    );
                });
            }
        }
        let key = construct_key(&c.name);
        direct.insert(key.clone(), eff);
        direct_unresolved.insert(key.clone(), unresolved);
        edges.insert(key, calls);
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

/// One lexical scope's own locally-known component-or-shadowed bindings,
/// for `local_component_bindings`'s own scope-stack (production-hardening
/// PR-it1129). `Some(name)` records a local variable as a tracked instance
/// of component `name`; `None` records that the variable is LOCALLY bound
/// to something else entirely (a shadow marker) -- present specifically so
/// a lookup for that name correctly STOPS at this scope level instead of
/// falling through to an OUTER, now-irrelevant tracked entry for the same
/// name (e.g. `let b = Bot()  match x { Some(b) => b.foo()  _ => () }` --
/// the match arm's OWN pattern-bound `b` must shadow the outer `Bot`
/// instance, not inherit it).
type LocalScope = HashMap<String, Option<String>>;

fn lookup_local_component<'a>(scopes: &'a [LocalScope], name: &str) -> Option<&'a str> {
    for scope in scopes.iter().rev() {
        if let Some(entry) = scope.get(name) {
            return entry.as_deref();
        }
    }
    None
}

/// Every name `p` binds, mirroring `compile.rs::bind_pattern_names` (kept
/// as a separate, self-contained copy rather than a cross-module `pub` --
/// this file's own established convention, e.g. `pure_funs`/`collect_expr`
/// never reach into compile.rs either).
fn pattern_bound_names(p: &Pattern, names: &mut Vec<String>) {
    match &p.kind {
        PatternKind::Bind(n) => names.push(n.clone()),
        PatternKind::Ctor { args, .. } => {
            for a in args {
                pattern_bound_names(a, names);
            }
        }
        PatternKind::At { name, inner } => {
            names.push(name.clone());
            pattern_bound_names(inner, names);
        }
        _ => {}
    }
}

/// Tracks LOCAL, syntactically-direct `let x = ComponentName(...)` bindings
/// (and their invalidation on reassignment/shadowing) within `block`'s own
/// lexical scopes, resolving each `x.method()` call site it can prove sound
/// -- production-hardening PR-it1129, implementing PR-it1128's own design
/// to close (for this common, direct-construction shape) the gap PR-it1124
/// found and documented: a component INSTANCE method call was entirely
/// invisible to effect inference, so a `pub fun` could perform real,
/// undeclared `io` (or any other effect) as long as it flowed through
/// `let b = Bot()  b.ask()` rather than a directly-named call.
///
/// Deliberately conservative: only the DIRECT `let x = ComponentName(...)`
/// shape is tracked -- no cross-function type inference, no tracking
/// through an intermediate value or a component stored in a record field/
/// prop/returned from another function, no tracking of typed function
/// PARAMETERS (a natural future extension, left out here to keep this
/// fix's own first version narrowly scoped). Anything more complex than
/// the narrow shape simply isn't recorded, falling back to `collect_expr`'s
/// own existing "unresolved" handling exactly as before this fix -- a
/// missed resolution is always safe (matches pre-fix behavior exactly).
/// This pass can never record a WRONG resolution: `x`'s own tracked
/// component is unconditionally invalidated (via a `None` shadow marker,
/// not merely removed -- see `LocalScope`'s own doc comment for why a
/// marker is required, not just absence) the moment `x`'s name is
/// reassigned (`Stmt::Assign`, legal ONLY for a bare `Ident` target --
/// K0223 already rejects any other assignment-target shape, so this is
/// exhaustive) or shadowed by an inner `let`, loop variable, `forall`
/// variable, lambda parameter, or match-arm pattern binding of the same
/// name. `collect_expr` itself double-checks any resolution this pass
/// produces against `funs` before trusting it (see its own call site) --
/// a second, independent safety net.
///
/// UPDATE (production-hardening PR-it1130): also seeds the OUTERMOST scope
/// with `params` whose own DECLARED type is a plain, literal component name
/// (`fun helper(b: Bot) { b.ask() }`) -- exactly as syntactically provable
/// as the `let`-bound case (no cross-function inference needed, the type is
/// right there in the signature), matching PR-it1128's own design's
/// explicitly-flagged "natural future extension". A parameter is ALWAYS
/// immutable in KUPL (`check.rs` inserts every parameter into scope with
/// `mutable: false`, confirmed via direct source read before implementing
/// this) -- so a well-typed program can never reassign one via
/// `Stmt::Assign` at all, but this pass's own reassignment-invalidation
/// logic above still applies unconditionally regardless (defensive: a
/// syntactically-valid-but-not-yet-type-checked program could still
/// contain such an assignment, and the SAME shadow-marker machinery
/// already handles it correctly with no special-casing needed). A
/// parameter typed as a GENERIC wrapping a component (`Option[Bot]`,
/// `List[Bot]`) or a type PARAMETER (`fun f[T](x: T)`) is correctly, safely
/// NOT tracked -- `TyExprKind::Name` only matches a bare, literal type
/// name, never a `Generic`, and a type parameter's own name essentially
/// never collides with a real component name.
fn local_component_bindings(
    block: &Block,
    params: &[Param],
    component_names: &HashSet<&str>,
) -> HashMap<usize, String> {
    let mut resolved = HashMap::new();
    let mut outer = LocalScope::new();
    for p in params {
        if let TyExprKind::Name(tn) = &p.ty.kind {
            if component_names.contains(tn.as_str()) {
                outer.insert(p.name.clone(), Some(tn.clone()));
            }
        }
    }
    let mut scopes: Vec<LocalScope> = vec![outer];
    scan_block_for_bindings(block, component_names, &mut scopes, &mut resolved);
    resolved
}

fn scan_block_for_bindings(
    block: &Block,
    component_names: &HashSet<&str>,
    scopes: &mut Vec<LocalScope>,
    resolved: &mut HashMap<usize, String>,
) {
    scopes.push(LocalScope::new());
    for stmt in &block.stmts {
        scan_stmt_for_bindings(stmt, component_names, scopes, resolved);
    }
    scopes.pop();
}

fn scan_stmt_for_bindings(
    stmt: &Stmt,
    component_names: &HashSet<&str>,
    scopes: &mut Vec<LocalScope>,
    resolved: &mut HashMap<usize, String>,
) {
    match stmt {
        Stmt::Let { name, init, .. } => {
            scan_expr_for_bindings(init, component_names, scopes, resolved);
            // ALWAYS insert an entry for `name` at the current scope --
            // either the tracked component (a direct `ComponentName(...)`
            // call) or an explicit shadow marker (`None`) otherwise, so a
            // non-component `let` correctly blocks an outer tracked entry
            // of the same name too (`let b = Bot()  { let b = 5  b.foo() }`
            // must NOT resolve `b.foo()` against `Bot`).
            let component = match &init.kind {
                ExprKind::Call { callee, .. } => match &callee.kind {
                    ExprKind::Ident(cn) if component_names.contains(cn.as_str()) => Some(cn.clone()),
                    _ => None,
                },
                _ => None,
            };
            scopes.last_mut().expect("at least one scope").insert(name.clone(), component);
        }
        Stmt::Assign { target, value, .. } => {
            scan_expr_for_bindings(value, component_names, scopes, resolved);
            if let ExprKind::Ident(name) = &target.kind {
                for scope in scopes.iter_mut().rev() {
                    if scope.contains_key(name.as_str()) {
                        scope.insert(name.clone(), None);
                        break;
                    }
                }
            }
        }
        Stmt::Expr(e) => scan_expr_for_bindings(e, component_names, scopes, resolved),
        Stmt::Return(Some(e), _) => scan_expr_for_bindings(e, component_names, scopes, resolved),
        Stmt::Return(None, _) => {}
        Stmt::While { cond, body, .. } => {
            scan_expr_for_bindings(cond, component_names, scopes, resolved);
            scan_block_for_bindings(body, component_names, scopes, resolved);
        }
        Stmt::For { var, iter, body, .. } => {
            scan_expr_for_bindings(iter, component_names, scopes, resolved);
            scopes.push(LocalScope::new());
            scopes.last_mut().expect("just pushed").insert(var.clone(), None);
            for s in &body.stmts {
                scan_stmt_for_bindings(s, component_names, scopes, resolved);
            }
            scopes.pop();
        }
        Stmt::Emit { arg: Some(e), .. } => scan_expr_for_bindings(e, component_names, scopes, resolved),
        Stmt::Emit { arg: None, .. } => {}
        Stmt::Expect(e, _) => scan_expr_for_bindings(e, component_names, scopes, resolved),
        Stmt::Forall { vars, body, .. } => {
            scopes.push(LocalScope::new());
            for (name, _) in vars {
                scopes.last_mut().expect("just pushed").insert(name.clone(), None);
            }
            for s in &body.stmts {
                scan_stmt_for_bindings(s, component_names, scopes, resolved);
            }
            scopes.pop();
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn scan_expr_for_bindings(
    expr: &Expr,
    component_names: &HashSet<&str>,
    scopes: &mut Vec<LocalScope>,
    resolved: &mut HashMap<usize, String>,
) {
    match &expr.kind {
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(e) = p {
                    scan_expr_for_bindings(e, component_names, scopes, resolved);
                }
            }
        }
        ExprKind::List(items) => {
            for i in items {
                scan_expr_for_bindings(i, component_names, scopes, resolved);
            }
        }
        ExprKind::Call { callee, args } => {
            scan_expr_for_bindings(callee, component_names, scopes, resolved);
            for a in args {
                scan_expr_for_bindings(&a.value, component_names, scopes, resolved);
            }
        }
        ExprKind::MethodCall { recv, args, .. } => {
            scan_expr_for_bindings(recv, component_names, scopes, resolved);
            for a in args {
                scan_expr_for_bindings(&a.value, component_names, scopes, resolved);
            }
            if let ExprKind::Ident(name) = &recv.kind {
                if let Some(cn) = lookup_local_component(scopes, name) {
                    resolved.insert(expr as *const Expr as usize, cn.to_string());
                }
            }
        }
        ExprKind::Field { recv, .. } => scan_expr_for_bindings(recv, component_names, scopes, resolved),
        ExprKind::Binary { lhs, rhs, .. } => {
            scan_expr_for_bindings(lhs, component_names, scopes, resolved);
            scan_expr_for_bindings(rhs, component_names, scopes, resolved);
        }
        ExprKind::Unary { operand, .. } => scan_expr_for_bindings(operand, component_names, scopes, resolved),
        ExprKind::If { cond, then_block, else_block } => {
            scan_expr_for_bindings(cond, component_names, scopes, resolved);
            scan_block_for_bindings(then_block, component_names, scopes, resolved);
            if let Some(e) = else_block {
                scan_expr_for_bindings(e, component_names, scopes, resolved);
            }
        }
        ExprKind::BlockExpr(b) => scan_block_for_bindings(b, component_names, scopes, resolved),
        ExprKind::Match { scrutinee, arms } => {
            scan_expr_for_bindings(scrutinee, component_names, scopes, resolved);
            for arm in arms {
                scopes.push(LocalScope::new());
                let mut names = Vec::new();
                pattern_bound_names(&arm.pattern, &mut names);
                for n in names {
                    scopes.last_mut().expect("just pushed").insert(n, None);
                }
                if let Some(g) = &arm.guard {
                    scan_expr_for_bindings(g, component_names, scopes, resolved);
                }
                scan_expr_for_bindings(&arm.body, component_names, scopes, resolved);
                scopes.pop();
            }
        }
        ExprKind::Lambda { params, body } => {
            scopes.push(LocalScope::new());
            for p in params {
                scopes.last_mut().expect("just pushed").insert(p.name.clone(), None);
            }
            for s in &body.stmts {
                scan_stmt_for_bindings(s, component_names, scopes, resolved);
            }
            scopes.pop();
        }
        ExprKind::Range { lo, hi, .. } => {
            scan_expr_for_bindings(lo, component_names, scopes, resolved);
            scan_expr_for_bindings(hi, component_names, scopes, resolved);
        }
        ExprKind::With { recv, updates } => {
            scan_expr_for_bindings(recv, component_names, scopes, resolved);
            for (_, v) in updates {
                scan_expr_for_bindings(v, component_names, scopes, resolved);
            }
        }
        ExprKind::Try(e) | ExprKind::Await(e) => scan_expr_for_bindings(e, component_names, scopes, resolved),
        ExprKind::Par(branches) => {
            for b in branches {
                scan_expr_for_bindings(b, component_names, scopes, resolved);
            }
        }
        _ => {}
    }
}

fn collect_expr(
    expr: &Expr,
    component: Option<&str>,
    funs: &HashMap<String, impl Sized>,
    component_method_names: &HashSet<&str>,
    component_names: &HashSet<&str>,
    ctor_names: &HashSet<&str>,
    fn_typed_params: &HashSet<&str>,
    resolved_methods: &HashMap<usize, String>,
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
    // A REAL fix (production-hardening PR-it1129, implementing PR-it1128's
    // own design): if `local_component_bindings`'s own pre-pass determined
    // THIS SPECIFIC method-call expression's receiver is a locally,
    // syntactically KNOWN component instance (a direct `let x =
    // ComponentName(...)` binding, never reassigned/shadowed since),
    // resolve it PRECISELY to that component's own method -- exactly as if
    // the caller had written `ComponentName().method()` directly -- instead
    // of falling into the broad "unresolved" bucket below. This is the ONE
    // case PR-it707's own "would create diagnostic noise" concern does NOT
    // apply to: unlike a BLANKET "any unresolved method call might have
    // effects" flag (which WOULD over-fire on ordinary, ambiguous method
    // calls), this resolution only ever fires for a target this pass can
    // PROVE, syntactically, is correct -- see `local_component_bindings`'s
    // own doc comment for the full conservative-by-construction design.
    // Double-checks `funs.contains_key` before trusting the pre-pass's own
    // resolution (falls through to the existing unresolved handling if the
    // resolved component doesn't actually have this method -- e.g. a
    // genuine typo `check.rs` would separately reject) -- a second,
    // independent safety net, never trusts the pre-pass blindly.
    if is_method_call {
        if let Some(component_name) = resolved_methods.get(&(expr as *const Expr as usize)) {
            let key = fun_key(Some(component_name), name);
            if funs.contains_key(&key) {
                calls.push(key);
                return;
            }
        }
    }
    // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
    // PR-it933, a close-read survey finding): a function-typed PARAMETER
    // of the enclosing function must ALWAYS shadow a same-named builtin/
    // component-local/top-level function for a PLAIN call -- matching
    // this codebase's established "a local binding shadowing a top-level
    // name must be respected" principle (PR-it894/it915/it931), extended
    // here to effects.rs's OWN call-resolution for the first time. This
    // MUST be checked before resolving against builtins/funs below (moved
    // here from its own former home inside the `is_plain_call || ...`
    // block further down, which only ran AFTER an early `return` on a
    // successful builtin/funs resolution -- so it could never actually
    // fire whenever the parameter's name happened to collide with an
    // existing function, exactly the one case it exists to catch).
    // Live-confirmed: `fun log(x: Int) -> Int { x }` alongside `pub fun
    // apply(log: fn(Int) -> Int, x: Int) -> Int { log(x) }` -- `kupl
    // check` reported ZERO diagnostics (no K0303) for `apply`'s call
    // through its OWN "log" parameter, silently misattributed as calling
    // the unrelated global `log` (a pure function) instead, while an
    // identical control case using a non-colliding parameter name (`cb`)
    // correctly emitted K0303. `pure_funs()` (the real-OS-thread `par_map`/
    // `par_filter` safety gate) also misclassified `apply` as pure in
    // isolation. This finding's PRACTICAL severity is bounded (not a
    // proven data race): any actually-impure closure passed through the
    // parameter is independently still caught by this file's OWN
    // conservative bare-Ident-reference tracking (PR-it569, immediately
    // above) wherever it's referenced by name in the reachable call
    // graph, and `parallel.rs::PortableValue` has no closure/function
    // variant at all (any function VALUE anywhere in a par_map/par_filter
    // payload already forces the sequential fallback) -- but relying on
    // two independently-existing protections to compose correctly by
    // accident is fragile, and effects.rs's OWN classification for this
    // shape was simply wrong regardless of what currently happens to
    // absorb the consequence.
    // A REAL, live-confirmed HIGH-severity soundness hole found+fixed
    // (production-hardening PR-it993, an Explore survey finding): this used
    // to require `is_plain_call` too -- so a function-typed parameter
    // referenced ONLY as a bare VALUE (forwarded to another call, e.g.
    // `xs.map(f)` or `helper(f)`, rather than invoked directly as `f()`)
    // never matched here at all. Unlike an ordinary bare-Ident reference to
    // a REAL function name (the `is_plain_call`-independent it569 heuristic
    // just above, which only fires when the referenced NAME happens to
    // collide with a genuine `funs` entry), a reference to one of THIS
    // function's own declared function-typed PARAMETERS can NEVER collide
    // with a real function this way -- its value is whatever the CALLER
    // happened to pass, unknowable here regardless of how it's used -- so
    // requiring `is_plain_call` bought no safety, it just left every
    // forwarding shape silently unclassified. Live-confirmed BEFORE this
    // fix, via a full three-function call chain with NO `uses` declaration
    // anywhere except the actual `print`-calling leaf: `fun noisy(x: Int)
    // uses io -> Int { print("{x}") x } pub fun bridge1(xs: List[Int], f:
    // fn(Int) -> Int) -> List[Int] { bridge2(xs, f) } fun bridge2(xs:
    // List[Int], f: fn(Int) -> Int) -> List[Int] { xs.map(f) } fun main() {
    // bridge1([1, 2, 3], noisy) }` -- `kupl check` reported "ok" (zero
    // diagnostics, not even the K0303 warning) and `kupl run` printed
    // `1`/`2`/`3`, genuinely executing undeclared `io` all the way up to
    // `main` itself. Fixed by dropping the `is_plain_call` requirement: ANY
    // reference to an enclosing function-typed parameter -- called directly
    // OR merely passed along as a value -- is equally a "laundering risk"
    // this pass cannot verify, matching K0303's own message ("calls a value
    // of function type -- its effects cannot be verified"), which already
    // reads correctly for the forwarding case too (a function that hands an
    // unverifiable callback to something else still cannot have its own
    // effects verified).
    if fn_typed_params.contains(name) {
        *unresolved = true;
        *plain_call_unresolved = true;
        return;
    }
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
    // A component-construction call (`Sink()`) -- resolved AFTER function
    // names (mirroring interp.rs/check.rs's own established priority order
    // for this exact ambiguity, PR-it931), since a plain call's name is
    // unambiguous once it's a real function; only otherwise-unresolved names
    // are checked against known component names. See `construct_key`'s own
    // doc comment (production-hardening PR-it951) for why this differs from
    // the deliberately-out-of-scope component-INSTANCE method-call case.
    //
    // Pushes the call-graph edge (so `check_effects`/K0301 correctly sees
    // whatever the state/prop initializers actually do) AND unconditionally
    // marks `unresolved` -- construction ITSELF (allocating a fresh
    // instance, registering it in the runtime's shared instance registry)
    // is an effect on shared interpreter/runtime state independent of
    // whatever the initializers compute, so `pure_funs()` (which gates the
    // real-OS-thread `par_map`/`par_filter` fast path) must keep excluding
    // ANY function that constructs a component, even one whose state/prop
    // initializers happen to be trivially pure (`state n: Int = 0`) --
    // otherwise this fix would have traded a missing-diagnostic bug for a
    // genuine thread-safety regression (two real threads concurrently
    // constructing instances via a wrongly-"pure"-classified function).
    // `plain_call_unresolved` (the narrower K0303 warning) is deliberately
    // NOT set here -- a construction call is not a call through an
    // unverifiable function-typed VALUE, it's an unambiguous, resolvable
    // component name, matching this file's own established precedent
    // (see `fn_typed_param_names`'s doc comment) that K0303 should not
    // fire on ordinary `Counter()`-style construction.
    if !resolved && is_plain_call && component_names.contains(name) {
        calls.push(construct_key(name));
        resolved = true;
        *unresolved = true;
    }
    // A plain call to a known type/ADT constructor (`Some(x)`, `Wrap(f)`, any
    // user `type` variant) -- see `type_ctor_names`'s own doc comment
    // (production-hardening PR-it953) for why this needs no call-graph edge
    // and no `*unresolved`/`*plain_call_unresolved` marking at all: every
    // field value is an ordinary argument expression already walked
    // separately, and construction itself is unconditionally effect-free.
    if !resolved && is_plain_call && ctor_names.contains(name) {
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
        // above -- it's ONLY ever set for a plain call whose name is one of
        // the ENCLOSING function's own declared function-typed parameters
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
        // `plain_call_unresolved`.
        //
        // The actual `fn_typed_params.contains(name)` check that sets
        // `plain_call_unresolved` now lives EARLIER in this function
        // (production-hardening PR-it933) -- it must run BEFORE the
        // component/top-level function resolution above, since a call
        // through a parameter that happens to collide with an existing
        // function's name would otherwise resolve successfully there and
        // return before ever reaching this point.
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
                walk_expr(&a.value, f);
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

    /// A REAL soundness gap in `check_effects`'s own K0301 enforcement,
    /// found+FIXED for the common case (production-hardening PR-it1124
    /// documented it; PR-it1129 fixes it, implementing PR-it1128's own
    /// design): `collect_expr`'s own `unresolved` flag for a component-
    /// instance method call (`s.method()`) is DELIBERATELY excluded from
    /// the shared `EffectSet` that `check_effects` consults (see
    /// `collect_expr`'s own doc comment on the `is_method_call &&
    /// component_method_names.contains(name)` branch, PR-it707) -- a
    /// DELIBERATE tradeoff, since flagging every unresolved method-call
    /// would force every ordinary function that legitimately constructs a
    /// component to declare effects it may not have. FIXED (for the common,
    /// direct-construction shape) by `local_component_bindings` (PR-it1129):
    /// a separate, additive pre-pass tracks the NARROW `let x =
    /// ComponentName(...)` shape via a lexical scope-stack, resolving
    /// `x.method()` PRECISELY to that component's own method instead of the
    /// blanket "unresolved" bucket -- see that function's own doc comment
    /// for the full conservative-by-construction design (a missed
    /// resolution is always safe; a WRONG resolution is structurally
    /// impossible, since `x`'s tracked component is unconditionally
    /// invalidated by reassignment or ANY shadowing, including a nested
    /// `let`, loop/`forall` variable, lambda parameter, or match-arm
    /// pattern binding of the same name). Confirmed live with the exact
    /// program below: `kupl check` now correctly reports K0301 for
    /// `sneaky`, and `kupl run` (once `uses io` is added) prints "bot
    /// speaking" as expected. This does NOT close the gap for every shape
    /// -- a component instance behind a typed function PARAMETER, stored in
    /// a record field/prop, returned from another function, or reassigned
    /// mid-function is still unresolved and STILL not required to declare
    /// its effects (see the sibling tests below for each of these residual
    /// cases, each asserting the CURRENT, still-conservative behavior) --
    /// `docs/PRODUCTION.md`'s own "Known gap" callout (updated this same
    /// iteration) is narrowed accordingly rather than removed.
    #[test]
    fn check_effects_now_catches_a_pub_fun_whose_only_io_flows_through_a_directly_constructed_component_instance() {
        let d = diags_for(
            "component Bot {\n    intent \"does IO when asked\"\n    \
             expose fun ask() uses io {\n        print(\"bot speaking\")\n    }\n}\n\
             pub fun sneaky() {\n    let b = Bot()\n    b.ask()\n}\n",
        );
        assert!(
            d.iter().any(|d| d.code == "K0301"),
            "PR-it1129's own fix must now catch this case: {d:?}"
        );
    }

    /// A legitimate `let`-bound component instance calling a PURE method
    /// must NOT be spuriously flagged -- `local_component_bindings`'s own
    /// resolution must be PRECISE (attributing exactly the resolved
    /// method's own effects), not a blanket "any component method might
    /// have effects" over-approximation, which would reintroduce PR-it707's
    /// own diagnostic-noise regression for the common case.
    #[test]
    fn a_directly_constructed_component_instance_calling_a_pure_method_is_not_flagged() {
        let d = diags_for(
            "component Quiet {\n    intent \"pure\"\n    \
             expose fun ask() -> Int {\n        42\n    }\n}\n\
             pub fun quiet_caller() -> Int {\n    let q = Quiet()\n    q.ask()\n}\n",
        );
        assert!(d.is_empty(), "a pure method call must not be spuriously flagged: {d:?}");
    }

    /// Shadowing a tracked component binding with a DIFFERENT component (a
    /// nested `let` of the same name) must correctly switch the resolution
    /// to the INNER, shadowing component -- not the outer one. Uses two
    /// components with a same-named method but DIFFERENT effects (`Bot.ask`
    /// performs `io`, `Quiet.ask` is pure) so a WRONG resolution (still
    /// attributing the outer `Bot`) would be directly observable as a
    /// spurious K0301.
    #[test]
    fn shadowing_a_tracked_component_binding_resolves_to_the_inner_shadowing_component() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             component Quiet {\n    intent \"pure\"\n    expose fun ask() -> Int {\n        42\n    }\n}\n\
             pub fun shadow_case() -> Int {\n    let b = Bot()\n    let b = Quiet()\n    b.ask()\n}\n",
        );
        assert!(
            d.is_empty(),
            "must resolve to the INNER `Quiet` binding (pure), not the shadowed outer `Bot`: {d:?}"
        );
    }

    /// A match arm's own PATTERN-bound name must shadow an outer tracked
    /// component binding of the same name -- `local_component_bindings`
    /// must seed each arm's own pushed scope with every name the arm's
    /// pattern binds (mirroring `compile.rs::bind_pattern_names`), not just
    /// track `let`/`for`/lambda-parameter bindings. Without this, `b.ask()`
    /// inside the arm would wrongly resolve against the OUTER `Bot`
    /// (io-effectful) instead of correctly falling back to unresolved for
    /// the pattern-bound `Quiet` (this test doesn't need `Quiet` to
    /// resolve — just that `Bot` is NOT wrongly attributed).
    #[test]
    fn a_match_arm_pattern_binding_shadows_an_outer_tracked_component_of_the_same_name() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             component Quiet {\n    intent \"pure\"\n    expose fun ask() -> Int {\n        42\n    }\n}\n\
             pub fun match_shadow_case(x: Option[Quiet]) -> Int {\n    let b = Bot()\n    \
             match x {\n        Some(b) => b.ask()\n        None => 0\n    }\n}\n",
        );
        assert!(
            d.is_empty(),
            "the pattern-bound `b` (Quiet) must shadow the outer `b` (Bot): {d:?}"
        );
    }

    /// Reassigning a tracked local (`var b = Bot()  b = Bot()`) must
    /// unconditionally drop tracking -- even though both assignments
    /// happen to construct the SAME component type, `Stmt::Assign`
    /// invalidates the binding regardless (a deliberately conservative
    /// choice: re-tracking would need to re-verify the RHS is ALSO a
    /// direct `ComponentName(...)` call at the assignment site, adding
    /// complexity for a rarer pattern than the basic `let`-and-use shape
    /// this fix targets). This test locks in that the residual gap
    /// (reassigned locals stay unresolved, matching pre-PR-it1129
    /// behavior) is exactly as documented, not silently wider or narrower.
    #[test]
    fn reassigning_a_tracked_local_drops_tracking_even_to_the_same_component_type() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             pub fun reassign_case() {\n    var b = Bot()\n    b = Bot()\n    b.ask()\n}\n",
        );
        assert!(
            !d.iter().any(|d| d.code == "K0301"),
            "documents the residual, deliberately-conservative gap for reassigned locals: {d:?}"
        );
    }

    /// A component-typed function PARAMETER (`fun helper(b: Bot) { b.ask() }`)
    /// is resolvable exactly as precisely as a direct `let`-bound instance --
    /// production-hardening PR-it1130, extending PR-it1129's own fix per
    /// PR-it1128's own design's explicitly-flagged "natural future
    /// extension" (a parameter's own DECLARED type is as syntactically
    /// provable as a `let`'s own direct `ComponentName(...)` initializer, no
    /// cross-function inference needed).
    #[test]
    fn a_component_typed_parameter_calling_an_effectful_method_is_now_caught() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             pub fun helper(b: Bot) {\n    b.ask()\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "PR-it1130's own fix must catch this case: {d:?}");
    }

    /// A LOCAL `let` binding must correctly SHADOW a component-typed
    /// parameter of the SAME name, resolving to the INNER, shadowing
    /// component instead of the outer parameter -- discriminating test
    /// (same technique as PR-it1129's own `Bot`/`Quiet` shadow test): if
    /// the parameter's OWN tracked type wrongly survived the shadow, this
    /// would spuriously require `uses io` even though the REAL `b.ask()`
    /// call resolves to the local `Quiet` (pure).
    #[test]
    fn a_local_let_shadows_a_component_typed_parameter_of_the_same_name() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             component Quiet {\n    intent \"pure\"\n    expose fun ask() -> Int {\n        42\n    }\n}\n\
             pub fun helper(b: Bot) -> Int {\n    let b = Quiet()\n    b.ask()\n}\n",
        );
        assert!(
            d.is_empty(),
            "must resolve to the INNER `Quiet` local, not the shadowed `Bot` parameter: {d:?}"
        );
    }

    /// A parameter typed as a GENERIC wrapping a component (`Option[Bot]`)
    /// -- as opposed to a bare, literal component name -- is correctly,
    /// safely NOT tracked (`TyExprKind::Name` never matches a `Generic`
    /// variant); the wrapped instance, once unwrapped via a match-arm
    /// pattern, is ALSO not tracked (pattern-bound names are only ever
    /// shadow-markers, never resolved) -- both fall back to the existing,
    /// pre-fix conservative "unresolved" handling, a residual gap
    /// documented at `docs/PRODUCTION.md`'s own "Known gap" callout, not a
    /// bug. Exists mainly as a no-crash smoke test for `TyExprKind::Generic`
    /// (a param type shape `local_component_bindings` must never panic on).
    #[test]
    fn a_generic_wrapped_component_parameter_is_not_tracked_but_does_not_crash() {
        let d = diags_for(
            "component Bot {\n    intent \"io\"\n    expose fun ask() uses io {\n        print(\"x\")\n    }\n}\n\
             pub fun helper(b: Option[Bot]) {\n    match b {\n        Some(inst) => inst.ask()\n        None => ()\n    }\n}\n",
        );
        assert!(
            !d.iter().any(|d| d.code == "K0301"),
            "documents the residual, deliberately-conservative gap for a generic-wrapped param: {d:?}"
        );
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

    /// A REAL bug found+fixed (production-hardening PR-it951, found via a
    /// breadth-first fuzzing-style survey): a component's `state`/`prop`
    /// field initializers are evaluated on EVERY construction, a real,
    /// unconditional execution path -- but were never walked by
    /// `check_effects` at all (unlike a plain function's parameter
    /// defaults, PR-it629). Confirmed live BEFORE this fix: a `pub fun`
    /// that only CONSTRUCTS a component with an effectful `state`
    /// initializer (no method call needed at all) compiled with NO `uses
    /// io` requirement, and `kupl run`/`kupl run --vm`/native all genuinely
    /// printed the undeclared side effect identically. This is DIFFERENT
    /// from PR-it707's already-documented, deliberately-out-of-scope
    /// "component-instance method call" limitation: a bare `Sink()`
    /// construction call names the component directly, by a LEXICALLY
    /// RESOLVABLE identifier right there in the AST -- no type information
    /// about a variable's instance type is needed.
    #[test]
    fn effectful_component_state_and_prop_initializers_require_a_declared_effect_on_construction() {
        // state field initializer
        let d = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    state n: Int = noisy()\n    \
             expose fun get() -> Int { n }\n}\n\
             pub fun make_and_get() -> Int {\n    let s = Sink()\n    s.get()\n}\n",
        );
        assert!(
            d.iter().any(|d| d.code == "K0301"),
            "a pub fun constructing a component with an effectful state initializer must \
             require `uses io`: {d:?}"
        );
        // prop default value, same shape
        let d2 = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    prop n: Int = noisy()\n    \
             expose fun get() -> Int { n }\n}\n\
             pub fun make_and_get() -> Int {\n    let s = Sink()\n    s.get()\n}\n",
        );
        assert!(
            d2.iter().any(|d| d.code == "K0301"),
            "a pub fun constructing a component with an effectful prop DEFAULT must \
             require `uses io`: {d2:?}"
        );
        // bare construction alone (no method call at all) must ALSO trigger it.
        let d3 = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    state n: Int = noisy()\n}\n\
             pub fun just_construct() {\n    let s = Sink()\n}\n",
        );
        assert!(
            d3.iter().any(|d| d.code == "K0301"),
            "bare construction alone (no method call) must still require `uses io`: {d3:?}"
        );
        // once correctly declared, no diagnostic at all.
        let ok = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    state n: Int = noisy()\n    \
             expose fun get() -> Int { n }\n}\n\
             pub fun make_and_get() uses io -> Int {\n    let s = Sink()\n    s.get()\n}\n",
        );
        assert!(ok.is_empty(), "correctly declared `uses io` must check clean: {ok:?}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it1058, found via a
    /// background close-read survey of this whole file): the SAME synthetic
    /// constructor node PR-it951 fixed above for `state`/`prop` initializers
    /// never walked a `ChildDecl`'s own construction-ARGUMENT expressions
    /// (`let child = Component(args)`) -- a real, unconditional execution
    /// path (`interp.rs::instantiate` evaluates every `child.args[i].value`
    /// on every construction of the parent; `compile.rs` compiles them into
    /// the parent's own init chunk identically). Confirmed live BEFORE this
    /// fix: `pub fun make_wrapper() { let w = Wrapper() }` (where `Wrapper`
    /// has `let s = Sink(noisy())` and `noisy` calls `print`) checked clean
    /// with NO `uses io` requirement, and `kupl run` genuinely printed the
    /// undeclared side effect. The same gap also produced a false-positive
    /// K0302 ("declares `uses io` but never uses it") for a caller who
    /// responsibly DID declare it up front.
    #[test]
    fn effectful_component_child_construction_arguments_require_a_declared_effect_on_construction() {
        let d = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    prop n: Int\n    \
             expose fun get() -> Int { n }\n}\n\
             component Wrapper {\n    intent \"w\"\n    let s = Sink(noisy())\n}\n\
             pub fun make_wrapper() {\n    let w = Wrapper()\n}\n",
        );
        assert!(
            d.iter().any(|d| d.code == "K0301"),
            "a pub fun constructing a component whose child's own construction \
             argument is effectful must require `uses io`: {d:?}"
        );
        // once correctly declared, no diagnostic at all -- including no
        // false-positive K0302 "declared but unused" (the second symptom
        // this same fix resolves).
        let ok = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    42\n}\n\
             component Sink {\n    intent \"s\"\n    prop n: Int\n    \
             expose fun get() -> Int { n }\n}\n\
             component Wrapper {\n    intent \"w\"\n    let s = Sink(noisy())\n}\n\
             pub fun make_wrapper() uses io {\n    let w = Wrapper()\n}\n",
        );
        assert!(ok.is_empty(), "correctly declared `uses io` must check clean, with no K0302: {ok:?}");
    }

    /// A REAL safety consideration caught DURING this same fix (production-
    /// hardening PR-it951): the naive version of the fix above (just adding
    /// a call-graph edge for construction) would have let `pure_funs()`
    /// wrongly classify a function that constructs a component with a
    /// GENUINELY PURE state initializer (e.g. `state n: Int = 0`, no calls
    /// at all) as pure -- but construction ITSELF (allocating a fresh
    /// instance, registering it in the runtime's shared instance registry)
    /// is an effect on shared interpreter/runtime state independent of
    /// whatever the initializers compute, matching this file's OWN
    /// established precedent (PR-it707, the test above) that ANY function
    /// constructing a component must stay conservatively excluded from the
    /// real-OS-thread `par_map`/`par_filter` fast path. Fixed by
    /// unconditionally marking `unresolved` at a construction call site (in
    /// ADDITION to pushing the call-graph edge K0301 needs), independent of
    /// whether the initializer itself is provably pure.
    #[test]
    fn pure_funs_excludes_a_function_that_constructs_a_component_even_with_a_trivially_pure_state_initializer() {
        let (p, d) = crate::parser::parse(
            "component Trivial {\n    intent \"t\"\n    state n: Int = 0\n    \
             expose fun get() -> Int { n }\n}\n\
             fun wrapper(x: Int) -> Int {\n    let t = Trivial()\n    x * 2\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(
            !pure.contains("wrapper"),
            "a function that merely CONSTRUCTS a component -- even one with a trivially pure \
             state initializer -- must NOT be classified pure, since construction itself \
             touches the runtime's shared instance registry: {pure:?}"
        );
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

    /// A REAL performance gap found+fixed (production-hardening PR-it953,
    /// found while independently re-verifying survey #107's investigation
    /// into `par_map`/`par_filter` thread-safety): `collect_expr` never
    /// resolved a plain call to a known type/ADT constructor name at all
    /// (`Item::Type(_) => {}` in both `check_effects`/`infer_effects`'s own
    /// per-item loops), so constructing ANY value -- a user ADT, or even
    /// `Some(x)`/`Ok(x)` -- fell through to the generic "unresolved plain
    /// call" branch, unconditionally marking `unresolved = true`. This
    /// never caused a missed diagnostic (K0301 doesn't act on the broader
    /// `unresolved` flag at all -- effects nested in a constructor's own
    /// ARGUMENTS were always correctly walked and attributed as separate
    /// sub-expressions regardless), but it DID mean `pure_funs()` -- which
    /// gates the real-OS-thread `par_map`/`par_filter` fast path -- wrongly
    /// excluded essentially any function using `Option`/`Result` or a
    /// custom ADT, even when otherwise trivially pure. Live-confirmed
    /// BEFORE this fix via a CPU-bound timing probe (a 100k-iteration spin
    /// loop over 2000 list elements): a plain pure callback ran at ~1650%
    /// CPU (real 16x thread parallelism engaged) while the IDENTICAL
    /// workload wrapped in `Some(...).unwrap_or(...)` ran at 99% CPU
    /// (sequential fallback) -- confirmed identically for both a
    /// user-defined ADT constructor and `Some`/`Ok`. Distinguished from a
    /// genuine safety concern (this file's own `construct_key` precedent
    /// for COMPONENT construction, which unconditionally marks
    /// `unresolved` because it touches the runtime's shared instance
    /// registry): constructing an ordinary data value has no such shared
    /// state, and `check.rs`'s K0275 ("constructor field cannot have a
    /// default value") independently guarantees no OMITTED-and-defaulted
    /// field can hide an unwalked expression the way a component's
    /// state/prop defaults could (PR-it951) -- confirmed live via
    /// `kupl check` rejecting `type Widget = Widget(n: Int = 5)` outright.
    #[test]
    fn pure_funs_includes_a_function_that_only_constructs_option_or_a_user_adt() {
        let (p, d) = crate::parser::parse(
            "type Wrap = Wrap(v: Int)\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n\
             fun wraps_user_adt(x: Int) -> Int {\n    let w = Wrap(pure_double(x))\n    \
             match w {\n        Wrap(v) => v\n    }\n}\n\
             fun wraps_option(x: Int) -> Int {\n    Some(pure_double(x)).unwrap_or(0)\n}\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(
            pure.contains("wraps_user_adt"),
            "a function that only constructs a user ADT around an otherwise-pure call must be \
             classified pure, unlocking the real-thread par_map/par_filter fast path: {pure:?}"
        );
        assert!(
            pure.contains("wraps_option"),
            "a function that only constructs Some(...) around an otherwise-pure call must be \
             classified pure: {pure:?}"
        );
    }

    /// The negative-control sibling of the test above: an effect genuinely
    /// nested INSIDE a constructor's own argument must still be correctly
    /// attributed and both fail `pure_funs()` and require `uses io` under
    /// K0301 -- confirming the new ctor-resolution branch only short-
    /// circuits the constructor call ITSELF, never the sub-expressions
    /// `walk_expr`/`walk_block` already walk independently.
    #[test]
    fn effect_nested_inside_a_constructor_argument_is_still_attributed() {
        let d = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    1\n}\n\
             pub fun wrapper() -> Option[Int] {\n    Some(noisy())\n}\n",
        );
        assert!(
            d.iter().any(|d| d.code == "K0301"),
            "an effectful call nested inside Some(...)'s own argument must still require \
             `uses io`: {d:?}"
        );
        let ok = diags_for(
            "fun noisy() -> Int {\n    print(\"boom\")\n    1\n}\n\
             pub fun wrapper() uses io -> Option[Int] {\n    Some(noisy())\n}\n",
        );
        assert!(ok.is_empty(), "correctly declared `uses io` must check clean: {ok:?}");

        let (p, d2) = crate::parser::parse(
            "fun noisy() -> Int {\n    print(\"boom\")\n    1\n}\n\
             fun wrapper() -> Option[Int] {\n    Some(noisy())\n}\n\
             fun pure_double(x: Int) -> Int { x * 2 }\n",
        );
        assert!(d2.is_empty(), "parse diags: {d2:?}");
        let pure = super::pure_funs(&p);
        assert!(
            !pure.contains("wrapper"),
            "a function whose Some(...) argument is genuinely effectful must NOT be classified \
             pure: {pure:?}"
        );
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
    fn effect_propagates_through_an_ai_funs_intent_interpolation() {
        // A REAL bug found+fixed (production-hardening PR-it866), the SAME
        // missed-traversal-site shape as it569/it584/it629/it689 above: an
        // `ai fun`'s `intent_expr` (the interpolated `intent
        // "...{expr}..."` string) is evaluated on EVERY call -- a real
        // execution path, both `interp.rs::eval` and `compile.rs` genuinely
        // evaluate it -- but was never walked, only `ai.tools` (it689's own
        // fix) and `decl.body` were. it689's own doc comment claimed
        // "`tools` is the ONLY way an `ai fun` can indirectly perform an
        // effect beyond `ai` itself" -- that reasoning was incomplete: a
        // function called from INSIDE the intent string's own `{...}`
        // interpolation is an equally real call site. Confirmed via a live
        // repro BEFORE this fix: `pub ai fun summarize(text: Str) -> Str {
        // intent "...{noisy()}" }` (where `noisy` calls `print`, `uses io`)
        // checked clean with NO `uses io` requirement on `summarize` at
        // all, and `kupl run` genuinely printed the undeclared side effect.
        let d = diags_for(
            "fun noisy() uses io -> Str {\n    print(\"side effect\")\n    \"logged\"\n}\n\
             pub ai fun summarize(text: Str) -> Str {\n    \
             intent \"Summarize: {text} note: {noisy()}\"\n}\n",
        );
        assert!(d.iter().any(|d| d.code == "K0301"), "{d:?}");
        // and the corresponding declaration is accepted with no spurious
        // "declared but unused" K0302 once correctly attributed.
        let ok = diags_for(
            "fun noisy() uses io -> Str {\n    print(\"side effect\")\n    \"logged\"\n}\n\
             pub ai fun summarize(text: Str) uses io -> Str {\n    \
             intent \"Summarize: {text} note: {noisy()}\"\n}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        // an `ai fun` whose intent calls a genuinely PURE function (or calls
        // nothing at all) stays unaffected -- this isn't a blanket "ai funs
        // must declare uses" rule, only a correct ATTRIBUTION of what the
        // interpolated call itself does.
        let pure_call = diags_for(
            "fun square(x: Int) -> Int {\n    x * x\n}\n\
             pub ai fun mathy(n: Int) -> Str {\n    intent \"Math: {square(n)}\"\n}\n",
        );
        assert!(pure_call.is_empty(), "{pure_call:?}");
        let no_call = diags_for(
            "pub ai fun classify(text: Str) -> Str {\n    intent \"Classify: {text}\"\n}\n",
        );
        assert!(no_call.is_empty(), "{no_call:?}");
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

    #[test]
    fn forwarding_a_function_typed_parameter_as_a_value_warns_k0303_not_just_calling_it_directly() {
        // A REAL, live-confirmed HIGH-severity soundness hole found+fixed
        // (production-hardening PR-it993, an Explore survey finding, ONE
        // HOP past PR-it750's own fix above): that fix only ever matched a
        // function-typed parameter used as a PLAIN CALL's OWN callee
        // (`f()`) -- a parameter merely PASSED as a value to something else
        // (`xs.map(f)`, or forwarded into another function's own parameter,
        // `helper(f)`) was invisible to `collect_expr` entirely, not even
        // triggering the file's OWN "bare Ident reference" over-attribution
        // heuristic (which only fires when the referenced NAME happens to
        // collide with a REAL function -- a parameter's value is whatever
        // the CALLER passed, so its own name can never collide that way).
        // Confirmed live BEFORE this fix, via a full THREE-FUNCTION call
        // chain with `uses io` declared ONLY on the actual `print`-calling
        // leaf: `fun noisy(x: Int) uses io -> Int { print("{x}") x } pub
        // fun bridge1(xs: List[Int], f: fn(Int) -> Int) -> List[Int] {
        // bridge2(xs, f) } fun bridge2(xs: List[Int], f: fn(Int) -> Int) ->
        // List[Int] { xs.map(f) } fun main() { bridge1([1, 2, 3], noisy) }`
        // -- `kupl check` reported "ok" (ZERO diagnostics, not even K0303)
        // and `kupl run` genuinely printed `1`/`2`/`3`, executing undeclared
        // `io` all the way up through `bridge1` to `main` itself with no
        // signal anywhere in the compile.
        let forwarded_to_method_call = diags_for(
            "pub fun run_with(xs: List[Int], f: fn(Int) -> Int) -> List[Int] {\n    xs.map(f)\n}\n",
        );
        assert!(
            forwarded_to_method_call.iter().any(|d| d.code == "K0303"),
            "a function-typed parameter forwarded to .map() must warn K0303, not compile silently: \
             {forwarded_to_method_call:?}"
        );
        let forwarded_to_another_fun = diags_for(
            "fun helper(f: fn() -> Int) -> Int {\n    f()\n}\n\
             pub fun outer(f: fn() -> Int) -> Int {\n    helper(f)\n}\n",
        );
        assert!(
            forwarded_to_another_fun.iter().any(|d| d.code == "K0303" && d.message.contains("outer")),
            "a function-typed parameter forwarded to another function must warn K0303 on the \
             FORWARDING function, not compile silently: {forwarded_to_another_fun:?}"
        );
        // The genuinely pure control case (no function-typed parameter
        // involved at all) must stay completely clean -- this fix must not
        // become an over-broad "any List[T] method warns" regression.
        let genuinely_pure = diags_for(
            "pub fun double_all(xs: List[Int]) -> List[Int] {\n    xs.map(fn(x) { x * 2 })\n}\n",
        );
        assert!(
            !genuinely_pure.iter().any(|d| d.code == "K0303"),
            "a plain lambda with no function-typed PARAMETER forwarding must not warn K0303: {genuinely_pure:?}"
        );
    }

    #[test]
    fn a_function_typed_parameter_shadowing_an_existing_function_still_warns_k0303_and_is_unresolved() {
        // A REAL, live-confirmed bug found+fixed (production-hardening
        // PR-it933, a close-read survey finding): when a function-typed
        // parameter's NAME happens to collide with an existing top-level
        // function, `collect_expr` used to resolve the call against that
        // UNRELATED function FIRST (returning early before ever reaching
        // the `fn_typed_params` check below it), silently misattributing
        // the call's effects to the collision partner's own effects
        // instead of correctly flagging it as unresolved. Confirmed live
        // BEFORE this fix: `fun log(x: Int) -> Int { x }` alongside `pub
        // fun apply(log: fn(Int) -> Int, x: Int) -> Int { log(x) }`
        // compiled with ZERO diagnostics (no K0303), while an identical
        // non-colliding control (parameter named `cb` instead of `log`)
        // correctly warned -- and `pure_funs()` (the real-OS-thread
        // `par_map`/`par_filter` safety gate) misclassified `apply` as
        // pure in isolation.
        let colliding = diags_for(
            "fun log(x: Int) -> Int {\n    x\n}\n\
             pub fun apply(log: fn(Int) -> Int, x: Int) -> Int {\n    log(x)\n}\n",
        );
        assert!(
            colliding.iter().any(|d| d.code == "K0303"),
            "a call through a parameter whose name collides with an existing function must \
             still warn K0303: {colliding:?}"
        );

        // the non-colliding control case must ALSO still warn, confirming
        // the ordinary (already-correct) case is unaffected by the fix.
        let control = diags_for(
            "fun log(x: Int) -> Int {\n    x\n}\n\
             pub fun apply(cb: fn(Int) -> Int, x: Int) -> Int {\n    cb(x)\n}\n",
        );
        assert!(control.iter().any(|d| d.code == "K0303"), "{control:?}");

        // directly exercise `pure_funs()` itself -- the actual safety gate
        // `par_map`/`par_filter`'s real-thread fast path relies on -- to
        // confirm `apply` is no longer wrongly admitted as pure.
        let (p, d) = parser::parse(
            "fun log(x: Int) -> Int {\n    x\n}\n\
             pub fun apply(log: fn(Int) -> Int, x: Int) -> Int {\n    log(x)\n}\n",
        );
        assert!(d.is_empty(), "parse diags: {d:?}");
        let pure = super::pure_funs(&p);
        assert!(
            !pure.contains("apply"),
            "apply must NOT be classified pure -- it calls through a parameter that collides \
             with an unrelated global function name: {pure:?}"
        );
    }
}
