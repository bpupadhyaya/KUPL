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

use std::collections::{BTreeSet, HashMap};

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

    // ---- direct effects + call edges per function ----
    let mut direct: HashMap<String, EffectSet> = HashMap::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for (key, info) in &funs {
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        // `ai fun` performs the `ai` effect; the keyword itself declares it.
        if info.decl.ai.is_some() {
            eff.insert("ai".to_string());
        }
        walk_block(&info.decl.body, &mut |expr| {
            collect_expr(expr, info.component, &funs, &mut eff, &mut calls);
        });
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
        // declared-but-unused (any fun that declares)
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

    diags
}

/// Infer the transitive effect set of every function (keyed as in
/// `check_effects`: top-level name, or `Component.fun`). Exposed so other passes
/// (e.g. the parallel scheduler) can ask which functions are pure.
pub fn infer_effects(program: &Program) -> HashMap<String, EffectSet> {
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

    let mut direct: HashMap<String, EffectSet> = HashMap::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for (key, (decl, component)) in &funs {
        let mut eff = EffectSet::new();
        let mut calls = Vec::new();
        if decl.ai.is_some() {
            eff.insert("ai".to_string());
        }
        walk_block(&decl.body, &mut |expr| {
            collect_expr(expr, *component, &funs, &mut eff, &mut calls);
        });
        direct.insert(key.clone(), eff);
        edges.insert(key.clone(), calls);
    }

    let mut inferred = direct;
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
                changed = true;
            }
            inferred.insert(key.clone(), acc);
        }
        if !changed {
            break;
        }
    }
    inferred
}

/// Top-level functions with NO inferred effects — referentially transparent, so
/// safe to evaluate on a worker thread. Component methods are excluded (they can
/// touch instance state); only bare `fun` names are returned.
pub fn pure_funs(program: &Program) -> std::collections::HashSet<String> {
    infer_effects(program)
        .into_iter()
        .filter(|(key, eff)| eff.is_empty() && !key.contains('.'))
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
        "http_get" | "http_post" => Some("io.net"),
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
    eff: &mut EffectSet,
    calls: &mut Vec<String>,
) {
    // A method-call name may be a UFCS call to a top-level function; a plain
    // call names the function directly. Both attribute that function's effects
    // (conservatively — over-attribution is sound).
    let call_name = match &expr.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Ident(name) => Some(name.as_str()),
            _ => None,
        },
        ExprKind::MethodCall { name, .. } => Some(name.as_str()),
        _ => None,
    };
    if let Some(name) = call_name {
        if let Some(e) = builtin_effects(name) {
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
    fn private_funs_stay_implicit() {
        let d = diags_for("fun helper() {\n    print(\"hi\")\n}\n");
        assert!(d.is_empty(), "{d:?}");
    }
}
