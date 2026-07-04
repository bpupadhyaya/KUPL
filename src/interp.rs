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
                Item::Type(_) => {}
            }
        }
        let ctors = checked
            .ctors
            .iter()
            .map(|(name, (ty, fields))| {
                (name.clone(), (ty.clone(), fields.iter().map(|(n, _)| n.clone()).collect()))
            })
            .collect();
        ProgramDb { funs, components, contracts, ctors }
    }
}

pub struct Instance {
    pub comp: Rc<ComponentDecl>,
    /// Props + state (+ children) — the instance's private heap.
    pub env: Env,
    /// out port -> [(target instance, target in port)]
    pub wires: HashMap<String, Vec<(usize, String)>>,
    pub last_emit: HashMap<String, Value>,
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
        }
        self.drain()?;
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
                    self.run_handler(id, h, value.clone())?;
                }
            }
        }
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
            Stmt::Break(_) => Err(Flow::Break),
            Stmt::Continue(_) => Err(Flow::Continue),
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

// ---------------- operators, patterns, builtin methods ----------------

fn binary_op(op: BinOp, l: Value, r: Value, span: Span) -> EvalResult {
    use BinOp::*;
    let overflow = |what: &str| Flow::Panic {
        msg: format!("integer overflow in {what}"),
        span,
    };
    match op {
        Eq => return Ok(Value::Bool(l == r)),
        Ne => return Ok(Value::Bool(l != r)),
        _ => {}
    }
    match (&l, &r) {
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            Ok(match op {
                Add => Value::Int(a.checked_add(b).ok_or_else(|| overflow("addition"))?),
                Sub => Value::Int(a.checked_sub(b).ok_or_else(|| overflow("subtraction"))?),
                Mul => Value::Int(a.checked_mul(b).ok_or_else(|| overflow("multiplication"))?),
                Div => {
                    if b == 0 {
                        return Err(Flow::Panic { msg: "division by zero".into(), span });
                    }
                    Value::Int(a.checked_div(b).ok_or_else(|| overflow("division"))?)
                }
                Rem => {
                    if b == 0 {
                        return Err(Flow::Panic { msg: "remainder by zero".into(), span });
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
        (Value::Str(a), Value::Str(b)) => match op {
            Add => Ok(Value::str(format!("{a}{b}"))),
            Lt => Ok(Value::Bool(a < b)),
            Le => Ok(Value::Bool(a <= b)),
            Gt => Ok(Value::Bool(a > b)),
            Ge => Ok(Value::Bool(a >= b)),
            _ => Err(Flow::Panic { msg: "invalid string operation".into(), span }),
        },
        _ => Err(Flow::Panic {
            msg: format!(
                "invalid operand types: {} and {}",
                l.type_name(),
                r.type_name()
            ),
            span,
        }),
    }
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

fn builtin_method(
    recv: Value,
    name: &str,
    args: Vec<Value>,
    span: Span,
    interp: &mut Interp,
) -> EvalResult {
    let panic = |msg: String| Flow::Panic { msg, span };
    match (&recv, name) {
        (Value::List(items), "len") => Ok(Value::Int(items.len() as i64)),
        (Value::List(items), "map") => {
            let f = args.into_iter().next().ok_or_else(|| panic("`map` needs a function".into()))?;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(interp.call_value(f.clone(), vec![item.clone()], span)?);
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "filter") => {
            let f = args.into_iter().next().ok_or_else(|| panic("`filter` needs a function".into()))?;
            let mut out = Vec::new();
            for item in items.iter() {
                if let Value::Bool(true) = interp.call_value(f.clone(), vec![item.clone()], span)? {
                    out.push(item.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "find") => {
            let f = args.into_iter().next().ok_or_else(|| panic("`find` needs a function".into()))?;
            for item in items.iter() {
                if let Value::Bool(true) = interp.call_value(f.clone(), vec![item.clone()], span)? {
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
                            .ok_or_else(|| panic("integer overflow in sum".into()))?
                    }
                    Value::Float(v) => {
                        is_float = true;
                        float_sum += v;
                    }
                    other => return Err(panic(format!("cannot sum {}", other.type_name()))),
                }
            }
            if is_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        (Value::List(items), "contains") => {
            let needle = args.into_iter().next().ok_or_else(|| panic("`contains` needs a value".into()))?;
            Ok(Value::Bool(items.iter().any(|v| *v == needle)))
        }
        (Value::List(items), "push") => {
            let v = args.into_iter().next().ok_or_else(|| panic("`push` needs a value".into()))?;
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
                _ => return Err(panic("`join` needs a Str separator".into())),
            };
            let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
            Ok(Value::str(parts.join(&sep)))
        }
        (Value::Str(s), "len") => Ok(Value::Int(s.chars().count() as i64)),
        (Value::Str(s), "contains") => match args.into_iter().next() {
            Some(Value::Str(n)) => Ok(Value::Bool(s.contains(n.as_str()))),
            _ => Err(panic("`contains` needs a Str".into())),
        },
        (Value::Str(s), "starts_with") => match args.into_iter().next() {
            Some(Value::Str(n)) => Ok(Value::Bool(s.starts_with(n.as_str()))),
            _ => Err(panic("`starts_with` needs a Str".into())),
        },
        (Value::Str(s), "to_upper") => Ok(Value::str(s.to_uppercase())),
        (Value::Str(s), "to_lower") => Ok(Value::str(s.to_lowercase())),
        (Value::Str(s), "trim") => Ok(Value::str(s.trim().to_string())),
        (Value::Str(s), "split") => match args.into_iter().next() {
            Some(Value::Str(sep)) => Ok(Value::List(Rc::new(
                s.split(sep.as_str()).map(Value::str).collect(),
            ))),
            _ => Err(panic("`split` needs a Str separator".into())),
        },
        (Value::Int(v), "to_str") => Ok(Value::str(v.to_string())),
        (Value::Int(v), "to_float") => Ok(Value::Float(*v as f64)),
        (Value::Int(v), "abs") => v
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| panic("integer overflow in abs".into())),
        (Value::Float(v), "to_str") => Ok(Value::str(v.to_string())),
        (Value::Float(v), "to_int") => Ok(Value::Int(*v as i64)),
        (Value::Float(v), "abs") => Ok(Value::Float(v.abs())),
        (Value::Float(v), "sqrt") => Ok(Value::Float(v.sqrt())),
        (Value::Ctor { variant, .. }, "is_some") => Ok(Value::Bool(variant.as_str() == "Some")),
        (Value::Ctor { variant, .. }, "is_none") => Ok(Value::Bool(variant.as_str() == "None")),
        (Value::Ctor { variant, .. }, "is_ok") => Ok(Value::Bool(variant.as_str() == "Ok")),
        (Value::Ctor { variant, .. }, "is_err") => Ok(Value::Bool(variant.as_str() == "Err")),
        (Value::Ctor { variant, fields, .. }, "unwrap_or") => {
            let default = args.into_iter().next().ok_or_else(|| panic("`unwrap_or` needs a default".into()))?;
            match variant.as_str() {
                "Some" | "Ok" => Ok(fields.first().cloned().unwrap_or(Value::Unit)),
                _ => Ok(default),
            }
        }
        (other, _) => Err(panic(format!("{} has no method `{name}`", other.type_name()))),
    }
}
