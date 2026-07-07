//! Type & semantic checker.
//!
//! Two passes: (1) collect signatures of types, functions, and components;
//! (2) check every body with local inference (fresh vars + unification).
//! Public boundaries (fun params/returns, ports, props) must be annotated —
//! that is enforced by the grammar itself in v0.1.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::types::{ComponentSig, ContractSig, IntW, Ty, TypeSig, Unifier, VariantSig};

#[derive(Default)]
pub struct Checked {
    pub types: HashMap<String, TypeSig>,
    /// variant name -> (owning type name, fields)
    pub ctors: HashMap<String, (String, Vec<(String, Ty)>)>,
    /// name -> (params, ret, quantified type-variable ids)
    pub funs: HashMap<String, (Vec<Ty>, Ty, Vec<u32>)>,
    pub components: HashMap<String, ComponentSig>,
    pub contracts: HashMap<String, ContractSig>,
    /// `ai fun` signatures: everything the runtime needs to execute the call.
    pub ai_funs: HashMap<String, crate::ai::AiFunMeta>,
}

pub fn check(program: &Program) -> (Checked, Vec<Diag>) {
    let mut ck = Checker {
        checked: Checked::default(),
        diags: Vec::new(),
        uni: Unifier::default(),
        tyvars: HashMap::new(),
    };
    ck.collect(program);
    ck.check_bodies(program);
    (ck.checked, ck.diags)
}

struct Checker {
    checked: Checked,
    diags: Vec<Diag>,
    uni: Unifier,
    /// In-scope type parameters while resolving a generic function.
    tyvars: HashMap<String, Ty>,
}

/// Lexical scope stack for body checking.
struct Scopes {
    stack: Vec<HashMap<String, (Ty, bool)>>,
}

impl Scopes {
    fn new() -> Self {
        Scopes { stack: vec![HashMap::new()] }
    }
    fn push(&mut self) {
        self.stack.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.stack.pop();
    }
    fn insert(&mut self, name: &str, ty: Ty, mutable: bool) {
        self.stack.last_mut().unwrap().insert(name.to_string(), (ty, mutable));
    }
    fn get(&self, name: &str) -> Option<(Ty, bool)> {
        for scope in self.stack.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    }
    /// All in-scope binding names (for "did you mean" suggestions).
    fn names(&self) -> impl Iterator<Item = &str> {
        self.stack.iter().flat_map(|s| s.keys().map(String::as_str))
    }
}

/// Levenshtein edit distance (small strings — identifier names).
/// Optimal-string-alignment (restricted Damerau-Levenshtein) edit distance. Unlike plain
/// Levenshtein, a transposition of two adjacent characters costs 1, not 2 — so a common typo
/// like `Itn` for `Int` or `lenght` for `length` is distance 1 and a "did you mean" fires even
/// for short names (which cap the allowed distance at 1). The distance is always <= the plain
/// Levenshtein distance, so this only ever adds suggestions, never removes them.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[n][m]
}

/// The nearest candidate name to `name` within a small edit distance, for a
/// "did you mean `…`?" hint. Deterministic: closest wins, ties broken
/// alphabetically. Returns `None` if nothing is close enough (so it never fires
/// spuriously). Short names require a closer match.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let max_dist = if name.chars().count() <= 3 { 1 } else { 2 };
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        if cand == name {
            continue;
        }
        let d = edit_distance(name, cand);
        if d > max_dist {
            continue;
        }
        let better = match &best {
            None => true,
            Some((bd, bc)) => d < *bd || (d == *bd && cand < *bc),
        };
        if better {
            best = Some((d, cand));
        }
    }
    best.map(|(_, c)| c.to_string())
}

/// Every built-in method name across all receiver types, for "did you mean"
/// suggestions on an unknown method (K0249). Suggestion-only and best-effort —
/// if a newly added method is missing here the only effect is a missed hint, so
/// it need not track the method-resolution match perfectly.
const BUILTIN_METHODS: &[&str] = &[
    "abs", "all", "and_then", "any", "band", "bnot", "bor", "bxor", "cbrt", "ceil",
    "chars", "chunk", "clamp", "concat", "contains", "contains_key", "cos", "count",
    "den", "difference", "dot", "drop", "drop_while", "ends_with", "exp", "filter",
    "find", "first", "flat_map", "flatten", "floor", "fmt", "fold", "format", "gcd",
    "get", "get_or", "group_by", "hypot", "index_of", "init", "insert", "intersect",
    "is_empty", "is_err", "is_even", "is_infinite", "is_nan", "is_negative", "is_none",
    "is_odd", "is_ok", "is_some", "is_subset", "isqrt", "join", "keys", "last", "len",
    "lines", "log", "map", "map_err", "map_values", "max", "max_by", "mean", "merge",
    "min", "min_by", "num", "ok", "ok_or", "pad_left", "pad_right", "par_each",
    "par_filter", "par_map", "parse_float", "parse_int", "partition", "position",
    "pow", "product", "push", "recip", "remove", "repeat", "replace", "replace_first",
    "reverse", "rfind", "round", "saturating_add", "saturating_mul", "saturating_sub",
    "scale", "scan", "shl", "shr", "sign", "sin", "slice", "sort", "sort_by", "split",
    "split_once", "sqrt", "starts_with", "sum", "symmetric_difference", "tail", "take",
    "take_while", "tan", "to_binary", "to_float", "to_hex", "to_int", "to_list",
    "to_lower", "to_octal", "to_radix", "to_str", "to_upper", "trim", "trim_end",
    "trim_start", "union", "unique", "unwrap_or", "ushr", "values", "window", "zip_with",
];

/// What surrounds the body being checked.
struct Ctx<'a> {
    scopes: Scopes,
    ret: Ty,
    /// Some(component) while checking handlers/funs of a component.
    component: Option<&'a ComponentDecl>,
    in_handler: bool,
    loop_depth: usize,
}

