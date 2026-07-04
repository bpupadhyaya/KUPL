//! Type & semantic checker.
//!
//! Two passes: (1) collect signatures of types, functions, and components;
//! (2) check every body with local inference (fresh vars + unification).
//! Public boundaries (fun params/returns, ports, props) must be annotated —
//! that is enforced by the grammar itself in v0.1.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::types::{ComponentSig, ContractSig, Ty, TypeSig, Unifier, VariantSig};

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
}

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
                    TypeSig { name: t.name.clone(), variants: Vec::new(), is_record: false },
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
                    let is_record = t.variants.len() == 1 && t.variants[0].name == t.name;
                    self.checked.types.insert(
                        t.name.clone(),
                        TypeSig { name: t.name.clone(), variants, is_record },
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
                other => {
                    if self.checked.types.contains_key(other) {
                        Ty::Named(other.to_string())
                    } else if self.checked.components.contains_key(other) {
                        Ty::Component(other.to_string())
                    } else if self.checked.contracts.contains_key(other) {
                        Ty::Contract(other.to_string())
                    } else {
                        self.err("K0205", format!("unknown type `{other}`"), t.span);
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
                // ai fun bodies are prompts, not code — nothing to body-check
                Item::Fun(f) if f.ai.is_some() => {}
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
        let body_ty = self.check_block(&f.body, &mut ctx);
        // The block's tail value must match the return type (unless Unit-returning).
        let ret = self.uni.apply(&ctx.ret.clone());
        if ret != Ty::Unit {
            self.unify(&ret, &body_ty, f.body.span, &format!("return value of `{}`", f.name));
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

    fn infer_expr(&mut self, expr: &Expr, ctx: &mut Ctx) -> Ty {
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
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
                    Ty::Named(tn) => {
                        let sig = self.checked.types.get(&tn).cloned();
                        match sig {
                            Some(sig) if sig.variants.len() == 1 => {
                                match sig.variants[0].fields.iter().find(|(fname, _)| fname == name) {
                                    Some((_, ty)) => ty.clone(),
                                    None => {
                                        self.err("K0230", format!("type `{tn}` has no field `{name}`"), expr.span);
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
                        let t = self.default_numeric(t);
                        if !t.is_numeric() && t != Ty::Str {
                            self.err("K0234", format!("cannot order values of type {t}"), expr.span);
                        }
                        Ty::Bool
                    }
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                        self.unify(&lt, &rt, expr.span, "arithmetic");
                        let t = self.uni.apply(&lt);
                        let t = self.default_numeric(t);
                        let str_ok = *op == BinOp::Add && t == Ty::Str;
                        let tensor_ok = t == Ty::Tensor && *op != BinOp::Rem;
                        if tensor_ok {
                            return t;
                        }
                        if !t.is_numeric() && !str_ok {
                            self.err("K0235", format!("arithmetic needs Int or Float operands, found {t}"), expr.span);
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
                let Ty::Named(tn) = &rt else {
                    self.err("K0233", format!("{rt} has no fields to update"), expr.span);
                    return self.uni.fresh();
                };
                let sig = self.checked.types.get(tn).cloned();
                match sig {
                    Some(sig) if sig.variants.len() == 1 => {
                        for (field, value) in updates {
                            let vt = self.infer_expr(value, ctx);
                            match sig.variants[0].fields.iter().find(|(f, _)| f == field) {
                                Some((_, fty)) => {
                                    self.unify(&fty.clone(), &vt, value.span, &format!("field `{field}`"));
                                }
                                None => self.err(
                                    "K0230",
                                    format!("type `{tn}` has no field `{field}`"),
                                    value.span,
                                ),
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
                let ok = self.uni.fresh();
                let err = self.uni.fresh();
                let expected = Ty::Result(Box::new(ok.clone()), Box::new(err.clone()));
                self.unify(&expected, &it, inner.span, "`?` operand (must be a Result)");
                if ctx.in_handler {
                    self.err(
                        "K0237",
                        "`?` is not allowed in handlers in v0.1 — handle the Result with `match`",
                        expr.span,
                    );
                } else {
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
                }
                self.uni.apply(&ok)
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
        fn subst(ty: &Ty, m: &HashMap<u32, Ty>) -> Ty {
            match ty {
                Ty::Var(id) => m.get(id).cloned().unwrap_or(Ty::Var(*id)),
                Ty::List(e) => Ty::List(Box::new(subst(e, m))),
                Ty::Set(e) => Ty::Set(Box::new(subst(e, m))),
                Ty::Map(k, v) => Ty::Map(Box::new(subst(k, m)), Box::new(subst(v, m))),
                Ty::Option(e) => Ty::Option(Box::new(subst(e, m))),
                Ty::Result(a, b) => Ty::Result(Box::new(subst(a, m)), Box::new(subst(b, m))),
                Ty::Fun(ps, r) => {
                    Ty::Fun(ps.iter().map(|p| subst(p, m)).collect(), Box::new(subst(r, m)))
                }
                other => other.clone(),
            }
        }
        (
            params.iter().map(|p| subst(p, &mapping)).collect(),
            subst(ret, &mapping),
        )
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
            if fields.is_empty() {
                return Ty::Named(tyname);
            }
            return Ty::Fun(
                fields.iter().map(|(_, t)| t.clone()).collect(),
                Box::new(Ty::Named(tyname)),
            );
        }
        self.err("K0240", format!("unknown name `{name}`"), span);
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
                self.check_named_args(name, &fields, args, span, ctx);
                return Ty::Named(tyname);
            }
            // component construction (props checked in the caller's own scope)
            if let Some(sig) = self.checked.components.get(name).cloned() {
                self.check_ctor_args(name, &sig, args, span, ctx);
                return Ty::Component(name.clone());
            }
        }
        // general callable
        let ct = self.infer_expr(callee, ctx);
        let mut arg_tys = Vec::new();
        for a in args {
            if a.name.is_some() {
                self.err("K0241", "named arguments are only allowed for constructors and props", a.value.span);
            }
            arg_tys.push(self.infer_expr(&a.value, ctx));
        }
        match self.uni.apply(&ct) {
            Ty::Fun(ps, _) if ps.len() != args.len() => {
                self.err(
                    "K0242",
                    format!("this function takes {} argument(s), {} given", ps.len(), args.len()),
                    span,
                );
                self.uni.fresh()
            }
            // concrete function: check each argument with contract assignability
            Ty::Fun(ps, r) => {
                for (p, at) in ps.iter().zip(arg_tys.iter()) {
                    self.check_assign(p, at, span, "function call");
                }
                self.uni.apply(&r)
            }
            // callee type not yet known (e.g. a type variable): fall back to
            // whole-function unification to drive inference
            _ => {
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
                        self.err("K0244", format!("`{ctor}` has no field named `{n}`"), arg.value.span);
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
            (Ty::List(t), "map") => {
                let u = self.uni.fresh();
                Some((
                    vec![Ty::Fun(vec![(**t).clone()], Box::new(u.clone()))],
                    Ty::List(Box::new(u)),
                ))
            }
            (Ty::List(t), "filter") => Some((
                vec![Ty::Fun(vec![(**t).clone()], Box::new(Ty::Bool))],
                Ty::List(t.clone()),
            )),
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
            (Ty::List(t), "window") | (Ty::List(t), "chunk") => {
                Some((vec![Ty::Int], Ty::List(Box::new(Ty::List(t.clone())))))
            }
            (Ty::Str, "len") => Some((vec![], Ty::Int)),
            (Ty::Str, "contains") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "starts_with") => Some((vec![Ty::Str], Ty::Bool)),
            (Ty::Str, "to_upper") | (Ty::Str, "to_lower") | (Ty::Str, "trim") => Some((vec![], Ty::Str)),
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
            (Ty::Int, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Int, "to_float") => Some((vec![], Ty::Float)),
            (Ty::Int, "abs") => Some((vec![], Ty::Int)),
            (Ty::Int, "min") | (Ty::Int, "max") | (Ty::Int, "pow") | (Ty::Int, "gcd") => {
                Some((vec![Ty::Int], Ty::Int))
            }
            (Ty::Int, "clamp") => Some((vec![Ty::Int, Ty::Int], Ty::Int)),
            (Ty::Int, "sign") => Some((vec![], Ty::Int)),
            (Ty::Int, "is_even") | (Ty::Int, "is_odd") => Some((vec![], Ty::Bool)),
            (Ty::Float, "to_str") => Some((vec![], Ty::Str)),
            (Ty::Float, "to_int") => Some((vec![], Ty::Int)),
            (Ty::Float, "abs") | (Ty::Float, "sqrt") => Some((vec![], Ty::Float)),
            (Ty::Float, "floor") | (Ty::Float, "ceil") | (Ty::Float, "round") => {
                Some((vec![], Ty::Float))
            }
            (Ty::Float, "log") | (Ty::Float, "log10") | (Ty::Float, "exp") | (Ty::Float, "sin")
            | (Ty::Float, "cos") | (Ty::Float, "tan") | (Ty::Float, "sign") => {
                Some((vec![], Ty::Float))
            }
            (Ty::Float, "clamp") => Some((vec![Ty::Float, Ty::Float], Ty::Float)),
            (Ty::Float, "is_nan") | (Ty::Float, "is_infinite") => Some((vec![], Ty::Bool)),
            (Ty::Float, "min") | (Ty::Float, "max") | (Ty::Float, "pow") => {
                Some((vec![Ty::Float], Ty::Float))
            }
            (Ty::Option(t), "is_some") | (Ty::Option(t), "is_none") => {
                let _ = t;
                Some((vec![], Ty::Bool))
            }
            (Ty::Option(t), "unwrap_or") => Some((vec![(**t).clone()], (**t).clone())),
            (Ty::Result(t, e), "is_ok") | (Ty::Result(t, e), "is_err") => {
                let _ = (t, e);
                Some((vec![], Ty::Bool))
            }
            (Ty::Result(t, _), "unwrap_or") => Some((vec![(**t).clone()], (**t).clone())),
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
            (Ty::Set(t), "insert") | (Ty::Set(t), "remove") => {
                Some((vec![(**t).clone()], Ty::Set(t.clone())))
            }
            (Ty::Set(t), "contains") => Some((vec![(**t).clone()], Ty::Bool)),
            (Ty::Set(_), "len") => Some((vec![], Ty::Int)),
            (Ty::Set(t), "union") | (Ty::Set(t), "intersect") | (Ty::Set(t), "difference") => {
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
                for a in args {
                    self.infer_expr(a, ctx);
                }
                self.err("K0249", format!("{rt} has no method `{name}`"), span);
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
                    None => self.err("K0254", format!("unknown constructor `{other}` in pattern"), pat.span),
                    Some((tyname, fields)) => {
                        self.unify(expected, &Ty::Named(tyname), pat.span, "pattern");
                        if args.len() != fields.len() {
                            self.err(
                                "K0255",
                                format!("`{other}` has {} field(s), pattern has {}", fields.len(), args.len()),
                                pat.span,
                            );
                        }
                        for (a, (_, fty)) in args.iter().zip(fields.iter()) {
                            self.check_pattern(a, fty, ctx);
                        }
                    }
                },
            },
        }
    }

    fn check_exhaustive(&mut self, scrut: &Ty, arms: &[MatchArm], span: Span) {
        let has_catch_all = arms
            .iter()
            .any(|a| matches!(a.pattern.kind, PatternKind::Wildcard | PatternKind::Bind(_)));
        if has_catch_all {
            return;
        }
        let covered: HashSet<&str> = arms
            .iter()
            .filter_map(|a| match &a.pattern.kind {
                PatternKind::Ctor { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = match self.uni.apply(scrut) {
            Ty::Bool => {
                let mut m = Vec::new();
                let has_true = arms.iter().any(|a| matches!(a.pattern.kind, PatternKind::Bool(true)));
                let has_false = arms.iter().any(|a| matches!(a.pattern.kind, PatternKind::Bool(false)));
                if !has_true {
                    m.push("true".into());
                }
                if !has_false {
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
            Ty::Named(tn) => match self.checked.types.get(&tn) {
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
