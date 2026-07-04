//! Tree-walking interpreter + single-threaded component runtime.
//!
//! Every component instance is an isolated actor with its own state env and a
//! mailbox; the runtime drains a global FIFO queue deterministically (v0.1 is
//! single-threaded — the semantics are what the future KVM scheduler must match).

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::ast::*;
use crate::check::Checked;
use crate::diag::Span;
use crate::value::{Closure, Env, Value};

/// Non-local control flow during evaluation.
pub enum Flow {
    Panic { msg: String, span: Span },
    Return(Value),
    Break,
    Continue,
}

pub type EvalResult = Result<Value, Flow>;

/// Owned, indexed view of the checked program.
pub struct ProgramDb {
    pub funs: HashMap<String, Rc<FunDecl>>,
    pub components: HashMap<String, Rc<ComponentDecl>>,
    pub contracts: HashMap<String, Rc<ContractDecl>>,
    /// variant name -> (type name, field names)
    pub ctors: HashMap<String, (String, Vec<String>)>,
    /// `ai fun` runtime signatures (from the checker).
    pub ai_funs: HashMap<String, Rc<crate::ai::AiFunMeta>>,
    /// type name -> variants (for `forall` value generation).
    pub type_variants: crate::prop::TypeDb,
}

impl ProgramDb {
    pub fn build(program: &Program, checked: &Checked) -> ProgramDb {
        let mut funs = HashMap::new();
        let mut components = HashMap::new();
        let mut contracts = HashMap::new();
        for item in &program.items {
            match item {
                Item::Fun(f) => {
                    funs.insert(f.name.clone(), Rc::new(f.clone()));
                }
                Item::Component(c) => {
                    components.insert(c.name.clone(), Rc::new(c.clone()));
                }
                Item::Contract(ct) => {
                    contracts.insert(ct.name.clone(), Rc::new(ct.clone()));
                }
                Item::Type(_) | Item::Law(_) => {}
            }
        }
        let ctors = checked
            .ctors
            .iter()
            .map(|(name, (ty, fields))| {
                (name.clone(), (ty.clone(), fields.iter().map(|(n, _)| n.clone()).collect()))
            })
            .collect();
        let ai_funs = checked
            .ai_funs
            .iter()
            .map(|(name, meta)| (name.clone(), Rc::new(meta.clone())))
            .collect();
        let mut type_variants = crate::prop::TypeDb::new();
        for item in &program.items {
            if let Item::Type(t) = item {
                let variants = t
                    .variants
                    .iter()
                    .map(|v| {
                        let fields =
                            v.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
                        (v.name.clone(), fields)
                    })
                    .collect();
                type_variants.insert(t.name.clone(), variants);
            }
        }
        ProgramDb { funs, components, contracts, ctors, ai_funs, type_variants }
    }
}

/// A live timer on an instance: which handler it fires, whether it recurs, its
/// interval, and its next virtual-time firing.
pub struct TimerState {
    pub handler_idx: usize,
    pub every: bool,
    pub interval: i64,
    pub next_fire: i64,
    pub active: bool,
}

pub struct Instance {
    pub comp: Rc<ComponentDecl>,
    /// Props + state (+ children) — the instance's private heap.
    pub env: Env,
    /// out port -> [(target instance, target in port)]
    pub wires: HashMap<String, Vec<(usize, String)>>,
    pub last_emit: HashMap<String, Value>,
    /// Set by the parent's `supervise child restart on_failure`.
    pub restart_on_failure: bool,
    /// Armed `on every`/`on after` timers.
    pub timers: Vec<TimerState>,
}

pub struct Interp {
    pub db: ProgramDb,
    pub instances: Vec<Instance>,
    pub queue: VecDeque<(usize, String, Value)>,
    /// Instance currently executing a handler (target of `emit`).
    pub current: Option<usize>,
    /// Print unwired emissions (used by `kupl run` for observable output).
    pub print_unwired: bool,
    pub globals: Env,
    /// The virtual clock (milliseconds). Advanced explicitly — never wall-clock,
    /// so timer-driven behavior is deterministic and reproducible.
    pub now: i64,
}

impl Interp {
    pub fn new(db: ProgramDb) -> Interp {
        Interp {
            db,
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
        }
    }

    fn panic_flow(msg: impl Into<String>, span: Span) -> Flow {
        Flow::Panic { msg: msg.into(), span }
    }

    // ---------------- component runtime ----------------

    /// Create an instance of `comp_name`; args are already-evaluated prop values.
    pub fn instantiate(
        &mut self,
        comp_name: &str,
        args: &[(Option<String>, Value)],
        span: Span,
    ) -> EvalResult {
        let Some(comp) = self.db.components.get(comp_name).cloned() else {
            return Err(Self::panic_flow(format!("unknown component `{comp_name}`"), span));
        };
        let env = self.globals.child();

        // props: by name or position, else default
        for (i, prop) in comp.props.iter().enumerate() {
            let supplied = args
                .iter()
                .enumerate()
                .find(|(j, (name, _))| match name {
                    Some(n) => n == &prop.name,
                    None => *j == i,
                })
                .map(|(_, (_, v))| v.clone());
            let value = match (supplied, &prop.default) {
                (Some(v), _) => v,
                (None, Some(d)) => self.eval(d, &env)?,
                (None, None) => {
                    return Err(Self::panic_flow(
                        format!("missing required prop `{}` for `{comp_name}`", prop.name),
                        span,
                    ))
                }
            };
            env.define(&prop.name, value);
        }

        // state
        for s in &comp.state {
            let v = self.eval(&s.init, &env)?;
            env.define(&s.name, v);
        }

        let id = self.instances.len();
        self.instances.push(Instance {
            comp: comp.clone(),
            env: env.clone(),
            wires: HashMap::new(),
            last_emit: HashMap::new(),
            restart_on_failure: false,
            timers: Vec::new(),
        });

        // children (constructed after the parent exists, in declaration order)
        let mut child_ids: HashMap<String, usize> = HashMap::new();
        for child in &comp.children {
            let mut child_args = Vec::new();
            for a in &child.args {
                let v = self.eval(&a.value, &env)?;
                child_args.push((a.name.clone(), v));
            }
            let v = self.instantiate(&child.component, &child_args, child.span)?;
            if let Value::Component(cid) = v {
                child_ids.insert(child.name.clone(), cid);
                let supervised = comp
                    .supervises
                    .iter()
                    .any(|s| s.child == child.name && s.policy == SupervisePolicy::RestartOnFailure);
                if supervised {
                    self.instances[cid].restart_on_failure = true;
                }
            }
            env.define(&child.name, v);
        }

        // wires: registered on the source child instance
        for wire in &comp.wires {
            let (from_child, from_port) = &wire.from;
            let (to_child, to_port) = &wire.to;
            let (Some(&src), Some(&dst)) = (child_ids.get(from_child), child_ids.get(to_child)) else {
                return Err(Self::panic_flow("wire references unknown child", wire.span));
            };
            self.instances[src]
                .wires
                .entry(from_port.clone())
                .or_default()
                .push((dst, to_port.clone()));
        }

        Ok(Value::Component(id))
    }

    /// Deliver `on start` to instance `id` and all its descendants (creation order).
    pub fn start_all(&mut self) -> Result<(), Flow> {
        for id in 0..self.instances.len() {
            self.run_lifecycle(id, &Trigger::Start)?;
            self.arm_timers(id);
        }
        self.drain()?;
        Ok(())
    }

    /// Arm the instance's timers relative to the current virtual time.
    fn arm_timers(&mut self, id: usize) {
        let comp = self.instances[id].comp.clone();
        let now = self.now;
        let mut timers = Vec::new();
        for (i, h) in comp.handlers.iter().enumerate() {
            let (every, interval) = match &h.trigger {
                Trigger::Every(ms) => (true, *ms),
                Trigger::After(ms) => (false, *ms),
                _ => continue,
            };
            timers.push(TimerState {
                handler_idx: i,
                every,
                interval,
                next_fire: now + interval,
                active: true,
            });
        }
        self.instances[id].timers = timers;
    }

    /// Advance the virtual clock by `dur` ms, firing every due timer in time
    /// order (ties broken by instance then declaration order — deterministic).
    /// Recurring timers reschedule; one-shots deactivate.
    pub fn advance(&mut self, dur: i64) -> Result<(), Flow> {
        if dur < 0 {
            return Err(Self::panic_flow("cannot advance the clock by a negative duration", Span::default()));
        }
        let target = self.now + dur;
        loop {
            // earliest active timer with next_fire <= target
            let mut best: Option<(i64, usize, usize)> = None;
            for (iid, inst) in self.instances.iter().enumerate() {
                for (ti, t) in inst.timers.iter().enumerate() {
                    if t.active && t.next_fire <= target {
                        let cand = (t.next_fire, iid, ti);
                        if best.map_or(true, |b| cand < b) {
                            best = Some(cand);
                        }
                    }
                }
            }
            let Some((fire_time, iid, ti)) = best else { break };
            self.now = fire_time;
            let handler_idx = self.instances[iid].timers[ti].handler_idx;
            let comp = self.instances[iid].comp.clone();
            let h = comp.handlers[handler_idx].clone();
            match self.run_handler(iid, &h, Value::Unit) {
                Ok(()) => {}
                Err(Flow::Panic { msg, .. }) if self.instances[iid].restart_on_failure => {
                    self.restart(iid, &msg)?;
                }
                Err(other) => return Err(other),
            }
            self.drain()?;
            let t = &mut self.instances[iid].timers[ti];
            if t.every {
                t.next_fire += t.interval;
            } else {
                t.active = false;
            }
        }
        self.now = target;
        Ok(())
    }

    /// For `kupl run`: fire up to `max_fires` timer events by advancing the
    /// clock to each next firing — bounds recurring timers so an app produces
    /// finite, deterministic output.
    pub fn run_timers(&mut self, max_fires: usize) -> Result<(), Flow> {
        for _ in 0..max_fires {
            let mut best: Option<(i64, usize, usize)> = None;
            for (iid, inst) in self.instances.iter().enumerate() {
                for (ti, t) in inst.timers.iter().enumerate() {
                    if t.active {
                        let cand = (t.next_fire, iid, ti);
                        if best.map_or(true, |b| cand < b) {
                            best = Some(cand);
                        }
                    }
                }
            }
            let Some((fire_time, _, _)) = best else { break };
            self.advance(fire_time - self.now)?;
        }
        Ok(())
    }