impl Checker {
    fn err(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diag::error(code, msg, span));
    }
    fn warn(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diag::warning(code, msg, span));
    }

    fn unify(&mut self, a: &Ty, b: &Ty, span: Span, what: &str) -> Ty {
        if let Err((x, y)) = self.uni.unify(a, b) {
            let x = self.uni.apply(&x);
            let y = self.uni.apply(&y);
            self.err("K0200", format!("type mismatch in {what}: expected {x}, found {y}"), span);
        }
        self.uni.apply(a)
    }

    /// True when `actual` can flow into a slot expecting `expected` beyond
    /// plain unification: a component that `fulfills` contract C is assignable
    /// to `Contract(C)`, and to another contract it also fulfills.
    fn contract_assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        let Ty::Contract(c) = self.uni.resolve(expected) else { return false };
        match self.uni.resolve(actual) {
            Ty::Component(x) => self
                .checked
                .components
                .get(&x)
                .is_some_and(|sig| sig.fulfills.iter().any(|f| f == &c)),
            Ty::Contract(x) => x == c,
            _ => false,
        }
    }

    /// Like `unify`, but first admits contract assignability (fulfilling
    /// component → contract type). Reports K0200 on a genuine mismatch.
    fn check_assign(&mut self, expected: &Ty, actual: &Ty, span: Span, what: &str) {
        if self.contract_assignable(expected, actual) {
            return;
        }
        self.unify(expected, actual, span, what);
    }

    // ---------------- pass 1: collect ----------------

    fn collect(&mut self, program: &Program) {
        // types first (functions/components may reference them)
        for item in &program.items {
            if let Item::Type(t) = item {
                if self.checked.types.contains_key(&t.name) {
                    self.err("K0201", format!("type `{}` is defined more than once", t.name), t.span);
                    continue;
                }
                // placeholder so recursive types resolve
                self.checked.types.insert(
                    t.name.clone(),
                    TypeSig {
                        name: t.name.clone(),
                        variants: Vec::new(),
                        is_record: false,
                        type_params: t.type_params.clone(),
                        qvars: Vec::new(),
                    },
                );
            }
        }
        for item in &program.items {
            if let Item::Component(c) = item {
                self.checked.components.insert(c.name.clone(), ComponentSig::default());
            }
            // register contract names early so contract-typed props/params resolve
            if let Item::Contract(ct) = item {
                self.checked.contracts.entry(ct.name.clone()).or_default();
            }
        }
        // now resolve type bodies
        for item in &program.items {
            match item {
                Item::Type(t) => {
                    // bind each type parameter to a fresh var (collected as qvars,
                    // like a generic function's scheme) so variant field types that
                    // reference `T` resolve to it; each use instantiates fresh vars.
                    let mut qvars = Vec::new();
                    self.tyvars.clear();
                    for tp in &t.type_params {
                        let v = self.uni.fresh();
                        if let Ty::Var(id) = v {
                            qvars.push(id);
                        }
                        self.tyvars.insert(tp.clone(), v);
                    }
                    let mut variants = Vec::new();
                    for v in &t.variants {
                        let mut fields = Vec::new();
                        for f in &v.fields {
                            let ty = self.resolve_ty(&f.ty);
                            fields.push((f.name.clone(), ty));
                        }
                        if self.checked.ctors.contains_key(&v.name) {
                            self.err(
                                "K0202",
                                format!("constructor `{}` is defined more than once", v.name),
                                v.span,
                            );
                        }
                        self.checked
                            .ctors
                            .insert(v.name.clone(), (t.name.clone(), fields.clone()));
                        variants.push(VariantSig { name: v.name.clone(), fields });
                    }
                    self.tyvars.clear();
                    let is_record = t.variants.len() == 1 && t.variants[0].name == t.name;
                    self.checked.types.insert(
                        t.name.clone(),
                        TypeSig {
                            name: t.name.clone(),
                            variants,
                            is_record,
                            type_params: t.type_params.clone(),
                            qvars,
                        },
                    );
                }
                Item::Fun(f) => {
                    let mut qvars = Vec::new();
                    self.tyvars.clear();
                    for tp in &f.type_params {
                        let v = self.uni.fresh();
                        if let Ty::Var(id) = v {
                            qvars.push(id);
                        }
                        self.tyvars.insert(tp.clone(), v);
                    }
                    let params: Vec<Ty> = f.params.iter().map(|p| self.resolve_ty(&p.ty)).collect();
                    let ret = f.ret.as_ref().map(|t| self.resolve_ty(t)).unwrap_or(Ty::Unit);
                    self.tyvars.clear();
                    if self.checked.funs.contains_key(&f.name) {
                        self.err("K0203", format!("function `{}` is defined more than once", f.name), f.span);
                    }
                    self.checked.funs.insert(f.name.clone(), (params, ret, qvars));
                }
                Item::Component(c) => {
                    let mut sig = ComponentSig::default();
                    for port in &c.ports {
                        let ty = self.resolve_ty(&port.ty);
                        let map = match port.dir {
                            PortDir::In => &mut sig.in_ports,
                            PortDir::Out => &mut sig.out_ports,
                        };
                        if map.insert(port.name.clone(), ty).is_some() {
                            self.err("K0204", format!("port `{}` declared twice", port.name), port.span);
                        }
                    }
                    for prop in &c.props {
                        let ty = self.resolve_ty(&prop.ty);
                        sig.props.push((prop.name.clone(), ty, prop.default.is_some()));
                    }
                    for f in &c.exposes {
                        let params: Vec<Ty> = f.params.iter().map(|p| self.resolve_ty(&p.ty)).collect();
                        let ret = f.ret.as_ref().map(|t| self.resolve_ty(t)).unwrap_or(Ty::Unit);
                        sig.exposes.insert(f.name.clone(), (params, ret));
                    }
                    sig.fulfills = c.fulfills.clone();
                    self.checked.components.insert(c.name.clone(), sig);
                    if c.intent.is_none() {
                        self.warn(
                            "K0300",
                            format!("component `{}` has no `intent` — every component should state its purpose", c.name),
                            c.span,
                        );
                    }
                }
                Item::Contract(ct) => {
                    let mut sig = ContractSig::default();
                    for s in &ct.sigs {
                        let params: Vec<Ty> = s.params.iter().map(|p| self.resolve_ty(&p.ty)).collect();
                        let ret = s.ret.as_ref().map(|t| self.resolve_ty(t)).unwrap_or(Ty::Unit);
                        sig.sigs.insert(s.name.clone(), (params, ret, s.effects.clone()));
                    }
                    // the name was pre-registered (empty); a non-empty existing
                    // sig means a genuine redefinition
                    match self.checked.contracts.get(&ct.name) {
                        Some(existing) if !existing.sigs.is_empty() => {
                            self.err("K0260", format!("contract `{}` is defined more than once", ct.name), ct.span);
                        }
                        _ => {
                            self.checked.contracts.insert(ct.name.clone(), sig);
                        }
                    }
                }
                Item::Law(_) => {} // no signature to collect
            }
        }
        self.collect_ai_funs(program);
    }

    /// Third pass: `ai fun` signatures. Runs after every type is resolved so
    /// return shapes can reference records declared anywhere in the program.
    fn collect_ai_funs(&mut self, program: &Program) {
        let records: std::collections::HashMap<String, (String, Vec<(String, Ty)>)> = self
            .checked
            .types
            .values()
            .filter(|t| t.variants.len() == 1)
            .map(|t| {
                (t.name.clone(), (t.variants[0].name.clone(), t.variants[0].fields.clone()))
            })
            .collect();
        for item in &program.items {
            let Item::Fun(f) = item else { continue };
            let Some(ai) = &f.ai else { continue };
            let Some(ret) = &f.ret else {
                self.err(
                    "K0270",
                    format!(
                        "`ai fun {}` must declare a return type — it defines the structured output",
                        f.name
                    ),
                    f.span,
                );
                continue;
            };
            let ret_ty = self.resolve_ty(ret);
            let ret_ty = self.uni.apply(&ret_ty);
            let (target, wraps_result) = match &ret_ty {
                Ty::Result(t, e) if **e == Ty::Str => ((**t).clone(), true),
                other => (other.clone(), false),
            };
            let shape = match crate::ai::build_shape(&target, &records, &mut Vec::new()) {
                Ok(shape) => shape,
                Err(msg) => {
                    self.err("K0271", format!("`ai fun {}`: {msg}", f.name), ret.span);
                    continue;
                }
            };
            let tools = self.resolve_ai_tools(f, ai, program, &records);
            self.checked.ai_funs.insert(
                f.name.clone(),
                crate::ai::AiFunMeta {
                    name: f.name.clone(),
                    intent: ai.intent.clone(),
                    model: ai.model.clone(),
                    params: f.params.iter().map(|p| p.name.clone()).collect(),
                    shape,
                    wraps_result,
                    tools,
                },
            );
        }
    }

    /// Resolve an `ai fun`'s `tools [...]` list into `ToolMeta`s. Each tool must
    /// be a non-generic, non-ai top-level function whose parameter and return
    /// types are representable as structured output.
    fn resolve_ai_tools(
        &mut self,
        owner: &FunDecl,
        ai: &AiDecl,
        program: &Program,
        records: &std::collections::HashMap<String, (String, Vec<(String, Ty)>)>,
    ) -> Vec<crate::ai::ToolMeta> {
        let mut out = Vec::new();
        for tool_name in &ai.tools {
            let decl = program.items.iter().find_map(|it| match it {
                Item::Fun(f) if &f.name == tool_name && f.ai.is_none() => Some(f),
                _ => None,
            });
            let Some(decl) = decl else {
                self.err(
                    "K0272",
                    format!(
                        "`ai fun {}` lists tool `{tool_name}`, which is not a top-level function",
                        owner.name
                    ),
                    owner.span,
                );
                continue;
            };
            if !decl.type_params.is_empty() {
                self.err(
                    "K0272",
                    format!("tool `{tool_name}` is generic — ai tools must be monomorphic"),
                    decl.span,
                );
                continue;
            }
            let Some(ret_ty) = &decl.ret else {
                self.err(
                    "K0272",
                    format!("tool `{tool_name}` must declare a return type"),
                    decl.span,
                );
                continue;
            };
            let mut params = Vec::new();
            let mut ok = true;
            for p in &decl.params {
                let pty = self.resolve_ty(&p.ty);
                match crate::ai::build_shape(&pty, records, &mut Vec::new()) {
                    Ok(shape) => params.push((p.name.clone(), shape)),
                    Err(msg) => {
                        self.err(
                            "K0272",
                            format!("tool `{tool_name}` parameter `{}`: {msg}", p.name),
                            p.span,
                        );
                        ok = false;
                    }
                }
            }
            let ret = self.resolve_ty(ret_ty);
            let ret_shape = match crate::ai::build_shape(&ret, records, &mut Vec::new()) {
                Ok(shape) => shape,
                Err(msg) => {
                    self.err("K0272", format!("tool `{tool_name}` return: {msg}"), ret_ty.span);
                    continue;
                }
            };
            if !ok {
                continue;
            }
            let sig: Vec<String> =
                decl.params.iter().map(|p| format!("{}: {}", p.name, crate::fmt::ty_str(&p.ty))).collect();
            let description = format!(
                "KUPL function `{tool_name}({}) -> {}`",
                sig.join(", "),
                crate::fmt::ty_str(ret_ty)
            );
            out.push(crate::ai::ToolMeta { name: tool_name.clone(), description, params, ret: ret_shape });
        }
        out
    }

    fn resolve_ty(&mut self, t: &TyExpr) -> Ty {
        match &t.kind {
            TyExprKind::Name(n) => match n.as_str() {
                _ if self.tyvars.contains_key(n.as_str()) => {
                    self.tyvars.get(n.as_str()).cloned().unwrap()
                }
                "Int" => Ty::Int,
                "Float" => Ty::Float,
                "Bool" => Ty::Bool,
                "Str" => Ty::Str,
                "Unit" => Ty::Unit,
                "Event" => Ty::Event,
                "Tensor" => Ty::Tensor,
                "f32" => Ty::F32,
                "BigInt" => Ty::BigInt,
                "Rational" => Ty::Rational,
                _ if crate::value::IntW::from_name(n.as_str()).is_some() => {
                    Ty::IntW(crate::value::IntW::from_name(n.as_str()).unwrap())
                }
                other => {
                    if let Some(sig) = self.checked.types.get(other) {
                        // a bare generic type name instantiates fresh type args
                        let n = sig.type_params.len();
                        let args = (0..n).map(|_| self.uni.fresh()).collect();
                        Ty::Named(other.to_string(), args)
                    } else if self.checked.components.contains_key(other) {
                        Ty::Component(other.to_string())
                    } else if self.checked.contracts.contains_key(other) {
                        Ty::Contract(other.to_string())
                    } else {
                        let suggestion = {
                            let builtins = [
                                "Int", "Float", "Str", "Bool", "Unit", "List", "Map", "Set",
                                "Option", "Result", "Json", "Tensor", "BigInt", "Rational",
                                "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32",
                            ]
                            .into_iter();
                            let cands = self
                                .checked
                                .types
                                .keys()
                                .map(String::as_str)
                                .chain(self.checked.components.keys().map(String::as_str))
                                .chain(self.checked.contracts.keys().map(String::as_str))
                                .chain(builtins);
                            suggest(other, cands)
                        };
                        let msg = match suggestion {
                            Some(s) => format!("unknown type `{other}` (did you mean `{s}`?)"),
                            None => format!("unknown type `{other}`"),
                        };
                        self.err("K0205", msg, t.span);
                        self.uni.fresh()
                    }
                }
            },
            TyExprKind::Generic(n, args) => {
                let mut ats: Vec<Ty> = args.iter().map(|a| self.resolve_ty(a)).collect();
                match (n.as_str(), ats.len()) {
                    ("List", 1) => Ty::List(Box::new(ats.remove(0))),
                    ("Set", 1) => Ty::Set(Box::new(ats.remove(0))),
                    ("Map", 2) => {
                        let v = ats.remove(1);
                        let k = ats.remove(0);
                        Ty::Map(Box::new(k), Box::new(v))
                    }
                    ("Option", 1) => Ty::Option(Box::new(ats.remove(0))),
                    ("Result", 2) => {
                        let e = ats.remove(1);
                        let ok = ats.remove(0);
                        Ty::Result(Box::new(ok), Box::new(e))
                    }
                    _ if self.checked.types.contains_key(n) => {
                        let params = self.checked.types.get(n).unwrap().type_params.len();
                        if params != ats.len() {
                            self.err(
                                "K0206",
                                format!("`{n}` takes {params} type argument(s), {} given", ats.len()),
                                t.span,
                            );
                        }
                        Ty::Named(n.clone(), ats)
                    }
                    _ => {
                        self.err(
                            "K0206",
                            format!("unknown generic type `{n}` with {} argument(s)", args.len()),
                            t.span,
                        );
                        self.uni.fresh()
                    }
                }
            }
            TyExprKind::Fun(params, ret) => {
                let ps = params.iter().map(|p| self.resolve_ty(p)).collect();
                let r = self.resolve_ty(ret);
                Ty::Fun(ps, Box::new(r))
            }
        }
    }

    // ---------------- pass 2: check bodies ----------------

    fn check_bodies(&mut self, program: &Program) {
        for item in &program.items {
            match item {
                // an ai fun's body is a prompt, not code — check_fun type-checks its
                // `intent` interpolation (undefined `{var}` -> K0240) but no block.
                Item::Fun(f) => self.check_fun(f, None),
                Item::Type(_) => {}
                Item::Component(c) => self.check_component(c),
                Item::Contract(ct) => self.check_contract(ct),
                Item::Law(l) => {
                    let mut ctx = Ctx {
                        scopes: Scopes::new(),
                        ret: Ty::Unit,
                        component: None,
                        in_handler: false,
                        loop_depth: 0,
                    };
                    self.check_block(&l.body, &mut ctx);
                }
            }
        }
    }

    /// Check law bodies: contract expose names are in scope as functions.
    fn check_contract(&mut self, ct: &ContractDecl) {
        let sig = self.checked.contracts.get(&ct.name).cloned().unwrap_or_default();
        for law in &ct.laws {
            let mut ctx = Ctx {
                scopes: Scopes::new(),
                ret: Ty::Unit,
                component: None,
                in_handler: false,
                loop_depth: 0,
            };
            for (name, (params, ret, _)) in &sig.sigs {
                ctx.scopes.insert(name, Ty::Fun(params.clone(), Box::new(ret.clone())), false);
            }
            self.check_block(&law.body, &mut ctx);
        }
    }

    /// A fulfilling component must expose every contract signature, with
    /// exactly matching types and effects within the contract's budget.
    fn check_fulfills(&mut self, c: &ComponentDecl) {
        for contract_name in &c.fulfills {
            let Some(contract) = self.checked.contracts.get(contract_name).cloned() else {
                self.err(
                    "K0261",
                    format!("`{}` fulfills unknown contract `{contract_name}`", c.name),
                    c.span,
                );
                continue;
            };
            let comp_sig = self.checked.components.get(&c.name).cloned().unwrap_or_default();
            for (fname, (params, ret, effects)) in &contract.sigs {
                match comp_sig.exposes.get(fname) {
                    None => self.err(
                        "K0262",
                        format!("`{}` fulfills `{contract_name}` but does not expose `{fname}`", c.name),
                        c.span,
                    ),
                    Some((cp, cr)) => {
                        let want = Ty::Fun(params.clone(), Box::new(ret.clone()));
                        let got = Ty::Fun(cp.clone(), Box::new(cr.clone()));
                        if self.uni.unify(&want, &got).is_err() {
                            self.err(
                                "K0263",
                                format!(
                                    "`{}` exposes `{fname}` as {got} but contract `{contract_name}` requires {want}",
                                    c.name
                                ),
                                c.span,
                            );
                        }
                        // the component's declared effects must fit the contract's budget
                        let decl = c.exposes.iter().find(|f| &f.name == fname);
                        if let Some(decl) = decl {
                            for e in &decl.effects {
                                if !effects.iter().any(|budget| covers_effect(budget, e)) {
                                    self.err(
                                        "K0264",
                                        format!(
                                            "`{}`.`{fname}` uses `{e}` but contract `{contract_name}` allows only [{}]",
                                            c.name,
                                            effects.join(", ")
                                        ),
                                        decl.span,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn check_fun(&mut self, f: &FunDecl, component: Option<&ComponentDecl>) {
        self.tyvars.clear();
        for tp in &f.type_params {
            let v = self.uni.fresh();
            self.tyvars.insert(tp.clone(), v);
        }
        let mut ctx = Ctx {
            scopes: Scopes::new(),
            ret: f.ret.as_ref().map(|t| self.resolve_ty(t)).unwrap_or(Ty::Unit),
            component,
            in_handler: false,
            loop_depth: 0,
        };
        if let Some(c) = component {
            self.bind_component_env(c, &mut ctx);
        }
        ctx.scopes.push();
        for p in &f.params {
            let ty = self.resolve_ty(&p.ty);
            ctx.scopes.insert(&p.name, ty, false);
        }
        if let Some(ai) = &f.ai {
            // An `ai fun`'s "body" is its `intent` string. Type-check its
            // interpolation holes against the params in scope, exactly like a
            // regular string — so an undefined `{var}` is a clean compile error
            // (K0240) instead of a runtime panic that also diverges interp vs KVM.
            self.infer_expr(&ai.intent_expr, &mut ctx);
        } else {
            let body_ty = self.check_block(&f.body, &mut ctx);
            // The block's tail value must match the return type (unless Unit-returning).
            let ret = self.uni.apply(&ctx.ret.clone());
            if ret != Ty::Unit {
                self.unify(&ret, &body_ty, f.body.span, &format!("return value of `{}`", f.name));
            }
        }
        self.tyvars.clear();
    }

    /// Put props (immutable) and state (mutable) in scope.
    fn bind_component_env(&mut self, c: &ComponentDecl, ctx: &mut Ctx) {
        for prop in &c.props {
            let ty = self.resolve_ty(&prop.ty);
            ctx.scopes.insert(&prop.name, ty, false);
        }
        // state types: annotation wins, else inferred from init in a props-only env
        for s in &c.state {
            let ty = match &s.ty {
                Some(t) => self.resolve_ty(t),
                None => {
                    let t = self.infer_expr(&s.init, ctx);
                    self.uni.apply(&t)
                }
            };
            ctx.scopes.insert(&s.name, ty, true);
        }
        // children in scope as component refs
        for child in &c.children {
            if self.checked.components.contains_key(&child.component) {
                ctx.scopes.insert(&child.name, Ty::Component(child.component.clone()), false);
            }
        }
        // component functions (private and exposed) callable from any body
        for f in c.funs.iter().chain(c.exposes.iter()) {
            let params: Vec<Ty> = f.params.iter().map(|p| self.resolve_ty(&p.ty)).collect();
            let ret = f.ret.as_ref().map(|t| self.resolve_ty(t)).unwrap_or(Ty::Unit);
            ctx.scopes.insert(&f.name, Ty::Fun(params, Box::new(ret)), false);
        }
    }

    fn check_component(&mut self, c: &ComponentDecl) {
        self.check_fulfills(c);
        // state inits against annotations
        {
            let mut ctx = Ctx {
                scopes: Scopes::new(),
                ret: Ty::Unit,
                component: Some(c),
                in_handler: false,
                loop_depth: 0,
            };
            for prop in &c.props {
                let ty = self.resolve_ty(&prop.ty);
                ctx.scopes.insert(&prop.name, ty, false);
            }
            for s in &c.state {
                let init_ty = self.infer_expr(&s.init, &mut ctx);
                if let Some(t) = &s.ty {
                    let ann = self.resolve_ty(t);
                    self.unify(&ann, &init_ty, s.span, &format!("state `{}`", s.name));
                    ctx.scopes.insert(&s.name, ann, true);
                } else {
                    ctx.scopes.insert(&s.name, self.uni.apply(&init_ty), true);
                }
            }
        }

        // children & wires
        let mut child_types: HashMap<String, String> = HashMap::new();
        for child in &c.children {
            if child_types.contains_key(&child.name) {
                self.err("K0207", format!("child `{}` declared twice", child.name), child.span);
            }
            let Some(sig) = self.checked.components.get(&child.component).cloned() else {
                self.err("K0208", format!("unknown component `{}`", child.component), child.span);
                continue;
            };
            child_types.insert(child.name.clone(), child.component.clone());
            let mut cctx = Ctx {
                scopes: Scopes::new(),
                ret: Ty::Unit,
                component: Some(c),
                in_handler: false,
                loop_depth: 0,
            };
            self.bind_component_env(c, &mut cctx);
            self.check_ctor_args(&child.component, &sig, &child.args, child.span, &mut cctx);
        }
        for wire in &c.wires {
            let from_ty = self.wire_port_ty(&child_types, &wire.from, true, wire.span);
            let to_ty = self.wire_port_ty(&child_types, &wire.to, false, wire.span);
            if let (Some(a), Some(b)) = (from_ty, to_ty) {
                self.unify(&a, &b, wire.span, "wire (out port must match in port)");
            }
        }
        for s in &c.supervises {
            if !child_types.contains_key(&s.child) {
                self.err(
                    "K0265",
                    format!("`supervise` references unknown child `{}`", s.child),
                    s.span,
                );
            }
        }

        // handlers
        let sig = self.checked.components.get(&c.name).cloned().unwrap_or_default();
        let mut seen_triggers: HashSet<String> = HashSet::new();
        for h in &c.handlers {
            let mut ctx = Ctx {
                scopes: Scopes::new(),
                ret: Ty::Unit,
                component: Some(c),
                in_handler: true,
                loop_depth: 0,
            };
            self.bind_component_env(c, &mut ctx);
            ctx.scopes.push();
            match &h.trigger {
                Trigger::Start | Trigger::Stop => {
                    let key = if matches!(h.trigger, Trigger::Start) { "start" } else { "stop" };
                    if !seen_triggers.insert(key.to_string()) {
                        self.err("K0209", format!("duplicate `on {key}` handler"), h.span);
                    }
                    if h.param.is_some() {
                        self.err("K0210", format!("`on {key}` takes no parameter"), h.span);
                    }
                }
                Trigger::Every(ms) | Trigger::After(ms) => {
                    // timer handlers carry no payload, like `on start`
                    if h.param.is_some() {
                        let kw = if matches!(h.trigger, Trigger::Every(_)) { "every" } else { "after" };
                        self.err("K0210", format!("`on {kw}` (a timer) takes no parameter"), h.span);
                    }
                    if *ms <= 0 {
                        self.err("K0266", "timer duration must be positive", h.span);
                    }
                }
                Trigger::Port(p) => {
                    if !seen_triggers.insert(p.clone()) {
                        self.err("K0209", format!("duplicate `on {p}` handler"), h.span);
                    }
                    match sig.in_ports.get(p) {
                        None => {
                            let hint = if sig.out_ports.contains_key(p) {
                                " (it is an `out` port — handlers react to `in` ports)"
                            } else {
                                ""
                            };
                            self.err("K0211", format!("`on {p}`: component `{}` has no `in` port named `{p}`{hint}", c.name), h.span);
                        }
                        Some(ty) => {
                            if let Some(param) = &h.param {
                                if *ty == Ty::Event {
                                    self.err(
                                        "K0212",
                                        format!("port `{p}` is an Event (no payload) — remove the parameter `{param}`"),
                                        h.span,
                                    );
                                } else {
                                    ctx.scopes.insert(param, ty.clone(), false);
                                }
                            }
                        }
                    }
                }
            }
            self.check_block(&h.body, &mut ctx);
        }

        // exposed + private functions
        for f in c.exposes.iter().chain(c.funs.iter()) {
            self.check_fun(f, Some(c));
        }

        // examples
        for ex in &c.examples {
            self.check_example(c, &sig, ex);
        }
    }

    fn wire_port_ty(
        &mut self,
        child_types: &HashMap<String, String>,
        end: &(String, String),
        is_from: bool,
        span: Span,
    ) -> Option<Ty> {
        let (child, port) = end;
        let Some(comp_name) = child_types.get(child) else {
            self.err("K0213", format!("`wire` references unknown child `{child}`"), span);
            return None;
        };
        let sig = self.checked.components.get(comp_name).cloned().unwrap_or_default();
        let (map, kind) = if is_from {
            (&sig.out_ports, "out")
        } else {
            (&sig.in_ports, "in")
        };
        match map.get(port) {
            Some(ty) => Some(ty.clone()),
            None => {
                self.err(
                    "K0214",
                    format!("component `{comp_name}` has no `{kind}` port named `{port}`"),
                    span,
                );
                None
            }
        }
    }

    /// Type-check constructor (prop) arguments against the component's prop
    /// types, using the caller's own scope so argument expressions can refer to
    /// locals/state in context. Contract-typed props admit any fulfilling
    /// component (contract assignability).
    fn check_ctor_args(
        &mut self,
        comp_name: &str,
        sig: &ComponentSig,
        args: &[Arg],
        span: Span,
        ctx: &mut Ctx,
    ) {
        let mut supplied: HashSet<String> = HashSet::new();
        for (i, arg) in args.iter().enumerate() {
            let target = match &arg.name {
                Some(n) => sig.props.iter().find(|(pn, _, _)| pn == n).cloned(),
                None => sig.props.get(i).cloned(),
            };
            let arg_ty = self.infer_expr(&arg.value, ctx);
            match target {
                Some((pname, pty, _)) => {
                    supplied.insert(pname.clone());
                    self.check_assign(&pty, &arg_ty, arg.value.span, &format!("prop `{pname}` of `{comp_name}`"));
                }
                None => {
                    self.err(
                        "K0215",
                        match &arg.name {
                            Some(n) => format!("component `{comp_name}` has no prop named `{n}`"),
                            None => format!("too many arguments for `{comp_name}` (has {} props)", sig.props.len()),
                        },
                        arg.value.span,
                    );
                }
            }
        }
        for (pname, _, has_default) in &sig.props {
            if !has_default && !supplied.contains(pname) {
                self.err(
                    "K0216",
                    format!("missing required prop `{pname}` when constructing `{comp_name}`"),
                    span,
                );
            }
        }
    }

    fn check_example(&mut self, c: &ComponentDecl, sig: &ComponentSig, ex: &Example) {
        for step in &ex.steps {
            match step {
                ExampleStep::Send { port, arg, span } => match sig.in_ports.get(port) {
                    None => self.err(
                        "K0217",
                        format!("`send {port}`: component `{}` has no `in` port named `{port}`", c.name),
                        *span,
                    ),
                    Some(ty) => {
                        let ty = ty.clone();
                        let mut ctx = Ctx {
                            scopes: Scopes::new(),
                            ret: Ty::Unit,
                            component: Some(c),
                            in_handler: false,
                            loop_depth: 0,
                        };
                        match (arg, ty) {
                            (None, Ty::Event) => {}
                            (Some(a), Ty::Event) => {
                                self.err("K0218", format!("port `{port}` is an Event and takes no payload"), a.span)
                            }
                            (None, other) => self.err(
                                "K0219",
                                format!("port `{port}` carries {other} — `send {port}(value)` needs a payload"),
                                *span,
                            ),
                            (Some(a), other) => {
                                let at = self.infer_expr(a, &mut ctx);
                                self.unify(&other, &at, a.span, &format!("payload for port `{port}`"));
                            }
                        }
                    }
                },
                ExampleStep::Expect { expr, span } => {
                    let mut ctx = Ctx {
                        scopes: Scopes::new(),
                        ret: Ty::Unit,
                        component: Some(c),
                        in_handler: false,
                        loop_depth: 0,
                    };
                    // out ports are bound to their last emitted value
                    for (name, ty) in &sig.out_ports {
                        ctx.scopes.insert(name, ty.clone(), false);
                    }
                    let t = self.infer_expr(expr, &mut ctx);
                    self.unify(&Ty::Bool, &t, *span, "`expect` condition");
                }
                // `advance` is a literal duration — nothing to type-check
                ExampleStep::Advance { .. } => {}
            }
        }
    }

    // ---------------- statements & expressions ----------------

    fn check_block(&mut self, block: &Block, ctx: &mut Ctx) -> Ty {
        ctx.scopes.push();
        let mut last: Ty = Ty::Unit;
        for (i, stmt) in block.stmts.iter().enumerate() {
            last = self.check_stmt(stmt, ctx);
            if i + 1 < block.stmts.len() {
                last = Ty::Unit;
            }
        }
        ctx.scopes.pop();
        last
    }

    fn check_stmt(&mut self, stmt: &Stmt, ctx: &mut Ctx) -> Ty {
        match stmt {
            Stmt::Let { name, ty, init, mutable, span } => {
                let init_ty = self.infer_expr(init, ctx);
                let final_ty = match ty {
                    Some(t) => {
                        let ann = self.resolve_ty(t);
                        self.check_assign(&ann, &init_ty, *span, &format!("`let {name}`"));
                        ann
                    }
                    None => self.uni.apply(&init_ty),
                };
                ctx.scopes.insert(name, final_ty, *mutable);
                Ty::Unit
            }
            Stmt::Assign { target, op, value, span } => {
                let value_ty = self.infer_expr(value, ctx);
                match &target.kind {
                    ExprKind::Ident(name) => match ctx.scopes.get(name) {
                        None => self.err("K0220", format!("unknown variable `{name}`"), target.span),
                        Some((ty, mutable)) => {
                            if !mutable {
                                self.err(
                                    "K0221",
                                    format!("`{name}` is immutable (declared with `let`; use `var` or `state`)"),
                                    target.span,
                                );
                            }
                            self.unify(&ty, &value_ty, *span, &format!("assignment to `{name}`"));
                            if *op != AssignOp::Set {
                                let rt = self.uni.apply(&ty);
                                let rt = self.default_numeric(rt);
                                if !rt.is_numeric() {
                                    self.err("K0222", format!("`{}=` needs a numeric variable, `{name}` is {rt}", op_sym(*op)), *span);
                                }
                            }
                        }
                    },
                    ExprKind::Field { .. } => {
                        self.err("K0223", "field assignment is not supported in v0.1 — rebuild the record with `with` planned for v0.2", target.span);
                    }
                    _ => {}
                }
                Ty::Unit
            }
            Stmt::Expr(e) => self.infer_expr(e, ctx),
            Stmt::Return(value, span) => {
                let vt = match value {
                    Some(v) => self.infer_expr(v, ctx),
                    None => Ty::Unit,
                };
                let ret = ctx.ret.clone();
                self.unify(&ret, &vt, *span, "return value");
                Ty::Unit
            }
            Stmt::While { cond, body, span: _ } => {
                let ct = self.infer_expr(cond, ctx);
                self.unify(&Ty::Bool, &ct, cond.span, "`while` condition");
                ctx.loop_depth += 1;
                self.check_block(body, ctx);
                ctx.loop_depth -= 1;
                Ty::Unit
            }
            Stmt::For { var, iter, body, span: _ } => {
                let it = self.infer_expr(iter, ctx);
                let elem = match self.uni.apply(&it) {
                    Ty::Range => Ty::Int,
                    Ty::List(e) => *e,
                    Ty::Var(_) => {
                        // default: range
                        self.unify(&it, &Ty::Range, iter.span, "`for` iterable");
                        Ty::Int
                    }
                    other => {
                        self.err("K0224", format!("`for` needs a Range or List, found {other}"), iter.span);
                        self.uni.fresh()
                    }
                };
                ctx.scopes.push();
                ctx.scopes.insert(var, elem, false);
                ctx.loop_depth += 1;
                self.check_block(body, ctx);
                ctx.loop_depth -= 1;
                ctx.scopes.pop();
                Ty::Unit
            }
            Stmt::Emit { port, arg, span } => {
                let Some(c) = ctx.component else {
                    self.err("K0225", "`emit` is only valid inside a component", *span);
                    return Ty::Unit;
                };
                let sig = self.checked.components.get(&c.name).cloned().unwrap_or_default();
                match sig.out_ports.get(port) {
                    None => {
                        let hint = if sig.in_ports.contains_key(port) {
                            " (it is an `in` port — you can only `emit` on `out` ports)"
                        } else {
                            ""
                        };
                        self.err("K0226", format!("component `{}` has no `out` port named `{port}`{hint}", c.name), *span);
                    }
                    Some(ty) => match (arg, ty.clone()) {
                        (None, Ty::Event) => {}
                        (Some(a), Ty::Event) => {
                            self.err("K0227", format!("port `{port}` is an Event and takes no payload"), a.span)
                        }
                        (None, other) => self.err(
                            "K0228",
                            format!("port `{port}` carries {other} — `emit {port}(value)` needs a payload"),
                            *span,
                        ),
                        (Some(a), other) => {
                            let at = self.infer_expr(a, ctx);
                            self.unify(&other, &at, a.span, &format!("payload for port `{port}`"));
                        }
                    },
                }
                Ty::Unit
            }
            Stmt::Expect(expr, span) => {
                let t = self.infer_expr(expr, ctx);
                self.unify(&Ty::Bool, &t, *span, "`expect` condition");
                Ty::Unit
            }
            Stmt::Break(span) | Stmt::Continue(span) => {
                if ctx.loop_depth == 0 {
                    self.err("K0229", "`break`/`continue` outside of a loop", *span);
                }
                Ty::Unit
            }
            Stmt::Forall { vars, body, .. } => {
                ctx.scopes.push();
                for (name, ty) in vars {
                    let t = self.resolve_ty(ty);
                    ctx.scopes.insert(name, t, false);
                }
                self.check_block(body, ctx);
                ctx.scopes.pop();
                Ty::Unit
            }
        }
    }

    fn default_numeric(&mut self, ty: Ty) -> Ty {
        match ty {
            Ty::Var(_) => {
                let _ = self.uni.unify(&ty, &Ty::Int);
                Ty::Int
            }
            other => other,
        }
    }

    /// Operator overloading: if `t` is a user-defined type and a matching
    /// two-argument operator function exists (`add`/`sub`/…/`lt`/…), type the
    /// expression as that function's return type. Returns `None` otherwise, so
    /// the built-in numeric/string path is untouched.
    fn operator_overload(&mut self, op: BinOp, t: &Ty, span: Span) -> Option<Ty> {
        if !matches!(t, Ty::Named(..)) {
            return None;
        }
        let fname = crate::interp::op_overload_name(op)?;
        let (params, ret, qvars) = self.checked.funs.get(fname).cloned()?;
        let (params, ret) = self.instantiate_scheme(&params, &ret, &qvars);
        if params.len() != 2 {
            return None;
        }
        self.unify(&params[0], t, span, "operator operand");
        self.unify(&params[1], t, span, "operator operand");
        Some(self.uni.apply(&ret))
    }

    fn infer_expr(&mut self, expr: &Expr, ctx: &mut Ctx) -> Ty {
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::SizedInt(_, w) => Ty::IntW(*w),
            ExprKind::F32(_) => Ty::F32,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Unit => Ty::Unit,
            ExprKind::Str(pieces) => {
                for p in pieces {
                    if let StrPiece::Expr(e) = p {
                        self.infer_expr(e, ctx); // any type; stringified at runtime
                    }
                }
                Ty::Str
            }
            ExprKind::List(items) => {
                let elem = self.uni.fresh();
                for item in items {
                    let t = self.infer_expr(item, ctx);
                    self.unify(&elem, &t, item.span, "list element");
                }
                Ty::List(Box::new(self.uni.apply(&elem)))
            }
            ExprKind::Range { lo, hi, .. } => {
                let lt = self.infer_expr(lo, ctx);
                self.unify(&Ty::Int, &lt, lo.span, "range bound");
                let ht = self.infer_expr(hi, ctx);
                self.unify(&Ty::Int, &ht, hi.span, "range bound");
                Ty::Range
            }
            ExprKind::Ident(name) => self.infer_ident(name, expr.span, ctx),
            ExprKind::Call { callee, args } => self.infer_call(callee, args, expr.span, ctx),
            ExprKind::MethodCall { recv, name, args } => self.infer_method(recv, name, args, expr.span, ctx),
            ExprKind::Field { recv, name } => {
                let rt = self.infer_expr(recv, ctx);
                match self.uni.apply(&rt) {
                    Ty::Named(tn, args) => {
                        let sig = self.checked.types.get(&tn).cloned();
                        match sig {
                            Some(sig) if sig.variants.len() == 1 => {
                                let m: HashMap<u32, Ty> =
                                    sig.qvars.iter().cloned().zip(args.iter().cloned()).collect();
                                match sig.variants[0].fields.iter().find(|(fname, _)| fname == name) {
                                    Some((_, ty)) => Self::subst_ty(ty, &m),
                                    None => {
                                        let msg = match suggest(
                                            name,
                                            sig.variants[0].fields.iter().map(|(f, _)| f.as_str()),
                                        ) {
                                            Some(s) => format!("type `{tn}` has no field `{name}` (did you mean `{s}`?)"),
                                            None => format!("type `{tn}` has no field `{name}`"),
                                        };
                                        self.err("K0230", msg, expr.span);
                                        self.uni.fresh()
                                    }
                                }
                            }
                            Some(_) => {
                                self.err(
                                    "K0231",
                                    format!("`{tn}` has multiple variants — use `match` to access fields"),
                                    expr.span,
                                );
                                self.uni.fresh()
                            }
                            None => self.uni.fresh(),
                        }
                    }
                    Ty::Var(_) => {
                        self.err(
                            "K0232",
                            format!("cannot infer the type of this value to access field `{name}` — add a type annotation"),
                            recv.span,
                        );
                        self.uni.fresh()
                    }
                    other => {
                        self.err("K0233", format!("{other} has no fields"), expr.span);
                        self.uni.fresh()
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lt = self.infer_expr(lhs, ctx);
                let rt = self.infer_expr(rhs, ctx);
                match op {
                    BinOp::And | BinOp::Or => {
                        self.unify(&Ty::Bool, &lt, lhs.span, "logical operand");
                        self.unify(&Ty::Bool, &rt, rhs.span, "logical operand");
                        Ty::Bool
                    }
                    BinOp::Eq | BinOp::Ne => {
                        self.unify(&lt, &rt, expr.span, "comparison");
                        Ty::Bool
                    }
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        self.unify(&lt, &rt, expr.span, "comparison");
                        let t = self.uni.apply(&lt);
                        if let Some(ret) = self.operator_overload(*op, &t, expr.span) {
                            return ret;
                        }
                        let t = self.default_numeric(t);
                        if !t.is_numeric() && t != Ty::Str {
                            self.err("K0234", format!("cannot order values of type {t}"), expr.span);
                        }
                        Ty::Bool
                    }
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                        self.unify(&lt, &rt, expr.span, "arithmetic");
                        let t = self.uni.apply(&lt);
                        if let Some(ret) = self.operator_overload(*op, &t, expr.span) {
                            return ret;
                        }
                        let t = self.default_numeric(t);
                        let str_ok = *op == BinOp::Add && t == Ty::Str;
                        let tensor_ok = t == Ty::Tensor && *op != BinOp::Rem;
                        if tensor_ok {
                            return t;
                        }
                        if !t.is_numeric() && !str_ok {
                            self.err(
                                "K0235",
                                format!(
                                    "arithmetic needs Int or Float operands, found {t}{}",
                                    if matches!(t, Ty::Named(..)) {
                                        format!(
                                            " — define `fun {}(a: {t}, b: {t}) -> {t}` to overload `{}`",
                                            crate::interp::op_overload_name(*op).unwrap_or("add"),
                                            op_symbol(*op),
                                        )
                                    } else {
                                        String::new()
                                    }
                                ),
                                expr.span,
                            );
                        }
                        t
                    }
                }
            }
            ExprKind::Unary { op, operand } => {
                let t = self.infer_expr(operand, ctx);
                match op {
                    UnOp::Neg => {
                        let t = self.uni.apply(&t);
                        let t = self.default_numeric(t);
                        if !t.is_numeric() {
                            self.err("K0236", format!("unary `-` needs Int or Float, found {t}"), expr.span);
                        }
                        t
                    }
                    UnOp::Not => {
                        self.unify(&Ty::Bool, &t, operand.span, "`!` operand");
                        Ty::Bool
                    }
                }
            }
            ExprKind::If { cond, then_block, else_block } => {
                let ct = self.infer_expr(cond, ctx);
                self.unify(&Ty::Bool, &ct, cond.span, "`if` condition");
                let tt = self.check_block(then_block, ctx);
                match else_block {
                    Some(e) => {
                        let et = self.infer_expr(e, ctx);
                        self.unify(&tt, &et, expr.span, "`if`/`else` branches");
                        self.uni.apply(&tt)
                    }
                    None => Ty::Unit,
                }
            }
            ExprKind::BlockExpr(b) => self.check_block(b, ctx),
            ExprKind::Match { scrutinee, arms } => {
                let st = self.infer_expr(scrutinee, ctx);
                let result = self.uni.fresh();
                for arm in arms {
                    ctx.scopes.push();
                    self.check_pattern(&arm.pattern, &st, ctx);
                    if let Some(guard) = &arm.guard {
                        let gt = self.infer_expr(guard, ctx);
                        self.unify(&Ty::Bool, &gt, guard.span, "match guard (must be Bool)");
                    }
                    let at = self.infer_expr(&arm.body, ctx);
                    self.unify(&result, &at, arm.body.span, "match arms (all arms must have the same type)");
                    ctx.scopes.pop();
                }
                self.check_exhaustive(&st, arms, expr.span);
                self.uni.apply(&result)
            }
            ExprKind::Lambda { params, body } => {
                ctx.scopes.push();
                let mut ptys = Vec::new();
                for p in params {
                    let ty = match &p.ty {
                        Some(t) => self.resolve_ty(t),
                        None => self.uni.fresh(),
                    };
                    ctx.scopes.insert(&p.name, ty.clone(), false);
                    ptys.push(ty);
                }
                let bt = self.check_block(body, ctx);
                ctx.scopes.pop();
                Ty::Fun(ptys, Box::new(bt))
            }
            ExprKind::With { recv, updates } => {
                let rt = self.infer_expr(recv, ctx);
                let rt = self.uni.apply(&rt);
                let Ty::Named(tn, args) = &rt else {
                    self.err("K0233", format!("{rt} has no fields to update"), expr.span);
                    return self.uni.fresh();
                };
                let sig = self.checked.types.get(tn).cloned();
                let m: HashMap<u32, Ty> = sig
                    .as_ref()
                    .map(|s| s.qvars.iter().cloned().zip(args.iter().cloned()).collect())
                    .unwrap_or_default();
                match sig {
                    Some(sig) if sig.variants.len() == 1 => {
                        for (field, value) in updates {
                            let vt = self.infer_expr(value, ctx);
                            match sig.variants[0].fields.iter().find(|(f, _)| f == field) {
                                Some((_, fty)) => {
                                    self.unify(&Self::subst_ty(fty, &m), &vt, value.span, &format!("field `{field}`"));
                                }
                                None => {
                                    let msg = match suggest(
                                        field,
                                        sig.variants[0].fields.iter().map(|(f, _)| f.as_str()),
                                    ) {
                                        Some(s) => format!("type `{tn}` has no field `{field}` (did you mean `{s}`?)"),
                                        None => format!("type `{tn}` has no field `{field}`"),
                                    };
                                    self.err("K0230", msg, value.span)
                                }
                            }
                        }
                        rt
                    }
                    _ => {
                        self.err(
                            "K0231",
                            format!("`{tn}` has multiple variants — use `match` to rebuild"),
                            expr.span,
                        );
                        self.uni.fresh()
                    }
                }
            }
            ExprKind::Try(inner) => {
                let it = self.infer_expr(inner, ctx);
                if ctx.in_handler {
                    self.err(
                        "K0237",
                        "`?` is not allowed in handlers in v0.1 — handle the Result or Option with `match`",
                        expr.span,
                    );
                    return match self.uni.apply(&it) {
                        Ty::Option(t) => self.uni.apply(&t),
                        Ty::Result(t, _) => self.uni.apply(&t),
                        _ => self.uni.fresh(),
                    };
                }
                // `?` works on both Option and Result. Dispatch on the operand's concrete
                // type; when it is not yet known, default to Result (prior behavior). The
                // enclosing function's return type must match the operand's family.
                if let Ty::Option(inner_ty) = self.uni.apply(&it) {
                    let ret_ok = self.uni.fresh();
                    let want = Ty::Option(Box::new(ret_ok));
                    let ret = ctx.ret.clone();
                    if self.uni.unify(&want, &ret).is_err() {
                        let r = self.uni.apply(&ret);
                        self.err(
                            "K0238",
                            format!("`?` on an Option requires the enclosing function to return an Option, but it returns {r}"),
                            expr.span,
                        );
                    }
                    self.uni.apply(&inner_ty)
                } else {
                    let ok = self.uni.fresh();
                    let err = self.uni.fresh();
                    let expected = Ty::Result(Box::new(ok.clone()), Box::new(err.clone()));
                    self.unify(&expected, &it, inner.span, "`?` operand (must be a Result or Option)");
                    let ret_err = self.uni.fresh();
                    let ret_ok = self.uni.fresh();
                    let want = Ty::Result(Box::new(ret_ok), Box::new(ret_err.clone()));
                    let ret = ctx.ret.clone();
                    if self.uni.unify(&want, &ret).is_err() {
                        let r = self.uni.apply(&ret);
                        self.err(
                            "K0238",
                            format!("`?` requires the enclosing function to return a Result, but it returns {r}"),
                            expr.span,
                        );
                    } else {
                        let _ = self.uni.unify(&err, &ret_err);
                    }
                    self.uni.apply(&ok)
                }
            }
            ExprKind::Await(inner) => self.infer_expr(inner, ctx),
            ExprKind::Par(branches) => {
                // all branches must agree; the result is a list of their values
                let elem = self.uni.fresh();
                for b in branches {
                    let t = self.infer_expr(b, ctx);
                    self.unify(&elem, &t, b.span, "`par` branch");
                }
                Ty::List(Box::new(self.uni.apply(&elem)))
            }
        }
    }

    /// Instantiate a function scheme: quantified vars become fresh vars.
    fn instantiate_scheme(&mut self, params: &[Ty], ret: &Ty, qvars: &[u32]) -> (Vec<Ty>, Ty) {
        if qvars.is_empty() {
            return (params.to_vec(), ret.clone());
        }
        let mut mapping: HashMap<u32, Ty> = HashMap::new();
        for q in qvars {
            mapping.insert(*q, self.uni.fresh());
        }
        (
            params.iter().map(|p| Self::subst_ty(p, &mapping)).collect(),
            Self::subst_ty(ret, &mapping),
        )
    }

    /// Replace inference-var ids in `ty` per `m` (used to instantiate a generic
    /// scheme or a generic ADT's field types). Recurses into every constructor.
    fn subst_ty(ty: &Ty, m: &HashMap<u32, Ty>) -> Ty {
        match ty {
            Ty::Var(id) => m.get(id).cloned().unwrap_or(Ty::Var(*id)),
            Ty::List(e) => Ty::List(Box::new(Self::subst_ty(e, m))),
            Ty::Set(e) => Ty::Set(Box::new(Self::subst_ty(e, m))),
            Ty::Map(k, v) => Ty::Map(Box::new(Self::subst_ty(k, m)), Box::new(Self::subst_ty(v, m))),
            Ty::Option(e) => Ty::Option(Box::new(Self::subst_ty(e, m))),
            Ty::Result(a, b) => Ty::Result(Box::new(Self::subst_ty(a, m)), Box::new(Self::subst_ty(b, m))),
            Ty::Fun(ps, r) => Ty::Fun(
                ps.iter().map(|p| Self::subst_ty(p, m)).collect(),
                Box::new(Self::subst_ty(r, m)),
            ),
            Ty::Named(n, args) => {
                Ty::Named(n.clone(), args.iter().map(|a| Self::subst_ty(a, m)).collect())
            }
            other => other.clone(),
        }
    }

    /// Instantiate a constructor's field types with fresh type args, returning
    /// (field types, the `Named` result type carrying those fresh args).
    fn instantiate_ctor(&mut self, tyname: &str, fields: &[(String, Ty)]) -> (Vec<Ty>, Ty) {
        let qvars = self
            .checked
            .types
            .get(tyname)
            .map(|s| s.qvars.clone())
            .unwrap_or_default();
        let mut m = HashMap::new();
        for q in &qvars {
            m.insert(*q, self.uni.fresh());
        }
        let field_tys = fields.iter().map(|(_, t)| Self::subst_ty(t, &m)).collect();
        let args = qvars.iter().map(|q| m[q].clone()).collect();
        (field_tys, Ty::Named(tyname.to_string(), args))
    }

    fn infer_ident(&mut self, name: &str, span: Span, ctx: &mut Ctx) -> Ty {
        if let Some((ty, _)) = ctx.scopes.get(name) {
            return ty;
        }
        if let Some((params, ret, qvars)) = self.checked.funs.get(name).cloned() {
            let (params, ret) = self.instantiate_scheme(&params, &ret, &qvars);
            return Ty::Fun(params, Box::new(ret));
        }
        // nullary constructors as values
        match name {
            "None" => return Ty::Option(Box::new(self.uni.fresh())),
            _ => {}
        }
        if let Some((tyname, fields)) = self.checked.ctors.get(name).cloned() {
            let (field_tys, result) = self.instantiate_ctor(&tyname, &fields);
            if fields.is_empty() {
                return result;
            }
            return Ty::Fun(field_tys, Box::new(result));
        }
        let suggestion = {
            // in-scope locals, user functions, user constructors, and the built-in
            // Option/Result constructors
            let builtins = ["Some", "None", "Ok", "Err"].into_iter();
            let cands = ctx
                .scopes
                .names()
                .chain(self.checked.funs.keys().map(String::as_str))
                .chain(self.checked.ctors.keys().map(String::as_str))
                .chain(builtins);
            suggest(name, cands)
        };
        let msg = match suggestion {
            Some(s) => format!("unknown name `{name}` (did you mean `{s}`?)"),
            None => format!("unknown name `{name}`"),
        };
        self.err("K0240", msg, span);
        self.uni.fresh()
    }

    fn infer_call(&mut self, callee: &Expr, args: &[Arg], span: Span, ctx: &mut Ctx) -> Ty {
        if let ExprKind::Ident(name) = &callee.kind {
            // builtins
            match (name.as_str(), args.len()) {
                ("print", 1) => {
                    self.infer_expr(&args[0].value, ctx);
                    return Ty::Unit;
                }
                ("to_str", 1) => {
                    self.infer_expr(&args[0].value, ctx);
                    return Ty::Str;
                }
                ("panic", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "panic message");
                    return self.uni.fresh();
                }
                ("Map", 0) => {
                    let k = self.uni.fresh();
                    let v = self.uni.fresh();
                    return Ty::Map(Box::new(k), Box::new(v));
                }
                ("Set", 0) => {
                    return Ty::Set(Box::new(self.uni.fresh()));
                }
                ("Set", 1) => {
                    let elem = self.uni.fresh();
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::List(Box::new(elem.clone())), &t, args[0].value.span, "Set(...) argument");
                    return Ty::Set(Box::new(self.uni.apply(&elem)));
                }
                ("tensor", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::List(Box::new(Ty::Float)), &t, args[0].value.span, "tensor(...) argument");
                    return Ty::Tensor;
                }
                ("zeros", 1) | ("arange", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &t, args[0].value.span, "tensor size");
                    return Ty::Tensor;
                }
                ("read_file", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "file path");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("write_file", 2) | ("append_file", 2) => {
                    let p = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &p, args[0].value.span, "file path");
                    let c = self.infer_expr(&args[1].value, ctx);
                    self.unify(&Ty::Str, &c, args[1].value.span, "file contents");
                    return Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Str));
                }
                ("delete_file", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "file path");
                    return Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Str));
                }
                ("file_exists", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "file path");
                    return Ty::Bool;
                }
                ("json_parse", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "json_parse argument");
                    return Ty::Result(
                        Box::new(Ty::Named("Json".into(), vec![])),
                        Box::new(Ty::Str),
                    );
                }
                ("json_stringify", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Named("Json".into(), vec![]), &t, args[0].value.span, "json_stringify argument");
                    return Ty::Str;
                }
                ("env_var", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "env_var name");
                    return Ty::Option(Box::new(Ty::Str));
                }
                ("args", 0) => return Ty::List(Box::new(Ty::Str)),
                ("read_line", 0) => return Ty::Option(Box::new(Ty::Str)),
                ("read_all", 0) => return Ty::Str,
                ("random_ints", 2) => {
                    for a in args {
                        let t = self.infer_expr(&a.value, ctx);
                        self.unify(&Ty::Int, &t, a.value.span, "random_ints argument");
                    }
                    return Ty::List(Box::new(Ty::Int));
                }
                ("random_floats", 2) => {
                    for a in args {
                        let t = self.infer_expr(&a.value, ctx);
                        self.unify(&Ty::Int, &t, a.value.span, "random_floats argument");
                    }
                    return Ty::List(Box::new(Ty::Float));
                }
                ("shuffle", 2) => {
                    let seed = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &seed, args[0].value.span, "shuffle seed");
                    let elem = self.uni.fresh();
                    let list = self.infer_expr(&args[1].value, ctx);
                    self.unify(&Ty::List(Box::new(elem.clone())), &list, args[1].value.span, "shuffle list");
                    return Ty::List(Box::new(self.uni.apply(&elem)));
                }
                ("http_get", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "http_get url");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("http_post", 2) => {
                    let u = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &u, args[0].value.span, "http_post url");
                    let b = self.infer_expr(&args[1].value, ctx);
                    self.unify(&Ty::Str, &b, args[1].value.span, "http_post body");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("http_serve", 2) => {
                    let p = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &p, args[0].value.span, "http_serve port");
                    let h = self.infer_expr(&args[1].value, ctx);
                    let want = Ty::Fun(vec![Ty::Str, Ty::Str], Box::new(Ty::Str));
                    self.unify(&want, &h, args[1].value.span, "http_serve handler");
                    return Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Str));
                }
                ("exec", 2) => {
                    let p = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &p, args[0].value.span, "exec program");
                    let a = self.infer_expr(&args[1].value, ctx);
                    self.unify(&Ty::List(Box::new(Ty::Str)), &a, args[1].value.span, "exec args");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("big", 1) => {
                    self.infer_expr(&args[0].value, ctx);
                    return Ty::BigInt;
                }
                ("rat", 2) => {
                    for a in args {
                        self.infer_expr(&a.value, ctx);
                    }
                    return Ty::Rational;
                }
                ("path_join", 2) => {
                    for a in args {
                        let t = self.infer_expr(&a.value, ctx);
                        self.unify(&Ty::Str, &t, a.value.span, "path_join");
                    }
                    return Ty::Str;
                }
                ("path_base", 1) | ("path_dir", 1) | ("path_ext", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "path");
                    return Ty::Str;
                }
                ("list_dir", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "list_dir path");
                    return Ty::Result(Box::new(Ty::List(Box::new(Ty::Str))), Box::new(Ty::Str));
                }
                ("make_dir", 1) | ("remove_dir", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "directory path");
                    return Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Str));
                }
                ("re_match", 2) | ("re_find", 2) | ("re_find_all", 2) | ("re_replace", 3) => {
                    for a in args {
                        let t = self.infer_expr(&a.value, ctx);
                        self.unify(&Ty::Str, &t, a.value.span, "regex argument");
                    }
                    return match name.as_str() {
                        "re_match" => Ty::Bool,
                        "re_find" => Ty::Option(Box::new(Ty::Str)),
                        "re_find_all" => Ty::List(Box::new(Ty::Str)),
                        _ => Ty::Str, // re_replace
                    };
                }
                ("format_time", 1) | ("date_iso", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &t, args[0].value.span, "format epoch");
                    return Ty::Str;
                }
                ("year_of", 1) | ("month_of", 1) | ("day_of", 1) | ("hour_of", 1)
                | ("minute_of", 1) | ("second_of", 1) | ("weekday_of", 1)
                | ("yearday_of", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &t, args[0].value.span, "epoch seconds");
                    return Ty::Int;
                }
                ("date_make", 6) => {
                    for a in args {
                        let t = self.infer_expr(&a.value, ctx);
                        self.unify(&Ty::Int, &t, a.value.span, "date_make component");
                    }
                    return Ty::Int;
                }
                ("parse_iso", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "parse_iso string");
                    return Ty::Result(Box::new(Ty::Int), Box::new(Ty::Str));
                }
                ("now", 0) => return Ty::Int,
                ("base64_encode", 1) | ("hex_encode", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "encode argument");
                    return Ty::Str;
                }
                ("base64_decode", 1) | ("hex_decode", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "decode argument");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("hash_fnv", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "hash_fnv argument");
                    return Ty::Int;
                }
                ("csv_parse", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "csv_parse argument");
                    return Ty::List(Box::new(Ty::List(Box::new(Ty::Str))));
                }
                ("csv_stringify", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(
                        &Ty::List(Box::new(Ty::List(Box::new(Ty::Str)))),
                        &t,
                        args[0].value.span,
                        "csv_stringify argument",
                    );
                    return Ty::Str;
                }
                ("url_encode", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "url_encode argument");
                    return Ty::Str;
                }
                ("url_decode", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "url_decode argument");
                    return Ty::Result(Box::new(Ty::Str), Box::new(Ty::Str));
                }
                ("query_parse", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Str, &t, args[0].value.span, "query_parse argument");
                    return Ty::List(Box::new(Ty::List(Box::new(Ty::Str))));
                }
                ("query_build", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(
                        &Ty::List(Box::new(Ty::List(Box::new(Ty::Str)))),
                        &t,
                        args[0].value.span,
                        "query_build argument",
                    );
                    return Ty::Str;
                }
                ("eprint", 1) => {
                    self.infer_expr(&args[0].value, ctx);
                    return Ty::Unit;
                }
                ("exit", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    self.unify(&Ty::Int, &t, args[0].value.span, "exit code");
                    return self.uni.fresh();
                }
                ("Some", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    return Ty::Option(Box::new(self.uni.apply(&t)));
                }
                ("Ok", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    return Ty::Result(Box::new(self.uni.apply(&t)), Box::new(self.uni.fresh()));
                }
                ("Err", 1) => {
                    let t = self.infer_expr(&args[0].value, ctx);
                    return Ty::Result(Box::new(self.uni.fresh()), Box::new(self.uni.apply(&t)));
                }
                _ => {}
            }
            // user constructor
            if let Some((tyname, fields)) = self.checked.ctors.get(name).cloned() {
                let (field_tys, result) = self.instantiate_ctor(&tyname, &fields);
                let inst: Vec<(String, Ty)> = fields
                    .iter()
                    .map(|(n, _)| n.clone())
                    .zip(field_tys)
                    .collect();
                self.check_named_args(name, &inst, args, span, ctx);
                return result;
            }
            // component construction (props checked in the caller's own scope)
            if let Some(sig) = self.checked.components.get(name).cloned() {
                self.check_ctor_args(name, &sig, args, span, ctx);
                return Ty::Component(name.clone());
            }
        }
        // general callable
        let ct = self.infer_expr(callee, ctx);
        match self.uni.apply(&ct) {
            Ty::Fun(ps, _) if ps.len() != args.len() => {
                // still walk the arguments so their sub-expressions are checked
                for a in args {
                    if a.name.is_some() {
                        self.err("K0241", "named arguments are only allowed for constructors and props", a.value.span);
                    }
                    self.infer_expr(&a.value, ctx);
                }
                self.err(
                    "K0242",
                    format!("this function takes {} argument(s), {} given", ps.len(), args.len()),
                    span,
                );
                self.uni.fresh()
            }
            // concrete function: check each argument LEFT-TO-RIGHT against its
            // expected parameter type. Checking a concrete earlier argument (e.g. a
            // `List[Item]`) binds the generic type variables a later argument depends
            // on, so a trailing closure like `fn it { it.qty }` sees its parameter's
            // real type instead of failing with K0232 (PR-it134).
            Ty::Fun(ps, r) => {
                for (i, a) in args.iter().enumerate() {
                    if a.name.is_some() {
                        self.err("K0241", "named arguments are only allowed for constructors and props", a.value.span);
                    }
                    let want = self.uni.apply(&ps[i]);
                    let at = self.check_expr_expecting(&a.value, &want, ctx);
                    let want = self.uni.apply(&ps[i]);
                    self.check_assign(&want, &at, span, "function call");
                }
                self.uni.apply(&r)
            }
            // callee type not yet known (e.g. a type variable): fall back to
            // whole-function unification to drive inference
            _ => {
                let mut arg_tys = Vec::new();
                for a in args {
                    if a.name.is_some() {
                        self.err("K0241", "named arguments are only allowed for constructors and props", a.value.span);
                    }
                    arg_tys.push(self.infer_expr(&a.value, ctx));
                }
                let ret = self.uni.fresh();
                let want = Ty::Fun(arg_tys, Box::new(ret.clone()));
                self.unify(&want, &ct, span, "function call");
                self.uni.apply(&ret)
            }
        }
    }

    fn check_named_args(
        &mut self,
        ctor: &str,
        fields: &[(String, Ty)],
        args: &[Arg],
        span: Span,
        ctx: &mut Ctx,
    ) {
        if args.len() != fields.len() {
            self.err(
                "K0243",
                format!("`{ctor}` has {} field(s), {} argument(s) given", fields.len(), args.len()),
                span,
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let target = match &arg.name {
                Some(n) => fields.iter().find(|(fname, _)| fname == n).cloned(),
                None => fields.get(i).cloned(),
            };
            let at = self.infer_expr(&arg.value, ctx);
            match target {
                Some((fname, fty)) => {
                    self.unify(&fty, &at, arg.value.span, &format!("field `{fname}` of `{ctor}`"));
                }
                None => {
                    if let Some(n) = &arg.name {
                        let msg = match suggest(n, fields.iter().map(|(f, _)| f.as_str())) {
                            Some(s) => format!("`{ctor}` has no field named `{n}` (did you mean `{s}`?)"),
                            None => format!("`{ctor}` has no field named `{n}`"),
                        };
                        self.err("K0244", msg, arg.value.span);
                    }
                }
            }
        }
    }

    fn infer_method(&mut self, recv: &Expr, name: &str, args: &[Expr], span: Span, ctx: &mut Ctx) -> Ty {
        let rt = self.infer_expr(recv, ctx);
        let rt = self.uni.apply(&rt);

        let sig: Option<(Vec<Ty>, Ty)> = match (&rt, name) {
            (Ty::List(_), "len") => Some((vec![], Ty::Int)),
            (Ty::List(t), "map") | (Ty::List(t), "par_map") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(u.clone()))],
                    Ty::List(Box::new(u)),
                ))
            }
            (Ty::List(t), "zip_with") => {
                let b = self.uni.fresh();
                let c = self.uni.fresh();
                Some((
                    vec![
                        Ty::List(Box::new(b.clone())),
                        Ty::Fun(vec![(**t).clone(), b], Box::new(c.clone())),
                    ],
                    Ty::List(Box::new(c)),
                ))
            }
            (Ty::List(t), "filter")
            | (Ty::List(t), "par_filter")
            | (Ty::List(t), "take_while")
            | (Ty::List(t), "drop_while") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::List(t.clone()),
            )),
            (Ty::List(t), "par_each") => {
                let u = self.uni.fresh();
                Some((vec![Ty::Fun(vec![(**t).clone()], Box::new(u))], Ty::Unit))
            }
            (Ty::List(t), "find") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::Option(t.clone()),
            )),
            (Ty::List(t), "sum") => {
                let elem = self.uni.apply(t);
                let elem = self.default_numeric(elem);
                if !elem.is_numeric() {
                    self.err("K0245", format!("`sum` needs a List[Int] or List[Float], found List[{elem}]"), span);
                }
                Some((vec![], elem))
            }
            (Ty::List(t), "contains") => Some((vec![(**t).clone()], Ty::Bool)),
            (Ty::List(t), "fold") => {
                let acc = self.uni.fresh();
                Some((
                    vec![
                        acc.clone(),
                        Ty::Fun(vec![acc.clone(), (**t).clone()], Box::new(acc.clone())),
                    ],
                    acc,
                ))
            }
            (Ty::List(t), "scan") => {
                // fold that keeps every running accumulator: (acc, fn(acc, elem) -> acc) -> List[acc]
                let acc = self.uni.fresh();
                Some((
                    vec![
                        acc.clone(),
                        Ty::Fun(vec![acc.clone(), (**t).clone()], Box::new(acc.clone())),
                    ],
                    Ty::List(Box::new(acc)),
                ))
            }
            (Ty::List(t), "any") | (Ty::List(t), "all") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::Bool,
            )),
            (Ty::List(t), "sort") => {
                let elem = self.uni.apply(t);
                if !matches!(elem, Ty::Int | Ty::Float | Ty::Str | Ty::Var(_)) {
                    self.err("K0234", format!("cannot order values of type {elem}"), span);
                }
                Some((vec![], Ty::List(t.clone())))
            }
            (Ty::List(t), "take") | (Ty::List(t), "drop") => {
                Some((vec![Ty::Int], Ty::List(t.clone())))
            }
            (Ty::List(t), "get") => Some((vec![Ty::Int], Ty::Option(t.clone()))),
            (Ty::List(t), "index_of") => {
                Some((vec![(**t).clone()], Ty::Option(Box::new(Ty::Int))))
            }
            (Ty::List(t), "push") => Some((vec![(**t).clone()], Ty::List(t.clone()))),
            (Ty::List(t), "first") | (Ty::List(t), "last") => Some((vec![], Ty::Option(t.clone()))),
            (Ty::List(t), "reverse") => Some((vec![], Ty::List(t.clone()))),
            (Ty::List(t), "join") => {
                let elem = self.uni.apply(t);
                if elem != Ty::Str && !matches!(elem, Ty::Var(_)) {
                    self.err("K0246", format!("`join` needs a List[Str], found List[{elem}]"), span);
                }
                Some((vec![Ty::Str], Ty::Str))
            }
            (Ty::List(_), "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::List(t), "concat") => Some((vec![Ty::List(t.clone())], Ty::List(t.clone()))),
            (Ty::List(t), "unique") | (Ty::List(t), "init") | (Ty::List(t), "tail") => {
                Some((vec![], Ty::List(t.clone())))
            }
            (Ty::List(t), "product") => {
                let elem = self.default_numeric(self.uni.apply(t));
                if !elem.is_numeric() {
                    self.err("K0245", format!("`product` needs a List[Int] or List[Float], found List[{elem}]"), span);
                }
                Some((vec![], elem))
            }
            (Ty::List(t), "min_by") | (Ty::List(t), "max_by") => {
                let key = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(key))],
                    Ty::Option(t.clone()),
                ))
            }
            (Ty::List(t), "min") | (Ty::List(t), "max") => {
                let elem = self.uni.apply(t);
                if !matches!(elem, Ty::Int | Ty::Float | Ty::Str | Ty::Var(_)) {
                    self.err("K0234", format!("cannot order values of type {elem}"), span);
                }
                Some((vec![], Ty::Option(t.clone())))
            }
            (Ty::List(t), "flatten") => {
                let inner = self.uni.fresh();
                self.unify(t, &Ty::List(Box::new(inner.clone())), span, "`flatten` element");
                Some((vec![], Ty::List(Box::new(self.uni.apply(&inner)))))
            }
            (Ty::List(t), "count") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::Int,
            )),
            (Ty::List(t), "flat_map") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::List(Box::new(u.clone()))))],
                    Ty::List(Box::new(u)),
                ))
            }
            (Ty::List(t), "sort_by") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Int))],
                Ty::List(t.clone()),
            )),
            (Ty::List(t), "group_by") => {
                let k = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(k.clone()))],
                    Ty::Map(Box::new(k), Box::new(Ty::List(t.clone()))),
                ))
            }
            (Ty::List(t), "position") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::Option(Box::new(Ty::Int)),
            )),
            (Ty::List(t), "partition") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::List(Box::new(Ty::List(t.clone()))),
            )),
            (Ty::List(t), "window") | (Ty::List(t), "chunk") => {
                Some((vec![Ty::Int], Ty::List(Box::new(Ty::List(t.clone())))))
            }
            (Ty::Str, "len") => Some((vec![], Ty::Int)),
            (Ty::Str, "contains") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "starts_with") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "to_upper") | (Ty::Str, "to_lower") | (Ty::Str, "trim") | (Ty::Str, "trim_start") | (Ty::Str, "trim_end") => Some((vec![], Ty::Str)),
            (Ty::Str, "split") => Some((vec![Ty::Str], Ty::List(Box::new(Ty::Str)))),
            (Ty::Str, "ends_with") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "replace") => Some((vec![Ty::Str, Ty::Str], Ty::Str)),
            (Ty::Str, "chars") => Some((vec![], Ty::List(Box::new(Ty::Str)))),
            (Ty::Str, "repeat") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Str, "parse_int") => Some((vec![], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "parse_float") => Some((vec![], Ty::Option(Box::new(Ty::Float)))),
            (Ty::Str, "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::Str, "reverse") => Some((vec![], Ty::Str)),
            (Ty::Str, "index_of") => Some((vec![Ty::Str], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "count") => Some((vec![Ty::Str], Ty::Int)),
            (Ty::Str, "slice") => Some((vec![Ty::Int, Ty::Int], Ty::Str)),
            (Ty::Str, "pad_left") | (Ty::Str, "pad_right") => Some((vec![Ty::Int, Ty::Str], Ty::Str)),
            (Ty::Str, "lines") => Some((vec![], Ty::List(Box::new(Ty::Str)))),
            (Ty::Str, "rfind") => Some((vec![Ty::Str], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "replace_first") => Some((vec![Ty::Str, Ty::Str], Ty::Str)),
            (Ty::Str, "split_once") => {
                Some((vec![Ty::Str], Ty::Option(Box::new(Ty::List(Box::new(Ty::Str))))))
            }
            (Ty::Int, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Int, "to_float") => Some((vec![], Ty::Float)),
            (Ty::Int, "to_i8") => Some((vec![], Ty::IntW(IntW::I8))),
            (Ty::Int, "to_i16") => Some((vec![], Ty::IntW(IntW::I16))),
            (Ty::Int, "to_i32") => Some((vec![], Ty::IntW(IntW::I32))),
            (Ty::Int, "to_i64") => Some((vec![], Ty::IntW(IntW::I64))),
            (Ty::Int, "to_u8") => Some((vec![], Ty::IntW(IntW::U8))),
            (Ty::Int, "to_u16") => Some((vec![], Ty::IntW(IntW::U16))),
            (Ty::Int, "to_u32") => Some((vec![], Ty::IntW(IntW::U32))),
            (Ty::Int, "to_u64") => Some((vec![], Ty::IntW(IntW::U64))),
            (Ty::IntW(_), "to_int") => Some((vec![], Ty::Int)),
            (Ty::IntW(_), "to_str") => Some((vec![], Ty::Str)),
            (Ty::IntW(_), "to_float") => Some((vec![], Ty::Float)),
            (Ty::IntW(_), "to_i8") => Some((vec![], Ty::IntW(IntW::I8))),
            (Ty::IntW(_), "to_i16") => Some((vec![], Ty::IntW(IntW::I16))),
            (Ty::IntW(_), "to_i32") => Some((vec![], Ty::IntW(IntW::I32))),
            (Ty::IntW(_), "to_i64") => Some((vec![], Ty::IntW(IntW::I64))),
            (Ty::IntW(_), "to_u8") => Some((vec![], Ty::IntW(IntW::U8))),
            (Ty::IntW(_), "to_u16") => Some((vec![], Ty::IntW(IntW::U16))),
            (Ty::IntW(_), "to_u32") => Some((vec![], Ty::IntW(IntW::U32))),
            (Ty::IntW(_), "to_u64") => Some((vec![], Ty::IntW(IntW::U64))),
            (Ty::IntW(w), "wrapping_add") | (Ty::IntW(w), "wrapping_sub")
            | (Ty::IntW(w), "wrapping_mul") | (Ty::IntW(w), "saturating_add")
            | (Ty::IntW(w), "saturating_sub") | (Ty::IntW(w), "saturating_mul")
            | (Ty::IntW(w), "band") | (Ty::IntW(w), "bor") | (Ty::IntW(w), "bxor") => {
                Some((vec![Ty::IntW(*w)], Ty::IntW(*w)))
            }
            (Ty::IntW(w), "bnot") => Some((vec![], Ty::IntW(*w))),
            (Ty::IntW(w), "shl") | (Ty::IntW(w), "shr") => Some((vec![Ty::Int], Ty::IntW(*w))),
            (Ty::F32, "to_float") => Some((vec![], Ty::Float)),
            (Ty::F32, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Float, "to_f32") => Some((vec![], Ty::F32)),
            (Ty::Int, "abs") => Some((vec![], Ty::Int)),
            (Ty::Int, "min") | (Ty::Int, "max") | (Ty::Int, "pow") | (Ty::Int, "gcd") => {
                Some((vec![Ty::Int], Ty::Int))
            }
            (Ty::Int, "clamp") => Some((vec![Ty::Int, Ty::Int], Ty::Int)),
            (Ty::Int, "sign") => Some((vec![], Ty::Int)),
            (Ty::Int, "is_even") | (Ty::Int, "is_odd") => Some((vec![], Ty::Bool)),
            (Ty::Int, "band") | (Ty::Int, "bor") | (Ty::Int, "bxor")
            | (Ty::Int, "shl") | (Ty::Int, "shr") | (Ty::Int, "ushr") => {
                Some((vec![Ty::Int], Ty::Int))
            }
            (Ty::Int, "bnot") => Some((vec![], Ty::Int)),
            (Ty::Int, "to_hex") | (Ty::Int, "to_binary") | (Ty::Int, "to_octal") => {
                Some((vec![], Ty::Str))
            }
            (Ty::Int, "to_radix") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Int, "isqrt") => Some((vec![], Ty::Int)),
            (Ty::Float, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Float, "fmt") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Float, "to_int") => Some((vec![], Ty::Int)),
            (Ty::Float, "abs") | (Ty::Float, "sqrt") => Some((vec![], Ty::Float)),
            (Ty::Float, "floor") | (Ty::Float, "ceil") | (Ty::Float, "round") => {
                Some((vec![], Ty::Float))
            }
            (Ty::Float, "log") | (Ty::Float, "log10") | (Ty::Float, "exp") | (Ty::Float, "sin")
            | (Ty::Float, "cos") | (Ty::Float, "tan") | (Ty::Float, "sign")
            | (Ty::Float, "log2") | (Ty::Float, "cbrt") => Some((vec![], Ty::Float)),
            (Ty::Float, "atan2") | (Ty::Float, "hypot") => Some((vec![Ty::Float], Ty::Float)),
            (Ty::Float, "format") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Float, "clamp") => Some((vec![Ty::Float, Ty::Float], Ty::Float)),
            (Ty::Float, "is_nan") | (Ty::Float, "is_infinite") => Some((vec![], Ty::Bool)),
            (Ty::Float, "min") | (Ty::Float, "max") | (Ty::Float, "pow") => {
                Some((vec![Ty::Float], Ty::Float))
            }
            (Ty::BigInt, "pow") => Some((vec![Ty::Int], Ty::BigInt)),
            (Ty::Rational, "num") | (Ty::Rational, "den") => Some((vec![], Ty::BigInt)),
            (Ty::Rational, "to_float") => Some((vec![], Ty::Float)),
            (Ty::Rational, "recip") => Some((vec![], Ty::Rational)),
            (Ty::BigInt, "abs") => Some((vec![], Ty::BigInt)),
            (Ty::BigInt, "is_negative") => Some((vec![], Ty::Bool)),
            (Ty::BigInt, "sign") => Some((vec![], Ty::Int)),
            (Ty::Option(t), "is_some") | (Ty::Option(t), "is_none") => {
                let _ = t;
                Some((vec![], Ty::Bool))
            }
            (Ty::Option(t), "unwrap_or") => Some((vec![(**t).clone()], (**t).clone())),
            (Ty::Option(t), "map") => {
                let u = self.uni.fresh();
                Some((vec![Ty::Fun(vec![(**t).clone()], Box::new(u.clone()))], Ty::Option(Box::new(u))))
            }
            (Ty::Option(t), "and_then") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Option(Box::new(u.clone()))))],
                    Ty::Option(Box::new(u)),
                ))
            }
            (Ty::Option(t), "filter") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::Option(t.clone()),
            )),
            (Ty::Option(t), "ok_or") => {
                let e = self.uni.fresh();
                Some((vec![e.clone()], Ty::Result(t.clone(), Box::new(e))))
            }
            (Ty::Result(t, e), "is_ok") | (Ty::Result(t, e), "is_err") => {
                let _ = (t, e);
                Some((vec![], Ty::Bool))
            }
            (Ty::Result(t, _), "unwrap_or") => Some((vec![(**t).clone()], (**t).clone())),
            (Ty::Result(t, e), "map") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(u.clone()))],
                    Ty::Result(Box::new(u), e.clone()),
                ))
            }
            (Ty::Result(t, e), "map_err") => {
                let f = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**e).clone()], Box::new(f.clone()))],
                    Ty::Result(t.clone(), Box::new(f)),
                ))
            }
            (Ty::Result(t, e), "and_then") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Result(Box::new(u.clone()), e.clone())))],
                    Ty::Result(Box::new(u), e.clone()),
                ))
            }
            (Ty::Result(t, _), "ok") => Some((vec![], Ty::Option(t.clone()))),
            (Ty::Map(k, v), "insert") => {
                Some((vec![(**k).clone(), (**v).clone()], Ty::Map(k.clone(), v.clone())))
            }
            (Ty::Map(k, v), "get") => Some((vec![(**k).clone()], Ty::Option(v.clone()))),
            (Ty::Map(k, v), "remove") => Some((vec![(**k).clone()], Ty::Map(k.clone(), v.clone()))),
            (Ty::Map(k, _), "contains_key") => Some((vec![(**k).clone()], Ty::Bool)),
            (Ty::Map(k, _), "keys") => Some((vec![], Ty::List(k.clone()))),
            (Ty::Map(_, v), "values") => Some((vec![], Ty::List(v.clone()))),
            (Ty::Map(_, _), "len") => Some((vec![], Ty::Int)),
            (Ty::Map(_, _), "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::Map(k, v), "merge") => {
                Some((vec![Ty::Map(k.clone(), v.clone())], Ty::Map(k.clone(), v.clone())))
            }
            (Ty::Map(k, v), "get_or") => Some((vec![(**k).clone(), (**v).clone()], (**v).clone())),
            (Ty::Map(k, v), "map_values") => {
                let w = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**v).clone()], Box::new(w.clone()))],
                    Ty::Map(k.clone(), Box::new(w)),
                ))
            }
            (Ty::Map(k, v), "filter") => Some((
                vec![Ty::Fun(vec![(**k).clone(), (**v).clone()], Box::new(Ty::Bool))],
                Ty::Map(k.clone(), v.clone()),
            )),
            (Ty::Map(k, v), "fold") => {
                let acc = self.uni.fresh();
                Some((
                    vec![
                        acc.clone(),
                        Ty::Fun(
                            vec![acc.clone(), (**k).clone(), (**v).clone()],
                            Box::new(acc.clone()),
                        ),
                    ],
                    acc,
                ))
            }
            (Ty::Set(t), "insert") | (Ty::Set(t), "remove") => {
                Some((vec![(**t).clone()], Ty::Set(t.clone())))
            }
            (Ty::Set(t), "contains") => Some((vec![(**t).clone()], Ty::Bool)),
            (Ty::Set(_), "len") => Some((vec![], Ty::Int)),
            (Ty::Set(t), "union") | (Ty::Set(t), "intersect") | (Ty::Set(t), "difference") | (Ty::Set(t), "symmetric_difference") => {
                Some((vec![Ty::Set(t.clone())], Ty::Set(t.clone())))
            }
            (Ty::Set(t), "to_list") => Some((vec![], Ty::List(t.clone()))),
            (Ty::Set(_), "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::Set(t), "is_subset") => Some((vec![Ty::Set(t.clone())], Ty::Bool)),
            (Ty::Tensor, "len") => Some((vec![], Ty::Int)),
            (Ty::Tensor, "get") => Some((vec![Ty::Int], Ty::Float)),
            (Ty::Tensor, "sum") | (Ty::Tensor, "mean") | (Ty::Tensor, "max") | (Ty::Tensor, "min") => {
                Some((vec![], Ty::Float))
            }
            (Ty::Tensor, "dot") => Some((vec![Ty::Tensor], Ty::Float)),
            (Ty::Tensor, "scale") => Some((vec![Ty::Float], Ty::Tensor)),
            (Ty::Tensor, "map") => Some((
                vec![Ty::Fun(vec![Ty::Float], Box::new(Ty::Float))],
                Ty::Tensor,
            )),
            (Ty::Tensor, "to_list") => Some((vec![], Ty::List(Box::new(Ty::Float)))),
            (Ty::Component(cname), _) => {
                let sig = self.checked.components.get(cname).cloned().unwrap_or_default();
                match sig.exposes.get(name) {
                    Some((ps, r)) => Some((ps.clone(), r.clone())),
                    None => {
                        self.err(
                            "K0247",
                            format!("component `{cname}` does not expose a function named `{name}`"),
                            span,
                        );
                        return self.uni.fresh();
                    }
                }
            }
            // dynamic dispatch through a contract interface
            (Ty::Contract(cname), _) => {
                let sig = self.checked.contracts.get(cname).cloned().unwrap_or_default();
                match sig.sigs.get(name) {
                    Some((ps, r, _)) => Some((ps.clone(), r.clone())),
                    None => {
                        self.err(
                            "K0247",
                            format!("contract `{cname}` has no function named `{name}`"),
                            span,
                        );
                        return self.uni.fresh();
                    }
                }
            }
            (Ty::Var(_), _) => {
                self.err(
                    "K0248",
                    format!("cannot infer the receiver type for `.{name}(…)` — add a type annotation"),
                    recv.span,
                );
                return self.uni.fresh();
            }
            _ => None,
        };

        match sig {
            None => {
                // UFCS: with no built-in method, `recv.name(args)` resolves to a
                // top-level function `name(recv, args…)` if one fits (built-in
                // methods always take precedence — this is the fallback).
                if let Some((params, ret, qvars)) = self.checked.funs.get(name).cloned() {
                    let (params, ret) = self.instantiate_scheme(&params, &ret, &qvars);
                    if params.len() == args.len() + 1 {
                        self.unify(&params[0], &rt, recv.span, "method receiver (UFCS)");
                        for (a, p) in args.iter().zip(params[1..].iter()) {
                            let at = self.infer_expr(a, ctx);
                            self.unify(p, &at, a.span, "method argument (UFCS)");
                        }
                        return self.uni.apply(&ret);
                    }
                }
                for a in args {
                    self.infer_expr(a, ctx);
                }
                // Suggest a close method name (a built-in) or a UFCS function.
                let cands = BUILTIN_METHODS
                    .iter()
                    .copied()
                    .chain(self.checked.funs.keys().map(String::as_str));
                let msg = match suggest(name, cands) {
                    Some(s) => format!("{rt} has no method `{name}` (did you mean `{s}`?)"),
                    None => format!("{rt} has no method `{name}`"),
                };
                self.err("K0249", msg, span);
                self.uni.fresh()
            }
            Some((params, ret)) => {
                if params.len() != args.len() {
                    for a in args {
                        self.infer_expr(a, ctx);
                    }
                    self.err(
                        "K0250",
                        format!("`.{name}` takes {} argument(s), {} given", params.len(), args.len()),
                        span,
                    );
                } else {
                    // Bidirectional: check each argument AGAINST its expected type,
                    // so lambda parameters get their types from the method signature
                    // before the body is checked (`xs.filter(fn e { e.key == k })`).
                    for (p, a) in params.iter().zip(args.iter()) {
                        let at = self.check_expr_expecting(a, p, ctx);
                        self.check_assign(p, &at, a.span, &format!("argument to `.{name}`"));
                    }
                }
                self.uni.apply(&ret)
            }
        }
    }

    /// Check `expr` against an expected type. For lambdas this pushes the
    /// expected parameter types into scope before checking the body; everything
    /// else falls back to plain inference (the caller unifies afterwards).
    fn check_expr_expecting(&mut self, expr: &Expr, expected: &Ty, ctx: &mut Ctx) -> Ty {
        if let (ExprKind::Lambda { params, body }, Ty::Fun(want_params, _)) =
            (&expr.kind, self.uni.apply(expected))
        {
            if params.len() == want_params.len() {
                ctx.scopes.push();
                let mut ptys = Vec::new();
                for (p, want) in params.iter().zip(want_params.iter()) {
                    let ty = match &p.ty {
                        Some(t) => {
                            let ann = self.resolve_ty(t);
                            self.unify(&ann, want, p.span, &format!("lambda parameter `{}`", p.name));
                            ann
                        }
                        None => want.clone(),
                    };
                    ctx.scopes.insert(&p.name, ty.clone(), false);
                    ptys.push(ty);
                }
                let bt = self.check_block(body, ctx);
                ctx.scopes.pop();
                return Ty::Fun(ptys, Box::new(bt));
            }
        }
        self.infer_expr(expr, ctx)
    }

    fn check_pattern(&mut self, pat: &Pattern, expected: &Ty, ctx: &mut Ctx) {
        match &pat.kind {
            PatternKind::Wildcard => {}
            PatternKind::Bind(name) => {
                let ty = self.uni.apply(expected);
                ctx.scopes.insert(name, ty, false);
            }
            PatternKind::Int(_) => {
                self.unify(expected, &Ty::Int, pat.span, "pattern");
            }
            PatternKind::Bool(_) => {
                self.unify(expected, &Ty::Bool, pat.span, "pattern");
            }
            PatternKind::Str(_) => {
                self.unify(expected, &Ty::Str, pat.span, "pattern");
            }
            PatternKind::Ctor { name, args } => match name.as_str() {
                "Some" => {
                    let inner = self.uni.fresh();
                    self.unify(expected, &Ty::Option(Box::new(inner.clone())), pat.span, "pattern");
                    if args.len() == 1 {
                        self.check_pattern(&args[0], &inner, ctx);
                    } else {
                        self.err("K0251", "`Some` pattern takes exactly one argument", pat.span);
                    }
                }
                "None" => {
                    let inner = self.uni.fresh();
                    self.unify(expected, &Ty::Option(Box::new(inner)), pat.span, "pattern");
                    if !args.is_empty() {
                        self.err("K0252", "`None` pattern takes no arguments", pat.span);
                    }
                }
                "Ok" | "Err" => {
                    let ok = self.uni.fresh();
                    let e = self.uni.fresh();
                    self.unify(
                        expected,
                        &Ty::Result(Box::new(ok.clone()), Box::new(e.clone())),
                        pat.span,
                        "pattern",
                    );
                    let inner = if name == "Ok" { ok } else { e };
                    if args.len() == 1 {
                        self.check_pattern(&args[0], &inner, ctx);
                    } else {
                        self.err("K0253", format!("`{name}` pattern takes exactly one argument"), pat.span);
                    }
                }
                other => match self.checked.ctors.get(other).cloned() {
                    None => {
                        let suggestion = {
                            let builtins = ["Some", "None", "Ok", "Err"].into_iter();
                            let cands =
                                self.checked.ctors.keys().map(String::as_str).chain(builtins);
                            suggest(other, cands)
                        };
                        let msg = match suggestion {
                            Some(s) => {
                                format!("unknown constructor `{other}` in pattern (did you mean `{s}`?)")
                            }
                            None => format!("unknown constructor `{other}` in pattern"),
                        };
                        self.err("K0254", msg, pat.span)
                    }
                    Some((tyname, fields)) => {
                        let (field_tys, result) = self.instantiate_ctor(&tyname, &fields);
                        self.unify(expected, &result, pat.span, "pattern");
                        if args.len() != field_tys.len() {
                            self.err(
                                "K0255",
                                format!("`{other}` has {} field(s), pattern has {}", field_tys.len(), args.len()),
                                pat.span,
                            );
                        }
                        for (a, fty) in args.iter().zip(field_tys.iter()) {
                            self.check_pattern(a, fty, ctx);
                        }
                    }
                },
            },
            PatternKind::Or(alts) => {
                for alt in alts {
                    if pattern_binds_var(alt) {
                        self.err(
                            "K0258",
                            "an or-pattern alternative cannot bind variables",
                            alt.span,
                        );
                    }
                    self.check_pattern(alt, expected, ctx);
                }
            }
            PatternKind::At { name, inner } => {
                let ty = self.uni.apply(expected);
                ctx.scopes.insert(name, ty, false);
                self.check_pattern(inner, expected, ctx);
            }
            PatternKind::Range { .. } => {
                self.unify(expected, &Ty::Int, pat.span, "range pattern");
            }
        }
    }

    fn check_exhaustive(&mut self, scrut: &Ty, arms: &[MatchArm], span: Span) {
        // Only UNGUARDED arms contribute to exhaustiveness — a guarded arm may
        // not run even when its pattern matches. An or-pattern arm covers each
        // of its alternatives.
        let mut catch_all = false;
        let mut covered: HashSet<String> = HashSet::new();
        let mut bools: HashSet<bool> = HashSet::new();
        fn collect(p: &Pattern, catch_all: &mut bool, covered: &mut HashSet<String>, bools: &mut HashSet<bool>) {
            match &p.kind {
                PatternKind::Wildcard | PatternKind::Bind(_) => *catch_all = true,
                PatternKind::Ctor { name, .. } => {
                    covered.insert(name.clone());
                }
                PatternKind::Bool(b) => {
                    bools.insert(*b);
                }
                PatternKind::Or(alts) => {
                    for a in alts {
                        collect(a, catch_all, covered, bools);
                    }
                }
                // `name @ inner` covers whatever `inner` covers (so `name @ _`
                // is a catch-all). Ranges never exhaust an unbounded Int.
                PatternKind::At { inner, .. } => collect(inner, catch_all, covered, bools),
                _ => {}
            }
        }
        for a in arms.iter().filter(|a| a.guard.is_none()) {
            collect(&a.pattern, &mut catch_all, &mut covered, &mut bools);
        }
        if catch_all {
            return;
        }
        let covered: HashSet<&str> = covered.iter().map(String::as_str).collect();
        let missing: Vec<String> = match self.uni.apply(scrut) {
            Ty::Bool => {
                let mut m = Vec::new();
                if !bools.contains(&true) {
                    m.push("true".into());
                }
                if !bools.contains(&false) {
                    m.push("false".into());
                }
                m
            }
            Ty::Option(_) => ["Some", "None"]
                .iter()
                .filter(|v| !covered.contains(**v))
                .map(|v| v.to_string())
                .collect(),
            Ty::Result(_, _) => ["Ok", "Err"]
                .iter()
                .filter(|v| !covered.contains(**v))
                .map(|v| v.to_string())
                .collect(),
            Ty::Named(tn, _) => match self.checked.types.get(&tn) {
                Some(sig) => sig
                    .variants
                    .iter()
                    .filter(|v| !covered.contains(v.name.as_str()))
                    .map(|v| v.name.clone())
                    .collect(),
                None => Vec::new(),
            },
            _ => {
                self.err(
                    "K0256",
                    "this `match` needs a catch-all arm (`_ => …`) — the scrutinee type has unbounded values",
                    span,
                );
                return;
            }
        };
        if !missing.is_empty() {
            self.err(
                "K0257",
                format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                span,
            );
        }
    }
}

