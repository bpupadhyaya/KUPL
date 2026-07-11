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
/// Render a count with a correctly pluralized noun: `plural(1, "argument")` -> "1 argument",
/// `plural(2, "argument")` -> "2 arguments". Clearer than a literal "argument(s)" in diagnostics.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

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
pub(crate) fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
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

/// Well-known method names from OTHER languages that edit-distance can't reach (too many
/// edits from the KUPL name), mapped to the KUPL method an AI most likely meant. Suggestion-only
/// and best-effort like `suggest` -- only consulted as a K0249 fallback, never changes resolution
/// (PR-it318). `length`/`size` (Java/JS/C#/Swift) -> `len` is the canonical case.
fn common_method_alias(name: &str) -> Option<&'static str> {
    match name {
        "length" | "size" | "count_elements" => Some("len"),
        "append" | "add" | "add_last" => Some("push"),
        "has" | "includes" | "member" => Some("contains"),
        "index_of_first" | "find_index" | "indexOf" => Some("position"),
        "to_string" | "toString" | "str" => Some("to_str"),
        "upper" | "uppercase" | "to_uppercase" => Some("to_upper"),
        "lower" | "lowercase" | "to_lowercase" => Some("to_lower"),
        "sort_asc" | "sorted" => Some("sort_by"),
        _ => None,
    }
}

/// Every built-in method name across all receiver types, for "did you mean"
/// suggestions on an unknown method (K0249). Suggestion-only and best-effort —
/// if a newly added method is missing here the only effect is a missed hint, so
/// it need not track the method-resolution match perfectly.
const BUILTIN_METHODS: &[&str] = &[
    "abs", "abs_diff", "all", "and_then", "any", "band", "bnot", "bor", "bxor", "capitalize", "cbrt", "ceil",
    "center", "chars", "chunk", "clamp", "concat", "contains", "contains_key", "copysign", "cos",
    "count", "count_ones",
    "dedup", "den", "difference", "digits", "div_euclid", "dot", "drop", "drop_while", "ends_with", "exp", "factorial", "filter",
    "find", "first", "flat_map", "flatten", "floor", "fmt", "fold", "format", "fract", "gcd",
    "get", "get_or", "group_by", "hypot", "index_of", "init", "insert", "intersect", "intersperse",
    "is_empty", "is_err", "is_even", "is_infinite", "is_nan", "is_negative", "is_none",
    "is_odd", "is_ok", "is_some", "is_subset", "is_superset", "isqrt", "join", "keys", "last", "lcm",
    "leading_zeros", "len",
    "lines", "log", "map", "map_err", "map_values", "max", "max_by", "mean", "merge",
    "min", "min_by", "mul_add", "num", "ok", "ok_or", "pad_left", "pad_right", "par_each",
    "par_filter", "par_map", "parse_float", "parse_int", "parse_radix", "partition", "position",
    "pow", "product", "push", "recip", "rem_euclid", "remove", "repeat", "replace", "replace_first",
    "reverse", "rfind", "rotate_left", "rotate_right", "round", "saturating_add", "saturating_mul", "saturating_sub",
    "scale", "scan", "shl", "shr", "sign", "sin", "slice", "sort", "sort_by", "split",
    "split_once", "sqrt", "starts_with", "sum", "swapcase", "symmetric_difference", "tail", "take",
    "take_while", "tan", "to_binary", "to_degrees", "to_float", "to_hex", "to_int", "to_list",
    "to_lower", "to_octal", "to_radians", "to_radix", "to_str", "to_upper", "trailing_zeros", "trim", "trim_end",
    "trim_start", "trunc", "union", "unique", "unwrap_or", "ushr", "values", "window", "zip_with",
];