    fn run_lifecycle(&mut self, id: usize, trigger: &Trigger) -> Result<(), Flow> {
        let comp = self.instances[id].comp.clone();
        let want_start = matches!(trigger, Trigger::Start);
        for h in &comp.handlers {
            let matches = matches!(
                (&h.trigger, want_start),
                (Trigger::Start, true) | (Trigger::Stop, false)
            );
            if matches {
                self.run_handler(id, h, Value::Unit)?;
            }
        }
        Ok(())
    }

    /// Queue a message and process until the queue is empty.
    pub fn send(&mut self, id: usize, port: &str, value: Value) -> Result<(), Flow> {
        self.queue.push_back((id, port.to_string(), value));
        self.drain()
    }

    fn drain(&mut self) -> Result<(), Flow> {
        while let Some((id, port, value)) = self.queue.pop_front() {
            let comp = self.instances[id].comp.clone();
            for h in &comp.handlers {
                if matches!(&h.trigger, Trigger::Port(p) if p == &port) {
                    match self.run_handler(id, h, value.clone()) {
                        Ok(()) => {}
                        Err(Flow::Panic { msg, .. }) if self.instances[id].restart_on_failure => {
                            self.restart(id, &msg)?;
                        }
                        Err(other) => return Err(other),
                    }
                }
            }
        }
        Ok(())
    }

    /// Supervision restart: reset state fields to their initial values, keep
    /// props/children/wires, re-run `on start`.
    fn restart(&mut self, id: usize, panic_msg: &str) -> Result<(), Flow> {
        let comp = self.instances[id].comp.clone();
        let env = self.instances[id].env.clone();
        eprintln!("[supervise] {} restarted after panic: {panic_msg}", comp.name);
        for s in &comp.state {
            let v = self.eval(&s.init, &env)?;
            env.define(&s.name, v);
        }
        for h in &comp.handlers {
            if matches!(h.trigger, Trigger::Start) {
                self.run_handler(id, h, Value::Unit)?;
            }
        }
        self.arm_timers(id);
        Ok(())
    }