/// Whether a pattern binds any variable (used to reject binding or-patterns).
fn pattern_binds_var(p: &Pattern) -> bool {
    match &p.kind {
        PatternKind::Bind(_) | PatternKind::At { .. } => true,
        PatternKind::Ctor { args, .. } => args.iter().any(pattern_binds_var),
        PatternKind::Or(alts) => alts.iter().any(pattern_binds_var),
        _ => false,
    }
}

/// `db` covers `db` and `db.read`; `db.read` covers only itself.
fn covers_effect(budget: &str, used: &str) -> bool {
    used == budget || used.starts_with(&format!("{budget}."))
}

fn op_sym(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Set => "",
        AssignOp::Add => "+",
        AssignOp::Sub => "-",
        AssignOp::Mul => "*",
        AssignOp::Div => "/",
    }
}

/// The source symbol for a binary operator (used in diagnostics).
fn op_symbol(op: crate::ast::BinOp) -> &'static str {
    use crate::ast::BinOp::*;
    match op {
        Add => "+", Sub => "-", Mul => "*", Div => "/", Rem => "%",
        Lt => "<", Le => "<=", Gt => ">", Ge => ">=",
        Eq => "==", Ne => "!=", And => "&&", Or => "||",
    }
}

#[cfg(test)]
mod generic_tests {
    /// Type-check a source string and return the error diagnostics.
    fn errors(src: &str) -> Vec<crate::diag::Diag> {
        let (mut program, mut diags) = crate::parser::parse(src);
        crate::run::inject_prelude(&mut program);
        let (_checked, cdiags) = super::check(&program);
        diags.extend(cdiags);
        diags
            .into_iter()
            .filter(|d| d.severity == crate::diag::Severity::Error)
            .collect()
    }

