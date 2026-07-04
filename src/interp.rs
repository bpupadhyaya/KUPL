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
        (Value::List(items), "map") => {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(call(f.clone(), vec![item.clone()])?);
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "filter") => {
            let f = args.into_iter().next().ok_or("`filter` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    out.push(item.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
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