    fn run_handler(&mut self, id: usize, h: &Handler, payload: Value) -> Result<(), Flow> {
        let env = self.instances[id].env.child();
        if let Some(param) = &h.param {
            env.define(param, payload);
        }
        let saved = self.current.replace(id);
        let result = self.exec_block(&h.body, &env);
        self.current = saved;
        match result {
            Ok(_) => Ok(()),
            Err(Flow::Return(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    fn emit(&mut self, port: &str, value: Value, span: Span) -> Result<(), Flow> {
        let Some(id) = self.current else {
            return Err(Self::panic_flow("`emit` outside of a component handler", span));
        };
        self.instances[id].last_emit.insert(port.to_string(), value.clone());
        let targets = self.instances[id].wires.get(port).cloned().unwrap_or_default();
        if targets.is_empty() {
            if self.print_unwired {
                let comp = self.instances[id].comp.name.clone();
                println!("{comp}.{port} = {value}");
            }
        } else {
            for (dst, dport) in targets {
                self.queue.push_back((dst, dport, value.clone()));
            }
        }
        Ok(())
    }

    // ---------------- statements ----------------

    /// Execute a single statement against a live environment (REPL entry point).
    pub fn exec_stmt_public(&mut self, stmt: &Stmt, env: &Env) -> EvalResult {
        self.exec_stmt(stmt, env)
    }

    pub fn exec_block(&mut self, block: &Block, env: &Env) -> EvalResult {
        let scope = env.child();
        let mut last = Value::Unit;
        for stmt in &block.stmts {
            last = self.exec_stmt(stmt, &scope)?;
        }
        Ok(last)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &Env) -> EvalResult {
        match stmt {
            Stmt::Let { name, init, .. } => {
                let v = self.eval(init, env)?;
                env.define(name, v);
                Ok(Value::Unit)
            }
            Stmt::Assign { target, op, value, span } => {
                let rhs = self.eval(value, env)?;
                let ExprKind::Ident(name) = &target.kind else {
                    return Err(Self::panic_flow("unsupported assignment target", *span));
                };
                let new_value = if *op == AssignOp::Set {
                    rhs
                } else {
                    let old = env.get(name).ok_or_else(|| {
                        Self::panic_flow(format!("unknown variable `{name}`"), *span)
                    })?;
                    let bin = match op {
                        AssignOp::Add => BinOp::Add,
                        AssignOp::Sub => BinOp::Sub,
                        AssignOp::Mul => BinOp::Mul,
                        AssignOp::Div => BinOp::Div,
                        AssignOp::Set => unreachable!(),
                    };
                    binary_op(bin, old, rhs, *span)?
                };
                if !env.set(name, new_value) {
                    return Err(Self::panic_flow(format!("unknown variable `{name}`"), *span));
                }
                Ok(Value::Unit)
            }
            Stmt::Expr(e) => self.eval(e, env),
            Stmt::Return(v, _) => {
                let value = match v {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                Err(Flow::Return(value))
            }
            Stmt::While { cond, body, span } => {
                loop {
                    let c = self.eval(cond, env)?;
                    let Value::Bool(b) = c else {
                        return Err(Self::panic_flow("`while` condition must be Bool", *span));
                    };
                    if !b {
                        break;
                    }
                    match self.exec_block(body, env) {
                        Ok(_) => {}
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Unit)
            }
            Stmt::For { var, iter, body, span } => {
                let it = self.eval(iter, env)?;
                let items: Vec<Value> = match it {
                    Value::Range(lo, hi, incl) => {
                        let hi = if incl { hi + 1 } else { hi };
                        (lo..hi).map(Value::Int).collect()
                    }
                    Value::List(items) => items.as_ref().clone(),
                    other => {
                        return Err(Self::panic_flow(
                            format!("`for` needs a Range or List, found {}", other.type_name()),
                            *span,
                        ))
                    }
                };
                for item in items {
                    let scope = env.child();
                    scope.define(var, item);
                    match self.exec_block(body, &scope) {
                        Ok(_) => {}
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Unit)
            }
            Stmt::Emit { port, arg, span } => {
                let value = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                self.emit(port, value, *span)?;
                Ok(Value::Unit)
            }
            Stmt::Expect(expr, span) => {
                let v = self.eval(expr, env)?;
                if v != Value::Bool(true) {
                    return Err(Flow::Panic { msg: "expectation failed".into(), span: *span });
                }
                Ok(Value::Unit)
            }
            Stmt::Forall { vars, body, span } => self.run_forall(vars, body, *span, env),
            Stmt::Break(_) => Err(Flow::Break),
            Stmt::Continue(_) => Err(Flow::Continue),
        }
    }

    /// Run a `forall` property: generate `CASES` deterministic bindings, run the
    /// body for each, and on the first failure shrink to a minimal counterexample
    /// and panic with a descriptive message. `expect`-failures and any panic in
    /// the body both count as a falsifying case.
    fn run_forall(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        span: Span,
        env: &Env,
    ) -> EvalResult {
        let types = self.db.type_variants.clone();
        let mut rng = crate::prop::Rng::new(crate::prop::SEED);
        for _ in 0..crate::prop::CASES {
            let mut vals = Vec::with_capacity(vars.len());
            for (_, ty) in vars {
                match crate::prop::generate(ty, &mut rng, &types, 0) {
                    Ok(v) => vals.push(v),
                    Err(e) => return Err(Self::panic_flow(e, span)),
                }
            }
            // if this case fails, shrink and report
            if self.forall_case(vars, body, &vals, env)?.is_some() {
                let vals = self.shrink_forall(vars, body, vals, env);
                let msg = self.forall_case(vars, body, &vals, env)?.unwrap_or_default();
                let binding: Vec<String> = vars
                    .iter()
                    .zip(&vals)
                    .map(|((n, _), v)| format!("{n} = {}", crate::prop::render(v)))
                    .collect();
                let detail = if msg == "expectation failed" || msg.is_empty() {
                    String::new()
                } else {
                    format!(" (panic: {msg})")
                };
                return Err(Self::panic_flow(
                    format!("property failed for {}{}", binding.join(", "), detail),
                    span,
                ));
            }
        }
        Ok(Value::Unit)
    }

    /// Run the body with one binding. `Ok(None)` = passed, `Ok(Some(msg))` =
    /// failed (msg is the panic message), `Err(flow)` = unexpected control flow.
    fn forall_case(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        vals: &[Value],
        env: &Env,
    ) -> Result<Option<String>, Flow> {
        let scope = env.child();
        for ((name, _), v) in vars.iter().zip(vals) {
            scope.define(name, v.clone());
        }
        match self.exec_block(body, &scope) {
            Ok(_) => Ok(None),
            Err(Flow::Panic { msg, .. }) => Ok(Some(msg)),
            Err(other) => Err(other),
        }
    }

    /// Greedily shrink a failing binding toward a minimal counterexample: for
    /// each position, try candidate smaller values; keep any that still fails.
    fn shrink_forall(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        mut vals: Vec<Value>,
        env: &Env,
    ) -> Vec<Value> {
        let mut budget = 1000usize;
        loop {
            let mut improved = false;
            for i in 0..vals.len() {
                for cand in crate::prop::shrink(&vals[i]) {
                    if budget == 0 {
                        return vals;
                    }
                    budget -= 1;
                    let mut trial = vals.clone();
                    trial[i] = cand;
                    // a candidate that itself triggers unexpected flow is skipped
                    if matches!(self.forall_case(vars, body, &trial, env), Ok(Some(_))) {
                        vals = trial;
                        improved = true;
                        break;
                    }
                }
                if improved {
                    break;
                }
            }
            if !improved {
                return vals;
            }
        }
    }

    // ---------------- expressions ----------------

    pub fn eval(&mut self, expr: &Expr, env: &Env) -> EvalResult {
        match &expr.kind {
            ExprKind::Int(v) => Ok(Value::Int(*v)),
            ExprKind::Float(v) => Ok(Value::Float(*v)),
            ExprKind::Bool(v) => Ok(Value::Bool(*v)),
            ExprKind::Unit => Ok(Value::Unit),
            ExprKind::Str(pieces) => {
                let mut out = String::new();
                for p in pieces {
                    match p {
                        StrPiece::Text(t) => out.push_str(t),
                        StrPiece::Expr(e) => {
                            let v = self.eval(e, env)?;
                            out.push_str(&v.to_string());
                        }
                    }
                }
                Ok(Value::str(out))
            }
            ExprKind::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for item in items {
                    vs.push(self.eval(item, env)?);
                }
                Ok(Value::List(Rc::new(vs)))
            }
            ExprKind::Range { lo, hi, inclusive } => {
                let l = self.eval(lo, env)?;
                let h = self.eval(hi, env)?;
                match (l, h) {
                    (Value::Int(a), Value::Int(b)) => Ok(Value::Range(a, b, *inclusive)),
                    _ => Err(Self::panic_flow("range bounds must be Int", expr.span)),
                }
            }
            ExprKind::Ident(name) => self.eval_ident(name, expr.span, env),
            ExprKind::Call { callee, args } => self.eval_call(callee, args, expr.span, env),
            ExprKind::MethodCall { recv, name, args } => {
                let r = self.eval(recv, env)?;
                let mut avs = Vec::with_capacity(args.len());
                for a in args {
                    avs.push(self.eval(a, env)?);
                }
                self.eval_method(r, name, avs, expr.span)
            }
            ExprKind::Field { recv, name } => {
                let r = self.eval(recv, env)?;
                match r {
                    Value::Ctor { ty, variant, fields } => {
                        let field_names = self
                            .db
                            .ctors
                            .get(variant.as_str())
                            .map(|(_, names)| names.clone())
                            .unwrap_or_default();
                        match field_names.iter().position(|f| f == name) {
                            Some(i) => Ok(fields[i].clone()),
                            None => Err(Self::panic_flow(
                                format!("`{ty}` value has no field `{name}`"),
                                expr.span,
                            )),
                        }
                    }
                    other => Err(Self::panic_flow(
                        format!("{} has no fields", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // short-circuit logic first
                if matches!(op, BinOp::And | BinOp::Or) {
                    let l = self.eval(lhs, env)?;
                    let Value::Bool(lb) = l else {
                        return Err(Self::panic_flow("logical operand must be Bool", lhs.span));
                    };
                    if (*op == BinOp::And && !lb) || (*op == BinOp::Or && lb) {
                        return Ok(Value::Bool(lb));
                    }
                    let r = self.eval(rhs, env)?;
                    let Value::Bool(rb) = r else {
                        return Err(Self::panic_flow("logical operand must be Bool", rhs.span));
                    };
                    return Ok(Value::Bool(rb));
                }
                let l = self.eval(lhs, env)?;
                let r = self.eval(rhs, env)?;
                binary_op(*op, l, r, expr.span)
            }
            ExprKind::Unary { op, operand } => {
                let v = self.eval(operand, env)?;
                match (op, v) {
                    (UnOp::Neg, Value::Int(i)) => i
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| Self::panic_flow("integer overflow in negation", expr.span)),
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (_, other) => Err(Self::panic_flow(
                        format!("invalid operand type {}", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::If { cond, then_block, else_block } => {
                let c = self.eval(cond, env)?;
                let Value::Bool(b) = c else {
                    return Err(Self::panic_flow("`if` condition must be Bool", cond.span));
                };
                if b {
                    self.exec_block(then_block, env)
                } else {
                    match else_block {
                        Some(e) => self.eval(e, env),
                        None => Ok(Value::Unit),
                    }
                }
            }
            ExprKind::BlockExpr(b) => self.exec_block(b, env),
            ExprKind::Match { scrutinee, arms } => {
                let v = self.eval(scrutinee, env)?;
                for arm in arms {
                    let scope = env.child();
                    if match_pattern(&arm.pattern, &v, &scope) {
                        return self.eval(&arm.body, &scope);
                    }
                }
                Err(Self::panic_flow(
                    format!("no match arm matched value `{v}`"),
                    expr.span,
                ))
            }
            ExprKind::Lambda { params, body } => Ok(Value::Closure(Rc::new(Closure {
                params: params.iter().map(|p| p.name.clone()).collect(),
                body: Rc::new(body.clone()),
                env: env.clone(),
            }))),
            ExprKind::With { recv, updates } => {
                let base = self.eval(recv, env)?;
                let Value::Ctor { ty, variant, fields } = base else {
                    return Err(Self::panic_flow(
                        format!("{} has no fields to update", base.type_name()),
                        expr.span,
                    ));
                };
                let names = self
                    .db
                    .ctors
                    .get(variant.as_str())
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default();
                let mut new_fields = fields.as_ref().clone();
                for (field, value) in updates {
                    let v = self.eval(value, env)?;
                    match names.iter().position(|f| f == field) {
                        Some(i) => new_fields[i] = v,
                        None => {
                            return Err(Self::panic_flow(
                                format!("`{ty}` has no field `{field}`"),
                                expr.span,
                            ))
                        }
                    }
                }
                Ok(Value::Ctor { ty, variant, fields: Rc::new(new_fields) })
            }
            ExprKind::Try(inner) => {
                let v = self.eval(inner, env)?;
                match &v {
                    Value::Ctor { variant, fields, .. } if variant.as_str() == "Ok" => {
                        Ok(fields.first().cloned().unwrap_or(Value::Unit))
                    }
                    Value::Ctor { variant, .. } if variant.as_str() == "Err" => {
                        Err(Flow::Return(v))
                    }
                    other => Err(Self::panic_flow(
                        format!("`?` needs a Result, found {}", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::Await(inner) => self.eval(inner, env),
            ExprKind::Par(branches) => {
                // Fork-join: evaluate each independent branch and collect the
                // results into a list. Branches share only the (immutable)
                // enclosing scope, so evaluation order does not affect results;
                // v1.0-alpha runs them in deterministic branch order (a
                // multi-threaded scheduler is a later, semantics-preserving step).
                let mut results = Vec::with_capacity(branches.len());
                for b in branches {
                    results.push(self.eval(b, env)?);
                }
                Ok(Value::List(Rc::new(results)))
            }
        }
    }

    fn eval_ident(&mut self, name: &str, span: Span, env: &Env) -> EvalResult {
        if let Some(v) = env.get(name) {
            return Ok(v);
        }
        if self.db.funs.contains_key(name) {
            return Ok(Value::Fun(Rc::new(name.to_string())));
        }
        if name == "None" {
            return Ok(Value::none());
        }
        // component-local function referenced as a value
        if let Some(id) = self.current {
            let comp = self.instances[id].comp.clone();
            if comp.funs.iter().chain(comp.exposes.iter()).any(|f| f.name == name) {
                return Ok(Value::Bound(id, Rc::new(name.to_string())));
            }
        }
        if let Some((tyname, fields)) = self.db.ctors.get(name).cloned() {
            if fields.is_empty() {
                return Ok(Value::Ctor {
                    ty: Rc::new(tyname),
                    variant: Rc::new(name.to_string()),
                    fields: Rc::new(vec![]),
                });
            }
        }
        Err(Self::panic_flow(format!("unknown name `{name}`"), span))
    }

    fn eval_call(&mut self, callee: &Expr, args: &[Arg], span: Span, env: &Env) -> EvalResult {
        if let ExprKind::Ident(name) = &callee.kind {
            match (name.as_str(), args.len()) {
                ("print", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    println!("{v}");
                    return Ok(Value::Unit);
                }
                ("to_str", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::str(v.to_string()));
                }
                ("panic", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Err(Self::panic_flow(v.to_string(), span));
                }
                ("Map", 0) => return Ok(Value::Map(Rc::new(Vec::new()))),
                ("Set", 0) => return Ok(Value::Set(Rc::new(Vec::new()))),
                ("Set", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return set_from_list(&v).map_err(|m| Self::panic_flow(m, span));
                }
                ("tensor", 1) | ("zeros", 1) | ("arange", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return tensor_builtin(name, &v).map_err(|m| Self::panic_flow(m, span));
                }
                ("read_file", 1) | ("write_file", 2) | ("append_file", 2)
                | ("delete_file", 1) | ("file_exists", 1) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return fs_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("json_parse", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    let s = match &v {
                        Value::Str(s) => s.as_str().to_string(),
                        other => other.to_string(),
                    };
                    return Ok(match crate::json::parse(&s) {
                        Ok(j) => Value::ok(j),
                        Err(e) => Value::err(Value::str(e)),
                    });
                }
                ("json_stringify", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return crate::json::stringify(&v)
                        .map(Value::str)
                        .map_err(|m| Self::panic_flow(m, span));
                }
                ("env_var", 1) | ("eprint", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return proc_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("args", 0) => return proc_builtin(name, &[]).map_err(|m| Self::panic_flow(m, span)),
                ("random_ints", 2) | ("random_floats", 2) | ("shuffle", 2) => {
                    let mut vals = Vec::with_capacity(2);
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return random_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("http_get", 1) | ("http_post", 2) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return http_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("re_match", 2) | ("re_find", 2) | ("re_find_all", 2) | ("re_replace", 3) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return regex_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("format_time", 1) | ("year_of", 1) | ("month_of", 1) | ("day_of", 1)
                | ("hour_of", 1) | ("minute_of", 1) | ("second_of", 1) | ("weekday_of", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return time_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("now", 0) => return Ok(Value::Int(now_seconds())),
                ("base64_encode", 1) | ("base64_decode", 1) | ("hex_encode", 1)
                | ("hex_decode", 1) | ("hash_fnv", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return encoding_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("csv_parse", 1) | ("csv_stringify", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return csv_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("exit", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    let code = match v {
                        Value::Int(n) => n as i32,
                        _ => 0,
                    };
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    std::process::exit(code);
                }
                ("Some", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::some(v));
                }
                ("Ok", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::ok(v));
                }
                ("Err", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::err(v));
                }
                _ => {}
            }
            // component-local function (private or exposed) with live state
            if let Some(id) = self.current {
                if env.get(name).is_none() {
                    let comp = self.instances[id].comp.clone();
                    if let Some(decl) = comp
                        .funs
                        .iter()
                        .chain(comp.exposes.iter())
                        .find(|f| f.name == *name)
                    {
                        let decl = decl.clone();
                        let mut avs = Vec::with_capacity(args.len());
                        for a in args {
                            avs.push(self.eval(&a.value, env)?);
                        }
                        let base = self.instances[id].env.clone();
                        return self.call_fun(&decl, avs, &base, span);
                    }
                }
            }
            // user constructor
            if let Some((tyname, field_names)) = self.db.ctors.get(name).cloned() {
                let mut fields = vec![Value::Unit; field_names.len()];
                for (i, a) in args.iter().enumerate() {
                    let v = self.eval(&a.value, env)?;
                    let idx = match &a.name {
                        Some(n) => field_names.iter().position(|f| f == n).ok_or_else(|| {
                            Self::panic_flow(format!("`{name}` has no field `{n}`"), a.value.span)
                        })?,
                        None => i,
                    };
                    if idx < fields.len() {
                        fields[idx] = v;
                    }
                }
                return Ok(Value::Ctor {
                    ty: Rc::new(tyname),
                    variant: Rc::new(name.to_string()),
                    fields: Rc::new(fields),
                });
            }
            // component construction
            if self.db.components.contains_key(name) {
                let comp_name = name.clone();
                let mut avs = Vec::new();
                for a in args {
                    let v = self.eval(&a.value, env)?;
                    avs.push((a.name.clone(), v));
                }
                return self.instantiate(&comp_name, &avs, span);
            }
        }
        // general call
        let f = self.eval(callee, env)?;
        let mut avs = Vec::with_capacity(args.len());
        for a in args {
            avs.push(self.eval(&a.value, env)?);
        }
        self.call_value(f, avs, span)
    }

    pub fn call_value(&mut self, f: Value, args: Vec<Value>, span: Span) -> EvalResult {
        match f {
            Value::Bound(id, name) => self.eval_method(Value::Component(id), &name, args, span),
            Value::Fun(name) => {
                let Some(decl) = self.db.funs.get(name.as_str()).cloned() else {
                    return Err(Self::panic_flow(format!("unknown function `{name}`"), span));
                };
                self.call_fun(&decl, args, &self.globals.clone(), span)
            }
            Value::Closure(c) => {
                if c.params.len() != args.len() {
                    return Err(Self::panic_flow(
                        format!("closure takes {} argument(s), {} given", c.params.len(), args.len()),
                        span,
                    ));
                }
                let scope = c.env.child();
                for (p, a) in c.params.iter().zip(args) {
                    scope.define(p, a);
                }
                match self.exec_block(&c.body, &scope) {
                    Err(Flow::Return(v)) => Ok(v),
                    other => other,
                }
            }
            other => Err(Self::panic_flow(
                format!("{} is not callable", other.type_name()),
                span,
            )),
        }
    }

    fn call_fun(&mut self, decl: &FunDecl, args: Vec<Value>, base_env: &Env, span: Span) -> EvalResult {
        if decl.params.len() != args.len() {
            return Err(Self::panic_flow(
                format!(
                    "`{}` takes {} argument(s), {} given",
                    decl.name,
                    decl.params.len(),
                    args.len()
                ),
                span,
            ));
        }
        if let Some(ai) = &decl.ai {
            let Some(meta) = self.db.ai_funs.get(&decl.name).cloned() else {
                return Err(Self::panic_flow(
                    format!("ai fun `{}` has no runtime signature", decl.name),
                    span,
                ));
            };
            // resolve the interpolated intent in a scope holding the arguments
            let scope = base_env.child();
            for (p, a) in decl.params.iter().zip(&args) {
                scope.define(&p.name, a.clone());
            }
            let intent = self.eval(&ai.intent_expr, &scope)?.to_string();
            return crate::ai::ai_call(&meta, &intent, &args, self)
                .map_err(|m| Self::panic_flow(m, span));
        }
        let scope = base_env.child();
        for (p, a) in decl.params.iter().zip(args) {
            scope.define(&p.name, a);
        }
        match self.exec_block(&decl.body, &scope) {
            Err(Flow::Return(v)) => Ok(v),
            other => other,
        }
    }

    fn eval_method(&mut self, recv: Value, name: &str, args: Vec<Value>, span: Span) -> EvalResult {
        // component expose call
        if let Value::Component(id) = recv {
            let comp = self.instances[id].comp.clone();
            let Some(decl) = comp.exposes.iter().chain(comp.funs.iter()).find(|f| f.name == name) else {
                return Err(Self::panic_flow(
                    format!("component `{}` does not expose `{name}`", comp.name),
                    span,
                ));
            };
            let instance_env = self.instances[id].env.clone();
            let saved = self.current.replace(id);
            let result = self.call_fun(&decl.clone(), args, &instance_env, span);
            self.current = saved;
            return result;
        }
        builtin_method(recv, name, args, span, self)
    }
}

impl crate::ai::ToolHost for Interp {
    /// The model asked to run tool `name`: call the top-level KUPL function of
    /// that name with the converted arguments. A panic in the tool surfaces as
    /// an `Err` so the ai fun can capture it (or panic itself).
    fn call_tool(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        let Some(decl) = self.db.funs.get(name).cloned() else {
            return Err(format!("tool `{name}` is not a top-level function"));
        };
        let env = self.globals.clone();
        match self.call_fun(&decl, args, &env, Span::default()) {
            Ok(v) => Ok(v),
            Err(Flow::Panic { msg, .. }) => Err(msg),
            Err(_) => Err(format!("tool `{name}` used non-local control flow")),
        }
    }
}

// ---------------- operators, patterns, builtin methods ----------------
// The raw (span-free) semantics live here and are SHARED by the tree-walking
// interpreter and the KVM — one implementation, no drift.

pub fn raw_binary_op(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    use BinOp::*;
    let overflow = |what: &str| format!("integer overflow in {what}");
    match op {
        Eq => return Ok(Value::Bool(l == r)),
        Ne => return Ok(Value::Bool(l != r)),
        _ => {}
    }
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            Ok(match op {
                Add => Value::Int(a.checked_add(b).ok_or_else(|| overflow("addition"))?),
                Sub => Value::Int(a.checked_sub(b).ok_or_else(|| overflow("subtraction"))?),
                Mul => Value::Int(a.checked_mul(b).ok_or_else(|| overflow("multiplication"))?),
                Div => {
                    if b == 0 {
                        return Err("division by zero".into());
                    }
                    Value::Int(a.checked_div(b).ok_or_else(|| overflow("division"))?)
                }
                Rem => {
                    if b == 0 {
                        return Err("remainder by zero".into());
                    }
                    Value::Int(a % b)
                }
                Lt => Value::Bool(a < b),
                Le => Value::Bool(a <= b),
                Gt => Value::Bool(a > b),
                Ge => Value::Bool(a >= b),
                _ => unreachable!(),
            })
        }
        (Value::Float(a), Value::Float(b)) => Ok(match op {
            Add => Value::Float(a + b),
            Sub => Value::Float(a - b),
            Mul => Value::Float(a * b),
            Div => Value::Float(a / b),
            Rem => Value::Float(a % b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            _ => unreachable!(),
        }),
        (Value::Tensor(a), Value::Tensor(b)) => {
            if a.len() != b.len() {
                return Err(format!("tensor length mismatch ({} vs {})", a.len(), b.len()));
            }
            let zip = a.iter().zip(b.iter());
            let data: Vec<f64> = match op {
                Add => zip.map(|(x, y)| x + y).collect(),
                Sub => zip.map(|(x, y)| x - y).collect(),
                Mul => zip.map(|(x, y)| x * y).collect(),
                Div => zip.map(|(x, y)| x / y).collect(),
                _ => return Err("invalid tensor operation".into()),
            };
            Ok(Value::Tensor(std::rc::Rc::new(data)))
        }
        (Value::Str(a), Value::Str(b)) => match op {
            Add => Ok(Value::str(format!("{a}{b}"))),
            Lt => Ok(Value::Bool(a < b)),
            Le => Ok(Value::Bool(a <= b)),
            Gt => Ok(Value::Bool(a > b)),
            Ge => Ok(Value::Bool(a >= b)),
            _ => Err("invalid string operation".into()),
        },
        _ => Err(format!(
            "invalid operand types: {} and {}",
            l.type_name(),
            r.type_name()
        )),
    }
}

fn binary_op(op: BinOp, l: Value, r: Value, span: Span) -> EvalResult {
    raw_binary_op(op, &l, &r).map_err(|msg| Flow::Panic { msg, span })
}

pub fn match_pattern(pat: &Pattern, value: &Value, env: &Env) -> bool {
    match (&pat.kind, value) {
        (PatternKind::Wildcard, _) => true,
        (PatternKind::Bind(name), v) => {
            env.define(name, v.clone());
            true
        }
        (PatternKind::Int(a), Value::Int(b)) => a == b,
        (PatternKind::Bool(a), Value::Bool(b)) => a == b,
        (PatternKind::Str(a), Value::Str(b)) => a == b.as_str(),
        (PatternKind::Ctor { name, args }, Value::Ctor { variant, fields, .. }) => {
            if name != variant.as_str() {
                return false;
            }
            if args.is_empty() {
                return true;
            }
            if args.len() != fields.len() {
                return false;
            }
            args.iter().zip(fields.iter()).all(|(p, v)| match_pattern(p, v, env))
        }
        _ => false,
    }
}

/// Callback used by function-taking methods (`map`, `filter`, `find`) to call
/// back into whichever engine is running.
pub type Caller<'a> = dyn FnMut(Value, Vec<Value>) -> Result<Value, String> + 'a;

/// Builtin method semantics, shared by interpreter and KVM.
pub fn shared_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    call: &mut Caller,
) -> Result<Value, String> {
    match (recv, name) {
        (Value::List(items), "len") => Ok(Value::Int(items.len() as i64)),
        // `map` and `par_map` share one implementation: par_map declares the
        // per-element work independent (safe to run in parallel); execution is
        // deterministic (input order) today — a real scheduler is a later,
        // semantics-preserving step. Same for `filter`/`par_filter`.
        (Value::List(items), "map") | (Value::List(items), "par_map") => {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(call(f.clone(), vec![item.clone()])?);
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "filter") | (Value::List(items), "par_filter") => {
            let f = args.into_iter().next().ok_or("`filter` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    out.push(item.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "par_each") => {
            let f = args.into_iter().next().ok_or("`par_each` needs a function")?;
            for item in items.iter() {
                call(f.clone(), vec![item.clone()])?;
            }
            Ok(Value::Unit)
        }
        (Value::List(items), "find") => {
            let f = args.into_iter().next().ok_or("`find` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::some(item.clone()));
                }
            }
            Ok(Value::none())
        }
        (Value::List(items), "sum") => {
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut is_float = false;
            for item in items.iter() {
                match item {
                    Value::Int(v) => {
                        int_sum = int_sum
                            .checked_add(*v)
                            .ok_or("integer overflow in sum")?
                    }
                    Value::Float(v) => {
                        is_float = true;
                        float_sum += v;
                    }
                    other => return Err(format!("cannot sum {}", other.type_name())),
                }
            }
            if is_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        (Value::List(items), "fold") => {
            let mut it = args.into_iter();
            let mut acc = it.next().ok_or("`fold` needs an initial value")?;
            let f = it.next().ok_or("`fold` needs a function")?;
            for item in items.iter() {
                acc = call(f.clone(), vec![acc, item.clone()])?;
            }
            Ok(acc)
        }
        (Value::List(items), "any") => {
            let f = args.into_iter().next().ok_or("`any` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        (Value::List(items), "all") => {
            let f = args.into_iter().next().ok_or("`all` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(false) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        (Value::List(items), "sort") => {
            let mut out = items.as_ref().clone();
            let mut err = None;
            out.sort_by(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => x.cmp(y),
                (Value::Float(x), Value::Float(y)) => {
                    x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                }
                (Value::Str(x), Value::Str(y)) => x.cmp(y),
                _ => {
                    err = Some("`sort` needs Int, Float, or Str elements".to_string());
                    std::cmp::Ordering::Equal
                }
            });
            match err {
                Some(e) => Err(e),
                None => Ok(Value::List(Rc::new(out))),
            }
        }
        (Value::List(items), "take") => match args.into_iter().next() {
            Some(Value::Int(n)) => {
                let n = (n.max(0) as usize).min(items.len());
                Ok(Value::List(Rc::new(items[..n].to_vec())))
            }
            _ => Err("`take` needs an Int".into()),
        },
        (Value::List(items), "drop") => match args.into_iter().next() {
            Some(Value::Int(n)) => {
                let n = (n.max(0) as usize).min(items.len());
                Ok(Value::List(Rc::new(items[n..].to_vec())))
            }
            _ => Err("`drop` needs an Int".into()),
        },
        (Value::List(items), "get") => match args.into_iter().next() {
            Some(Value::Int(i)) => Ok(if i >= 0 && (i as usize) < items.len() {
                Value::some(items[i as usize].clone())
            } else {
                Value::none()
            }),
            _ => Err("`get` needs an Int".into()),
        },
        (Value::List(items), "index_of") => {
            let needle = args.into_iter().next().ok_or("`index_of` needs a value")?;
            Ok(items
                .iter()
                .position(|v| *v == needle)
                .map(|i| Value::some(Value::Int(i as i64)))
                .unwrap_or_else(Value::none))
        }
        (Value::List(items), "contains") => {
            let needle = args.into_iter().next().ok_or("`contains` needs a value")?;
            Ok(Value::Bool(items.iter().any(|v| *v == needle)))
        }
        (Value::List(items), "push") => {
            let v = args.into_iter().next().ok_or("`push` needs a value")?;
            let mut out = items.as_ref().clone();
            out.push(v);
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "first") => Ok(items.first().cloned().map(Value::some).unwrap_or_else(Value::none)),
        (Value::List(items), "last") => Ok(items.last().cloned().map(Value::some).unwrap_or_else(Value::none)),
        (Value::List(items), "reverse") => {
            let mut out = items.as_ref().clone();
            out.reverse();
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "join") => {
            let sep = match args.into_iter().next() {
                Some(Value::Str(s)) => s.as_str().to_string(),
                _ => return Err("`join` needs a Str separator".into()),
            };
            let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
            Ok(Value::str(parts.join(&sep)))
        }
        (Value::List(items), "is_empty") => Ok(Value::Bool(items.is_empty())),
        (Value::List(items), "concat") => match args.into_iter().next() {
            Some(Value::List(other)) => {
                let mut out = items.as_ref().clone();
                out.extend(other.iter().cloned());
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`concat` needs a List".into()),
        },
        (Value::List(items), "unique") => {
            let mut out: Vec<Value> = Vec::new();
            for it in items.iter() {
                if !out.iter().any(|x| x == it) {
                    out.push(it.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "init") => {
            let n = items.len().saturating_sub(1);
            Ok(Value::List(Rc::new(items[..n].to_vec())))
        }
        (Value::List(items), "tail") => {
            let start = if items.is_empty() { 0 } else { 1 };
            Ok(Value::List(Rc::new(items[start..].to_vec())))
        }
        (Value::List(items), "product") => {
            let mut int_prod: i64 = 1;
            let mut float_prod: f64 = 1.0;
            let mut is_float = false;
            for item in items.iter() {
                match item {
                    Value::Int(v) => {
                        int_prod = int_prod
                            .checked_mul(*v)
                            .ok_or("integer overflow in product")?
                    }
                    Value::Float(v) => {
                        is_float = true;
                        float_prod *= v;
                    }
                    other => return Err(format!("cannot multiply {}", other.type_name())),
                }
            }
            if is_float {
                Ok(Value::Float(float_prod * int_prod as f64))
            } else {
                Ok(Value::Int(int_prod))
            }
        }
        (Value::List(items), "min") | (Value::List(items), "max") => {
            let want_min = name == "min";
            let mut best: Option<Value> = None;
            for item in items.iter() {
                let take = match &best {
                    None => true,
                    Some(b) => {
                        let ord = list_order(b, item)?;
                        if want_min {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        }
                    }
                };
                if take {
                    best = Some(item.clone());
                }
            }
            Ok(best.map(Value::some).unwrap_or_else(Value::none))
        }
        (Value::List(items), "flatten") => {
            let mut out = Vec::new();
            for item in items.iter() {
                match item {
                    Value::List(inner) => out.extend(inner.iter().cloned()),
                    other => return Err(format!("`flatten` needs a List of Lists, found {}", other.type_name())),
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "count") => {
            let f = args.into_iter().next().ok_or("`count` needs a function")?;
            let mut n = 0i64;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    n += 1;
                }
            }
            Ok(Value::Int(n))
        }
        (Value::List(items), "flat_map") => {
            let f = args.into_iter().next().ok_or("`flat_map` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                match call(f.clone(), vec![item.clone()])? {
                    Value::List(inner) => out.extend(inner.iter().cloned()),
                    other => return Err(format!("`flat_map` function must return a List, got {}", other.type_name())),
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "window") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 1 => {
                let n = n as usize;
                let mut out = Vec::new();
                if items.len() >= n {
                    for i in 0..=items.len() - n {
                        out.push(Value::List(Rc::new(items[i..i + n].to_vec())));
                    }
                }
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`window` needs a positive Int".into()),
        },
        (Value::List(items), "chunk") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 1 => {
                let n = n as usize;
                let out: Vec<Value> = items
                    .chunks(n)
                    .map(|c| Value::List(Rc::new(c.to_vec())))
                    .collect();
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`chunk` needs a positive Int".into()),
        },
        (Value::Str(s), "len") => Ok(Value::Int(s.chars().count() as i64)),
        (Value::Str(s), "contains") => match args.into_iter().next() {
            Some(Value::Str(n)) => Ok(Value::Bool(s.contains(n.as_str()))),
            _ => Err("`contains` needs a Str".into()),
        },
        (Value::Str(s), "starts_with") => match args.into_iter().next() {
            Some(Value::Str(n)) => Ok(Value::Bool(s.starts_with(n.as_str()))),
            _ => Err("`starts_with` needs a Str".into()),
        },
        (Value::Str(s), "to_upper") => Ok(Value::str(s.to_uppercase())),
        (Value::Str(s), "to_lower") => Ok(Value::str(s.to_lowercase())),
        (Value::Str(s), "trim") => Ok(Value::str(s.trim().to_string())),
        (Value::Str(s), "ends_with") => match args.into_iter().next() {
            Some(Value::Str(n)) => Ok(Value::Bool(s.ends_with(n.as_str()))),
            _ => Err("`ends_with` needs a Str".into()),
        },
        (Value::Str(s), "replace") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Str(from)), Some(Value::Str(to))) => {
                    Ok(Value::str(s.replace(from.as_str(), to.as_str())))
                }
                _ => Err("`replace` needs two Str arguments".into()),
            }
        }
        (Value::Str(s), "chars") => Ok(Value::List(Rc::new(
            s.chars().map(|c| Value::str(c.to_string())).collect(),
        ))),
        (Value::Str(s), "repeat") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 0 => {
                if s.len().saturating_mul(n as usize) > 100_000_000 {
                    return Err("`repeat` result too large".into());
                }
                Ok(Value::str(s.repeat(n as usize)))
            }
            _ => Err("`repeat` needs a non-negative Int".into()),
        },
        (Value::Str(s), "parse_int") => Ok(s
            .parse::<i64>()
            .map(|v| Value::some(Value::Int(v)))
            .unwrap_or_else(|_| Value::none())),
        (Value::Str(s), "parse_float") => Ok(s
            .parse::<f64>()
            .map(|v| Value::some(Value::Float(v)))
            .unwrap_or_else(|_| Value::none())),
        (Value::Str(s), "split") => match args.into_iter().next() {
            Some(Value::Str(sep)) => Ok(Value::List(Rc::new(
                s.split(sep.as_str()).map(Value::str).collect(),
            ))),
            _ => Err("`split` needs a Str separator".into()),
        },
        (Value::Str(s), "is_empty") => Ok(Value::Bool(s.is_empty())),
        (Value::Str(s), "reverse") => Ok(Value::str(s.chars().rev().collect::<String>())),
        (Value::Str(s), "lines") => Ok(Value::List(Rc::new(
            s.lines().map(Value::str).collect(),
        ))),
        (Value::Str(s), "index_of") => match args.into_iter().next() {
            Some(Value::Str(sub)) => Ok(match s.find(sub.as_str()) {
                // byte offset -> character index
                Some(byte) => Value::some(Value::Int(s[..byte].chars().count() as i64)),
                None => Value::none(),
            }),
            _ => Err("`index_of` needs a Str".into()),
        },
        (Value::Str(s), "count") => match args.into_iter().next() {
            Some(Value::Str(sub)) if !sub.is_empty() => {
                Ok(Value::Int(s.matches(sub.as_str()).count() as i64))
            }
            Some(Value::Str(_)) => Err("`count` needs a non-empty Str".into()),
            _ => Err("`count` needs a Str".into()),
        },
        (Value::Str(s), "slice") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(a)), Some(Value::Int(b))) => {
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let lo = a.clamp(0, len) as usize;
                    let hi = b.clamp(a.max(0), len) as usize;
                    Ok(Value::str(chars[lo..hi].iter().collect::<String>()))
                }
                _ => Err("`slice` needs two Int arguments".into()),
            }
        }
        (Value::Str(s), "pad_left") | (Value::Str(s), "pad_right") => {
            let left = name == "pad_left";
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(width)), Some(Value::Str(ch))) => {
                    let fill = ch.chars().next().unwrap_or(' ');
                    let cur = s.chars().count() as i64;
                    if cur >= width || width > 100_000_000 {
                        Ok(Value::str(s.as_str().to_string()))
                    } else {
                        let pad: String = std::iter::repeat(fill).take((width - cur) as usize).collect();
                        Ok(Value::str(if left {
                            format!("{pad}{s}")
                        } else {
                            format!("{s}{pad}")
                        }))
                    }
                }
                _ => Err("`pad_left`/`pad_right` need an Int width and a Str fill".into()),
            }
        }
        (Value::Int(v), "to_str") => Ok(Value::str(v.to_string())),
        (Value::Int(v), "to_float") => Ok(Value::Float(*v as f64)),
        (Value::Int(v), "abs") => v
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| "integer overflow in abs".to_string()),
        (Value::Int(v), "min") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int((*v).min(w))),
            _ => Err("`min` needs an Int".into()),
        },
        (Value::Int(v), "max") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int((*v).max(w))),
            _ => Err("`max` needs an Int".into()),
        },
        (Value::Int(v), "pow") => match args.into_iter().next() {
            Some(Value::Int(e)) if e >= 0 && e <= u32::MAX as i64 => (*v)
                .checked_pow(e as u32)
                .map(Value::Int)
                .ok_or_else(|| "integer overflow in pow".to_string()),
            Some(Value::Int(_)) => Err("`pow` needs a non-negative exponent".into()),
            _ => Err("`pow` needs an Int".into()),
        },
        (Value::Int(v), "gcd") => match args.into_iter().next() {
            Some(Value::Int(w)) => {
                let (mut a, mut b) = (v.unsigned_abs(), w.unsigned_abs());
                while b != 0 {
                    let t = b;
                    b = a % b;
                    a = t;
                }
                Ok(Value::Int(a as i64))
            }
            _ => Err("`gcd` needs an Int".into()),
        },
        (Value::Int(v), "clamp") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(lo)), Some(Value::Int(hi))) => {
                    if lo > hi {
                        Err("`clamp`: lo must not exceed hi".into())
                    } else {
                        Ok(Value::Int((*v).clamp(lo, hi)))
                    }
                }
                _ => Err("`clamp` needs two Int arguments".into()),
            }
        }
        (Value::Int(v), "sign") => Ok(Value::Int(v.signum())),
        (Value::Int(v), "is_even") => Ok(Value::Bool(v % 2 == 0)),
        (Value::Int(v), "is_odd") => Ok(Value::Bool(v % 2 != 0)),
        (Value::Int(v), "to_hex") => Ok(Value::str(int_to_radix(*v, 16))),
        (Value::Int(v), "to_binary") => Ok(Value::str(int_to_radix(*v, 2))),
        (Value::Int(v), "to_octal") => Ok(Value::str(int_to_radix(*v, 8))),
        (Value::Int(v), "to_radix") => match args.into_iter().next() {
            Some(Value::Int(b)) if (2..=36).contains(&b) => {
                Ok(Value::str(int_to_radix(*v, b as u32)))
            }
            Some(Value::Int(_)) => Err("`to_radix` base must be in 2..=36".into()),
            _ => Err("`to_radix` needs an Int base".into()),
        },
        (Value::Int(v), "isqrt") => {
            if *v < 0 {
                Err("`isqrt` of a negative Int".into())
            } else {
                Ok(Value::Int(int_isqrt(*v)))
            }
        }
        (Value::Int(v), "band") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v & w)),
            _ => Err("`band` needs an Int".into()),
        },
        (Value::Int(v), "bor") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v | w)),
            _ => Err("`bor` needs an Int".into()),
        },
        (Value::Int(v), "bxor") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v ^ w)),
            _ => Err("`bxor` needs an Int".into()),
        },
        (Value::Int(v), "bnot") => Ok(Value::Int(!v)),
        (Value::Int(v), "shl") => match args.into_iter().next() {
            Some(Value::Int(n)) if (0..=63).contains(&n) => Ok(Value::Int(v << n)),
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`shl` needs an Int".into()),
        },
        (Value::Int(v), "shr") => match args.into_iter().next() {
            // arithmetic shift right (sign-preserving), matching i64 `>>`
            Some(Value::Int(n)) if (0..=63).contains(&n) => Ok(Value::Int(v >> n)),
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`shr` needs an Int".into()),
        },
        (Value::Int(v), "ushr") => match args.into_iter().next() {
            // logical (unsigned) shift right — zero-fills from the left
            Some(Value::Int(n)) if (0..=63).contains(&n) => {
                Ok(Value::Int(((*v as u64) >> n) as i64))
            }
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`ushr` needs an Int".into()),
        },
        (Value::Float(v), "to_str") => Ok(Value::str(v.to_string())),
        (Value::Float(v), "to_int") => Ok(Value::Int(*v as i64)),
        (Value::Float(v), "abs") => Ok(Value::Float(v.abs())),
        (Value::Float(v), "sqrt") => Ok(Value::Float(v.sqrt())),
        (Value::Float(v), "floor") => Ok(Value::Float(v.floor())),
        (Value::Float(v), "ceil") => Ok(Value::Float(v.ceil())),
        (Value::Float(v), "round") => Ok(Value::Float(v.round())),
        (Value::Float(v), "min") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.min(w))),
            _ => Err("`min` needs a Float".into()),
        },
        (Value::Float(v), "max") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.max(w))),
            _ => Err("`max` needs a Float".into()),
        },
        (Value::Float(v), "pow") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.powf(w))),
            _ => Err("`pow` needs a Float".into()),
        },
        (Value::Float(v), "log") => Ok(Value::Float(v.ln())),
        (Value::Float(v), "log10") => Ok(Value::Float(v.log10())),
        (Value::Float(v), "log2") => Ok(Value::Float(v.log2())),
        (Value::Float(v), "cbrt") => Ok(Value::Float(v.cbrt())),
        (Value::Float(v), "atan2") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.atan2(w))),
            _ => Err("`atan2` needs a Float".into()),
        },
        (Value::Float(v), "hypot") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.hypot(w))),
            _ => Err("`hypot` needs a Float".into()),
        },
        (Value::Float(v), "format") => match args.into_iter().next() {
            Some(Value::Int(d)) if (0..=100).contains(&d) => {
                Ok(Value::str(format!("{:.*}", d as usize, v)))
            }
            Some(Value::Int(_)) => Err("`format` decimals must be in 0..=100".into()),
            _ => Err("`format` needs an Int number of decimals".into()),
        },
        (Value::Float(v), "exp") => Ok(Value::Float(v.exp())),
        (Value::Float(v), "sin") => Ok(Value::Float(v.sin())),
        (Value::Float(v), "cos") => Ok(Value::Float(v.cos())),
        (Value::Float(v), "tan") => Ok(Value::Float(v.tan())),
        (Value::Float(v), "sign") => Ok(Value::Float(if *v > 0.0 {
            1.0
        } else if *v < 0.0 {
            -1.0
        } else {
            *v // preserves 0.0 / -0.0 / NaN
        })),
        (Value::Float(v), "is_nan") => Ok(Value::Bool(v.is_nan())),
        (Value::Float(v), "is_infinite") => Ok(Value::Bool(v.is_infinite())),
        (Value::Float(v), "clamp") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Float(lo)), Some(Value::Float(hi))) => {
                    if lo > hi {
                        Err("`clamp`: lo must not exceed hi".into())
                    } else {
                        Ok(Value::Float(v.clamp(lo, hi)))
                    }
                }
                _ => Err("`clamp` needs two Float arguments".into()),
            }
        }
        (Value::Map(pairs), "insert") => {
            let mut it = args.into_iter();
            let (k, v) = (
                it.next().ok_or("`insert` needs a key")?,
                it.next().ok_or("`insert` needs a value")?,
            );
            let mut out = pairs.as_ref().clone();
            match out.iter_mut().find(|(pk, _)| *pk == k) {
                Some(pair) => pair.1 = v,
                None => out.push((k, v)),
            }
            Ok(Value::Map(Rc::new(out)))
        }
        (Value::Map(pairs), "get") => {
            let k = args.into_iter().next().ok_or("`get` needs a key")?;
            Ok(pairs
                .iter()
                .find(|(pk, _)| *pk == k)
                .map(|(_, v)| Value::some(v.clone()))
                .unwrap_or_else(Value::none))
        }
        (Value::Map(pairs), "remove") => {
            let k = args.into_iter().next().ok_or("`remove` needs a key")?;
            Ok(Value::Map(Rc::new(
                pairs.iter().filter(|(pk, _)| *pk != k).cloned().collect(),
            )))
        }
        (Value::Map(pairs), "contains_key") => {
            let k = args.into_iter().next().ok_or("`contains_key` needs a key")?;
            Ok(Value::Bool(pairs.iter().any(|(pk, _)| *pk == k)))
        }
        (Value::Map(pairs), "keys") => Ok(Value::List(Rc::new(
            pairs.iter().map(|(k, _)| k.clone()).collect(),
        ))),
        (Value::Map(pairs), "values") => Ok(Value::List(Rc::new(
            pairs.iter().map(|(_, v)| v.clone()).collect(),
        ))),
        (Value::Map(pairs), "len") => Ok(Value::Int(pairs.len() as i64)),
        (Value::Map(pairs), "is_empty") => Ok(Value::Bool(pairs.is_empty())),
        (Value::Map(pairs), "get_or") => {
            let mut it = args.into_iter();
            let k = it.next().ok_or("`get_or` needs a key")?;
            let default = it.next().ok_or("`get_or` needs a default")?;
            Ok(pairs
                .iter()
                .find(|(pk, _)| *pk == k)
                .map(|(_, v)| v.clone())
                .unwrap_or(default))
        }
        (Value::Map(pairs), "merge") => match args.into_iter().next() {
            Some(Value::Map(other)) => {
                let mut out = pairs.as_ref().clone();
                for (k, v) in other.iter() {
                    match out.iter_mut().find(|(pk, _)| pk == k) {
                        Some(pair) => pair.1 = v.clone(),
                        None => out.push((k.clone(), v.clone())),
                    }
                }
                Ok(Value::Map(Rc::new(out)))
            }
            _ => Err("`merge` needs a Map".into()),
        },
        (Value::Map(pairs), "map_values") => {
            let f = args.into_iter().next().ok_or("`map_values` needs a function")?;
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs.iter() {
                out.push((k.clone(), call(f.clone(), vec![v.clone()])?));
            }
            Ok(Value::Map(Rc::new(out)))
        }
        (Value::Set(items), "insert") => {
            let v = args.into_iter().next().ok_or("`insert` needs a value")?;
            if items.iter().any(|x| *x == v) {
                Ok(Value::Set(items.clone()))
            } else {
                let mut out = items.as_ref().clone();
                out.push(v);
                Ok(Value::Set(Rc::new(out)))
            }
        }
        (Value::Set(items), "remove") => {
            let v = args.into_iter().next().ok_or("`remove` needs a value")?;
            Ok(Value::Set(Rc::new(
                items.iter().filter(|x| **x != v).cloned().collect(),
            )))
        }
        (Value::Set(items), "contains") => {
            let v = args.into_iter().next().ok_or("`contains` needs a value")?;
            Ok(Value::Bool(items.iter().any(|x| *x == v)))
        }
        (Value::Set(items), "len") => Ok(Value::Int(items.len() as i64)),
        (Value::Set(items), "union") => match args.into_iter().next() {
            Some(Value::Set(other)) => {
                let mut out = items.as_ref().clone();
                for x in other.iter() {
                    if !out.iter().any(|y| y == x) {
                        out.push(x.clone());
                    }
                }
                Ok(Value::Set(Rc::new(out)))
            }
            _ => Err("`union` needs a Set".into()),
        },
        (Value::Set(items), "intersect") => match args.into_iter().next() {
            Some(Value::Set(other)) => Ok(Value::Set(Rc::new(
                items.iter().filter(|x| other.iter().any(|y| y == *x)).cloned().collect(),
            ))),
            _ => Err("`intersect` needs a Set".into()),
        },
        (Value::Set(items), "difference") => match args.into_iter().next() {
            Some(Value::Set(other)) => Ok(Value::Set(Rc::new(
                items.iter().filter(|x| !other.iter().any(|y| y == *x)).cloned().collect(),
            ))),
            _ => Err("`difference` needs a Set".into()),
        },
        (Value::Set(items), "to_list") => Ok(Value::List(Rc::new(items.as_ref().clone()))),
        (Value::Set(items), "is_empty") => Ok(Value::Bool(items.is_empty())),
        (Value::Set(items), "is_subset") => match args.into_iter().next() {
            Some(Value::Set(other)) => {
                Ok(Value::Bool(items.iter().all(|x| other.iter().any(|y| y == x))))
            }
            _ => Err("`is_subset` needs a Set".into()),
        },
        (Value::Tensor(d), "len") => Ok(Value::Int(d.len() as i64)),
        (Value::Tensor(d), "get") => match args.into_iter().next() {
            Some(Value::Int(i)) if i >= 0 && (i as usize) < d.len() => Ok(Value::Float(d[i as usize])),
            Some(Value::Int(_)) => Err("tensor index out of range".into()),
            _ => Err("`get` needs an Int index".into()),
        },
        (Value::Tensor(d), "sum") => Ok(Value::Float(d.iter().sum())),
        (Value::Tensor(d), "mean") => {
            if d.is_empty() {
                return Err("mean of an empty tensor".into());
            }
            Ok(Value::Float(d.iter().sum::<f64>() / d.len() as f64))
        }
        (Value::Tensor(d), "max") => d
            .iter()
            .cloned()
            .fold(None::<f64>, |m, x| Some(m.map_or(x, |m| m.max(x))))
            .map(Value::Float)
            .ok_or_else(|| "max of an empty tensor".to_string()),
        (Value::Tensor(d), "min") => d
            .iter()
            .cloned()
            .fold(None::<f64>, |m, x| Some(m.map_or(x, |m| m.min(x))))
            .map(Value::Float)
            .ok_or_else(|| "min of an empty tensor".to_string()),
        (Value::Tensor(a), "dot") => match args.into_iter().next() {
            Some(Value::Tensor(b)) => {
                if a.len() != b.len() {
                    return Err(format!("dot: length mismatch ({} vs {})", a.len(), b.len()));
                }
                Ok(Value::Float(a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()))
            }
            _ => Err("`dot` needs a Tensor".into()),
        },
        (Value::Tensor(d), "scale") => match args.into_iter().next() {
            Some(Value::Float(k)) => Ok(Value::Tensor(Rc::new(d.iter().map(|x| x * k).collect()))),
            _ => Err("`scale` needs a Float".into()),
        },
        (Value::Tensor(d), "map") => {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let mut out = Vec::with_capacity(d.len());
            for x in d.iter() {
                match call(f.clone(), vec![Value::Float(*x)])? {
                    Value::Float(y) => out.push(y),
                    other => return Err(format!("tensor map must return Float, got {}", other.type_name())),
                }
            }
            Ok(Value::Tensor(Rc::new(out)))
        }
        (Value::Tensor(d), "to_list") => Ok(Value::List(Rc::new(
            d.iter().map(|x| Value::Float(*x)).collect(),
        ))),
        (Value::Ctor { variant, .. }, "is_some") => Ok(Value::Bool(variant.as_str() == "Some")),
        (Value::Ctor { variant, .. }, "is_none") => Ok(Value::Bool(variant.as_str() == "None")),
        (Value::Ctor { variant, .. }, "is_ok") => Ok(Value::Bool(variant.as_str() == "Ok")),
        (Value::Ctor { variant, .. }, "is_err") => Ok(Value::Bool(variant.as_str() == "Err")),
        (Value::Ctor { variant, fields, .. }, "unwrap_or") => {
            let default = args.into_iter().next().ok_or("`unwrap_or` needs a default")?;
            match variant.as_str() {
                "Some" | "Ok" => Ok(fields.first().cloned().unwrap_or(Value::Unit)),
                _ => Ok(default),
            }
        }
        (other, _) => Err(format!("{} has no method `{name}`", other.type_name())),
    }
}