    #[test]
    fn try_operator_accepts_option_and_matches_return_type() {
        // `?` on an Option in an Option-returning function type-checks (PR-it135).
        let ok = "fun lookup(m: Map[Str, Int], k: Str) -> Option[Int] { let v = m.get(k)?\n    Some(v * 2) }\n\
                  fun main() uses io { print(\"{lookup(Map(), \"x\")}\") }\n";
        assert!(errors(ok).is_empty(), "`?` on Option in an Option fun must compile: {:?}", errors(ok));
        // `?` on an Option in a Result-returning function is a K0238 error.
        let mismatch1 = "fun bad(m: Map[Str, Int]) -> Result[Int, Str] { let v = m.get(\"a\")?\n    Ok(v) }\n";
        assert!(errors(mismatch1).iter().any(|d| d.code == "K0238"), "Option ? in a Result fun must be K0238");
        // `?` on a Result in an Option-returning function is a K0238 error.
        let mismatch2 = "fun half(n: Int) -> Result[Int, Str] { if n % 2 == 0 { Ok(n / 2) } else { Err(\"odd\") } }\n\
                         fun bad(n: Int) -> Option[Int] { let v = half(n)?\n    Some(v) }\n";
        assert!(errors(mismatch2).iter().any(|d| d.code == "K0238"), "Result ? in an Option fun must be K0238");
    }

