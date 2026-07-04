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
        diags: Vec::new(),
        fun_names: funs.iter().map(|f| f.name.clone()).collect(),
    };

    for (i, f) in funs.iter().enumerate() {
        let chunk = compile_fun(&mut shared, f);
        shared.module.chunks[i] = chunk;
    }

    if shared.diags.is_empty() {
        Ok(shared.module)
    } else {
        Err(shared.diags)
    }
}

struct Shared {
    module: Module,
    ctor_idx: HashMap<String, u16>,
    ctor_fields: HashMap<String, Vec<String>>,
    diags: Vec<Diag>,
    fun_names: HashSet<String>,
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

struct FnCompiler<'s> {
    shared: &'s mut Shared,
    chunk: Chunk,
    /// scope stack of (name, reg)
    scopes: Vec<Vec<(String, Reg)>>,
    next: u16,
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
            scopes: vec![Vec::new()],
            next: 0,
            loops: Vec::new(),
            pending_continues: Vec::new(),
            too_large: false,
        }
    }

    fn finish(mut self) -> Chunk {
        self.chunk.nregs = self.next.max(1);
        self.chunk
    }

    fn err(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.shared.diags.push(Diag::error(code, msg, span));
    }

    fn alloc(&mut self, span: Span) -> Reg {
        let r = self.next;
        self.next += 1;
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
    fn block(&mut self, b: &Block) -> Option<Reg> {
        self.scopes.push(Vec::new());
        let mut last: Option<Reg> = None;
        for stmt in &b.stmts {
            last = self.stmt(stmt);
        }
        self.scopes.pop();
        last
    }

    fn stmt(&mut self, stmt: &Stmt) -> Option<Reg> {
        match stmt {
            Stmt::Let { name, init, span, .. } => {
                let r = self.expr(init);
                let local = self.bind_local(name);
                self.emit(Op::Move(local, r), *span);
                None
            }
            Stmt::Assign { target, op, value, span } => {
                let rhs = self.expr(value);
                let ExprKind::Ident(name) = &target.kind else {
                    self.err("K0803", "unsupported assignment target on KVM", *span);
                    return None;
                };
                let Some(local) = self.lookup(name) else {
                    self.err(
                        "K0803",
                        format!("cannot assign to `{name}` on KVM (captured or component state)"),
                        *span,
                    );
                    return None;
                };
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
            Stmt::Emit { span, .. } => {
                self.err(
                    "K0802",
                    "components are not yet supported by the KVM backend (run without --vm)",
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
                let r = self.block(b);
                r.unwrap_or_else(|| self.const_reg(Value::Unit, span))
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
        ExprKind::Try(inner) | ExprKind::Await(inner) => free_vars_expr(inner, bound, free),
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