/// Format an i64 in a given base (2..=36) — lowercase digits, a leading `-`
/// on the magnitude for negatives. Shared with the cgen C mirror.
fn int_to_radix(v: i64, base: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = v.unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % base as u64) as usize]);
        n /= base as u64;
    }
    if v < 0 {
        buf.push(b'-');
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Integer square root (floor) of a non-negative i64.
fn int_isqrt(v: i64) -> i64 {
    let n = v as u64;
    if n == 0 {
        return 0;
    }
    let mut x = (n as f64).sqrt() as u64;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x as i64
}

/// Ordering for `List.min`/`max` — Int, Float, or Str elements only.
fn list_order(a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => Ok(x.partial_cmp(y).unwrap_or(Ordering::Equal)),
        (Value::Str(x), Value::Str(y)) => Ok(x.cmp(y)),
        _ => Err("`min`/`max` need Int, Float, or Str elements".into()),
    }
}

/// Build a Set from a List, dropping duplicates (shared by all engines).
pub fn set_from_list(v: &Value) -> Result<Value, String> {
    match v {
        Value::List(items) => {
            let mut out: Vec<Value> = Vec::new();
            for it in items.iter() {
                if !out.iter().any(|x| x == it) {
                    out.push(it.clone());
                }
            }
            Ok(Value::Set(Rc::new(out)))
        }
        other => Err(format!("Set(...) needs a List, found {}", other.type_name())),
    }
}