    #[test]
    fn generic_call_infers_type_var_before_checking_a_later_closure() {
        // Calling a generic fun with a concrete argument followed by a closure that
        // depends on the inferred type parameter now type-checks: the concrete `List[Item]`
        // binds T = Item before the `fn it { it.qty }` closure body is checked, so the
        // field access resolves instead of failing K0232 (PR-it134).
        let src = "type Item = { name: Str, qty: Int }\n\
                   fun first_where[T](xs: List[T], pred: fn(T) -> Bool) -> Option[T] { xs.filter(pred).get(0) }\n\
                   fun main() uses io {\n    let items = [Item(name: \"a\", qty: 7), Item(name: \"b\", qty: 0)]\n    \
                   match first_where(items, fn it { it.qty == 0 }) {\n        Some(_) => print(\"out\")\n        None => print(\"in\")\n    }\n}\n";
        assert!(errors(src).is_empty(), "generic call with a field-accessing closure must type-check: {:?}", errors(src));
    }

    #[test]
    fn contract_conformance_is_structurally_enforced() {
        // A component that `fulfills` a contract must expose every method the contract
        // requires, with a matching signature — structural conformance is checked at
        // compile time with precise, distinct diagnostics (PR-it129).
        let has = |src: &str, code: &str| errors(src).iter().any(|d| d.code == code);

        // Missing a required method -> K0262, naming the method.
        let missing = "contract Store {\n    expose fun put(k: Str, v: Str) -> Bool\n    expose fun size() -> Int\n}\n\
                       component Bad fulfills Store {\n    state n: Int = 0\n    expose fun put(k: Str, v: Str) -> Bool { true }\n}\n";
        assert!(has(missing, "K0262"), "missing contract method must be K0262");

        // Implementing a method with the wrong signature (here the return type) -> K0263.
        let sig = "contract Store {\n    expose fun put(k: Str, v: Str) -> Bool\n}\n\
                   component Bad fulfills Store {\n    state n: Int = 0\n    expose fun put(k: Str, v: Str) -> Int { 5 }\n}\n";
        assert!(has(sig, "K0263"), "signature mismatch must be K0263");

        // Fulfilling an unknown contract -> K0261.
        let unknown = "component Bad fulfills Nonexistent {\n    state n: Int = 0\n}\n";
        assert!(has(unknown, "K0261"), "unknown contract must be K0261");

        // A fully-conforming component is accepted (no conformance error).
        let ok = "contract Greeter {\n    intent \"g\"\n    expose fun greet(name: Str) -> Str\n}\n\
                  component Formal fulfills Greeter {\n    intent \"f\"\n    expose fun greet(name: Str) -> Str { \"hi {name}\" }\n}\n";
        let codes: Vec<_> = errors(ok).into_iter().map(|d| d.code).collect();
        assert!(!codes.iter().any(|c| c.starts_with("K026")), "conforming component must not error: {codes:?}");
    }

