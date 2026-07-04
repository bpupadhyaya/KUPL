//! AST -> KVM bytecode compiler (functional core).
//!
//! v0.4 compiles top-level functions (and the lambdas inside them). Components
//! still run on the tree-walking interpreter; they move to the VM when the
//! actor runtime is ported. Register model: every local and temporary gets a
//! fresh frame-local register (no reuse — correctness first, allocation later).

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::*;
use crate::bytecode::*;
use crate::check::Checked;
use crate::diag::{Diag, Span};
use crate::value::Value;

pub fn compile_module(program: &Program, checked: &Checked) -> Result<Module, Vec<Diag>> {
    let mut module = Module::default();
    let mut ctor_idx: HashMap<String, u16> = HashMap::new();

    // builtin constructors first, then user constructors (sorted: deterministic)
    for (ty, variant, arity) in [
        ("Option", "Some", 1u8),
        ("Option", "None", 0),
        ("Result", "Ok", 1),
        ("Result", "Err", 1),
    ] {
        ctor_idx.insert(variant.to_string(), module.ctors.len() as u16);
        module.ctors.push(CtorMeta {
            type_name: ty.into(),
            variant: variant.into(),
            arity,
        });
    }
    let mut user_ctors: Vec<(&String, &(String, Vec<(String, crate::types::Ty)>))> =
        checked.ctors.iter().collect();
    user_ctors.sort_by(|a, b| a.0.cmp(b.0));
    for (variant, (ty, fields)) in user_ctors {
        ctor_idx.insert(variant.clone(), module.ctors.len() as u16);
        module.ctors.push(CtorMeta {
            type_name: ty.clone(),
            variant: variant.clone(),
            arity: fields.len() as u8,
        });
    }

    // pre-assign chunk indices for all top-level funs
    let funs: Vec<&FunDecl> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Fun(f) => Some(f),
            _ => None,
        })
        .collect();
    for (i, f) in funs.iter().enumerate() {
        module.funs.insert(f.name.clone(), i as u16);
        module.chunks.push(Chunk {
            name: f.name.clone(),
            ncaps: 0,
            nparams: f.params.len() as u8,
            nregs: 0,
            consts: Vec::new(),
            code: Vec::new(),
            spans: Vec::new(),
        });
    }

    let mut ctor_fields: HashMap<String, Vec<String>> = HashMap::new();
    for (variant, (_, fields)) in &checked.ctors {
        ctor_fields.insert(variant.clone(), fields.iter().map(|(n, _)| n.clone()).collect());
    }
    for v in ["Some", "Ok", "Err"] {
        ctor_fields.insert(v.into(), vec!["value".into()]);
    }
    module.ctor_field_names = ctor_fields.clone();

    let mut shared = Shared {
        module,
        ctor_idx,
        ctor_fields,
        comp_props: HashMap::new(),
        diags: Vec::new(),
        fun_names: funs.iter().map(|f| f.name.clone()).collect(),
    };

    // components: register names first (constructions may be mutually recursive)
    let comps: Vec<&ComponentDecl> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Component(c) => Some(c),
            _ => None,
        })
        .collect();
    for (i, c) in comps.iter().enumerate() {
        shared.module.component_names.insert(c.name.clone(), i as u16);
    }

    // phase A: prop default chunks + prop tables (needed by any construction site)
    for c in &comps {
        let mut props = Vec::new();
        for p in &c.props {
            let default = p.default.as_ref().map(|d| {
                let mut fc = FnCompiler::new(&mut shared, &format!("{}::default::{}", c.name, p.name), 0, 0);
                let r = fc.expr(d);
                fc.emit(Op::Ret(r), p.span);
                let chunk = fc.finish();
                shared.module.chunks.push(chunk);
                (shared.module.chunks.len() - 1) as u16
            });
            props.push((p.name.clone(), default));
        }
        shared.comp_props.insert(c.name.clone(), props);
    }

    for (i, f) in funs.iter().enumerate() {
        let chunk = if f.ai.is_some() {
            compile_ai_fun(&mut shared, f, checked)
        } else {
            compile_fun(&mut shared, f)
        };
        shared.module.chunks[i] = chunk;
    }

    for c in &comps {
        let meta = compile_component(&mut shared, c);
        shared.module.components.push(meta);
    }

    if shared.diags.is_empty() {
        Ok(shared.module)
    } else {
        Err(shared.diags)
    }
}