/// Every built-in free-function name (called as `name(...)`, no receiver), for "did you mean"
/// suggestions on an unknown name (K0240). Suggestion-only and best-effort — same discipline as
/// BUILTIN_METHODS: a missing entry only costs a hint, never changes resolution (PR-it249).
const BUILTIN_FUNS: &[&str] = &[
    "append_file", "arange", "args", "big", "delete_file", "env_var", "exec", "file_exists",
    "http_get", "http_post", "http_serve", "json_parse", "json_stringify", "list_dir", "make_dir",
    "panic", "path_base", "path_dir", "path_ext", "path_join", "print", "random_floats",
    "random_ints", "rat", "re_find", "re_find_all", "re_match", "re_replace", "read_all",
    "read_file", "read_line", "remove_dir", "shuffle", "tensor", "to_str", "write_file", "zeros",
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

/// One "column" of the joint exhaustiveness matrix (`Checker::joint_exhaustive`):
/// either a real source sub-pattern, or a synthetic wildcard produced when
/// specializing a row whose pattern at this position was already a
/// catch-all (a bare `_`/bind covers every value, so it "expands" into one
/// synthetic wildcard per field of whichever constructor is being checked).
#[derive(Clone, Copy)]
enum Slot<'a> {
    Pat(&'a Pattern),
    Wild,
}

impl<'a> Slot<'a> {
    fn is_catch_all(&self) -> bool {
        match self {
            Slot::Wild => true,
            Slot::Pat(p) => Checker::pattern_is_catch_all(p),
        }
    }
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

    /// When two DIFFERENT concrete component types have no textual contract
    /// annotation anywhere nearby (so `contract_assignable` can't fire in either
    /// direction — neither side already resolves to `Ty::Contract`), check
    /// whether they nonetheless share exactly ONE contract both `fulfills`. Two
    /// or more shared contracts is ambiguous (which one did the author mean?)
    /// and is left to fail normally rather than silently guessing.
    fn common_fulfilled_contract(&self, a: &Ty, b: &Ty) -> Option<String> {
        let (Ty::Component(x), Ty::Component(y)) = (self.uni.resolve(a), self.uni.resolve(b)) else {
            return None;
        };
        if x == y {
            return None;
        }
        let sig_x = self.checked.components.get(&x)?;
        let sig_y = self.checked.components.get(&y)?;
        let mut common = sig_x.fulfills.iter().filter(|c| sig_y.fulfills.contains(c));
        let first = common.next()?;
        if common.next().is_some() {
            return None;
        }
        Some(first.clone())
    }

    /// Merge two branch types (`if`/`else`, `match` arms): like `unify`, but
    /// admits contract assignability SYMMETRICALLY (either branch may already be
    /// contract-typed) and, when both branches are bare, different concrete
    /// component types with no annotation, widens to their one shared `fulfills`
    /// contract if there is exactly one (e.g. `if b { Mem() } else { Prefix() }`
    /// where both fulfill `Store` — no `unify`-based check, however wrapped,
    /// can accept this on its own, since NEITHER side is `Ty::Contract` yet).
    /// Falls through to plain `unify` (reporting the usual K0200) otherwise.
    fn check_merge(&mut self, a: &Ty, b: &Ty, span: Span, what: &str) -> Ty {
        if self.contract_assignable(a, b) {
            return self.uni.apply(a);
        }
        if self.contract_assignable(b, a) {
            return self.uni.apply(b);
        }
        if let Some(c) = self.common_fulfilled_contract(a, b) {
            return Ty::Contract(c);
        }
        self.unify(a, b, span, what)
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
                                format!("`{n}` takes {}, {} given", plural(params, "type argument"), ats.len()),
                                t.span,
                            );
                            // The annotation is malformed; return an unconstrained type var so we
                            // don't ALSO emit a confusing secondary K0200 "expected Box[Int, Str],
                            // found Box[Int]" when this is later unified (PR-it221).
                            self.uni.fresh()
                        } else {
                            Ty::Named(n.clone(), ats)
                        }
                    }
                    _ => {
                        // Suggest the closest known type (user-declared or a builtin generic) so a
                        // typo like `Opton[Int]` points at `Option` (PR-it480).
                        let mut m = format!("unknown generic type `{n}` with {}", plural(args.len(), "argument"));
                        let builtins = ["List", "Set", "Map", "Option", "Result"];
                        let cands = self
                            .checked
                            .types
                            .keys()
                            .map(|k| k.as_str())
                            .chain(builtins.iter().copied());
                        if let Some(s) = suggest(n, cands) {
                            m.push_str(&format!(" — did you mean `{s}`?"));
                        }
                        self.err("K0206", m, t.span);
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
                // Did-you-mean, matching the same courtesy already given to unknown
                // free-fns/methods/fields/types/ctors/child-components (K0249/K0100/
                // K0206/K0247/K0254/K0208) -- a typo'd `fulfills` contract name got
                // left bare (PR-it512).
                let mut msg = format!("`{}` fulfills unknown contract `{contract_name}`", c.name);
                if let Some(s) = suggest(contract_name, self.checked.contracts.keys().map(String::as_str)) {
                    msg.push_str(&format!(" — did you mean `{s}`?"));
                }
                self.err("K0261", msg, c.span);
                continue;
            };
            let comp_sig = self.checked.components.get(&c.name).cloned().unwrap_or_default();
            for (fname, (params, ret, effects)) in &contract.sigs {
                match comp_sig.exposes.get(fname) {
                    None => {
                        // Did-you-mean, matching K0261's courtesy right above (a typo'd
                        // `fulfills` contract name) -- a typo'd EXPOSED METHOD name landed
                        // here bare: `expose fun gett(...)` for a contract requiring `get`
                        // named the missing method but never suggested the close-by typo
                        // actually exposed, unlike the sibling `.method()` call-site lookup
                        // (`find_method`) which already does this (PR-it581).
                        let mut msg =
                            format!("`{}` fulfills `{contract_name}` but does not expose `{fname}`", c.name);
                        if let Some(s) = suggest(fname, comp_sig.exposes.keys().map(String::as_str)) {
                            msg.push_str(&format!(" — did you mean `{s}`?"));
                        }
                        self.err("K0262", msg, c.span);
                    }
                    Some((cp, cr)) => {
                        // The component's own `expose fun {fname}` declaration, used
                        // below by BOTH the K0263 signature-mismatch span AND the
                        // K0264 effect-budget check right after it -- K0264 already
                        // correctly points at `decl.span` (the specific offending
                        // method), but K0263 used the whole `c.span` (the component's
                        // header line) instead, even though this exact tighter span
                        // was available two lines below it (PR-it585).
                        let decl = c.exposes.iter().find(|f| &f.name == fname);
                        let want = Ty::Fun(params.clone(), Box::new(ret.clone()));
                        let got = Ty::Fun(cp.clone(), Box::new(cr.clone()));
                        if self.uni.unify(&want, &got).is_err() {
                            self.err(
                                "K0263",
                                format!(
                                    "`{}` exposes `{fname}` as {got} but contract `{contract_name}` requires {want}",
                                    c.name
                                ),
                                decl.map(|d| d.span).unwrap_or(c.span),
                            );
                        }
                        // the component's declared effects must fit the contract's budget
                        if let Some(decl) = decl {
                            for e in &decl.effects {
                                if !effects.iter().any(|budget| covers_effect(budget, e)) {
                                    // A contract with an empty effect budget reads more clearly
                                    // as "allows no effects" than "allows only []".
                                    let allowed = if effects.is_empty() {
                                        "no effects".to_string()
                                    } else {
                                        format!("only [{}]", effects.join(", "))
                                    };
                                    self.err(
                                        "K0264",
                                        format!(
                                            "`{}`.`{fname}` uses `{e}` but contract `{contract_name}` allows {allowed}",
                                            c.name,
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
                self.check_assign(&ret, &body_ty, f.body.span, &format!("return value of `{}`", f.name));
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
                    self.check_assign(&ann, &init_ty, s.span, &format!("state `{}`", s.name));
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
                // Did-you-mean, matching the same courtesy already given to unknown
                // free-fns/methods/fields/types/ctors/contract-fns (K0249/K0100/K0206/
                // K0247/K0254) -- a typo'd child-component name got left bare (PR-it511).
                let mut msg = format!("unknown component `{}`", child.component);
                if let Some(s) = suggest(&child.component, self.checked.components.keys().map(String::as_str)) {
                    msg.push_str(&format!(" — did you mean `{s}`?"));
                }
                self.err("K0208", msg, child.span);
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
                // Did-you-mean, matching K0213's `wire`-child lookup (the SAME
                // `child_types` pool) -- a typo'd `supervise` child name named the miss
                // but never suggested the close declared child (PR-it582).
                let mut msg = format!("`supervise` references unknown child `{}`", s.child);
                if let Some(sug) = suggest(&s.child, child_types.keys().map(String::as_str)) {
                    msg.push_str(&format!(" — did you mean `{sug}`?"));
                }
                self.err("K0265", msg, s.span);
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
                        // Name the trigger keyword and the actual (always-zero -- a negative
                        // duration is already rejected earlier by parse_duration, which only
                        // accepts a bare Int token, so `-5ms` fails to parse as a duration at
                        // all) duration, and say WHY it's rejected: a zero-duration timer would
                        // become due again the instant it fires, an infinite tight loop every
                        // time the clock advances (PR-it521).
                        let kw = if matches!(h.trigger, Trigger::Every(_)) { "every" } else { "after" };
                        self.err(
                            "K0266",
                            format!("`on {kw} {ms}ms` — timer duration must be positive (a zero-duration timer would refire on every tick, an infinite loop)"),
                            h.span,
                        );
                    }
                }
                Trigger::Port(p) => {
                    if !seen_triggers.insert(p.clone()) {
                        self.err("K0209", format!("duplicate `on {p}` handler"), h.span);
                    }
                    match sig.in_ports.get(p) {
                        None => {
                            // Did-you-mean, matching the same courtesy already given to
                            // unknown contracts/methods/fields/ctors/child-components
                            // (K0261/K0262/K0247/K0230/K0254/K0208) -- a typo'd port name
                            // in `on <port>` named the miss but never suggested the close
                            // in-scope port name (PR-it582).
                            let hint = if sig.out_ports.contains_key(p) {
                                " (it is an `out` port — handlers react to `in` ports)".to_string()
                            } else if let Some(s) = suggest(p, sig.in_ports.keys().map(String::as_str)) {
                                format!(" — did you mean `{s}`?")
                            } else {
                                String::new()
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
            let mut msg = format!("`wire` references unknown child `{child}`");
            if let Some(s) = suggest(child, child_types.keys().map(String::as_str)) {
                msg.push_str(&format!(" — did you mean `{s}`?"));
            }
            self.err("K0213", msg, span);
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
                // Did-you-mean, matching the sibling child-name lookup right above
                // (K0213) -- a typo'd PORT name (once the child itself resolved fine)
                // named the miss but never suggested the close in-scope port (PR-it582).
                let mut msg = format!("component `{comp_name}` has no `{kind}` port named `{port}`");
                if let Some(s) = suggest(port, map.keys().map(String::as_str)) {
                    msg.push_str(&format!(" — did you mean `{s}`?"));
                }
                self.err("K0214", msg, span);
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
                    // A prop supplied twice (two named, or a positional colliding with a named on
                    // the same slot) was silently accepted, mirroring the record-field hole fixed
                    // in PR-it213/214 — reject it here for the component-prop path too (PR-it215).
                    if !supplied.insert(pname.clone()) {
                        self.err("K0215", format!("duplicate prop `{pname}` when constructing `{comp_name}`"), arg.value.span);
                    }
                    self.check_assign(&pty, &arg_ty, arg.value.span, &format!("prop `{pname}` of `{comp_name}`"));
                }
                None => {
                    let msg = match &arg.name {
                        Some(n) => {
                            // Did-you-mean, matching the same courtesy given to unknown
                            // record fields/ctor fields/exposed methods (K0230/K0244/
                            // K0247) -- a typo'd NAMED prop named the miss but never
                            // suggested the close prop actually declared (PR-it582).
                            let mut msg = format!("component `{comp_name}` has no prop named `{n}`");
                            if let Some(s) = suggest(n, sig.props.iter().map(|(pn, _, _)| pn.as_str())) {
                                msg.push_str(&format!(" — did you mean `{s}`?"));
                            }
                            msg
                        }
                        None => format!("too many arguments for `{comp_name}` (has {} props)", sig.props.len()),
                    };
                    self.err("K0215", msg, arg.value.span);
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
                    None => {
                        // Did-you-mean, matching K0211's sibling `on <port>` lookup --
                        // a typo'd port name in `send <port>` named the miss but never
                        // suggested the close in-scope `in` port (PR-it582).
                        let mut msg =
                            format!("`send {port}`: component `{}` has no `in` port named `{port}`", c.name);
                        if let Some(s) = suggest(port, sig.in_ports.keys().map(String::as_str)) {
                            msg.push_str(&format!(" — did you mean `{s}`?"));
                        }
                        self.err("K0217", msg, *span);
                    }
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
                        None => {
                            let mut msg = format!("unknown variable `{name}`");
                            if let Some(s) = suggest(name, ctx.scopes.names()) {
                                msg.push_str(&format!(" — did you mean `{s}`?"));
                            }
                            self.err("K0220", msg, target.span);
                        }
                        Some((ty, mutable)) => {
                            if !mutable {
                                self.err(
                                    "K0221",
                                    // Applies to both `let` bindings and function parameters, which
                                    // are immutable by default — the old wording wrongly claimed a
                                    // parameter was "declared with `let`" (PR-it220).
                                    format!("`{name}` is immutable — use `var` for a reassignable local (or `state` in a component)"),
                                    target.span,
                                );
                            }
                            self.check_assign(&ty, &value_ty, *span, &format!("assignment to `{name}`"));
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
                self.check_assign(&ret, &vt, *span, "return value");
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
                        // Did-you-mean, matching K0211/K0217's sibling port lookups --
                        // a typo'd port name in `emit <port>` named the miss but never
                        // suggested the close in-scope `out` port (PR-it582).
                        let hint = if sig.in_ports.contains_key(port) {
                            " (it is an `in` port — you can only `emit` on `out` ports)".to_string()
                        } else if let Some(s) = suggest(port, sig.out_ports.keys().map(String::as_str)) {
                            format!(" — did you mean `{s}`?")
                        } else {
                            String::new()
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
            Stmt::Break(span) => {
                if ctx.loop_depth == 0 {
                    self.err("K0229", "`break` outside of a loop", *span);
                }
                Ty::Unit
            }
            Stmt::Continue(span) => {
                if ctx.loop_depth == 0 {
                    self.err("K0229", "`continue` outside of a loop", *span);
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
                // Threaded as a plain Rust value once any two elements widen to a
                // shared `fulfills` contract via check_merge (same shape as the
                // match-arm merge, PR-it566): that widening doesn't rebind the
                // Unifier's own type variable, so a THIRD element must merge
                // against the already-widened contract type, not a stale
                // first-element type. `[Mem(), Prefix()]` (all fulfilling one
                // contract) now infers `List[Contract]` instead of a bare K0200.
                let mut elem: Option<Ty> = None;
                for item in items {
                    let t = self.infer_expr(item, ctx);
                    elem = Some(match elem {
                        None => {
                            let fresh = self.uni.fresh();
                            self.check_merge(&fresh, &t, item.span, "list element")
                        }
                        Some(e) => self.check_merge(&e, &t, item.span, "list element"),
                    });
                }
                match elem {
                    Some(e) => Ty::List(Box::new(self.uni.apply(&e))),
                    // an empty list literal `[]` stays a fresh, unresolved
                    // element type -- inferred later from context (e.g. the
                    // `let`/`var` annotation it's assigned to).
                    None => Ty::List(Box::new(self.uni.fresh())),
                }
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
                            Some(sig) => {
                                // Name the actual variants so the fix is immediately visible instead of
                                // leaving the user to look up the type definition (PR-it498).
                                let names = sig.variants.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ");
                                self.err(
                                    "K0231",
                                    format!("`{tn}` has multiple variants ({names}) — use `match` to access `.{name}`"),
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
                            format!(
                                "cannot infer the type of this value to access field `{name}` — \
                                 annotate its binding or parameter so the record type is known \
                                 (e.g. `let acc: List[Row] = []` for an empty-list fold seed)"
                            ),
                            recv.span,
                        );
                        self.uni.fresh()
                    }
                    other => {
                        // A field access on a LIST is a frequent mistake -- e.g. reaching for `.fst`/`.snd`
                        // on a `split_once` result (which returns a List[Str], not a record). Point at the
                        // list accessors instead of the bare "has no fields" (PR-it486).
                        let hint = if matches!(other, Ty::List(_)) {
                            " — a list is indexed, not field-accessed: use `.get(i)` (returns Option), `.first()`, or `.last()`"
                        } else {
                            ""
                        };
                        self.err(
                            "K0233",
                            format!("{other} has no fields (only records and components have fields){hint}"),
                            expr.span,
                        );
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
                            self.err("K0234", format!("cannot order values of type {t}; only Int, Float, Str, and other numeric types can be compared"), expr.span);
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
                                    if *op == BinOp::Add && matches!(t, Ty::List(..)) {
                                        " — `+` is arithmetic, not list concatenation; use `[a, b].flatten()` to join two lists".to_string()
                                    } else if matches!(t, Ty::Named(..)) {
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
                        self.check_merge(&tt, &et, expr.span, "`if`/`else` branches")
                    }
                    None => Ty::Unit,
                }
            }
            ExprKind::BlockExpr(b) => self.check_block(b, ctx),
            ExprKind::Match { scrutinee, arms } => {
                let st = self.infer_expr(scrutinee, ctx);
                // `result` is threaded as a plain Rust value (not left to the
                // Unifier's internal type-variable state) once any arm widens to
                // a shared `fulfills` contract via check_merge, since that
                // widening doesn't rebind any underlying type variable -- a
                // later arm must be merged against the WIDENED type, not a
                // stale variable that still thinks it's bound to the first
                // arm's bare concrete component type.
                let mut result: Option<Ty> = None;
                for arm in arms {
                    ctx.scopes.push();
                    self.check_pattern(&arm.pattern, &st, ctx);
                    if let Some(guard) = &arm.guard {
                        let gt = self.infer_expr(guard, ctx);
                        self.unify(&Ty::Bool, &gt, guard.span, "match guard (must be Bool)");
                    }
                    let at = self.infer_expr(&arm.body, ctx);
                    result = Some(match result {
                        None => {
                            let fresh = self.uni.fresh();
                            self.check_merge(&fresh, &at, arm.body.span, "match arms (all arms must have the same type)")
                        }
                        Some(r) => self.check_merge(&r, &at, arm.body.span, "match arms (all arms must have the same type)"),
                    });
                    ctx.scopes.pop();
                }
                self.check_exhaustive(&st, arms, expr.span);
                match result {
                    Some(r) => self.uni.apply(&r),
                    // no arms at all: unreachable in practice (the parser
                    // requires at least one arm), but Unit is a safe fallback.
                    None => Ty::Unit,
                }
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
                    self.err(
                        "K0233",
                        format!("{rt} has no fields to update (only records and components have fields)"),
                        expr.span,
                    );
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
                                    self.check_assign(&Self::subst_ty(fty, &m), &vt, value.span, &format!("field `{field}`"));
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
                    Some(sig) => {
                        // Name the actual variants, same as the field-access K0231 (PR-it498).
                        let names = sig.variants.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ");
                        self.err(
                            "K0231",
                            format!("`{tn}` has multiple variants ({names}) — use `match` to rebuild"),
                            expr.span,
                        );
                        self.uni.fresh()
                    }
                    None => {
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
                        // When the enclosing function returns a Result, the fix is to convert the
                        // Option to a Result before `?` (parse_int() etc. return Option) (PR-it252).
                        let hint = if matches!(r, Ty::Result(..)) {
                            " — convert it first with `.ok_or(err)?`"
                        } else {
                            ""
                        };
                        self.err(
                            "K0238",
                            format!("`?` on an Option requires the enclosing function to return an Option, but it returns {r}{hint}"),
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
                        // When the enclosing function returns an Option, the fix is to convert the
                        // Result to an Option before `?` (PR-it252).
                        let hint = if matches!(r, Ty::Option(_)) {
                            " — convert it first with `.ok()?`"
                        } else {
                            ""
                        };
                        self.err(
                            "K0238",
                            format!("`?` requires the enclosing function to return a Result, but it returns {r}{hint}"),
                            expr.span,
                        );
                    } else {
                        // The operand's Err type must match the enclosing function's Err type --
                        // `?` propagates it as-is. This was previously unified and DISCARDED (`let _ =`),
                        // so a mismatched Err type (e.g. inner returns Result[_, Int], outer returns
                        // Result[_, Str]) silently type-checked; propagating Err(42) through `?` produced
                        // no diagnostic even though a direct `Err(42)` return in the same function was
                        // correctly rejected as K0200 (PR-it494).
                        self.unify(&ret_err, &err, expr.span, "`?` error type (propagated by `?` into the return type)");
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
            let builtins = ["Some", "None", "Ok", "Err", "Map", "Set"].into_iter();
            let cands = ctx
                .scopes
                .names()
                .chain(self.checked.funs.keys().map(String::as_str))
                .chain(self.checked.ctors.keys().map(String::as_str))
                .chain(builtins)
                .chain(BUILTIN_FUNS.iter().copied());
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
                    if let Some(n) = &a.name {
                        self.err(
                            "K0241",
                            format!(
                                "`{n}:` is a named argument, but named arguments are only allowed for constructors and props here -- call positionally instead: `{}`",
                                crate::fmt::expr_str(&a.value, 0)
                            ),
                            a.value.span,
                        );
                    }
                    self.infer_expr(&a.value, ctx);
                }
                self.err(
                    "K0242",
                    format!("this function takes {}, {} given", plural(ps.len(), "argument"), args.len()),
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
                    if let Some(n) = &a.name {
                        self.err(
                            "K0241",
                            format!(
                                "`{n}:` is a named argument, but named arguments are only allowed for constructors and props here -- call positionally instead: `{}`",
                                crate::fmt::expr_str(&a.value, 0)
                            ),
                            a.value.span,
                        );
                    }
                    let want = self.uni.apply(&ps[i]);
                    let at = self.check_expr_expecting(&a.value, &want, ctx);
                    let want = self.uni.apply(&ps[i]);
                    // Name which argument mismatched (1-based) so a multi-arg call points at the
                    // offending position instead of a bare "in function call" (PR-it236).
                    self.check_assign(&want, &at, a.value.span, &format!("argument {}", i + 1));
                }
                self.uni.apply(&r)
            }
            // callee is a KNOWN concrete non-function type (e.g. calling `x(3)` where x: Int):
            // say so plainly instead of unifying it against an invented `fn(Int) -> ?N`, which
            // surfaced a confusing "expected fn(Int) -> ?0, found Int" with a raw type variable
            // (PR-it204). Still walk the arguments so their sub-expressions are checked.
            other if !matches!(other, Ty::Var(_)) => {
                for a in args {
                    if let Some(n) = &a.name {
                        self.err(
                            "K0241",
                            format!(
                                "`{n}:` is a named argument, but named arguments are only allowed for constructors and props here -- call positionally instead: `{}`",
                                crate::fmt::expr_str(&a.value, 0)
                            ),
                            a.value.span,
                        );
                    }
                    self.infer_expr(&a.value, ctx);
                }
                self.err(
                    "K0200",
                    format!("cannot call a value of type {other}; it is not a function"),
                    span,
                );
                self.uni.fresh()
            }
            // callee type not yet known (a type variable): fall back to whole-function
            // unification to drive inference
            _ => {
                let mut arg_tys = Vec::new();
                for a in args {
                    if let Some(n) = &a.name {
                        self.err(
                            "K0241",
                            format!(
                                "`{n}:` is a named argument, but named arguments are only allowed for constructors and props here -- call positionally instead: `{}`",
                                crate::fmt::expr_str(&a.value, 0)
                            ),
                            a.value.span,
                        );
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
            let mut m = format!("`{ctor}` has {}, {} given", plural(fields.len(), "field"), plural(args.len(), "argument"));
            // When too few arguments are given AND every one is named, we know exactly which
            // fields were left out -- name them instead of leaving the user to diff the two
            // lists (a far more actionable message than a bare count) (PR-it484).
            if args.len() < fields.len() && !args.is_empty() && args.iter().all(|a| a.name.is_some()) {
                let named: HashSet<&str> = args.iter().filter_map(|a| a.name.as_deref()).collect();
                let missing: Vec<String> = fields
                    .iter()
                    .filter(|(f, _)| !named.contains(f.as_str()))
                    .map(|(f, _)| format!("`{f}`"))
                    .collect();
                if !missing.is_empty() {
                    m.push_str(&format!(" — missing {}", missing.join(", ")));
                }
            }
            self.err("K0243", m, span);
        }
        // Track supplied field names so a repeated named field is caught rather than silently
        // overwriting (interp) or crashing at runtime (KVM) — a duplicate can even mask a missing
        // field when the argument count happens to match (PR-it213).
        let mut supplied: HashSet<String> = HashSet::new();
        for (i, arg) in args.iter().enumerate() {
            let target = match &arg.name {
                Some(n) => {
                    if fields.iter().any(|(fname, _)| fname == n) && !supplied.insert(n.clone()) {
                        self.err("K0244", format!("duplicate field `{n}` in `{ctor}`"), arg.value.span);
                    }
                    fields.iter().find(|(fname, _)| fname == n).cloned()
                }
                None => {
                    // A positional argument fills field `i`; record it so a later named arg for the
                    // same field (or vice versa) is caught as a duplicate (PR-it214).
                    if let Some((fname, _)) = fields.get(i) {
                        if !supplied.insert(fname.clone()) {
                            self.err("K0244", format!("duplicate field `{fname}` in `{ctor}`"), arg.value.span);
                        }
                    }
                    fields.get(i).cloned()
                }
            };
            let at = self.infer_expr(&arg.value, ctx);
            match target {
                Some((fname, fty)) => {
                    self.check_assign(&fty, &at, arg.value.span, &format!("field `{fname}` of `{ctor}`"));
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
        // When the argument count lines up, a duplicate can still hide a field that was never
        // supplied — the count check above wouldn't fire, so name each field no argument reached.
        // `supplied` now tracks positional slots too, so mixed positional+named cases are covered.
        if args.len() == fields.len() && !args.is_empty() {
            for (fname, _) in fields {
                if !supplied.contains(fname) {
                    self.err("K0243", format!("missing field `{fname}` in `{ctor}`"), span);
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
                    // The receiver LIST's element type is wrong, not the `.sum()` call
                    // syntax itself -- point at `recv.span` (the list expression), not
                    // the whole method-call `span` (receiver-through-closing-paren),
                    // matching the tighter span other call-argument diagnostics already
                    // use elsewhere in this file (PR-it585).
                    self.err("K0245", format!("`sum` needs a numeric List (Int/Float/sized int/f32/BigInt/Rational), found List[{elem}]"), recv.span);
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
                // Widened PR-it549: sized ints/f32/BigInt/Rational are all orderable (the
                // runtime's `list_order` already backs `min`/`max`/min_by/max_by with them,
                // and native's k_cmp always supported them) — `.sort()` was needlessly
                // narrower than what the language could already do.
                if !(elem.is_numeric() || elem == Ty::Str || matches!(elem, Ty::Var(_))) {
                    // Point at the receiver list, not the whole `.sort()` call (PR-it585).
                    self.err("K0234", format!("cannot order values of type {elem}; only Int, Float, Str, and other numeric types can be compared"), recv.span);
                }
                Some((vec![], Ty::List(t.clone())))
            }
            (Ty::List(t), "take")
            | (Ty::List(t), "drop")
            | (Ty::List(t), "rotate_left")
            | (Ty::List(t), "rotate_right") => Some((vec![Ty::Int], Ty::List(t.clone()))),
            (Ty::List(t), "get") => Some((vec![Ty::Int], Ty::Option(t.clone()))),
            (Ty::List(t), "index_of") => {
                Some((vec![(**t).clone()], Ty::Option(Box::new(Ty::Int))))
            }
            (Ty::List(t), "push") => Some((vec![(**t).clone()], Ty::List(t.clone()))),
            (Ty::List(t), "intersperse") => Some((vec![(**t).clone()], Ty::List(t.clone()))),
            (Ty::List(t), "first") | (Ty::List(t), "last") => Some((vec![], Ty::Option(t.clone()))),
            (Ty::List(t), "reverse") => Some((vec![], Ty::List(t.clone()))),
            (Ty::List(t), "join") => {
                let elem = self.uni.apply(t);
                if elem != Ty::Str && !matches!(elem, Ty::Var(_)) {
                    // Point at the receiver list, not the whole `.join(...)` call (PR-it585).
                    self.err("K0246", format!("`join` needs a List[Str], found List[{elem}]"), recv.span);
                }
                Some((vec![Ty::Str], Ty::Str))
            }
            (Ty::List(_), "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::List(t), "concat") => Some((vec![Ty::List(t.clone())], Ty::List(t.clone()))),
            (Ty::List(t), "unique") | (Ty::List(t), "dedup") | (Ty::List(t), "init") | (Ty::List(t), "tail") => {
                Some((vec![], Ty::List(t.clone())))
            }
            (Ty::List(t), "product") => {
                let elem = self.default_numeric(self.uni.apply(t));
                if !elem.is_numeric() {
                    // Point at the receiver list, not the whole `.product()` call (PR-it585).
                    self.err("K0245", format!("`product` needs a numeric List (Int/Float/sized int/f32/BigInt/Rational), found List[{elem}]"), recv.span);
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
                if !(elem.is_numeric() || elem == Ty::Str || matches!(elem, Ty::Var(_))) {
                    // Point at the receiver list, not the whole `.min()`/`.max()` call (PR-it585).
                    self.err("K0234", format!("cannot order values of type {elem}; only Int, Float, Str, and other numeric types can be compared"), recv.span);
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
            (Ty::Str, "to_upper") | (Ty::Str, "to_lower") | (Ty::Str, "capitalize") | (Ty::Str, "swapcase") | (Ty::Str, "trim") | (Ty::Str, "trim_start") | (Ty::Str, "trim_end") => Some((vec![], Ty::Str)),
            (Ty::Str, "split") => Some((vec![Ty::Str], Ty::List(Box::new(Ty::Str)))),
            (Ty::Str, "ends_with") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "replace") => Some((vec![Ty::Str, Ty::Str], Ty::Str)),
            (Ty::Str, "chars") => Some((vec![], Ty::List(Box::new(Ty::Str)))),
            (Ty::Str, "repeat") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Str, "parse_int") => Some((vec![], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "parse_radix") => Some((vec![Ty::Int], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "parse_float") => Some((vec![], Ty::Option(Box::new(Ty::Float)))),
            (Ty::Str, "is_empty") => Some((vec![], Ty::Bool)),
            (Ty::Str, "reverse") => Some((vec![], Ty::Str)),
            (Ty::Str, "index_of") => Some((vec![Ty::Str], Ty::Option(Box::new(Ty::Int)))),
            (Ty::Str, "count") => Some((vec![Ty::Str], Ty::Int)),
            (Ty::Str, "slice") => Some((vec![Ty::Int, Ty::Int], Ty::Str)),
            (Ty::Str, "pad_left") | (Ty::Str, "pad_right") | (Ty::Str, "center") => {
                Some((vec![Ty::Int, Ty::Str], Ty::Str))
            }
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
            (Ty::Int, "abs_diff") => Some((vec![Ty::Int], Ty::Int)),
            (Ty::Int, "digits") => Some((vec![], Ty::List(Box::new(Ty::Int)))),
            (Ty::Int, "min") | (Ty::Int, "max") | (Ty::Int, "pow") | (Ty::Int, "gcd")
            | (Ty::Int, "lcm") | (Ty::Int, "rem_euclid") | (Ty::Int, "div_euclid") => {
                Some((vec![Ty::Int], Ty::Int))
            }
            (Ty::Int, "clamp") => Some((vec![Ty::Int, Ty::Int], Ty::Int)),
            (Ty::Int, "sign") => Some((vec![], Ty::Int)),
            (Ty::Int, "is_even") | (Ty::Int, "is_odd") => Some((vec![], Ty::Bool)),
            (Ty::Int, "band") | (Ty::Int, "bor") | (Ty::Int, "bxor")
            | (Ty::Int, "shl") | (Ty::Int, "shr") | (Ty::Int, "ushr") => {
                Some((vec![Ty::Int], Ty::Int))
            }
            (Ty::Int, "bnot") | (Ty::Int, "count_ones") | (Ty::Int, "leading_zeros")
            | (Ty::Int, "trailing_zeros") => Some((vec![], Ty::Int)),
            (Ty::Int, "to_hex") | (Ty::Int, "to_binary") | (Ty::Int, "to_octal") => {
                Some((vec![], Ty::Str))
            }
            (Ty::Int, "to_radix") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Int, "isqrt") | (Ty::Int, "factorial") => Some((vec![], Ty::Int)),
            (Ty::Float, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Float, "fmt") => Some((vec![Ty::Int], Ty::Str)),
            (Ty::Float, "to_int") => Some((vec![], Ty::Int)),
            (Ty::Float, "abs") | (Ty::Float, "sqrt") => Some((vec![], Ty::Float)),
            (Ty::Float, "floor") | (Ty::Float, "ceil") | (Ty::Float, "round")
            | (Ty::Float, "trunc") | (Ty::Float, "fract") => {
                Some((vec![], Ty::Float))
            }
            (Ty::Float, "log") | (Ty::Float, "log10") | (Ty::Float, "exp") | (Ty::Float, "sin")
            | (Ty::Float, "cos") | (Ty::Float, "tan") | (Ty::Float, "sign")
            | (Ty::Float, "log2") | (Ty::Float, "cbrt") | (Ty::Float, "to_degrees")
            | (Ty::Float, "to_radians") => Some((vec![], Ty::Float)),
            (Ty::Float, "atan2") | (Ty::Float, "hypot") | (Ty::Float, "copysign") => {
                Some((vec![Ty::Float], Ty::Float))
            }
            (Ty::Float, "mul_add") => Some((vec![Ty::Float, Ty::Float], Ty::Float)),
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
            (Ty::Set(t), "is_subset") | (Ty::Set(t), "is_superset") => {
                Some((vec![Ty::Set(t.clone())], Ty::Bool))
            }
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
                        // A frequent mistake is calling a PORT as a method (`c.click()`). Ports are
                        // not methods: an in-port receives via `wire … -> inst.port` (or `send`),
                        // an out-port is read via `wire inst.port -> …`. Name that instead of the
                        // bare "does not expose a function" (PR-it232).
                        let msg = if sig.in_ports.contains_key(name) {
                            format!("`{name}` is an input port of `{cname}`, not a method — deliver to it with `wire … -> {name}` (or `send`), don't call it")
                        } else if sig.out_ports.contains_key(name) {
                            format!("`{name}` is an output port of `{cname}`, not a method — read it with `wire {name} -> …`, don't call it")
                        } else {
                            // Not a port either — a plain typo on an exposed function. Suggest the
                            // closest exposed name so the fix is one edit away (PR-it477).
                            let mut m = format!("component `{cname}` does not expose a function named `{name}`");
                            if let Some(s) = suggest(name, sig.exposes.keys().map(|k| k.as_str())) {
                                m.push_str(&format!(" — did you mean `{s}`?"));
                            }
                            m
                        };
                        self.err("K0247", msg, span);
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
                        // Same courtesy for contract dynamic dispatch: name the closest
                        // contract function instead of a bare "has no function" (PR-it477).
                        let mut m = format!("contract `{cname}` has no function named `{name}`");
                        if let Some(s) = suggest(name, sig.sigs.keys().map(|k| k.as_str())) {
                            m.push_str(&format!(" — did you mean `{s}`?"));
                        }
                        self.err("K0247", m, span);
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
                // A known cross-language alias (length/size/append/...) is a high-confidence intent, so
                // prefer it over a coincidental edit-distance neighbor (e.g. `size` is 2 edits from `sign`
                // but the user means `len`); fall back to edit-distance otherwise.
                let hint = common_method_alias(name)
                    .map(String::from)
                    .or_else(|| suggest(name, cands));
                let msg = match hint {
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
                    // Name the expected parameter TYPES, not just the count, so a wrong-arity call
                    // shows the signature -- e.g. `.center` takes 2 arguments (Int, Str) -- instead of
                    // leaving the user to guess which args and in what order (PR-it490).
                    let mut msg = format!("`.{name}` takes {}", plural(params.len(), "argument"));
                    if !params.is_empty() {
                        let sig = params
                            .iter()
                            .map(|p| self.uni.apply(p).to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        msg.push_str(&format!(" ({sig})"));
                    }
                    msg.push_str(&format!(", {} given", args.len()));
                    self.err("K0250", msg, span);
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
                            // Ctor patterns are positional, so when the pattern under-specifies
                            // (args.len() < fields.len()) the missing fields are exactly the
                            // trailing ones by position -- name them, mirroring K0243's
                            // missing-field hint for constructor calls (PR-it484).
                            let mut msg = format!("`{other}` has {}, pattern has {}", plural(field_tys.len(), "field"), args.len());
                            if args.len() < fields.len() {
                                let missing = fields[args.len()..]
                                    .iter()
                                    .map(|(n, _)| format!("`{n}`"))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                msg.push_str(&format!(" — missing {missing}"));
                            }
                            self.err("K0255", msg, pat.span);
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
        // not run even when its pattern matches.
        let patterns: Vec<&Pattern> =
            arms.iter().filter(|a| a.guard.is_none()).map(|a| &a.pattern).collect();
        if patterns.iter().any(|p| Self::pattern_is_catch_all(p)) {
            return;
        }
        match self.uni.apply(scrut) {
            Ty::Bool | Ty::Option(_) | Ty::Result(_, _) | Ty::Named(..) => {
                let missing = self.exhaustive_missing(scrut, &patterns);
                if !missing.is_empty() {
                    self.err(
                        "K0257",
                        format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                        span,
                    );
                    return;
                }
                // `exhaustive_missing` checks each matched constructor's
                // fields INDEPENDENTLY, which is a strictly WEAKER guarantee
                // than true joint coverage for a MULTI-field constructor:
                // e.g. `P(Circle(_), _) => .., P(Square(_), Circle(_)) => ..`
                // on `P(a: Shape, b: Shape)` looks fully covered field-by-
                // field (field 0 sees Circle+Square; field 1 sees a
                // catch-all) but is actually missing the SPECIFIC combination
                // `P(Square(_), Square(_))`. Run the full joint/decision-tree
                // check as a safety net whenever the cheaper per-field check
                // found nothing (PR-it570; the per-field check still owns the
                // common, precise single-field-per-constructor message).
                let rows: Vec<Vec<Slot>> = patterns.iter().map(|p| vec![Slot::Pat(p)]).collect();
                if !self.joint_exhaustive(&rows, std::slice::from_ref(scrut)) {
                    self.err(
                        "K0257",
                        "non-exhaustive `match`: the arms shown do not jointly cover every \
                         combination of the matched fields — add a catch-all arm (`_ => …`) \
                         or handle the missing combination explicitly"
                            .to_string(),
                        span,
                    );
                }
            }
            _ => {
                self.err(
                    "K0256",
                    "this `match` needs a catch-all arm (`_ => …`) — the scrutinee type has unbounded values",
                    span,
                );
            }
        }
    }

    /// True joint (multi-column) exhaustiveness: does `rows` (each a
    /// pattern-tuple for the remaining `tys` positions, contributed by an
    /// arm still relevant at this point) cover every possible combination
    /// of values across ALL of `tys` together? This is the proper
    /// decision-tree/specialization algorithm real exhaustiveness checkers
    /// use, as opposed to `exhaustive_missing`'s cheaper per-field-
    /// independent approximation (PR-it570).
    fn joint_exhaustive(&self, rows: &[Vec<Slot>], tys: &[Ty]) -> bool {
        // A row where EVERY remaining position is a catch-all trivially
        // covers ANY combination of values for `tys`, no matter how deep --
        // this is both a correctness shortcut (a bare `_`/bind genuinely
        // matches anything) and the TERMINATION guarantee for recursive ADTs
        // (e.g. `type Tree = Leaf | Node(l: Tree, r: Tree)`): without this
        // short-circuit, specializing an all-wildcard row by `Node` would
        // keep expanding into MORE wildcard `Tree` columns forever.
        if rows.iter().any(|r| r.iter().all(Slot::is_catch_all)) {
            return true;
        }
        let Some((t0, rest)) = tys.split_first() else {
            // no positions left to decide: covered iff some row reached here
            return !rows.is_empty();
        };
        match self.uni.apply(t0) {
            Ty::Bool => {
                for b in [true, false] {
                    let specialized = Self::specialize_bool(rows, b);
                    if !self.joint_exhaustive(&specialized, rest) {
                        return false;
                    }
                }
                true
            }
            ty @ (Ty::Option(_) | Ty::Result(_, _) | Ty::Named(..)) => {
                let variants = self.variant_field_tys(&ty);
                if variants.is_empty() {
                    // unknown/unresolved type: don't false-reject
                    return true;
                }
                for (vname, field_tys) in &variants {
                    let specialized = Self::specialize_ctor(rows, vname, field_tys.len());
                    let mut sub_tys = field_tys.clone();
                    sub_tys.extend(rest.iter().cloned());
                    if !self.joint_exhaustive(&specialized, &sub_tys) {
                        return false;
                    }
                }
                true
            }
            // unbounded scalar column (Int, Str, Float, ...): same leniency
            // as `exhaustive_missing` -- this position's OWN value-space
            // isn't required to be fully covered. But that must not
            // short-circuit the WHOLE check: every row still passes through
            // unconditionally (we're choosing not to discriminate on this
            // column, not declaring the remaining columns irrelevant), so
            // `rest` is still checked for joint coverage using each row's
            // own leftover positions.
            _ => {
                let passthrough: Vec<Vec<Slot>> = rows.iter().map(|r| r[1..].to_vec()).collect();
                self.joint_exhaustive(&passthrough, rest)
            }
        }
    }

    /// `(variant name, field types)` for every variant of `ty`, with type
    /// parameters substituted; empty for a type this pass can't resolve.
    fn variant_field_tys(&self, ty: &Ty) -> Vec<(String, Vec<Ty>)> {
        match ty {
            Ty::Option(inner) => vec![("Some".to_string(), vec![(**inner).clone()]), ("None".to_string(), vec![])],
            Ty::Result(ok, err) => {
                vec![("Ok".to_string(), vec![(**ok).clone()]), ("Err".to_string(), vec![(**err).clone()])]
            }
            Ty::Named(tn, targs) => match self.checked.types.get(tn).cloned() {
                Some(sig) => {
                    let m: HashMap<u32, Ty> = sig.qvars.iter().cloned().zip(targs.iter().cloned()).collect();
                    sig.variants
                        .iter()
                        .map(|v| {
                            (
                                v.name.clone(),
                                v.fields.iter().map(|(_, fty)| Self::subst_ty(fty, &m)).collect(),
                            )
                        })
                        .collect()
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Flatten every row's position-0 slot (unwrapping `At`, expanding `Or`
    /// into one row per alternative) before specializing against a
    /// constructor/literal — matches `pattern_is_catch_all`'s and
    /// `ctor_args_for`'s existing At/Or handling.
    fn flatten_rows<'a>(rows: &[Vec<Slot<'a>>]) -> Vec<Vec<Slot<'a>>> {
        fn expand<'a>(s: Slot<'a>, out: &mut Vec<Slot<'a>>) {
            match s {
                Slot::Wild => out.push(Slot::Wild),
                Slot::Pat(p) => match &p.kind {
                    PatternKind::At { inner, .. } => expand(Slot::Pat(inner), out),
                    PatternKind::Or(alts) => {
                        for a in alts {
                            expand(Slot::Pat(a), out);
                        }
                    }
                    _ => out.push(Slot::Pat(p)),
                },
            }
        }
        let mut out = Vec::new();
        for row in rows {
            let mut heads = Vec::new();
            expand(row[0], &mut heads);
            for h in heads {
                let mut new_row = vec![h];
                new_row.extend(row[1..].iter().copied());
                out.push(new_row);
            }
        }
        out
    }

    fn specialize_bool<'a>(rows: &[Vec<Slot<'a>>], b: bool) -> Vec<Vec<Slot<'a>>> {
        let mut out = Vec::new();
        for row in Self::flatten_rows(rows) {
            if row[0].is_catch_all() {
                out.push(row[1..].to_vec());
                continue;
            }
            if let Slot::Pat(p) = row[0] {
                if let PatternKind::Bool(rb) = &p.kind {
                    if *rb == b {
                        out.push(row[1..].to_vec());
                    }
                }
            }
        }
        out
    }

    fn specialize_ctor<'a>(rows: &[Vec<Slot<'a>>], variant_name: &str, arity: usize) -> Vec<Vec<Slot<'a>>> {
        let mut out = Vec::new();
        for row in Self::flatten_rows(rows) {
            if row[0].is_catch_all() {
                let mut new_row = vec![Slot::Wild; arity];
                new_row.extend(row[1..].iter().copied());
                out.push(new_row);
                continue;
            }
            if let Slot::Pat(p) = row[0] {
                if let PatternKind::Ctor { name, args } = &p.kind {
                    if name == variant_name {
                        let mut new_row: Vec<Slot> = args.iter().map(Slot::Pat).collect();
                        new_row.extend(row[1..].iter().copied());
                        out.push(new_row);
                    }
                }
            }
        }
        out
    }

    fn pattern_is_catch_all(p: &Pattern) -> bool {
        match &p.kind {
            PatternKind::Wildcard | PatternKind::Bind(_) => true,
            // `name @ inner` covers whatever `inner` covers (so `name @ _` is
            // a catch-all). An or-pattern arm is a catch-all the moment ANY
            // alternative is, since a value failing every other alternative
            // still matches that one.
            PatternKind::Or(alts) => alts.iter().any(Self::pattern_is_catch_all),
            PatternKind::At { inner, .. } => Self::pattern_is_catch_all(inner),
            _ => false,
        }
    }

    /// Recursively collect missing-case descriptions for `ty` given the
    /// patterns that reach this position from all (unguarded) enclosing
    /// arms. Bool/Option/Result/Named types are checked recursively into
    /// each variant's fields — e.g. `Some(Good(_))` alone does NOT cover
    /// `Some`, since `Some`'s payload (`R`) itself has an uncovered `Bad`
    /// case; this used to be missed entirely, since the old checker only
    /// asked "does some arm mention the OUTER constructor name," never
    /// recursing into `args` (PR-it568). Any other type (Int, Str, Float,
    /// BigInt, Rational, SizedInt, unresolved type variables, ...) is
    /// treated as already covered at THIS position — full exhaustiveness
    /// over an unbounded scalar FIELD (as opposed to the scrutinee itself,
    /// which the caller already rejects via K0256) is a separate, broader
    /// concern intentionally left out of this fix's scope.
    fn exhaustive_missing(&self, ty: &Ty, patterns: &[&Pattern]) -> Vec<String> {
        if patterns.iter().any(|p| Self::pattern_is_catch_all(p)) {
            return Vec::new();
        }
        match self.uni.apply(ty) {
            Ty::Bool => {
                let mut present: HashSet<bool> = HashSet::new();
                for p in patterns {
                    Self::collect_bools(p, &mut present);
                }
                let mut missing = Vec::new();
                if !present.contains(&true) {
                    missing.push("true".to_string());
                }
                if !present.contains(&false) {
                    missing.push("false".to_string());
                }
                missing
            }
            Ty::Option(inner) => self.exhaustive_missing_variants(
                &[("Some", vec![*inner]), ("None", vec![])],
                patterns,
            ),
            Ty::Result(ok, err) => self.exhaustive_missing_variants(
                &[("Ok", vec![*ok]), ("Err", vec![*err])],
                patterns,
            ),
            Ty::Named(tn, targs) => match self.checked.types.get(&tn).cloned() {
                Some(sig) => {
                    let m: HashMap<u32, Ty> =
                        sig.qvars.iter().cloned().zip(targs.iter().cloned()).collect();
                    let variants: Vec<(&str, Vec<Ty>)> = sig
                        .variants
                        .iter()
                        .map(|v| {
                            (
                                v.name.as_str(),
                                v.fields.iter().map(|(_, fty)| Self::subst_ty(fty, &m)).collect(),
                            )
                        })
                        .collect();
                    self.exhaustive_missing_variants(&variants, patterns)
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// For each `(variant_name, field_types)`, checks that some arm's
    /// top-level pattern targets it, and — if so — that the field patterns
    /// contributed by ALL arms targeting it jointly cover each field
    /// (recursively). A variant with zero matching arms is simply missing; a
    /// variant with matching arms whose fields aren't jointly exhaustive is
    /// reported as `Variant(<what's missing in each under-covered field>)`.
    fn exhaustive_missing_variants(&self, variants: &[(&str, Vec<Ty>)], patterns: &[&Pattern]) -> Vec<String> {
        let mut missing = Vec::new();
        for (vname, field_tys) in variants {
            let arg_tuples = Self::ctor_args_for(patterns, vname);
            if arg_tuples.is_empty() {
                missing.push((*vname).to_string());
                continue;
            }
            let mut field_missing = Vec::new();
            for (i, fty) in field_tys.iter().enumerate() {
                let field_pats: Vec<&Pattern> =
                    arg_tuples.iter().filter_map(|args| args.get(i)).collect();
                let m = self.exhaustive_missing(fty, &field_pats);
                if !m.is_empty() {
                    field_missing.push(m.join(" or "));
                }
            }
            if !field_missing.is_empty() {
                missing.push(format!("{vname}({})", field_missing.join(", ")));
            }
        }
        missing
    }

    fn collect_bools(p: &Pattern, out: &mut HashSet<bool>) {
        match &p.kind {
            PatternKind::Bool(b) => {
                out.insert(*b);
            }
            PatternKind::Or(alts) => {
                for a in alts {
                    Self::collect_bools(a, out);
                }
            }
            PatternKind::At { inner, .. } => Self::collect_bools(inner, out),
            _ => {}
        }
    }

    /// Collect, from `patterns` (expanding `Or` and unwrapping `At` as
    /// needed), the field-pattern tuple of every `Ctor` arm matching
    /// `variant_name` — one entry per arm that targets this variant.
    fn ctor_args_for<'a>(patterns: &[&'a Pattern], variant_name: &str) -> Vec<&'a [Pattern]> {
        fn walk<'a>(p: &'a Pattern, variant_name: &str, out: &mut Vec<&'a [Pattern]>) {
            match &p.kind {
                PatternKind::Ctor { name, args } if name == variant_name => out.push(args.as_slice()),
                PatternKind::Or(alts) => {
                    for a in alts {
                        walk(a, variant_name, out);
                    }
                }
                PatternKind::At { inner, .. } => walk(inner, variant_name, out),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for p in patterns {
            walk(p, variant_name, &mut out);
        }
        out
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
    fn k0235_list_plus_names_flatten_fix() {
        // Error-msg round 19 (PR-it283): `a + b` on two lists is a common mistake -- users expect `+`
        // to concatenate. The K0235 diagnostic now names the fix (`[a, b].flatten()`) instead of leaving
        // "arithmetic needs Int or Float operands, found List[Int]" bare. Verify the code + the hint.
        let src = "fun probe() -> List[Int] { let a = [1, 2]\n    let b = [3, 4]\n    a + b }\n";
        let errs = errors(src);
        let e = errs.iter().find(|d| d.code == "K0235").expect("list + must be K0235");
        assert!(
            e.message.contains("list concatenation") && e.message.contains("[a, b].flatten()"),
            "K0235 for list `+` must name the flatten fix, got: {}",
            e.message
        );
        // The named-type overload hint must still fire (not clobbered by the list branch).
        let overload = "type P = { x: Int }\nfun probe() -> P { P(x: 1) + P(x: 2) }\n";
        let oe = errors(overload);
        let od = oe.iter().find(|d| d.code == "K0235").expect("record + must be K0235");
        assert!(od.message.contains("to overload"), "record `+` must still suggest overload, got: {}", od.message);
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
        // ...and it names the fix: convert the Option to a Result with `.ok_or(err)?` (PR-it252).
        assert!(
            errors(mismatch1).iter().any(|d| d.code == "K0238" && d.message.contains("`.ok_or(err)?`")),
            "Option ? in a Result fun must hint .ok_or: {:?}",
            errors(mismatch1).iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        // `?` on a Result in an Option-returning function is a K0238 error.
        let mismatch2 = "fun half(n: Int) -> Result[Int, Str] { if n % 2 == 0 { Ok(n / 2) } else { Err(\"odd\") } }\n\
                         fun bad(n: Int) -> Option[Int] { let v = half(n)?\n    Some(v) }\n";
        assert!(errors(mismatch2).iter().any(|d| d.code == "K0238"), "Result ? in an Option fun must be K0238");
        // ...and it names the fix: convert the Result to an Option with `.ok()?` (PR-it252).
        assert!(
            errors(mismatch2).iter().any(|d| d.code == "K0238" && d.message.contains("`.ok()?`")),
            "Result ? in an Option fun must hint .ok: {:?}",
            errors(mismatch2).iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn try_operator_on_result_checks_the_propagated_error_type() {
        // SOUNDNESS FIX (PR-it494): `?` on a Result propagates the operand's Err type into the
        // enclosing function's Err type. That unification result was previously DISCARDED
        // (`let _ = self.uni.unify(&err, &ret_err);`), so a mismatched Err type silently
        // type-checked -- `inner() -> Result[Int, Int]` propagated through `?` into a function
        // declared `-> Result[Int, Str]` produced NO diagnostic, even though a direct
        // `Err(42)` return in the same function is correctly rejected as K0200. Found via
        // bug-hunt probing: a direct-return mismatch is caught, but the identical mismatch
        // propagated through `?` was not -- an asymmetry between two paths to the same invalid
        // program. Now K0200 fires at the `?` site, naming the expected/found Err types.
        let mismatched = "fun inner() -> Result[Int, Int] { Err(42) }\n\
                           fun outer() -> Result[Int, Str] {\n    let x = inner()?\n    Ok(x)\n}\n";
        let ds = errors(mismatched);
        assert!(
            ds.iter().any(|d| d.code == "K0200" && d.message.contains("expected Str") && d.message.contains("found Int")),
            "`?` must catch a propagated Err-type mismatch: {ds:?}"
        );
        // A matching Err type still type-checks (no regression).
        let matching = "fun inner() -> Result[Int, Str] { Err(\"bad\") }\n\
                         fun outer() -> Result[Int, Str] {\n    let x = inner()?\n    Ok(x)\n}\n\
                         fun main() uses io { print(\"{outer()}\") }\n";
        assert!(errors(matching).is_empty(), "matching Err type via `?` must still compile: {:?}", errors(matching));
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
    fn k0263_span_points_at_the_offending_method_not_the_whole_component() {
        // A REAL BUG found+fixed (PR-it585): K0263 (a fulfilling component exposes a
        // method with the WRONG signature) used the whole component's `c.span` --
        // underlining the `component Foo fulfills Bar {` header line -- instead of the
        // specific `expose fun` declaration actually at fault, even though that exact
        // tighter span was computed two lines later in the SAME function and already
        // used correctly by the sibling K0264 effect-budget check right next to it.
        let src = "contract Store {\n    intent \"s\"\n    expose fun get(k: Str) -> Int\n}\n\
                   component Bad fulfills Store {\n    intent \"b\"\n    expose fun get(k: Str) -> Str {\n        \"wrong\"\n    }\n}\n";
        let d = errors(src);
        let err = d.iter().find(|d| d.code == "K0263").expect("K0263 must fire");
        let text = &src[err.span.start as usize..err.span.end as usize];
        assert!(
            text.contains("fun get") && !text.contains("component Bad"),
            "span must cover the offending `get` declaration, not the component header: {text:?}"
        );
    }

    #[test]
    fn list_builtin_type_errors_point_at_the_receiver_not_the_whole_call() {
        // A REAL BUG found+fixed (PR-it585): `sum`/`product`/`join`/`sort`/`min`/`max`'s
        // wrong-element-type diagnostics (K0245/K0246/K0234) used the WHOLE method-call
        // span (receiver through the closing `)`) instead of just the receiver list --
        // needlessly underlining `.sum()`'s own call syntax alongside the actual culprit,
        // even though `recv.span` (the receiver expression alone) was already a live,
        // in-scope parameter of the enclosing function.
        let span_text = |src: &str, code: &str| {
            let d = errors(src);
            let err = d.iter().find(|d| d.code == code).unwrap_or_else(|| panic!("{code} must fire: {d:?}"));
            src[err.span.start as usize..err.span.end as usize].to_string()
        };
        let sum = "fun main() uses io {\n    let xs = [\"alpha\", \"beta\", \"gamma\"].sum()\n}\n";
        let t = span_text(sum, "K0245");
        assert!(t.contains("[\"alpha\"") && !t.contains(".sum()"), "sum span: {t:?}");

        let join = "fun main() uses io {\n    let xs = [1, 2, 3].join(\",\")\n}\n";
        let t = span_text(join, "K0246");
        assert!(t.contains("[1, 2, 3]") && !t.contains(".join"), "join span: {t:?}");

        let sort = "fun main() uses io {\n    let xs = [true, false].sort()\n}\n";
        let t = span_text(sort, "K0234");
        assert!(t.contains("[true, false]") && !t.contains(".sort()"), "sort span: {t:?}");
    }

    #[test]
    fn k0261_unknown_contract_suggests_closest_name() {
        // Error-msg round 37 (PR-it512): a typo'd `fulfills` contract name in `component
        // MemStore fulfills Stor { ... }` was flat "fulfills unknown contract `Stor`" -- named
        // the miss, not the fix. Extends did-you-mean already on free-fns/methods/fields/types/
        // ctors/child-components (K0249/K0100/K0206/K0247/K0254/K0208) to K0261.
        let typo = errors("contract Store {\n    intent \"s\"\n    expose fun get(k: Str) -> Int\n}\n\
                           component MemStore fulfills Stor {\n    intent \"m\"\n    expose fun get(k: Str) -> Int {\n        0\n    }\n}\n");
        assert!(
            typo.iter().any(|d| d.code == "K0261" && d.message.contains("unknown contract `Stor`") && d.message.contains("did you mean `Store`?")),
            "typo'd contract name should suggest the close match: {typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean).
        let none = errors("component MemStore fulfills Zqxwbly {\n    intent \"m\"\n}\n");
        assert!(
            none.iter().any(|d| d.code == "K0261" && !d.message.contains("did you mean")),
            "unrelated name should stay bare: {none:?}"
        );
    }

    #[test]
    fn k0262_missing_contract_method_suggests_closest_exposed_name() {
        // A REAL BUG found+fixed (PR-it581), a sibling to K0261's did-you-mean (right
        // above): K0261 suggests a close-by CONTRACT name when `fulfills` names an unknown
        // contract, but K0262 (a component fulfilling a KNOWN contract while missing one
        // of its required methods) never checked the component's OWN exposed methods for a
        // close match -- `expose fun gett(...)` for a contract requiring `get` named the
        // miss ("does not expose `get`") but never pointed at the typo actually present,
        // unlike the reverse case (`recv.gett()` on a call site, which `find_method`
        // already suggests correctly).
        let typo = errors(
            "contract KeyStore {\n    intent \"kv\"\n    expose fun put(key: Str, value: Str) -> Bool\n    \
             expose fun get(key: Str) -> Option[Str]\n}\n\
             component MemoryStore fulfills KeyStore {\n    intent \"m\"\n    state entries: List[Str] = []\n    \
             expose fun put(key: Str, value: Str) -> Bool { true }\n    \
             expose fun gett(key: Str) -> Option[Str] { None }\n}\n",
        );
        assert!(
            typo.iter().any(|d| d.code == "K0262"
                && d.message.contains("does not expose `get`")
                && d.message.contains("did you mean `gett`?")),
            "typo'd exposed method should suggest the close match: {typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean); this is the
        // SAME repro `contract_conformance_is_structurally_enforced` already locks for
        // K0262's bare existence, re-asserted here specifically for the ABSENCE of a
        // bogus suggestion.
        let none = errors(
            "contract KeyStore {\n    intent \"kv\"\n    expose fun get(key: Str) -> Option[Str]\n}\n\
             component MemoryStore fulfills KeyStore {\n    intent \"m\"\n    state entries: List[Str] = []\n    \
             expose fun size() -> Int { 0 }\n}\n",
        );
        assert!(
            none.iter().any(|d| d.code == "K0262" && !d.message.contains("did you mean")),
            "unrelated exposed name should stay bare: {none:?}"
        );
    }

    #[test]
    fn wiring_port_and_supervise_typos_suggest_closest_name() {
        // A REAL BUG found+fixed (PR-it582), SIX more instances of the same sibling-
        // consistency gap as K0261/K0262 (it512/it581): a systematic sweep of every
        // "unknown X"-shaped diagnostic in check.rs found K0211/K0214/K0215/K0217/K0226/
        // K0265 all had an obvious in-scope candidate pool (in_ports/out_ports/props/
        // child names) sitting right next to the `self.err(...)` call, but never called
        // `suggest(...)` -- each named the miss with zero pointer to the close-by typo.
        let has_suggestion = |src: &str, code: &str, wanted: &str| {
            errors(src).iter().any(|d| d.code == code && d.message.contains(&format!("did you mean `{wanted}`?")))
        };

        // K0211: `on <port>` handler trigger.
        assert!(has_suggestion(
            "component Widget {\n    intent \"x\"\n    in trigger: Int\n    on triger(n) { }\n}\n",
            "K0211", "trigger"
        ));
        // K0214: `wire a.port -> b.port` (port typo, child name already resolved).
        assert!(has_suggestion(
            "component Src {\n    intent \"s\"\n    out value: Int\n}\n\
             component Consumer {\n    intent \"c\"\n    in value: Int\n    on value(n) { }\n}\n\
             component Top {\n    intent \"t\"\n    let producer = Src()\n    let consumer = Consumer()\n    \
             wire producer.valu -> consumer.value\n}\n",
            "K0214", "value"
        ));
        // K0215: a NAMED prop typo in component construction.
        assert!(has_suggestion(
            "component Widget {\n    intent \"x\"\n    prop label: Str\n}\n\
             fun main() uses io {\n    let w = Widget(lable: \"hi\")\n}\n",
            "K0215", "label"
        ));
        // K0217: `example { send <port>(...) }`.
        assert!(has_suggestion(
            "component Widget {\n    intent \"x\"\n    in trigger: Int\n    on trigger(n) { }\n    \
             example {\n        send triger(1)\n    }\n}\n",
            "K0217", "trigger"
        ));
        // K0226: `emit <port>(...)`.
        assert!(has_suggestion(
            "component Widget {\n    intent \"x\"\n    out result: Int\n    in go: Int\n    on go(n) {\n        \
             emit resutl(n)\n    }\n}\n",
            "K0226", "result"
        ));
        // K0265: `supervise <child> restart on_failure`.
        assert!(has_suggestion(
            "component Divider {\n    intent \"d\"\n}\n\
             component Top {\n    intent \"t\"\n    let divider = Divider()\n    supervise dividr restart on_failure\n}\n",
            "K0265", "divider"
        ));

        // Nothing close for either -> stays bare, no false-positive did-you-mean.
        let none1 = errors("component Widget {\n    intent \"x\"\n    in trigger: Int\n    on zzzzzzz(n) { }\n}\n");
        assert!(none1.iter().any(|d| d.code == "K0211" && !d.message.contains("did you mean")));
        let none2 = errors(
            "component Divider {\n    intent \"d\"\n}\n\
             component Top {\n    intent \"t\"\n    let divider = Divider()\n    supervise zzzzzzz restart on_failure\n}\n",
        );
        assert!(none2.iter().any(|d| d.code == "K0265" && !d.message.contains("did you mean")));
    }

    #[test]
    fn k0241_names_the_argument_and_the_positional_fix() {
        // Error-msg round 38 (PR-it520): named arguments through an INDIRECT function value
        // (e.g. `let f = add; f(a: 1, b: 2)` -- the checker only has `f`'s structural type,
        // Fun(Int,Int)->Int, not the original `add` declaration's parameter NAMES, so named-arg
        // resolution is impossible in general) reported a bare "named arguments are only
        // allowed for constructors and props" -- didn't say WHICH argument, or how to fix it.
        // Direct calls to a named function/constructor (`add(a: 1, b: 2)`) are unaffected --
        // `callargs::resolve_call_args` already resolves those into positional form before the
        // checker even sees them, so K0241 only fires on this indirect-call path.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() {\n    let f = add\n    print(f(a: 1, b: 2))\n}\n";
        let e = errors(src);
        assert!(
            e.iter().any(|d| d.code == "K0241" && d.message.contains("`a:` is a named argument") && d.message.contains("call positionally instead: `1`")),
            "K0241 should name the argument and show the positional fix: {e:?}"
        );
        assert!(
            e.iter().any(|d| d.code == "K0241" && d.message.contains("`b:` is a named argument") && d.message.contains("call positionally instead: `2`")),
            "K0241 should report EACH named argument separately: {e:?}"
        );
        // A DIRECT named call to `add` itself still type-checks cleanly (`kupl check`'s real
        // pipeline runs `callargs::resolve_call_args` BEFORE the checker, rewriting a direct
        // named call into positional form -- so the checker itself never sees a named arg
        // here at all, unlike `errors()`'s bare parse+check harness above which skips that
        // pass and would otherwise misleadingly show K0241 even for the direct case).
        assert!(
            crate::run::compile("fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() {\n    print(add(a: 1, b: 2))\n}\n").is_ok(),
            "direct named call must compile cleanly through the real pipeline (resolve_call_args rewrites it to positional form)"
        );
    }

    #[test]
    fn k0266_names_the_trigger_keyword_and_the_duration() {
        // Error-msg round 39 (PR-it521): `on every 0ms` / `on after 0s` was flat "timer
        // duration must be positive" -- didn't say WHICH trigger keyword or WHAT duration was
        // rejected, and didn't explain WHY. A NEGATIVE duration can never actually reach this
        // check: `parse_duration` only accepts a bare Int token as the FIRST token, so `on
        // every -5ms` fails to parse as a duration at all (K0120) before this check ever runs
        // -- meaning K0266 is, in practice, ALWAYS about a ZERO duration specifically.
        let every0 = errors("component T {\n    intent \"t\"\n    on every 0ms {\n        print(\"x\")\n    }\n}\n");
        assert!(
            every0.iter().any(|d| d.code == "K0266" && d.message.contains("`on every 0ms`") && d.message.contains("infinite loop")),
            "K0266 should name the `every` keyword, the 0ms duration, and explain why: {every0:?}"
        );
        let after0 = errors("component T {\n    intent \"t\"\n    on after 0s {\n        print(\"x\")\n    }\n}\n");
        assert!(
            after0.iter().any(|d| d.code == "K0266" && d.message.contains("`on after 0ms`")),
            "K0266 should name the `after` keyword (0s converts to 0ms internally): {after0:?}"
        );
        // Positive durations for both keywords still type-check cleanly (no behavior change).
        assert!(errors("component T {\n    intent \"t\"\n    on every 5ms {\n        print(\"x\")\n    }\n    on after 1s {\n        print(\"y\")\n    }\n}\n").is_empty());
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
    fn did_you_mean_builtin_free_functions() {
        // K0240 now suggests built-in free functions (print, json_parse, env_var, ...), not just
        // user functions and constructors — a typo'd `prnt`/`json_pares` names the real one (PR-it249).
        for (typo, want) in [("prnt", "print"), ("json_pares", "json_parse"), ("env_vr", "env_var"), ("tensr", "tensor")] {
            let e = errors(&format!("fun main() uses io {{ let x = {typo}(5)\n    print(x) }}\n"));
            assert!(
                e.iter().any(|d| d.code == "K0240" && d.message.contains(&format!("did you mean `{want}`?"))),
                "{typo}: {:?}",
                e.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
        }
        // A name far from every builtin still gets no spurious hint.
        let none = errors("fun main() uses io { print(zzqqxx(5)) }\n");
        assert!(none.iter().any(|d| d.code == "K0240"));
        assert!(!none.iter().any(|d| d.message.contains("did you mean")));
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
    fn arity_diagnostics_pluralize_correctly() {
        // Arg/field count diagnostics use proper pluralization ("1 argument" / "2 arguments")
        // instead of a literal "argument(s)" (PR-it172).
        let many = errors("fun add(a: Int, b: Int) -> Int { a + b }\nfun main() uses io { print(add(1, 2, 3)) }\n");
        assert!(
            many.iter().any(|d| d.code == "K0242" && d.message.contains("takes 2 arguments, 3 given")),
            "{many:?}"
        );
        let one = errors("fun neg(a: Int) -> Int { 0 - a }\nfun main() uses io { print(neg(1, 2)) }\n");
        assert!(
            one.iter().any(|d| d.code == "K0242" && d.message.contains("takes 1 argument, 2 given")),
            "{one:?}"
        );
        let ctor = errors("type P = P(x: Int, y: Int)\nfun main() uses io { let p = P(x: 1)\n print(p.x) }\n");
        assert!(
            ctor.iter().any(|d| d.code == "K0243" && d.message.contains("has 2 fields, 1 argument given")),
            "{ctor:?}"
        );
        // Exhaustiveness already names the missing variants — certify it stays that way.
        let exh = errors("type T = A | B | C\nfun f(t: T) -> Int { match t { A => 1 } }\nfun main() {}\n");
        assert!(
            exh.iter().any(|d| d.code == "K0257" && d.message.contains("missing B, C")),
            "{exh:?}"
        );
    }

    #[test]
    fn exhaustiveness_checker_recurses_into_nested_ctor_patterns() {
        // `check_exhaustive` used to collect only the OUTER constructor name
        // mentioned by each arm (`PatternKind::Ctor { name, .. } =>
        // covered.insert(name)`) and never looked at `args` at all -- so
        // `Some(Good(a))` alone was treated as fully covering `Some`,
        // regardless of what (if anything) the `R` payload's OTHER variant
        // (`Bad`) needed. `kupl check` reported "ok" and every engine then
        // panicked "no match arm matched" at runtime on the uncovered case
        // (PR-it568) -- a genuine soundness gap: valid-looking, "checked"
        // KUPL crashed in production. Fixed by recursively checking each
        // matched variant's field types too, not just the top-level tag.
        let nested = errors(
            "type R = Good(v: Int) | Bad(msg: Str)\n\
             fun f(x: Option[R]) -> Int {\n    \
             match x {\n        Some(Good(a)) => a\n        None => 0\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(
            nested.iter().any(|d| d.code == "K0257" && d.message.contains("Some(Bad)")),
            "{nested:?}"
        );
        // A genuinely exhaustive nested match (every payload variant covered
        // too) must NOT be flagged -- no new false rejection of valid code.
        let ok = errors(
            "type R = Good(v: Int) | Bad(msg: Str)\n\
             fun f(x: Option[R]) -> Int {\n    \
             match x {\n        Some(Good(a)) => a\n        Some(Bad(_)) => 0\n        None => -1\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        // The same nested wildcard (`Some(_)`) still trivially covers `Some`
        // regardless of the payload's own variants -- a nested wildcard/bind
        // is a catch-all at that position, same as before this fix.
        let wildcard_payload = errors(
            "type R = Good(v: Int) | Bad(msg: Str)\n\
             fun f(x: Option[R]) -> Int {\n    \
             match x {\n        Some(_) => 1\n        None => 0\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(wildcard_payload.is_empty(), "{wildcard_payload:?}");
    }

    #[test]
    fn exhaustiveness_checker_catches_multi_field_cross_product_gaps() {
        // it568's fix checks each matched constructor's fields INDEPENDENTLY,
        // which under-counts a MULTI-field constructor: `P(Circle(_), _) =>
        // .., P(Square(_), Circle(_)) => ..` on `P(a: Shape, b: Shape)` looks
        // fully covered field-by-field (field 0 sees Circle+Square; field 1
        // sees a catch-all) but is actually missing the SPECIFIC combination
        // `P(Square(_), Square(_))` -- a real, previously-documented,
        // deliberately-scoped-out gap. Fixed with a proper joint/decision-
        // tree exhaustiveness check (`joint_exhaustive`) that specializes
        // POOLED row-tuples by each field's own constructors and recurses,
        // run as a safety net whenever the cheaper per-field check finds
        // nothing (PR-it570).
        let missing = errors(
            "type Shape = Circle(r: Int) | Square(s: Int)\n\
             type Pair = P(a: Shape, b: Shape)\n\
             fun f(x: Pair) -> Int {\n    \
             match x {\n        P(Circle(r), _) => r\n        P(Square(s), Circle(r2)) => s + r2\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(missing.iter().any(|d| d.code == "K0257"), "{missing:?}");
        // A genuinely exhaustive multi-field match (every combination
        // covered) must NOT be flagged -- no new false rejection.
        let ok = errors(
            "type Shape = Circle(r: Int) | Square(s: Int)\n\
             type Pair = P(a: Shape, b: Shape)\n\
             fun f(x: Pair) -> Int {\n    \
             match x {\n        \
             P(Circle(r), Circle(r2)) => r + r2\n        \
             P(Circle(r), Square(s)) => r + s\n        \
             P(Square(s), Circle(r2)) => s + r2\n        \
             P(Square(s), Square(s2)) => s + s2\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn exhaustiveness_checker_terminates_on_recursive_adt_matches() {
        // The joint/decision-tree check specializes a matched constructor's
        // OWN field types too, which for a RECURSIVE type (`Tree` has fields
        // of type `Tree` itself) could recurse forever without a proper
        // termination rule: a row that's already a bare wildcard at every
        // remaining position trivially covers ANY value (including further
        // recursive structure) and must short-circuit WITHOUT expanding into
        // more wildcard sub-columns (PR-it570). Both a genuinely exhaustive
        // and a genuinely non-exhaustive recursive match must resolve
        // promptly (this test itself would hang the whole suite otherwise).
        let exhaustive = errors(
            "type Tree = Leaf | Node(l: Tree, r: Tree)\n\
             fun sum(t: Tree) -> Int {\n    \
             match t {\n        Leaf => 0\n        Node(l, r) => sum(l) + sum(r)\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(exhaustive.is_empty(), "{exhaustive:?}");
        let non_exhaustive = errors(
            "type Tree = Leaf | Node(l: Tree, r: Tree)\n\
             fun sum(t: Tree) -> Int {\n    \
             match t {\n        Leaf => 0\n        Node(Leaf, r) => sum(r)\n    }\n}\n\
             fun main() {}\n",
        );
        assert!(non_exhaustive.iter().any(|d| d.code == "K0257"), "{non_exhaustive:?}");
    }

    #[test]
    fn generic_arity_mismatch_shows_only_the_clear_error() {
        // A wrong type-argument count now yields ONLY the clear K0206 "takes N type arguments",
        // not also a confusing secondary K0200 "expected Box[Int, Str], found Box[Int]" — the
        // malformed annotation resolves to a fresh var so it doesn't cascade (PR-it221).
        let too_many = errors("type Box[T] = Box(v: T)\nfun main() { let b: Box[Int, Str] = Box(v: 5)\n    let _ = b }\n");
        assert!(too_many.iter().any(|d| d.code == "K0206" && d.message.contains("takes 1 type argument, 2 given")), "K0206: {too_many:?}");
        assert!(too_many.iter().all(|d| d.code != "K0200"), "no cascading K0200: {too_many:?}");
        let too_few = errors("type Pair[A, B] = Pair(a: A, b: B)\nfun main() { let p: Pair[Int] = Pair(a: 1, b: 2)\n    let _ = p }\n");
        assert!(too_few.iter().any(|d| d.code == "K0206" && d.message.contains("takes 2 type arguments, 1 given")), "K0206: {too_few:?}");
        assert!(too_few.iter().all(|d| d.code != "K0200"), "no cascading K0200: {too_few:?}");
        // Correct type-argument counts still type-check.
        assert!(errors("type Box[T] = Box(v: T)\ntype Pair[A, B] = Pair(a: A, b: B)\nfun main() { let _: Box[Int] = Box(v: 5)\n    let _: Pair[Int, Str] = Pair(a: 1, b: \"x\") }\n").is_empty());
        // An unknown generic type close to a known one now suggests it (PR-it480): a user type...
        let ut = errors("type Pair[A, B] = Pair(a: A, b: B)\nfun f(x: Pare[Int, Str]) -> Int { 0 }\nfun main() { }\n");
        assert!(
            ut.iter().any(|d| d.code == "K0206" && d.message.contains("unknown generic type `Pare`") && d.message.contains("did you mean `Pair`?")),
            "unknown user generic suggests the type: {ut:?}"
        );
        // ...and a builtin generic (Option/List/...).
        let bt = errors("fun f(x: Opton[Int]) -> Int { 0 }\nfun main() { }\n");
        assert!(bt.iter().any(|d| d.code == "K0206" && d.message.contains("did you mean `Option`?")), "builtin generic suggestion: {bt:?}");
        // Nothing close -> no suggestion, just the bare K0206.
        let none = errors("fun f(x: Zqxw[Int]) -> Int { 0 }\nfun main() { }\n");
        assert!(none.iter().any(|d| d.code == "K0206" && !d.message.contains("did you mean")), "no bogus suggestion: {none:?}");
    }

    #[test]
    fn immutable_assign_message_fits_params_and_lets() {
        // K0221 fires for both `let` bindings and function parameters (both immutable by default).
        // The old text wrongly claimed a parameter was "declared with `let`"; the new text just
        // gives the fix and is accurate for either (PR-it220).
        let fix = "use `var` for a reassignable local";
        let on_param = errors("fun f(x: Int) -> Int { x = 5\n    x }\n");
        assert!(on_param.iter().any(|d| d.code == "K0221" && d.message.contains(fix)), "param: {on_param:?}");
        assert!(on_param.iter().all(|d| !d.message.contains("declared with `let`")), "param wrongly says let: {on_param:?}");
        let on_let = errors("fun main() { let x = 5\n    x = 6 }\n");
        assert!(on_let.iter().any(|d| d.code == "K0221" && d.message.contains(fix)), "let: {on_let:?}");
        // Valid `var` reassignment (including `+=`) still type-checks.
        assert!(errors("fun main() { var x = 5\n    x = 6\n    x += 10 }\n").is_empty());
    }

    #[test]
    fn duplicate_component_prop_is_rejected_at_check_time() {
        // Sibling of the record-field hole (PR-it213/214): a component prop supplied twice used to
        // be silently accepted when all required props were present. Now rejected (PR-it215).
        let comp = "component Widget {\n    intent \"t\"\n    prop w: Int\n    prop h: Int\n    in tick: Event\n    out area: Int\n    state a: Int = 0\n    on tick { a = w * h\n        emit area(a) }\n}\n";
        let dup = errors(&format!("{comp}fun main() {{ let _ = Widget(w: 5, h: 6, w: 7) }}\n"));
        assert!(dup.iter().any(|d| d.code == "K0215" && d.message.contains("duplicate prop `w`")), "dup: {dup:?}");
        // positional colliding with a named prop on the same slot is also a duplicate.
        let mix = errors(&format!("{comp}fun main() {{ let _ = Widget(5, w: 6) }}\n"));
        assert!(mix.iter().any(|d| d.code == "K0215" && d.message.contains("duplicate prop `w`")), "mix: {mix:?}");
        // Valid constructions — named, positional, and mixed on distinct props — still type-check.
        assert!(errors(&format!("{comp}fun main() {{ let _ = Widget(w: 5, h: 6)\n    let _ = Widget(5, 6)\n    let _ = Widget(5, h: 6) }}\n")).is_empty());
    }

    #[test]
    fn duplicate_record_field_is_rejected_at_check_time() {
        // A repeated named field used to slip past the checker (arg count matched) and then
        // DIVERGE at runtime: interp silently left the missing field Unit while KVM crashed. Now
        // it's rejected at check time — duplicate `x` AND the masked missing `y` (PR-it213).
        let e = errors("type P = { x: Int, y: Int }\nfun main() { let p = P(x: 1, x: 2)\n    let _ = p.x }\n");
        assert!(e.iter().any(|d| d.code == "K0244" && d.message.contains("duplicate field `x`")), "dup: {e:?}");
        assert!(e.iter().any(|d| d.code == "K0243" && d.message.contains("missing field `y`")), "missing: {e:?}");
        // A duplicate with a surplus arg still flags the duplicate.
        let e3 = errors("type P = { x: Int, y: Int }\nfun main() { let _ = P(x: 1, x: 2, y: 3) }\n");
        assert!(e3.iter().any(|d| d.code == "K0244" && d.message.contains("duplicate field `x`")), "dup3: {e3:?}");
        // A positional argument colliding with a named one on the SAME field is also a duplicate
        // that used to slip through and diverge (interp printed a value, KVM crashed) (PR-it214).
        let em = errors("type P = { x: Int, y: Int }\nfun main() { let _ = P(1, x: 2) }\n");
        assert!(em.iter().any(|d| d.code == "K0244" && d.message.contains("duplicate field `x`")), "mixed dup: {em:?}");
        assert!(em.iter().any(|d| d.code == "K0243" && d.message.contains("missing field `y`")), "mixed missing: {em:?}");
        // Valid constructions — in order, out of order, a generic ctor, and a legitimate mixed
        // positional+named form filling DISTINCT fields — all still type-check.
        assert!(errors("type P = { x: Int, y: Int }\nfun main() { let _ = P(x: 1, y: 2)\n    let _ = P(y: 20, x: 10) }\n").is_empty());
        assert!(errors("type Box[T] = Box(v: T)\nfun main() { let _ = Box(v: 5) }\n").is_empty());
        assert!(errors("type T = { a: Int, b: Int, c: Int }\nfun main() { let _ = T(1, 2, c: 3)\n    let _ = T(1, b: 2, c: 3) }\n").is_empty());
        // Too FEW all-named args now names the missing field(s) in the arity K0243 (PR-it484).
        let mf = errors("type T = { a: Int, b: Int, c: Int }\nfun main() { let _ = T(a: 1, c: 3) }\n");
        assert!(
            mf.iter().any(|d| d.code == "K0243" && d.message.contains("3 fields, 2 arguments given") && d.message.contains("missing `b`")),
            "arity K0243 should name the missing field: {mf:?}"
        );
        // A POSITIONAL too-few call keeps the bare count (no reliable field->slot naming).
        let pf = errors("type P = { x: Int, y: Int }\nfun main() { let _ = P(1) }\n");
        assert!(pf.iter().any(|d| d.code == "K0243" && d.message.contains("2 fields, 1 argument given") && !d.message.contains("missing")), "positional keeps bare count: {pf:?}");
    }

    #[test]
    fn k0255_ctor_pattern_arity_names_the_missing_fields() {
        // Error-msg round 34 (PR-it503): a ctor PATTERN with too few sub-patterns (e.g.
        // `match r { R(x) => x }` when R has 3 fields) was bare "`R` has 3 fields, pattern
        // has 1" -- didn't say WHICH fields the pattern left unmatched. Ctor patterns are
        // strictly positional, so an under-specified pattern's missing fields are exactly
        // the trailing ones by position -- name them, mirroring K0243's missing-field hint
        // for constructor CALLS (PR-it484), now extended to constructor PATTERNS.
        let too_few = errors(
            "type Rec = R(a: Int, b: Str, c: Bool)\nfun probe() -> Int { let r = R(a: 1, b: \"x\", c: true)\n    match r { R(x) => x } }\n",
        );
        assert!(
            too_few.iter().any(|d| d.code == "K0255"
                && d.message.contains("3 fields, pattern has 1")
                && d.message.contains("missing `b`, `c`")),
            "under-specified ctor pattern should name the missing fields: {too_few:?}"
        );
        // Too MANY sub-patterns keeps the bare count -- there's no field beyond the last one
        // to name, so a "missing" hint would be meaningless here.
        let too_many = errors(
            "type Rec = R(a: Int, b: Str)\nfun probe() -> Int { let r = R(a: 1, b: \"x\")\n    match r { R(x, y, z) => x } }\n",
        );
        assert!(
            too_many.iter().any(|d| d.code == "K0255" && d.message.contains("2 fields, pattern has 3") && !d.message.contains("missing")),
            "over-specified ctor pattern keeps bare count: {too_many:?}"
        );
        // Correct arity still type-checks cleanly.
        assert!(errors(
            "type Rec = R(a: Int, b: Str)\nfun probe() -> Int { let r = R(a: 1, b: \"x\")\n    match r { R(x, _) => x } }\n"
        )
        .is_empty());
    }

    #[test]
    fn k0229_names_the_actual_keyword() {
        // Error-msg round 35 (PR-it506): `break`/`continue` outside a loop used to report the
        // AMBIGUOUS bare "`break`/`continue` outside of a loop" for BOTH keywords -- even though
        // the checker matched `Stmt::Break` and `Stmt::Continue` as separate AST nodes and knew
        // exactly which one the user wrote. Split the match arms so K0229 names the actual
        // keyword: "`break` outside of a loop" / "`continue` outside of a loop".
        let b = errors("fun probe() -> Int { break\n    5 }\n");
        assert!(
            b.iter().any(|d| d.code == "K0229" && d.message == "`break` outside of a loop"),
            "bare `break` should name itself, not `break`/`continue`: {b:?}"
        );
        let c = errors("fun probe() -> Int { continue\n    5 }\n");
        assert!(
            c.iter().any(|d| d.code == "K0229" && d.message == "`continue` outside of a loop"),
            "bare `continue` should name itself, not `break`/`continue`: {c:?}"
        );
        // `break`/`continue` INSIDE a loop still type-check cleanly (no behavior change).
        assert!(errors("fun probe() -> Int { while false { break }\n    5 }\n").is_empty());
        assert!(errors("fun probe() -> Int { while false { continue }\n    5 }\n").is_empty());
    }

    #[test]
    fn k0208_unknown_child_component_suggests_closest_name() {
        // Error-msg round 36 (PR-it511): a typo'd child-component name in `let w = Widgt()`
        // was flat "unknown component `Widgt`" -- named the miss, not the fix. Extends the
        // did-you-mean courtesy already given to unknown free-fns/methods/fields/types/ctors/
        // contract-fns (K0249/K0100/K0206/K0247/K0254) to K0208, the one unknown-name site that
        // still lacked it.
        let typo = errors("component Widget {\n    intent \"w\"\n}\ncomponent Main {\n    intent \"m\"\n    let w = Widgt()\n}\n");
        assert!(
            typo.iter().any(|d| d.code == "K0208" && d.message.contains("unknown component `Widgt`") && d.message.contains("did you mean `Widget`?")),
            "typo'd child component should suggest the close match: {typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean).
        let none = errors("component Main {\n    intent \"m\"\n    let w = Zqxwbly()\n}\n");
        assert!(
            none.iter().any(|d| d.code == "K0208" && !d.message.contains("did you mean")),
            "unrelated name should stay bare: {none:?}"
        );
        // A correct child-component reference still type-checks cleanly.
        assert!(errors("component Widget {\n    intent \"w\"\n}\ncomponent Main {\n    intent \"m\"\n    let w = Widget()\n}\n").is_empty());
    }

    #[test]
    fn k0213_unknown_wire_child_suggests_closest_name() {
        // Error-msg round 40 (PR-it526): a typo'd child NAME on the left/right end of a
        // `wire` statement (distinct from K0208's unknown child-COMPONENT-TYPE, fixed
        // it511) was flat "unknown child `producr`" -- extends the same did-you-mean
        // courtesy to the one remaining unknown-child-name site that lacked it.
        let src = "component Src {\n    intent \"s\"\n    out val: Int\n}\ncomponent Sink {\n    intent \"k\"\n    in val: Int\n}\ncomponent Main {\n    intent \"m\"\n    let producer = Src()\n    let consumer = Sink()\n    wire producr.val -> consumer.val\n}\n";
        let typo = errors(src);
        assert!(
            typo.iter().any(|d| d.code == "K0213" && d.message.contains("unknown child `producr`") && d.message.contains("did you mean `producer`?")),
            "typo'd wire-endpoint child name should suggest the close match: {typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean).
        let none_src = "component Src {\n    intent \"s\"\n    out val: Int\n}\ncomponent Main {\n    intent \"m\"\n    let producer = Src()\n    wire zqxwbly.val -> producer.val\n}\n";
        let none = errors(none_src);
        assert!(
            none.iter().any(|d| d.code == "K0213" && !d.message.contains("did you mean")),
            "unrelated name should stay bare: {none:?}"
        );
        // A correct wire reference still type-checks cleanly.
        let ok_src = "component Src {\n    intent \"s\"\n    out val: Int\n}\ncomponent Sink {\n    intent \"k\"\n    in val: Int\n}\ncomponent Main {\n    intent \"m\"\n    let producer = Src()\n    let consumer = Sink()\n    wire producer.val -> consumer.val\n}\n";
        assert!(errors(ok_src).is_empty());
    }

    #[test]
    fn k0220_unknown_assignment_target_suggests_closest_name() {
        // Error-msg round 41 (PR-it533): `countr = 5` (typo'd assignment target,
        // distinct from K0240's unknown NAME in an expression/read position, which
        // already got the full did-you-mean treatment) was flat "unknown variable
        // `countr`" -- the exact same `ctx.scopes.names()` candidate set K0240
        // already uses was sitting right there, just never threaded through this
        // second unknown-variable site. `Scopes::names()` includes STATE FIELDS
        // too (inserted into scope alongside locals when checking a component), so
        // the fix covers `state n: Int = 0 ... on start { m = 5 }` as well, not
        // just plain function-local `var`s.
        let src = "fun main() {\n    var counter = 0\n    countr = 5\n}\n";
        let typo = errors(src);
        assert!(
            typo.iter().any(|d| d.code == "K0220" && d.message.contains("unknown variable `countr`") && d.message.contains("did you mean `counter`?")),
            "typo'd assignment target should suggest the close match: {typo:?}"
        );
        // Component STATE field candidates are included too.
        let state_src = "component Counter {\n    intent \"c\"\n    state n: Int = 0\n    on start {\n        m = 5\n    }\n}\n";
        let state_typo = errors(state_src);
        assert!(
            state_typo.iter().any(|d| d.code == "K0220" && d.message.contains("unknown variable `m`") && d.message.contains("did you mean `n`?")),
            "typo'd assignment to a state field should suggest the close match: {state_typo:?}"
        );
        // Nothing close -> no suggestion (no false-positive did-you-mean).
        let none = errors("fun main() {\n    zqxwbly = 5\n}\n");
        assert!(
            none.iter().any(|d| d.code == "K0220" && !d.message.contains("did you mean")),
            "unrelated name should stay bare: {none:?}"
        );
        // A correct assignment still type-checks cleanly.
        assert!(errors("fun main() {\n    var counter = 0\n    counter = 5\n}\n").is_empty());
    }

    #[test]
    fn tuple_attempt_points_to_list_or_record() {
        // `(a, b)` is a common tuple attempt; KUPL has none, so the parse error now names the
        // list/record alternatives instead of the bare "expected `)`, found `,`" (PR-it210).
        let hint = "KUPL has no tuples; use a list `[a, b]` or a record";
        let two = errors("fun main() { let x = (1, 2) }\n");
        assert!(two.iter().any(|d| d.code == "K0100" && d.message.contains(hint)), "two: {two:?}");
        let three = errors("fun main() { let p = (\"a\", 3, true) }\n");
        assert!(three.iter().any(|d| d.code == "K0100" && d.message.contains(hint)), "three: {three:?}");
        // Valid parenthesized expressions and unit still parse cleanly (no behavior change).
        assert!(errors("fun main() { let a = (1 + 2) * 3\n    let b = ((4))\n    let c = (true) }\n").is_empty());
        assert!(errors("fun noop() { () }\nfun main() { noop() }\n").is_empty());
    }

    #[test]
    fn argument_type_mismatch_names_the_position() {
        // A wrong-typed call argument now names WHICH argument (1-based) instead of a bare
        // "type mismatch in function call", so a multi-arg call points at the offending slot
        // (PR-it236).
        let a2 = errors("fun add(a: Int, b: Int) -> Int { a + b }\nfun main() { let _ = add(1, \"two\") }\n");
        assert!(a2.iter().any(|d| d.code == "K0200" && d.message.contains("argument 2") && d.message.contains("expected Int, found Str")), "{a2:?}");
        let a1 = errors("fun add(a: Int, b: Int) -> Int { a + b }\nfun main() { let _ = add(\"one\", 2) }\n");
        assert!(a1.iter().any(|d| d.code == "K0200" && d.message.contains("argument 1")), "{a1:?}");
        // A correctly-typed call still type-checks.
        assert!(errors("fun add(a: Int, b: Int) -> Int { a + b }\nfun main() { let _ = add(1, 2) }\n").is_empty());
    }

    #[test]
    fn calling_a_port_as_a_method_names_it_as_a_port() {
        // Calling a component's PORT as a method (`c.click()`) is a frequent mistake — ports are
        // wired/sent to, not called. K0247 now says which kind of port it is and how to reach it,
        // instead of the bare "does not expose a function" (PR-it232).
        let comp = "component Counter {\n    intent \"c\"\n    in click: Event\n    out value: Int\n    state n: Int = 0\n    on click { n = n + 1\n        emit value(n) }\n}\n";
        let inp = errors(&format!("{comp}fun main() {{ let c = Counter()\n    c.click() }}\n"));
        assert!(inp.iter().any(|d| d.code == "K0247" && d.message.contains("input port") && d.message.contains("wire")), "in-port: {inp:?}");
        let out = errors(&format!("{comp}fun main() {{ let c = Counter()\n    c.value() }}\n"));
        assert!(out.iter().any(|d| d.code == "K0247" && d.message.contains("output port")), "out-port: {out:?}");
        // A genuinely unknown method keeps the plain "does not expose a function" wording.
        let unk = errors(&format!("{comp}fun main() {{ let c = Counter()\n    c.frobnicate() }}\n"));
        assert!(unk.iter().any(|d| d.code == "K0247" && d.message.contains("does not expose a function")), "unknown: {unk:?}");
        // A close TYPO on an exposed function now names the closest exposed name (PR-it477).
        let typo = errors("component C {\n    intent \"c\"\n    state n: Int = 0\n    expose fun total() -> Int { n }\n}\nfun main() { let c = C()\n    let _ = c.totl() }\n");
        assert!(
            typo.iter().any(|d| d.code == "K0247" && d.message.contains("does not expose a function named `totl`") && d.message.contains("did you mean `total`?")),
            "typo should suggest the exposed name: {typo:?}"
        );
        // A real exposed function still type-checks.
        assert!(errors("component C {\n    intent \"c\"\n    state n: Int = 0\n    expose fun bump() -> Int { n = n + 1\n        n }\n}\nfun main() { let c = C()\n    let _ = c.bump() }\n").is_empty());
    }

    #[test]
    fn calling_a_non_function_says_so_plainly() {
        // Calling a concrete non-function value now reports "cannot call a value of type X; it is
        // not a function" instead of the confusing "expected fn(Int) -> ?0, found Int" with a raw
        // type variable (PR-it204). Still K0200 — message-text only, no accept/reject change.
        let hint = "it is not a function";
        for (src, ty) in [
            ("fun main() { let x = 5\n    let _ = x(3) }\n", "Int"),
            ("fun main() { let s = \"hi\"\n    let _ = s(3) }\n", "Str"),
            ("fun main() { let xs = [1, 2, 3]\n    let _ = xs(0) }\n", "List[Int]"),
        ] {
            let e = errors(src);
            assert!(
                e.iter().any(|d| d.code == "K0200"
                    && d.message.contains(hint)
                    && d.message.contains(ty)),
                "{ty}: {e:?}"
            );
        }
        // Real function / closure / HOF calls still type-check (no behavior change).
        assert!(errors(
            "fun add(a: Int, b: Int) -> Int { a + b }\nfun main() { let f = fn x { x * 2 }\n    let _ = add(2, 3)\n    let _ = f(10)\n    let _ = [1, 2, 3].map(fn x { x + 1 }) }\n"
        )
        .is_empty());
    }

    #[test]
    fn method_arity_error_names_the_parameter_types() {
        // K0250 for a wrong-argument-count method call now shows the expected parameter TYPES, not just
        // the count, so the user sees the signature -- e.g. `.center` takes 2 arguments (Int, Str) (PR-it490).
        let c = errors("fun main() { let _ = \"hi\".center(5) }\n");
        assert!(
            c.iter().any(|d| d.code == "K0250" && d.message.contains("takes 2 arguments (Int, Str)") && d.message.contains("1 given")),
            "center arity names the types: {c:?}"
        );
        // A zero-parameter method called with an argument keeps the bare count (no empty `()`).
        let z = errors("fun main() { let _ = \"hi\".to_upper(3) }\n");
        assert!(
            z.iter().any(|d| d.code == "K0250" && d.message.contains("takes 0 arguments") && !d.message.contains("()")),
            "zero-param keeps bare count: {z:?}"
        );
        // A correct-arity call still type-checks.
        assert!(errors("fun main() { let _ = \"hi\".center(5, \"*\")\n    let _ = \"x\".to_upper() }\n").is_empty());
    }

    #[test]
    fn k0231_names_the_variants() {
        // Error-message round 33 (PR-it498): K0231 (field access or `with`-rebuild on a multi-variant
        // ADT, which requires `match` instead) said only "`Shape` has multiple variants -- use `match`
        // to access fields" -- naming neither the actual variants nor the field the user tried. Now it
        // names both the variant list and the attempted field:
        //   `Shape` has multiple variants (Circle, Square, Rect) -- use `match` to access `.r`
        let field_access = errors(
            "type Shape = Circle(r: Int) | Square(side: Int) | Rect(w: Int, h: Int)\n\
             fun probe(s: Shape) -> Int { s.r }\n\
             fun main() { 0 }\n",
        );
        assert!(
            field_access.iter().any(|d| d.code == "K0231"
                && d.message.contains("(Circle, Square, Rect)")
                && d.message.contains("access `.r`")),
            "field-access K0231 must name the variants and the attempted field: {field_access:?}"
        );
        // The `with`-rebuild path (a separate call site) gets the same treatment.
        let with_update = errors(
            "type Shape = Circle(r: Int) | Square(side: Int)\n\
             fun probe(s: Shape) -> Shape { s with r: 5 }\n\
             fun main() { 0 }\n",
        );
        assert!(
            with_update.iter().any(|d| d.code == "K0231" && d.message.contains("(Circle, Square)") && d.message.contains("rebuild")),
            "with-update K0231 must name the variants: {with_update:?}"
        );
    }

    #[test]
    fn no_fields_error_names_what_has_fields() {
        // K0233 now tells the user which kinds of type DO have fields, so accessing a field on a
        // non-record type points toward the fix (PR-it197). Message-text only — record field
        // access is unchanged.
        let hint = "only records and components have fields";
        let on_int = errors("fun main() { let x = 5\n    let _ = x.field }\n");
        assert!(on_int.iter().any(|d| d.code == "K0233" && d.message.contains(hint)), "int: {on_int:?}");
        let on_str = errors("fun main() { let _ = \"hi\".foo }\n");
        assert!(on_str.iter().any(|d| d.code == "K0233" && d.message.contains(hint)), "str: {on_str:?}");
        // A field access on a LIST also names the list accessors (a frequent split_once mistake) (PR-it486).
        let on_list = errors("fun main() { let xs = [1, 2, 3]\n    let _ = xs.fst }\n");
        assert!(
            on_list.iter().any(|d| d.code == "K0233" && d.message.contains("a list is indexed") && d.message.contains(".get(i)")),
            "list hint: {on_list:?}"
        );
        // A non-list keeps the bare message (no bogus list hint).
        assert!(on_int.iter().all(|d| !d.message.contains("a list is indexed")), "int has no list hint: {on_int:?}");
        // Real record field access still type-checks (no behavior change).
        assert!(errors("type Item = { name: Str, qty: Int }\nfun main() { let it = Item(name: \"a\", qty: 1)\n    let _ = it.name\n    let _ = it.qty }\n").is_empty());
    }

    #[test]
    fn uninferred_field_access_names_the_annotation_fix() {
        // K0232 now points at the concrete fix -- annotate the binding/parameter so the record type is
        // known -- naming the empty-list fold-seed case that most often triggers it (PR-it323). The typo
        // trail: an untyped `[]` flows into a higher-order fn and the field access can't resolve the
        // element type. Message-text only; the annotated form still type-checks unchanged.
        let e = errors("type Row = { n: Int }\nfun main() uses io {\n    let _ = [].fold(0, fn(s, r) { s + r.n })\n}\n");
        assert!(
            e.iter().any(|d| d.code == "K0232"
                && d.message.contains("annotate its binding or parameter")
                && d.message.contains("List[Row]")),
            "{e:?}"
        );
        // The annotated fold seed the hint recommends type-checks cleanly (the fix works, no behavior change).
        assert!(errors(
            "type Row = { n: Int }\nfun main() uses io {\n    let seed: List[Row] = []\n    let _ = seed.fold(0, fn(s, r) { s + r.n })\n}\n"
        ).is_empty());
    }

    #[test]
    fn order_error_names_the_orderable_types() {
        // K0234 now names which types ARE orderable so the fix is obvious, at all three trigger
        // sites: a comparison operator, List.sort, and List.min/max (PR-it193). Widened again
        // in PR-it549 to also name the numeric types beyond Int/Float (sized ints, f32,
        // BigInt, Rational are all orderable now too) — the wording here was updated to
        // match, and the final assertion below now also covers the newly-accepted types.
        let hint = "only Int, Float, Str, and other numeric types can be compared";
        let cmp = errors("type P = P(x: Int)\nfun main() { let b = P(1) < P(2)\n    let _ = b }\n");
        assert!(cmp.iter().any(|d| d.code == "K0234" && d.message.contains(hint)), "cmp: {cmp:?}");
        let sort = errors("type P = P(x: Int)\nfun main() { let _ = [P(1), P(2)].sort() }\n");
        assert!(sort.iter().any(|d| d.code == "K0234" && d.message.contains(hint)), "sort: {sort:?}");
        let max = errors("type P = P(x: Int)\nfun main() { let _ = [P(1), P(2)].max() }\n");
        assert!(max.iter().any(|d| d.code == "K0234" && d.message.contains(hint)), "max: {max:?}");
        // Orderable element types are still accepted (no behavior change).
        assert!(errors("fun main() { let _ = [3, 1, 2].sort()\n    let _ = \"a\" < \"b\" }\n").is_empty());
        // Sized ints, f32, BigInt, and Rational are ALL orderable too (PR-it549) — sort/
        // min/max no longer wrongly reject types the comparison operators already accept.
        assert!(errors("fun main() { let xs: List[i32] = [3i32, 1i32]\n    let _ = xs.sort() }\n").is_empty());
        assert!(errors("fun main() { let xs: List[f32] = [3.0f32, 1.0f32]\n    let _ = xs.max() }\n").is_empty());
        assert!(errors("fun main() { let xs = [big(3), big(1)]\n    let _ = xs.min() }\n").is_empty());
        assert!(errors("fun main() { let xs = [rat(1, 2), rat(1, 3)]\n    let _ = xs.sort() }\n").is_empty());
    }

    #[test]
    fn contract_effect_budget_message_is_clear() {
        // A component method whose effects exceed the contract's budget is a K0264 error; the
        // message reads "allows no effects" for an empty budget (clearer than "only []") and
        // "allows only [<effects>]" otherwise (PR-it168).
        let empty = errors(
            "contract Pure {\n    intent \"none\"\n    expose fun compute() -> Int\n}\ncomponent Bad fulfills Pure {\n    intent \"io\"\n    expose fun compute() uses io -> Int { 42 }\n}\nfun main() {}\n",
        );
        assert!(
            empty.iter().any(|d| d.code == "K0264" && d.message.contains("allows no effects")),
            "{empty:?}"
        );
        let nonempty = errors(
            "contract L {\n    intent \"log\"\n    expose fun act() uses io\n}\ncomponent C fulfills L {\n    intent \"io+exec\"\n    expose fun act() uses io, exec {}\n}\nfun main() {}\n",
        );
        assert!(
            nonempty.iter().any(|d| d.code == "K0264" && d.message.contains("uses `exec`") && d.message.contains("allows only [io]")),
            "{nonempty:?}"
        );
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
        // cross-language alias too far for edit-distance -> named via the common-alias table (PR-it318):
        // `.length` (Java/JS/C#) is 3 edits from `len`, so only the alias table can name it.
        let e4 = errors("fun main() uses io { print([1, 2, 3].length()) }\n");
        assert!(
            e4.iter().any(|d| d.code == "K0249" && d.message.contains("did you mean `len`?")),
            "{e4:?}"
        );
        // `.size` -> `len` and `.append` -> `push` are the other common ones.
        let e5 = errors("fun main() uses io { print([1].size()) }\n");
        assert!(e5.iter().any(|d| d.message.contains("did you mean `len`?")), "{e5:?}");
        let e6 = errors("fun main() uses io { print([1].append(2)) }\n");
        assert!(e6.iter().any(|d| d.message.contains("did you mean `push`?")), "{e6:?}");
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