    #[test]
    fn deep_nesting_is_a_clean_error_not_a_hang() {
        // Pathologically deep expression nesting (which used to make the type
        // checker hang superlinearly on the owned Ty tree) now yields a clean
        // K0121 diagnostic. A normal nesting depth is unaffected. Run on a
        // production-sized (8 MiB) stack — the default 2 MiB test-thread stack is
        // smaller than the real CLI main thread, and the recursive-descent parser
        // recurses (bounded by K0121) while building the pathological input.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let deep = format!("fun main() {{ let x = {}1{} }}\n", "[".repeat(2000), "]".repeat(2000));
                let e = errors(&deep);
                assert!(e.iter().any(|d| d.code == "K0121"), "expected K0121: {e:?}");
                // deeply nested TYPE ANNOTATIONS parse via a separate path — also bounded
                let deep_ty = format!("fun main() {{ let x: {}Int{} = 0 }}\n", "List[".repeat(2000), "]".repeat(2000));
                let et = errors(&deep_ty);
                assert!(et.iter().any(|d| d.code == "K0121"), "expected K0121 for type: {et:?}");
                // a realistically-nested literal + type still type-check with no errors
                let ok = errors("fun main() { let x = [[[[[1]]]]] }\n");
                assert!(ok.is_empty(), "normal nesting must be clean: {ok:?}");
                let ok_ty = errors("fun f(xs: List[List[List[Int]]]) -> Int { 0 }\nfun main() {}\n");
                assert!(ok_ty.is_empty(), "normal type nesting must be clean: {ok_ty:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn did_you_mean_suggestions() {
        // a typo'd function name suggests the real one; a genuinely unknown name
        // gets no spurious hint
        let e = errors("fun compute(x: Int) -> Int { x }\nfun main() { let y = comptue(1) }\n");
        assert!(
            e.iter().any(|d| d.message.contains("did you mean `compute`?")),
            "{:?}",
            e.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let e2 = errors("fun main() { let z = zzzzqqq }\n");
        assert!(e2.iter().any(|d| d.code == "K0240"));
        assert!(!e2.iter().any(|d| d.message.contains("did you mean")));
    }

    #[test]
    fn did_you_mean_types_and_ctors() {
        // unknown type -> nearest user type or built-in
        let e = errors("type Shape = Circle(r: Int)\nfun f(x: Shpe) -> Int { 1 }\nfun main() {}\n");
        assert!(e.iter().any(|d| d.message.contains("did you mean `Shape`?")), "{e:?}");
        let e2 = errors("fun f(x: Flot) -> Int { 1 }\nfun main() {}\n");
        assert!(e2.iter().any(|d| d.message.contains("did you mean `Float`?")), "{e2:?}");
        // unknown constructor in a pattern -> nearest ctor
        let e3 = errors(
            "type T = Foo | Bar\nfun f(x: T) -> Int { match x { Fooo => 1\n _ => 0 } }\nfun main() {}\n",
        );
        assert!(e3.iter().any(|d| d.message.contains("did you mean `Foo`?")), "{e3:?}");
    }

    #[test]
    fn did_you_mean_handles_transpositions() {
        // A transposition of two adjacent characters (a very common typo) is edit-distance 1
        // under the Damerau-Levenshtein metric, so "did you mean" fires even for short names
        // where the allowed distance is 1 — across types, methods, constructors, and names.
        let ty = errors("fun f(x: Itn) -> Int { x }\nfun main() {}\n");
        assert!(ty.iter().any(|d| d.code == "K0205" && d.message.contains("did you mean `Int`?")), "{ty:?}");
        let meth = errors("fun main() uses io {\n    print([1, 2, 3].frist())\n}\n");
        assert!(meth.iter().any(|d| d.code == "K0249" && d.message.contains("did you mean `first`?")), "{meth:?}");
        let ctor = errors("type T = Foo | Bar\nfun f(x: T) -> Int { match x { Bra => 1\n _ => 0 } }\nfun main() {}\n");
        assert!(ctor.iter().any(|d| d.message.contains("did you mean `Bar`?")), "{ctor:?}");
        // A genuinely far-off name still gets no suggestion (no spurious hints).
        let far = errors("fun main() { let z = zzzzqqq }\n");
        assert!(!far.iter().any(|d| d.message.contains("did you mean")), "{far:?}");
    }

    #[test]
    fn did_you_mean_fields() {
        // K0230 field access -> nearest field name.
        let e = errors("type Point = Point(x: Int, y: Int)\nfun main() uses io {\n    let p = Point(x: 1, y: 2)\n    print(p.xx)\n}\n");
        assert!(e.iter().any(|d| d.code == "K0230" && d.message.contains("did you mean `x`?")), "{e:?}");
        // K0244 constructor field -> nearest field name.
        let e2 = errors("type Point = Point(x: Int, y: Int)\nfun main() uses io {\n    let p = Point(x: 1, yy: 2)\n    print(p.x)\n}\n");
        assert!(e2.iter().any(|d| d.code == "K0244" && d.message.contains("did you mean `y`?")), "{e2:?}");
        // `with` update unknown field -> nearest field name (also K0230).
        let e3 = errors("type Point = Point(x: Int, y: Int)\nfun main() uses io {\n    let p = Point(x: 1, y: 2)\n    let q = p with yy: 5\n    print(q.x)\n}\n");
        assert!(e3.iter().any(|d| d.message.contains("did you mean `y`?")), "{e3:?}");
        // far-off name -> no bogus suggestion.
        let e4 = errors("type Point = Point(x: Int, y: Int)\nfun main() uses io {\n    let p = Point(x: 1, y: 2)\n    print(p.zzzzz)\n}\n");
        assert!(e4.iter().any(|d| d.code == "K0230" && !d.message.contains("did you mean")), "{e4:?}");
    }

    #[test]
    fn did_you_mean_methods() {
        // K0249 unknown method -> nearest built-in method name (a common typo).
        let e = errors("fun main() uses io { print([1].puhs(2)) }\n");
        assert!(
            e.iter().any(|d| d.code == "K0249" && d.message.contains("did you mean `push`?")),
            "{e:?}"
        );
        let e2 = errors("fun main() uses io { print([1, 2].revrese()) }\n");
        assert!(
            e2.iter().any(|d| d.message.contains("did you mean `reverse`?")),
            "{e2:?}"
        );
        // no close match -> plain message (no bogus suggestion)
        let e3 = errors("fun main() uses io { print([1].frobnicate()) }\n");
        assert!(
            e3.iter().any(|d| d.code == "K0249" && !d.message.contains("did you mean")),
            "{e3:?}"
        );
    }

    #[test]
    fn effect_declaration_is_enforced_on_public_functions() {
        // A `pub`/`expose` function must declare every effect it performs (K0301),
        // and the requirement propagates through the private helpers it calls.
        // effects are a separate frontend pass (effects::check_effects), not the type checker
        let eff = |src: &str| -> Vec<crate::diag::Diag> {
            let (mut program, _) = crate::parser::parse(src);
            crate::run::inject_prelude(&mut program);
            crate::effects::check_effects(&program)
        };
        let has_k0301 = |src: &str| eff(src).iter().any(|d| d.code == "K0301");
        // pub fun that does io but doesn't declare it -> K0301
        assert!(has_k0301("pub fun f() -> Int { print(1)\n    2 }\n"));
        // declaring the effect fixes it (no effect errors)
        assert!(!eff("pub fun f() uses io -> Int { print(1)\n    2 }\n").iter().any(|d| d.code == "K0301"));
        // the effect propagates: a pub fun calling a private io helper must declare io
        assert!(has_k0301("fun helper() uses io { print(1) }\npub fun f() -> Int { helper()\n    0 }\n"));
        // an ai fun call requires `uses ai` on a public caller (names the missing effect)
        let ai = eff("ai fun classify(t: Str) -> Str tools [] {\n    intent \"c\"\n}\npub fun f(t: Str) -> Str { classify(t) }\n");
        assert!(ai.iter().any(|d| d.code == "K0301" && d.message.contains("uses ai")));
        // a NON-public function may freely perform effects without declaring them
        assert!(!has_k0301("fun f() -> Int { print(1)\n    2 }\n"));
    }

    #[test]
    fn edit_distance_and_suggest() {
        // A transposition of two adjacent chars is distance 1 (Damerau-Levenshtein), not 2.
        assert_eq!(super::edit_distance("compute", "comptue"), 1);
        assert_eq!(super::edit_distance("total", "totl"), 1);
        assert_eq!(super::edit_distance("abc", "xyz"), 3);
        // ties broken alphabetically; nothing close -> None
        assert_eq!(super::suggest("cat", ["car", "bat"].into_iter()), Some("bat".into()));
        assert_eq!(super::suggest("xyzzy", ["hello", "world"].into_iter()), None);
    }

    #[test]
    fn generic_adt_infers_and_is_sound() {
        // a well-typed generic program has no errors
        assert!(errors(
            "type Box[T] = Box(v: T)\nfun main() { let a: Int = Box(v: 5).v }\n"
        )
        .is_empty());
        // Box[Int] cannot hold a Str — a real type error, not a crash
        let errs = errors(
            "type Box[T] = Box(v: T)\nfun main() { let b: Box[Int] = Box(v: \"x\") }\n",
        );
        assert!(errs.iter().any(|d| d.code == "K0200"), "expected K0200: {errs:?}");
        // two instantiations of the same generic type coexist
        assert!(errors(
            "type Box[T] = Box(v: T)\nfun main() {\n  let a = Box(v: 1)\n  let b = Box(v: \"s\")\n}\n"
        )
        .is_empty());
    }
}