/// Compile a component: slot layout (props, state, children), default chunks,
/// init chunk (state inits + children + wires), handler chunks, expose chunks.
fn compile_component(shared: &mut Shared, c: &ComponentDecl) -> ComponentMeta {
    // slot layout
    let mut slots: HashMap<String, u8> = HashMap::new();
    let mut slot = 0u8;
    for p in &c.props {
        slots.insert(p.name.clone(), slot);
        slot += 1;
    }
    for s in &c.state {
        slots.insert(s.name.clone(), slot);
        slot += 1;
    }
    for child in &c.children {
        slots.insert(child.name.clone(), slot);
        slot += 1;
    }
    // pre-assign chunk indices for ALL component functions (mutual recursion)
    let mut fun_chunks: HashMap<String, u16> = HashMap::new();
    for f in c.funs.iter().chain(c.exposes.iter()) {
        let idx = shared.module.chunks.len() as u16;
        shared.module.chunks.push(Chunk {
            name: format!("{}::{}", c.name, f.name),
            ncaps: 0,
            nparams: f.params.len() as u8,
            nregs: 0,
            consts: Vec::new(),
            code: Vec::new(),
            spans: Vec::new(),
        });
        fun_chunks.insert(f.name.clone(), idx);
    }
    let comp_ctx = CompCtx { slots: slots.clone(), funs: fun_chunks.clone() };
    let props = shared.comp_props.get(&c.name).cloned().unwrap_or_default();

    // restart chunk: state inits only (supervision resets state, keeps wiring)
    let restart_chunk = {
        let mut fc = FnCompiler::new(shared, &format!("{}::restart", c.name), 0, 0);
        fc.comp = Some(comp_ctx.clone());
        for s in &c.state {
            let r = fc.expr(&s.init);
            fc.emit(Op::StateSet(slots[&s.name], r), s.span);
        }
        let u = fc.const_reg(Value::Unit, c.span);
        fc.emit(Op::Ret(u), c.span);
        let chunk = fc.finish();
        shared.module.chunks.push(chunk);
        (shared.module.chunks.len() - 1) as u16
    };

    // init chunk: state inits, children, wires (instance is current)
    let init_chunk = {
        let mut fc = FnCompiler::new(shared, &format!("{}::init", c.name), 0, 0);
        fc.comp = Some(comp_ctx.clone());
        for s in &c.state {
            let r = fc.expr(&s.init);
            fc.emit(Op::StateSet(slots[&s.name], r), s.span);
        }
        for child in &c.children {
            let supervised = c.supervises.iter().any(|s| {
                s.child == child.name && s.policy == SupervisePolicy::RestartOnFailure
            });
            let r = fc.instance_expr(&child.component, &child.args, child.span, supervised as u8);
            fc.emit(Op::StateSet(slots[&child.name], r), child.span);
        }
        for w in &c.wires {
            let from = fc.slot_reg(&w.from.0, w.span);
            let to = fc.slot_reg(&w.to.0, w.span);
            let out_port = fc.const_idx(Value::str(w.from.1.clone()));
            let in_port = fc.const_idx(Value::str(w.to.1.clone()));
            fc.emit(Op::WireOp { from, out_port, to, in_port }, w.span);
        }
        let u = fc.const_reg(Value::Unit, c.span);
        fc.emit(Op::Ret(u), c.span);
        let chunk = fc.finish();
        shared.module.chunks.push(chunk);
        (shared.module.chunks.len() - 1) as u16
    };

    // handlers (ports + lifecycle) and timers
    let mut handlers = Vec::new();
    let mut timers = Vec::new();
    for (i, h) in c.handlers.iter().enumerate() {
        let (key, label, timer) = match &h.trigger {
            Trigger::Start => ("@start".to_string(), "start".to_string(), None),
            Trigger::Stop => ("@stop".to_string(), "stop".to_string(), None),
            Trigger::Port(p) => (p.clone(), p.clone(), None),
            Trigger::Every(ms) => {
                (format!("@every#{i}"), format!("every {ms}ms"), Some((true, *ms)))
            }
            Trigger::After(ms) => {
                (format!("@after#{i}"), format!("after {ms}ms"), Some((false, *ms)))
            }
        };
        let has_param = h.param.is_some();
        let mut fc = FnCompiler::new(
            shared,
            &format!("{}::on {}", c.name, label),
            0,
            if has_param { 1 } else { 0 },
        );
        fc.comp = Some(comp_ctx.clone());
        if let Some(p) = &h.param {
            fc.bind_local(p);
        }
        fc.block(&h.body);
        let u = fc.const_reg(Value::Unit, h.span);
        fc.emit(Op::Ret(u), h.span);
        let chunk = fc.finish();
        shared.module.chunks.push(chunk);
        let chunk_idx = (shared.module.chunks.len() - 1) as u16;
        match timer {
            Some((every, interval_ms)) => {
                timers.push(TimerMeta { chunk: chunk_idx, every, interval_ms });
            }
            None => handlers.push((key, chunk_idx, has_param)),
        }
    }

    // component functions (private + exposed) into their reserved chunk slots
    let mut exposes = HashMap::new();
    for f in c.funs.iter().chain(c.exposes.iter()) {
        let idx = fun_chunks[&f.name];
        let mut fc = FnCompiler::new(
            shared,
            &format!("{}::{}", c.name, f.name),
            0,
            f.params.len() as u8,
        );
        fc.comp = Some(comp_ctx.clone());
        for p in &f.params {
            fc.bind_local(&p.name);
        }
        let last = fc.block(&f.body);
        let r = last.unwrap_or_else(|| fc.const_reg(Value::Unit, f.span));
        fc.emit(Op::Ret(r), f.span);
        let mut chunk = fc.finish();
        chunk.name = format!("{}::{}", c.name, f.name);
        shared.module.chunks[idx as usize] = chunk;
    }
    for f in &c.exposes {
        exposes.insert(f.name.clone(), fun_chunks[&f.name]);
    }

    ComponentMeta {
        name: c.name.clone(),
        is_app: c.is_app,
        props,
        nslots: slot,
        init_chunk,
        restart_chunk,
        handlers,
        exposes,
        out_ports: c
            .ports
            .iter()
            .filter(|p| p.dir == PortDir::Out)
            .map(|p| p.name.clone())
            .collect(),
        timers,
    }
}

struct Shared {
    module: Module,
    ctor_idx: HashMap<String, u16>,
    ctor_fields: HashMap<String, Vec<String>>,
    /// component name -> props (name, optional default chunk) — built before bodies
    comp_props: HashMap<String, Vec<(String, Option<u16>)>>,
    diags: Vec<Diag>,
    fun_names: HashSet<String>,
}

/// Component compilation context: name -> instance slot (props, state, children),
/// plus the chunk indices of the component's own functions (private + exposed).
#[derive(Clone)]
struct CompCtx {
    slots: HashMap<String, u8>,
    funs: HashMap<String, u16>,
}

fn compile_fun(shared: &mut Shared, f: &FunDecl) -> Chunk {
    let mut fc = FnCompiler::new(shared, &f.name, 0, f.params.len() as u8);
    for p in &f.params {
        fc.bind_local(&p.name);
    }
    let last = fc.block(&f.body);
    let r = last.unwrap_or_else(|| fc.const_reg(Value::Unit, f.span));
    fc.emit(Op::Ret(r), f.span);
    fc.finish()
}