/// Deterministic PRNG (xorshift64*) behind the seeded-random builtins. The
/// exact algorithm — state init, `next`, the `>> 11` float mapping, and the
/// Fisher-Yates order — is mirrored byte-for-byte in cgen.rs so `random_*` and
/// `shuffle` give identical results on the interpreter, KVM, and native.
struct SeedRng(u64);

impl SeedRng {
    fn new(seed: i64) -> Self {
        // xorshift needs a non-zero state
        SeedRng(if seed as u64 == 0 { 1 } else { seed as u64 })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Seeded random builtins — shared by interpreter and KVM. Pure: a given seed
/// always yields the same output, so results are reproducible.
pub fn random_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_int = |v: &Value| match v {
        Value::Int(n) => *n,
        _ => 0,
    };
    match name {
        "random_ints" => {
            let mut r = SeedRng::new(as_int(&args[0]));
            let n = as_int(&args[1]).max(0);
            if n > 100_000_000 {
                return Err("random count too large".into());
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                out.push(Value::Int(r.next_u64() as i64));
            }
            Ok(Value::List(Rc::new(out)))
        }
        "random_floats" => {
            let mut r = SeedRng::new(as_int(&args[0]));
            let n = as_int(&args[1]).max(0);
            if n > 100_000_000 {
                return Err("random count too large".into());
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                // top 53 bits → a double in [0, 1)
                out.push(Value::Float(
                    (r.next_u64() >> 11) as f64 * (1.0 / 9007199254740992.0),
                ));
            }
            Ok(Value::List(Rc::new(out)))
        }
        "shuffle" => {
            let list = match &args[1] {
                Value::List(xs) => xs,
                other => return Err(format!("`shuffle` needs a List, found {}", other.type_name())),
            };
            let mut out = list.as_ref().clone();
            let mut r = SeedRng::new(as_int(&args[0]));
            // Fisher-Yates from the end: swap i with a random j in 0..=i
            let mut i = out.len();
            while i > 1 {
                i -= 1;
                let j = (r.next_u64() % (i as u64 + 1)) as usize;
                out.swap(i, j);
            }
            Ok(Value::List(Rc::new(out)))
        }
        _ => Err(format!("unknown random builtin `{name}`")),
    }
}

/// The program's own command-line arguments. When KUPL is run through the
/// toolchain (`kupl run prog.kupl -- a b c`), the program's args are everything
/// after `--`; with no `--`, there are none. (The native backend reads argv
/// directly.)
pub fn program_args() -> Vec<String> {
    let all: Vec<String> = std::env::args().collect();
    match all.iter().position(|a| a == "--") {
        Some(i) => all[i + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// Environment & process builtins that return a value — shared by interpreter
/// and KVM. `env_var`/`args` carry the `io.env` effect; `eprint` carries `io`.
/// (`exit` diverges and is handled inline, like `panic`.)
pub fn proc_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "env_var" => {
            let key = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            };
            Ok(match std::env::var(&key) {
                Ok(v) => Value::some(Value::str(v)),
                Err(_) => Value::none(),
            })
        }
        "args" => Ok(Value::List(Rc::new(
            program_args().into_iter().map(Value::str).collect(),
        ))),
        "eprint" => {
            eprintln!("{}", args[0]);
            Ok(Value::Unit)
        }
        _ => Err(format!("unknown process builtin `{name}`")),
    }
}

/// CSV builtins — shared by interpreter and KVM. Pure.
pub fn csv_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "csv_parse" => {
            let text = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            };
            let rows = crate::csv::parse(&text);
            let out: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    Value::List(Rc::new(row.into_iter().map(Value::str).collect()))
                })
                .collect();
            Ok(Value::List(Rc::new(out)))
        }
        "csv_stringify" => {
            let rows = match &args[0] {
                Value::List(rows) => rows,
                other => return Err(format!("`csv_stringify` needs a List, found {}", other.type_name())),
            };
            let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len());
            for row in rows.iter() {
                let fields = match row {
                    Value::List(fs) => fs,
                    other => return Err(format!("`csv_stringify` rows must be Lists, found {}", other.type_name())),
                };
                grid.push(fields.iter().map(|f| match f {
                    Value::Str(s) => s.as_str().to_string(),
                    other => other.to_string(),
                }).collect());
            }
            Ok(Value::str(crate::csv::stringify(&grid)))
        }
        _ => Err(format!("unknown csv builtin `{name}`")),
    }
}