/// An `ai fun` chunk builds the interpolated intent string from the parameter
/// registers, then issues one `CallAi` — the runtime signature (model, shape,
/// tools) lives in `module.ai_funs`.
fn compile_ai_fun(shared: &mut Shared, f: &FunDecl, checked: &Checked) -> Chunk {
    let info = shared.module.ai_funs.len() as u16;
    let meta = checked.ai_funs.get(&f.name).cloned().unwrap_or(crate::ai::AiFunMeta {
        name: f.name.clone(),
        intent: f.ai.as_ref().map(|a| a.intent.clone()).unwrap_or_default(),
        model: f.ai.as_ref().and_then(|a| a.model.clone()),
        params: f.params.iter().map(|p| p.name.clone()).collect(),
        shape: crate::ai::AiShape::Str,
        wraps_result: false,
        tools: Vec::new(),
    });
    shared.module.ai_funs.push(meta);
    let mut fc = FnCompiler::new(shared, &f.name, 0, f.params.len() as u8);
    for p in &f.params {
        fc.bind_local(&p.name);
    }
    let intent = match f.ai.as_ref() {
        Some(ai) => fc.expr(&ai.intent_expr),
        None => fc.const_reg(Value::str(String::new()), f.span),
    };
    let dst = fc.alloc(f.span);
    fc.emit(Op::CallAi { dst, info, intent }, f.span);
    fc.emit(Op::Ret(dst), f.span);
    fc.finish()
}

struct FnCompiler<'s> {
    shared: &'s mut Shared,
    chunk: Chunk,
    /// Some(...) while compiling inside a component (state/prop/child slots)
    comp: Option<CompCtx>,
    /// scope stack of (name, reg)
    scopes: Vec<Vec<(String, Reg)>>,
    next: u16,
    /// Highest register ever allocated (frame size), independent of resets.
    high_water: u16,
    /// (continue_target, break_patches); usize::MAX target = for-loop, whose
    /// continues are collected in `pending_continues` and patched at the
    /// increment position.
    loops: Vec<(usize, Vec<usize>)>,
    pending_continues: Vec<usize>,
    too_large: bool,
}

impl<'s> FnCompiler<'s> {
    fn new(shared: &'s mut Shared, name: &str, ncaps: u8, nparams: u8) -> Self {
        FnCompiler {
            shared,
            chunk: Chunk {
                name: name.to_string(),
                ncaps,
                nparams,
                nregs: 0,
                consts: Vec::new(),
                code: Vec::new(),
                spans: Vec::new(),
            },
            comp: None,
            scopes: vec![Vec::new()],
            next: 0,
            high_water: 0,
            loops: Vec::new(),
            pending_continues: Vec::new(),
            too_large: false,
        }
    }

    /// Load a component slot (state/prop/child) — or a local — into a register.
    fn slot_reg(&mut self, name: &str, span: Span) -> Reg {
        if let Some(r) = self.lookup(name) {
            return r;
        }
        if let Some(ctx) = &self.comp {
            if let Some(&s) = ctx.slots.get(name) {
                let dst = self.alloc(span);
                self.emit(Op::StateGet(dst, s), span);
                return dst;
            }
        }
        self.err("K0240", format!("unknown name `{name}` (KVM)"), span);
        0
    }

    /// Construct a component instance: args ordered to prop order, defaults
    /// filled by calling the prop's default chunk.
    fn instance_expr(&mut self, comp_name: &str, args: &[Arg], span: Span, policy: u8) -> Reg {
        let Some(&comp_idx) = self.shared.module.component_names.get(comp_name) else {
            self.err("K0208", format!("unknown component `{comp_name}` (KVM)"), span);
            return self.const_reg(Value::Unit, span);
        };
        let props = self.shared.comp_props.get(comp_name).cloned().unwrap_or_default();
        let mut supplied: Vec<Option<Expr>> = vec![None; props.len()];
        for (i, a) in args.iter().enumerate() {
            let idx = match &a.name {
                Some(n) => props.iter().position(|(pn, _)| pn == n).unwrap_or(i),
                None => i,
            };
            if idx < supplied.len() {
                supplied[idx] = Some(a.value.clone());
            }
        }
        let temps: Vec<Reg> = props
            .iter()
            .zip(supplied)
            .map(|((pname, default), s)| match s {
                Some(e) => self.expr(&e),
                None => match default {
                    Some(chunk) => {
                        let dst = self.alloc(span);
                        self.emit(Op::Call { dst, fun: *chunk, start: 0, argc: 0 }, span);
                        dst
                    }
                    None => {
                        self.err(
                            "K0216",
                            format!("missing required prop `{pname}` for `{comp_name}`"),
                            span,
                        );
                        self.const_reg(Value::Unit, span)
                    }
                },
            })
            .collect();
        let start = self.next as Reg;
        for t in temps {
            let r = self.alloc(span);
            self.emit(Op::Move(r, t), span);
        }
        let dst = self.alloc(span);
        self.emit(
            Op::MakeInstance { dst, comp: comp_idx, start, argc: props.len() as u8, policy },
            span,
        );
        dst
    }

    fn finish(mut self) -> Chunk {
        self.chunk.nregs = self.high_water.max(self.next).max(1);
        self.chunk
    }

    fn err(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.shared.diags.push(Diag::error(code, msg, span));
    }

    fn alloc(&mut self, span: Span) -> Reg {
        let r = self.next;
        self.next += 1;
        if self.next > self.high_water {
            self.high_water = self.next;
        }
        if r > 255 && !self.too_large {
            self.too_large = true;
            self.err("K0801", "function too large for KVM v0 (more than 256 registers)", span);
        }
        (r & 0xff) as Reg
    }

    fn bind_local(&mut self, name: &str) -> Reg {
        let r = self.alloc(Span::default());
        self.scopes.last_mut().unwrap().push((name.to_string(), r));
        r
    }

    fn lookup(&self, name: &str) -> Option<Reg> {
        for scope in self.scopes.iter().rev() {
            for (n, r) in scope.iter().rev() {
                if n == name {
                    return Some(*r);
                }
            }
        }
        None
    }

    fn emit(&mut self, op: Op, span: Span) -> usize {
        self.chunk.code.push(op);
        self.chunk.spans.push(span);
        self.chunk.code.len() - 1
    }

    fn here(&self) -> usize {
        self.chunk.code.len()
    }

    fn patch_jump(&mut self, at: usize) {
        let target = self.here();
        match &mut self.chunk.code[at] {
            Op::Jump(t) | Op::JumpIfFalse(_, t) | Op::JumpIfTrue(_, t) => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }

    fn const_idx(&mut self, v: Value) -> u16 {
        // interning: reuse identical constants
        for (i, c) in self.chunk.consts.iter().enumerate() {
            if *c == v {
                return i as u16;
            }
        }
        self.chunk.consts.push(v);
        (self.chunk.consts.len() - 1) as u16
    }

    fn const_reg(&mut self, v: Value, span: Span) -> Reg {
        let idx = self.const_idx(v);
        let dst = self.alloc(span);
        self.emit(Op::Const(dst, idx), span);
        dst
    }

    // ---------------- blocks & statements ----------------

    /// Compile a block; returns the register of the trailing expression value.
    ///
    /// Register reclamation: each statement's temporaries are freed when it
    /// ends (`next` resets to the statement mark plus any locals it created,
    /// which are always allocated first). Registers are compile-time slots,
    /// so loop bodies reuse the same registers every iteration.
    fn block(&mut self, b: &Block) -> Option<Reg> {
        self.scopes.push(Vec::new());
        let mut last: Option<Reg> = None;
        for stmt in &b.stmts {
            let mark = self.next;
            let (created, val) = self.stmt(stmt);
            last = val;
            self.next = mark + created;
        }
        self.scopes.pop();
        last
    }

    /// Returns (locals created at the statement mark, trailing value register).
    fn stmt(&mut self, stmt: &Stmt) -> (u16, Option<Reg>) {
        if let Stmt::Let { name, init, span, .. } = stmt {
            // local FIRST (at the statement mark, survives the temp reset);
            // the name is only visible after the initializer (shadowing).
            let local = self.alloc(*span);
            let r = self.expr(init);
            self.emit(Op::Move(local, r), *span);
            self.scopes.last_mut().unwrap().push((name.clone(), local));
            return (1, None);
        }
        (0, self.stmt_rest(stmt))
    }

    fn stmt_rest(&mut self, stmt: &Stmt) -> Option<Reg> {
        match stmt {
            Stmt::Let { .. } => unreachable!("handled by stmt()"),
            Stmt::Assign { target, op, value, span } => {
                let rhs = self.expr(value);
                let ExprKind::Ident(name) = &target.kind else {
                    self.err("K0803", "unsupported assignment target on KVM", *span);
                    return None;
                };
                if let Some(local) = self.lookup(name) {
                    match op {
                        AssignOp::Set => {
                            self.emit(Op::Move(local, rhs), *span);
                        }
                        other => {
                            let bin = match other {
                                AssignOp::Add => Op::Add(local, local, rhs),
                                AssignOp::Sub => Op::Sub(local, local, rhs),
                                AssignOp::Mul => Op::Mul(local, local, rhs),
                                AssignOp::Div => Op::Div(local, local, rhs),
                                AssignOp::Set => unreachable!(),
                            };
                            self.emit(bin, *span);
                        }
                    }
                    return None;
                }
                // component state slot
                let slot = self.comp.as_ref().and_then(|c| c.slots.get(name.as_str()).copied());
                if let Some(slot) = slot {
                    match op {
                        AssignOp::Set => {
                            self.emit(Op::StateSet(slot, rhs), *span);
                        }
                        other => {
                            let t = self.alloc(*span);
                            self.emit(Op::StateGet(t, slot), *span);
                            let bin = match other {
                                AssignOp::Add => Op::Add(t, t, rhs),
                                AssignOp::Sub => Op::Sub(t, t, rhs),
                                AssignOp::Mul => Op::Mul(t, t, rhs),
                                AssignOp::Div => Op::Div(t, t, rhs),
                                AssignOp::Set => unreachable!(),
                            };
                            self.emit(bin, *span);
                            self.emit(Op::StateSet(slot, t), *span);
                        }
                    }
                    return None;
                }
                self.err(
                    "K0803",
                    format!("cannot assign to `{name}` on KVM (captured variable)"),
                    *span,
                );
                None
            }
            Stmt::Expr(e) => Some(self.expr(e)),
            Stmt::Return(v, span) => {
                let r = match v {
                    Some(e) => self.expr(e),
                    None => self.const_reg(Value::Unit, *span),
                };
                self.emit(Op::Ret(r), *span);
                None
            }
            Stmt::While { cond, body, span } => {
                let top = self.here();
                let c = self.expr(cond);
                let exit = self.emit(Op::JumpIfFalse(c, 0), *span);
                self.loops.push((top, vec![exit]));
                self.block(body);
                self.emit(Op::Jump(top), *span);
                let (_, breaks) = self.loops.pop().unwrap();
                for b in breaks {
                    self.patch_jump(b);
                }
                None
            }
            Stmt::For { var, iter, body, span } => {
                let it = self.expr(iter);
                let len = self.alloc(*span);
                self.emit(Op::IterLen(len, it), *span);
                let i = self.const_reg(Value::Int(0), *span);
                let one = self.const_reg(Value::Int(1), *span);
                let cond = self.alloc(*span);
                let top = self.here();
                self.emit(Op::Lt(cond, i, len), *span);
                let exit = self.emit(Op::JumpIfFalse(cond, 0), *span);
                self.scopes.push(Vec::new());
                let v = self.bind_local(var);
                self.emit(Op::IterGet { dst: v, iter: it, idx: i }, *span);
                // continue jumps to the increment, which sits after the body
                self.loops.push((usize::MAX, vec![exit])); // placeholder; continue patched below
                let cont_patches_start = self.loop_continue_marker();
                self.block(body);
                let inc_at = self.here();
                self.fix_continues(cont_patches_start, inc_at);
                self.emit(Op::Add(i, i, one), *span);
                self.emit(Op::Jump(top), *span);
                let (_, breaks) = self.loops.pop().unwrap();
                for b in breaks {
                    self.patch_jump(b);
                }
                self.scopes.pop();
                None
            }
            Stmt::Expect(e, span) => {
                let r = self.expr(e);
                let ok = self.emit(Op::JumpIfTrue(r, 0), *span);
                let msg = self.const_idx(Value::str("expectation failed"));
                self.emit(Op::Panic(msg), *span);
                self.patch_jump(ok);
                None
            }
            Stmt::Break(span) => {
                if self.loops.is_empty() {
                    self.err("K0229", "`break` outside of a loop", *span);
                    return None;
                }
                let j = self.emit(Op::Jump(0), *span);
                self.loops.last_mut().unwrap().1.push(j);
                None
            }
            Stmt::Continue(span) => {
                match self.loops.last() {
                    None => {
                        self.err("K0229", "`continue` outside of a loop", *span);
                    }
                    Some((target, _)) => {
                        let target = *target;
                        if target == usize::MAX {
                            // for-loop: patched at the increment position later
                            let j = self.emit(Op::Jump(usize::MAX), *span);
                            self.pending_continues.push(j);
                        } else {
                            self.emit(Op::Jump(target), *span);
                        }
                    }
                }
                None
            }
            Stmt::Emit { port, arg, span } => {
                if self.comp.is_none() {
                    self.err("K0225", "`emit` is only valid inside a component", *span);
                    return None;
                }
                let payload = arg.as_ref().map(|e| self.expr(e));
                let port_idx = self.const_idx(Value::str(port.clone()));
                self.emit(Op::EmitOp { port: port_idx, payload }, *span);
                None
            }
            Stmt::Forall { span, .. } => {
                // `forall` is a property-test construct run by the interpreter
                // (`kupl test`); it is not compiled to the KVM.
                self.err(
                    "K0804",
                    "`forall` runs only under `kupl test` (interpreter); not supported on the KVM",
                    *span,
                );
                None
            }
        }
    }

    fn loop_continue_marker(&self) -> usize {
        self.pending_continues.len()
    }

    fn fix_continues(&mut self, from: usize, target: usize) {
        let fixes: Vec<usize> = self.pending_continues.drain(from..).collect();
        for at in fixes {
            if let Op::Jump(t) = &mut self.chunk.code[at] {
                *t = target;
            }
        }
    }

    // ---------------- expressions ----------------

    fn expr(&mut self, e: &Expr) -> Reg {
        let span = e.span;
        match &e.kind {
            ExprKind::Int(v) => self.const_reg(Value::Int(*v), span),
            ExprKind::Float(v) => self.const_reg(Value::Float(*v), span),
            ExprKind::Bool(v) => self.const_reg(Value::Bool(*v), span),
            ExprKind::Unit => self.const_reg(Value::Unit, span),
            ExprKind::Str(pieces) => {
                if pieces.len() == 1 {
                    if let StrPiece::Text(t) = &pieces[0] {
                        return self.const_reg(Value::str(t.clone()), span);
                    }
                }
                let mut acc = self.const_reg(Value::str(""), span);
                for p in pieces {
                    let part = match p {
                        StrPiece::Text(t) => self.const_reg(Value::str(t.clone()), span),
                        StrPiece::Expr(inner) => {
                            let r = self.expr(inner);
                            let s = self.alloc(span);
                            self.emit(Op::ToStr(s, r), span);
                            s
                        }
                    };
                    let joined = self.alloc(span);
                    self.emit(Op::Concat(joined, acc, part), span);
                    acc = joined;
                }
                acc
            }
            ExprKind::List(items) => {
                let start = self.consecutive(items, span);
                let dst = self.alloc(span);
                self.emit(Op::MakeList { dst, start, len: items.len() as u8 }, span);
                dst
            }
            ExprKind::Range { lo, hi, inclusive } => {
                let l = self.expr(lo);
                let h = self.expr(hi);
                let dst = self.alloc(span);
                self.emit(Op::MakeRange { dst, lo: l, hi: h, inclusive: *inclusive }, span);
                dst
            }
            ExprKind::Ident(name) => {
                if let Some(r) = self.lookup(name) {
                    return r;
                }
                if let Some(ctx) = &self.comp {
                    if let Some(&s) = ctx.slots.get(name.as_str()) {
                        let dst = self.alloc(span);
                        self.emit(Op::StateGet(dst, s), span);
                        return dst;
                    }
                }
                if self.shared.fun_names.contains(name) {
                    return self.const_reg(Value::Fun(std::rc::Rc::new(name.clone())), span);
                }
                if let Some(&idx) = self.shared.ctor_idx.get(name.as_str()) {
                    if self.shared.module.ctors[idx as usize].arity == 0 {
                        let dst = self.alloc(span);
                        self.emit(Op::MakeCtor { dst, ctor: idx, start: 0, len: 0 }, span);
                        return dst;
                    }
                }
                self.err("K0240", format!("unknown name `{name}` (KVM)"), span);
                self.const_reg(Value::Unit, span)
            }
            ExprKind::Call { callee, args } => self.call(callee, args, span),
            ExprKind::MethodCall { recv, name, args } => {
                let r = self.expr(recv);
                let exprs: Vec<Expr> = args.clone();
                let start = self.consecutive(&exprs, span);
                let name_idx = self.const_idx(Value::str(name.clone()));
                let dst = self.alloc(span);
                self.emit(
                    Op::Method { dst, recv: r, name: name_idx, start, argc: args.len() as u8 },
                    span,
                );
                dst
            }
            ExprKind::Field { recv, name } => {
                let r = self.expr(recv);
                let name_idx = self.const_idx(Value::str(name.clone()));
                let dst = self.alloc(span);
                self.emit(Op::GetFieldNamed { dst, obj: r, name: name_idx }, span);
                dst
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    let dst = self.alloc(span);
                    let l = self.expr(lhs);
                    self.emit(Op::Move(dst, l), span);
                    let short = match op {
                        BinOp::And => self.emit(Op::JumpIfFalse(dst, 0), span),
                        _ => self.emit(Op::JumpIfTrue(dst, 0), span),
                    };
                    let r = self.expr(rhs);
                    self.emit(Op::Move(dst, r), span);
                    self.patch_jump(short);
                    return dst;
                }
                let a = self.expr(lhs);
                let b = self.expr(rhs);
                let dst = self.alloc(span);
                let op = match op {
                    BinOp::Add => Op::Add(dst, a, b),
                    BinOp::Sub => Op::Sub(dst, a, b),
                    BinOp::Mul => Op::Mul(dst, a, b),
                    BinOp::Div => Op::Div(dst, a, b),
                    BinOp::Rem => Op::Rem(dst, a, b),
                    BinOp::Eq => Op::Eq(dst, a, b),
                    BinOp::Ne => Op::Ne(dst, a, b),
                    BinOp::Lt => Op::Lt(dst, a, b),
                    BinOp::Le => Op::Le(dst, a, b),
                    BinOp::Gt => Op::Gt(dst, a, b),
                    BinOp::Ge => Op::Ge(dst, a, b),
                    BinOp::And | BinOp::Or => unreachable!(),
                };
                self.emit(op, span);
                dst
            }
            ExprKind::Unary { op, operand } => {
                let r = self.expr(operand);
                let dst = self.alloc(span);
                match op {
                    UnOp::Neg => self.emit(Op::Neg(dst, r), span),
                    UnOp::Not => self.emit(Op::Not(dst, r), span),
                };
                dst
            }
            ExprKind::If { cond, then_block, else_block } => {
                let dst = self.alloc(span);
                let c = self.expr(cond);
                let to_else = self.emit(Op::JumpIfFalse(c, 0), span);
                let t = self.block(then_block);
                let tr = t.unwrap_or_else(|| self.const_reg(Value::Unit, span));
                self.emit(Op::Move(dst, tr), span);
                let to_end = self.emit(Op::Jump(0), span);
                self.patch_jump(to_else);
                match else_block {
                    Some(e) => {
                        let er = self.expr(e);
                        self.emit(Op::Move(dst, er), span);
                    }
                    None => {
                        let u = self.const_reg(Value::Unit, span);
                        self.emit(Op::Move(dst, u), span);
                    }
                }
                self.patch_jump(to_end);
                dst
            }
            ExprKind::BlockExpr(b) => {
                let dst = self.alloc(span);
                let r = self.block(b);
                let r = r.unwrap_or_else(|| self.const_reg(Value::Unit, span));
                self.emit(Op::Move(dst, r), span);
                dst
            }
            ExprKind::Match { scrutinee, arms } => {
                let s = self.expr(scrutinee);
                let dst = self.alloc(span);
                let mut end_jumps = Vec::new();
                for arm in arms {
                    self.scopes.push(Vec::new());
                    let mut fails = Vec::new();
                    self.pattern(&arm.pattern, s, &mut fails);
                    let r = self.expr(&arm.body);
                    self.emit(Op::Move(dst, r), arm.span);
                    end_jumps.push(self.emit(Op::Jump(0), arm.span));
                    for f in fails {
                        self.patch_jump(f);
                    }
                    self.scopes.pop();
                }
                let msg = self.const_idx(Value::str("no match arm matched"));
                self.emit(Op::Panic(msg), span);
                for j in end_jumps {
                    self.patch_jump(j);
                }
                dst
            }
            ExprKind::Lambda { params, body } => {
                // free-variable analysis decides what to capture (by value)
                let mut bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
                let mut free = BTreeSet::new();
                free_vars_block(body, &mut bound, &mut free);
                let captures: Vec<(String, Reg)> = free
                    .into_iter()
                    .filter_map(|n| self.lookup(&n).map(|r| (n, r)))
                    .collect();

                // compile the lambda body as its own chunk
                let proto_idx = {
                    let mut lc = FnCompiler::new(
                        self.shared,
                        &format!("{}::lambda", self.chunk.name),
                        captures.len() as u8,
                        params.len() as u8,
                    );
                    // lambdas inside a component see live state via the
                    // caller's instance context
                    lc.comp = self.comp.clone();
                    for (n, _) in &captures {
                        lc.bind_local(n);
                    }
                    for p in params {
                        lc.bind_local(&p.name);
                    }
                    let last = lc.block(body);
                    let r = last.unwrap_or_else(|| lc.const_reg(Value::Unit, span));
                    lc.emit(Op::Ret(r), span);
                    let chunk = lc.finish();
                    self.shared.module.chunks.push(chunk);
                    (self.shared.module.chunks.len() - 1) as u16
                };

                // copy captured values into consecutive regs
                let start = self.next as Reg;
                for (_, src) in &captures {
                    let r = self.alloc(span);
                    self.emit(Op::Move(r, *src), span);
                }
                let dst = self.alloc(span);
                self.emit(
                    Op::MakeClosure { dst, proto: proto_idx, start, ncaps: captures.len() as u8 },
                    span,
                );
                dst
            }
            ExprKind::With { recv, updates } => {
                let mut cur = self.expr(recv);
                for (field, value) in updates {
                    let v = self.expr(value);
                    let name_idx = self.const_idx(Value::str(field.clone()));
                    let dst = self.alloc(span);
                    self.emit(Op::WithField { dst, obj: cur, name: name_idx, value: v }, span);
                    cur = dst;
                }
                cur
            }
            ExprKind::Try(inner) => {
                let r = self.expr(inner);
                let err_idx = self.shared.ctor_idx["Err"];
                let t = self.alloc(span);
                self.emit(Op::TagIs { dst: t, obj: r, ctor: err_idx }, span);
                let ok = self.emit(Op::JumpIfFalse(t, 0), span);
                self.emit(Op::Ret(r), span);
                self.patch_jump(ok);
                let dst = self.alloc(span);
                self.emit(Op::GetField { dst, obj: r, idx: 0 }, span);
                dst
            }
            ExprKind::Await(inner) => self.expr(inner),
            ExprKind::Par(branches) => {
                // fork-join: evaluate each branch, collect into a list (same
                // deterministic branch order as the interpreter)
                let start = self.consecutive(branches, span);
                let dst = self.alloc(span);
                self.emit(Op::MakeList { dst, start, len: branches.len() as u8 }, span);
                dst
            }
        }
    }