/// Encoding & hash builtins — shared by interpreter and KVM. All pure.
/// `*_decode` returns a `Result` value; encode/hash always succeed.
pub fn encoding_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let s = match &args[0] {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    use crate::encoding as enc;
    Ok(match name {
        "base64_encode" => Value::str(enc::base64_encode(&s)),
        "hex_encode" => Value::str(enc::hex_encode(&s)),
        "hash_fnv" => Value::Int(enc::hash_fnv(&s)),
        "base64_decode" => match enc::base64_decode(&s) {
            Ok(v) => Value::ok(Value::str(v)),
            Err(e) => Value::err(Value::str(e)),
        },
        "hex_decode" => match enc::hex_decode(&s) {
            Ok(v) => Value::ok(Value::str(v)),
            Err(e) => Value::err(Value::str(e)),
        },
        _ => return Err(format!("unknown encoding builtin `{name}`")),
    })
}

/// Time/date builtins — shared by interpreter and KVM. All PURE (a timestamp
/// in, a string or Int out); `now` is separate (wall clock, `io.time`).
pub fn time_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let t = match &args[0] {
        Value::Int(n) => *n,
        _ => 0,
    };
    use crate::time as tm;
    Ok(match name {
        "format_time" => Value::str(tm::format_time(t)),
        "year_of" => Value::Int(tm::year_of(t)),
        "month_of" => Value::Int(tm::month_of(t)),
        "day_of" => Value::Int(tm::day_of(t)),
        "hour_of" => Value::Int(tm::hour_of(t)),
        "minute_of" => Value::Int(tm::minute_of(t)),
        "second_of" => Value::Int(tm::second_of(t)),
        "weekday_of" => Value::Int(tm::weekday_of(t)),
        _ => return Err(format!("unknown time builtin `{name}`")),
    })
}