    /// Compile expressions into freshly-allocated CONSECUTIVE registers;
    /// returns the first register.
    fn consecutive(&mut self, exprs: &[Expr], span: Span) -> Reg {
        let temps: Vec<Reg> = exprs.iter().map(|e| self.expr(e)).collect();
        let start = self.next as Reg;
        for t in temps {
            let r = self.alloc(span);
            self.emit(Op::Move(r, t), span);
        }
        start
    }

    fn call(&mut self, callee: &Expr, args: &[Arg], span: Span) -> Reg {
        if let ExprKind::Ident(name) = &callee.kind {
            // builtins
            let builtin = match (name.as_str(), args.len()) {
                ("print", 1) => Some(BUILTIN_PRINT),
                ("to_str", 1) => Some(BUILTIN_TO_STR),
                ("panic", 1) => Some(BUILTIN_PANIC),
                ("Map", 0) => Some(BUILTIN_MAP_NEW),
                ("Set", 0) => Some(BUILTIN_SET_NEW),
                ("Set", 1) => Some(BUILTIN_SET_FROM),
                ("tensor", 1) => Some(BUILTIN_TENSOR),
                ("zeros", 1) => Some(BUILTIN_ZEROS),
                ("arange", 1) => Some(BUILTIN_ARANGE),
                ("read_file", 1) => Some(BUILTIN_READ_FILE),
                ("write_file", 2) => Some(BUILTIN_WRITE_FILE),
                ("append_file", 2) => Some(BUILTIN_APPEND_FILE),
                ("delete_file", 1) => Some(BUILTIN_DELETE_FILE),
                ("file_exists", 1) => Some(BUILTIN_FILE_EXISTS),
                ("json_parse", 1) => Some(BUILTIN_JSON_PARSE),
                ("json_stringify", 1) => Some(BUILTIN_JSON_STRINGIFY),
                ("env_var", 1) => Some(BUILTIN_ENV_VAR),
                ("args", 0) => Some(BUILTIN_ARGS),
                ("eprint", 1) => Some(BUILTIN_EPRINT),
                ("exit", 1) => Some(BUILTIN_EXIT),
                ("random_ints", 2) => Some(BUILTIN_RANDOM_INTS),
                ("random_floats", 2) => Some(BUILTIN_RANDOM_FLOATS),
                ("shuffle", 2) => Some(BUILTIN_SHUFFLE),
                ("http_get", 1) => Some(BUILTIN_HTTP_GET),
                ("http_post", 2) => Some(BUILTIN_HTTP_POST),
                _ => None,
            };
            if let Some(which) = builtin {
                let exprs: Vec<Expr> = args.iter().map(|a| a.value.clone()).collect();
                let start = self.consecutive(&exprs, span);
                let dst = self.alloc(span);
                self.emit(Op::CallBuiltin { dst, which, start, argc: args.len() as u8 }, span);
                return dst;
            }
            // constructors (builtin Some/Ok/Err and user ctors, incl. named args)
            if let Some(&idx) = self.shared.ctor_idx.get(name.as_str()).filter(|_| {
                matches!(name.as_str(), "Some" | "Ok" | "Err")
                    || !self.shared.fun_names.contains(name)
                       && self.lookup(name).is_none()
            }) {
                let meta = self.shared.module.ctors[idx as usize].clone();
                // order named args by ctor field order
                let ordered = self.order_ctor_args(&meta.variant, args, span);
                let start = self.consecutive(&ordered, span);
                let dst = self.alloc(span);
                self.emit(Op::MakeCtor { dst, ctor: idx, start, len: ordered.len() as u8 }, span);
                return dst;
            }
            // component-local function (private or exposed)
            if self.lookup(name).is_none() {
                let comp_fun = self.comp.as_ref().and_then(|c| c.funs.get(name.as_str()).copied());
                if let Some(fun) = comp_fun {
                    let exprs: Vec<Expr> = args.iter().map(|a| a.value.clone()).collect();
                    let start = self.consecutive(&exprs, span);
                    let dst = self.alloc(span);
                    self.emit(Op::CallComp { dst, fun, start, argc: args.len() as u8 }, span);
                    return dst;
                }
            }
            // component construction
            if self.lookup(name).is_none()
                && self.shared.module.component_names.contains_key(name.as_str())
            {
                let comp_name = name.clone();
                return self.instance_expr(&comp_name, args, span, 0);
            }
            // direct call to a top-level fun
            if self.lookup(name).is_none() {
                if let Some(&fun) = self.shared.module.funs.get(name.as_str()) {
                    let exprs: Vec<Expr> = args.iter().map(|a| a.value.clone()).collect();
                    let start = self.consecutive(&exprs, span);
                    let dst = self.alloc(span);
                    self.emit(Op::Call { dst, fun, start, argc: args.len() as u8 }, span);
                    return dst;
                }
            }
        }
        // indirect call
        let f = self.expr(callee);
        let exprs: Vec<Expr> = args.iter().map(|a| a.value.clone()).collect();
        let start = self.consecutive(&exprs, span);
        let dst = self.alloc(span);
        self.emit(Op::CallValue { dst, f, start, argc: args.len() as u8 }, span);
        dst
    }

    /// Reorder constructor args to field order using named-arg info.
    fn order_ctor_args(&mut self, variant: &str, args: &[Arg], span: Span) -> Vec<Expr> {
        // field names come from Checked via ctor order captured at module build;
        // for builtin ctors and positional calls this is the identity.
        let field_names = self.shared.ctor_fields.get(variant).cloned().unwrap_or_default();
        let mut ordered: Vec<Option<Expr>> = vec![None; args.len()];
        for (i, a) in args.iter().enumerate() {
            let idx = match &a.name {
                Some(n) => field_names.iter().position(|f| f == n).unwrap_or(i),
                None => i,
            };
            if idx < ordered.len() {
                ordered[idx] = Some(a.value.clone());
            }
        }
        ordered
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                e.unwrap_or_else(|| {
                    self.err("K0243", format!("missing field {i} for `{variant}`"), span);
                    Expr { kind: ExprKind::Unit, span }
                })
            })
            .collect()
    }

    // ---------------- patterns ----------------

    /// Emit tests for `pat` against register `v`; failures jump (patched by caller).
    fn pattern(&mut self, pat: &Pattern, v: Reg, fails: &mut Vec<usize>) {
        let span = pat.span;
        match &pat.kind {
            PatternKind::Wildcard => {}
            PatternKind::Bind(name) => {
                let local = self.bind_local(name);
                self.emit(Op::Move(local, v), span);
            }
            PatternKind::Int(x) => {
                let c = self.const_reg(Value::Int(*x), span);
                let t = self.alloc(span);
                self.emit(Op::Eq(t, v, c), span);
                fails.push(self.emit(Op::JumpIfFalse(t, 0), span));
            }
            PatternKind::Bool(x) => {
                let c = self.const_reg(Value::Bool(*x), span);
                let t = self.alloc(span);
                self.emit(Op::Eq(t, v, c), span);
                fails.push(self.emit(Op::JumpIfFalse(t, 0), span));
            }
            PatternKind::Str(x) => {
                let c = self.const_reg(Value::str(x.clone()), span);
                let t = self.alloc(span);
                self.emit(Op::Eq(t, v, c), span);
                fails.push(self.emit(Op::JumpIfFalse(t, 0), span));
            }
            PatternKind::Ctor { name, args } => {
                let Some(&idx) = self.shared.ctor_idx.get(name.as_str()) else {
                    self.err("K0254", format!("unknown constructor `{name}` (KVM)"), span);
                    return;
                };
                let t = self.alloc(span);
                self.emit(Op::TagIs { dst: t, obj: v, ctor: idx }, span);
                fails.push(self.emit(Op::JumpIfFalse(t, 0), span));
                for (i, arg) in args.iter().enumerate() {
                    let f = self.alloc(span);
                    self.emit(Op::GetField { dst: f, obj: v, idx: i as u8 }, span);
                    self.pattern(arg, f, fails);
                }
            }
        }
    }
}