/// Current Unix epoch seconds (wall clock). Effect `io.time`.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Regex builtins — shared by interpreter and KVM. Pure; a malformed pattern
/// panics with a clear message (the pattern is program text, so this is a bug
/// to surface, like a bad format string).
pub fn regex_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| match v {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    let re = crate::regex::compile(&as_str(&args[0]))
        .map_err(|e| format!("invalid regex: {e}"))?;
    let text = as_str(&args[1]);
    Ok(match name {
        "re_match" => Value::Bool(re.is_match(&text)),
        "re_find" => re
            .find(&text)
            .map(|m| Value::some(Value::str(m)))
            .unwrap_or_else(Value::none),
        "re_find_all" => Value::List(Rc::new(
            re.find_all(&text).into_iter().map(Value::str).collect(),
        )),
        "re_replace" => Value::str(re.replace_all(&text, &as_str(&args[2]))),
        _ => return Err(format!("unknown regex builtin `{name}`")),
    })
}

/// HTTP builtins — shared by interpreter and KVM. Effect `io.net`. Transport is
/// the system `curl` (the same zero-dependency approach the AI runtime uses).
/// Returns a `Result` value: `Ok(body)` on a successful request, `Err(message)`
/// otherwise (unreachable host, non-2xx, curl missing, …). The `Err` text is a
/// human-readable description and may vary by platform — match `Ok`/`Err`.
pub fn http_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| match v {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    let url = as_str(&args[0]);
    // `--fail` makes curl return a non-zero status (and thus an Err) on HTTP
    // 4xx/5xx; `-sS` silences the progress meter but keeps error messages.
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "--fail", "--max-time", "30"]);
    let result = match name {
        "http_get" => {
            cmd.arg(&url);
            run_curl(cmd, None)
        }
        "http_post" => {
            let body = as_str(&args[1]);
            cmd.args(["-X", "POST", "--data-binary", "@-", &url]);
            run_curl(cmd, Some(body))
        }
        _ => return Err(format!("unknown http builtin `{name}`")),
    };
    Ok(match result {
        Ok(body) => Value::ok(Value::str(body)),
        Err(msg) => Value::err(Value::str(msg)),
    })
}