// ---------------- free-variable analysis ----------------

fn free_vars_block(b: &Block, bound: &mut HashSet<String>, free: &mut BTreeSet<String>) {
    let added: Vec<String> = Vec::new();
    let mut local_added = added;
    for stmt in &b.stmts {
        free_vars_stmt(stmt, bound, free, &mut local_added);
    }
    for n in local_added {
        bound.remove(&n);
    }
}

fn free_vars_stmt(
    stmt: &Stmt,
    bound: &mut HashSet<String>,
    free: &mut BTreeSet<String>,
    added: &mut Vec<String>,
) {
    match stmt {
        Stmt::Let { name, init, .. } => {
            free_vars_expr(init, bound, free);
            if bound.insert(name.clone()) {
                added.push(name.clone());
            }
        }
        Stmt::Assign { target, value, .. } => {
            free_vars_expr(target, bound, free);
            free_vars_expr(value, bound, free);
        }
        Stmt::Expr(e) | Stmt::Expect(e, _) => free_vars_expr(e, bound, free),
        Stmt::Forall { vars, body, .. } => {
            let mut inner_added = Vec::new();
            for (n, _) in vars {
                if bound.insert(n.clone()) {
                    inner_added.push(n.clone());
                }
            }
            free_vars_block(body, bound, free);
            for n in inner_added {
                bound.remove(&n);
            }
        }
        Stmt::Return(Some(e), _) => free_vars_expr(e, bound, free),
        Stmt::Return(None, _) => {}
        Stmt::While { cond, body, .. } => {
            free_vars_expr(cond, bound, free);
            free_vars_block(body, bound, free);
        }
        Stmt::For { var, iter, body, .. } => {
            free_vars_expr(iter, bound, free);
            let added_var = bound.insert(var.clone());
            free_vars_block(body, bound, free);
            if added_var {
                bound.remove(var);
            }
        }
        Stmt::Emit { arg: Some(e), .. } => free_vars_expr(e, bound, free),
        Stmt::Emit { arg: None, .. } => {}
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn free_vars_expr(e: &Expr, bound: &HashSet<String>, free: &mut BTreeSet<String>) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) {
                free.insert(n.clone());
            }
        }
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(inner) = p {
                    free_vars_expr(inner, bound, free);
                }
            }
        }
        ExprKind::List(items) => {
            for i in items {
                free_vars_expr(i, bound, free);
            }
        }
        ExprKind::Call { callee, args } => {
            free_vars_expr(callee, bound, free);
            for a in args {
                free_vars_expr(&a.value, bound, free);
            }
        }
        ExprKind::MethodCall { recv, args, .. } => {
            free_vars_expr(recv, bound, free);
            for a in args {
                free_vars_expr(a, bound, free);
            }
        }
        ExprKind::Field { recv, .. } => free_vars_expr(recv, bound, free),
        ExprKind::Binary { lhs, rhs, .. } => {
            free_vars_expr(lhs, bound, free);
            free_vars_expr(rhs, bound, free);
        }
        ExprKind::Unary { operand, .. } => free_vars_expr(operand, bound, free),
        ExprKind::If { cond, then_block, else_block } => {
            free_vars_expr(cond, bound, free);
            let mut b = bound.clone();
            free_vars_block(then_block, &mut b, free);
            if let Some(el) = else_block {
                free_vars_expr(el, bound, free);
            }
        }
        ExprKind::BlockExpr(b) => {
            let mut bb = bound.clone();
            free_vars_block(b, &mut bb, free);
        }
        ExprKind::Match { scrutinee, arms } => {
            free_vars_expr(scrutinee, bound, free);
            for arm in arms {
                let mut b = bound.clone();
                bind_pattern_names(&arm.pattern, &mut b);
                free_vars_expr(&arm.body, &b, free);
            }
        }
        ExprKind::Lambda { params, body } => {
            let mut b = bound.clone();
            for p in params {
                b.insert(p.name.clone());
            }
            free_vars_block(body, &mut b, free);
        }
        ExprKind::Range { lo, hi, .. } => {
            free_vars_expr(lo, bound, free);
            free_vars_expr(hi, bound, free);
        }
        ExprKind::With { recv, updates } => {
            free_vars_expr(recv, bound, free);
            for (_, v) in updates {
                free_vars_expr(v, bound, free);
            }
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => free_vars_expr(inner, bound, free),
        ExprKind::Par(branches) => {
            for b in branches {
                free_vars_expr(b, bound, free);
            }
        }
        _ => {}
    }
}

fn bind_pattern_names(p: &Pattern, bound: &mut HashSet<String>) {
    match &p.kind {
        PatternKind::Bind(n) => {
            bound.insert(n.clone());
        }
        PatternKind::Ctor { args, .. } => {
            for a in args {
                bind_pattern_names(a, bound);
            }
        }
        _ => {}
    }
}