fn run_curl(mut cmd: std::process::Command, body: Option<String>) -> Result<String, String> {
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if body.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(b) = body {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b.as_bytes()).map_err(|e| format!("curl stdin: {e}"))?;
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("request failed (curl exit {})", out.status.code().unwrap_or(-1))
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// File I/O builtins — shared by interpreter and KVM. Effect `io.fs`.
///
/// All return a `Result` value (KUPL has no exceptions): read/write/append/
/// delete give `Result[Str|Unit, Str]` (the `Err` carries the OS message);
/// `file_exists` gives a plain `Bool`. A wrong argument *type* is a checker
/// error, so here we assume the types the checker guaranteed.
pub fn fs_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| -> String {
        match v {
            Value::Str(s) => s.as_str().to_string(),
            other => other.to_string(),
        }
    };
    match name {
        "read_file" => Ok(match std::fs::read_to_string(as_str(&args[0])) {
            Ok(contents) => Value::ok(Value::str(contents)),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "write_file" => Ok(match std::fs::write(as_str(&args[0]), as_str(&args[1])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "append_file" => {
            use std::io::Write;
            let result = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(as_str(&args[0]))
                .and_then(|mut f| f.write_all(as_str(&args[1]).as_bytes()));
            Ok(match result {
                Ok(()) => Value::ok(Value::Unit),
                Err(e) => Value::err(Value::str(e.to_string())),
            })
        }
        "delete_file" => Ok(match std::fs::remove_file(as_str(&args[0])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "file_exists" => Ok(Value::Bool(std::path::Path::new(&as_str(&args[0])).exists())),
        _ => Err(format!("unknown file builtin `{name}`")),
    }
}

/// tensor / zeros / arange — shared by interpreter and KVM.
pub fn tensor_builtin(name: &str, arg: &Value) -> Result<Value, String> {
    match (name, arg) {
        ("tensor", Value::List(items)) => {
            let mut data = Vec::with_capacity(items.len());
            for it in items.iter() {
                match it {
                    Value::Float(f) => data.push(*f),
                    Value::Int(i) => data.push(*i as f64),
                    other => return Err(format!("tensor() needs Float elements, found {}", other.type_name())),
                }
            }
            Ok(Value::Tensor(Rc::new(data)))
        }
        ("zeros", Value::Int(n)) => {
            if *n < 0 {
                return Err("zeros() needs a non-negative size".into());
            }
            Ok(Value::Tensor(Rc::new(vec![0.0; *n as usize])))
        }
        ("arange", Value::Int(n)) => {
            if *n < 0 {
                return Err("arange() needs a non-negative size".into());
            }
            Ok(Value::Tensor(Rc::new((0..*n).map(|i| i as f64).collect())))
        }
        _ => Err(format!("invalid argument for {name}()")),
    }
}

fn builtin_method(
    recv: Value,
    name: &str,
    args: Vec<Value>,
    span: Span,
    interp: &mut Interp,
) -> EvalResult {
    let mut call = |f: Value, args: Vec<Value>| -> Result<Value, String> {
        match interp.call_value(f, args, span) {
            Ok(v) => Ok(v),
            Err(Flow::Panic { msg, .. }) => Err(msg),
            Err(_) => Err("invalid control flow in callback".into()),
        }
    };
    match shared_method(&recv, name, args, &mut call) {
        Ok(v) => Ok(v),
        Err(msg) => Err(Flow::Panic { msg, span }),
    }
}
