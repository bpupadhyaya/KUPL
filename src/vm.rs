//! The KVM: a register-based virtual machine executing KUPL bytecode.
//!
//! Semantics are defined by the tree-walking interpreter; the VM shares its
//! operator (`raw_binary_op`) and method (`shared_method`) implementations, and
//! differential tests in this module assert both engines agree.

use std::rc::Rc;

use crate::bytecode::*;
use crate::diag::Span;
use crate::interp::{raw_binary_op, shared_method};
use crate::value::Value;

#[derive(Debug)]
pub struct VmError {
    pub msg: String,
    pub span: Span,
}

struct Frame {
    chunk: u16,
    ip: usize,
    base: usize,
    /// Register in the CALLER's frame that receives the return value.
    dst: Reg,
    /// Component instance this frame executes for (handlers/exposes/init).
    inst: Option<usize>,
}

/// One armed timer on a VM instance (mirrors the interpreter's TimerState).
struct VmTimer {
    chunk: u16,
    every: bool,
    interval: i64,
    next_fire: i64,
    active: bool,
}

/// A live component instance: slots hold props, then state, then children.
struct VmInstance {
    comp: u16,
    slots: Vec<Value>,
    /// out port -> [(target instance, target in port)]
    wires: std::collections::HashMap<String, Vec<(usize, String)>>,
    restart_on_failure: bool,
    timers: Vec<VmTimer>,
}

pub struct Vm<'m> {
    module: &'m Module,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    instances: Vec<VmInstance>,
    queue: std::collections::VecDeque<(usize, String, Value)>,
    pub print_unwired: bool,
    /// Virtual clock (ms), advanced explicitly — same model as the interpreter.
    now: i64,
    /// Send+Sync program snapshot enabling the real-thread `par_map`/`par_filter`
    /// fast path. Set on source runs (`run --vm`); `None` for a bare `.kx` run
    /// (no AST) → sequential, and left `None` in the differential harness so the
    /// KVM stays the sequential reference that proves the parallel path correct.
    image: Option<std::sync::Arc<crate::parallel::ProgramImage>>,
}

impl<'m> Vm<'m> {
    pub fn new(module: &'m Module) -> Self {
        Vm {
            module,
            stack: Vec::new(),
            frames: Vec::new(),
            instances: Vec::new(),
            queue: std::collections::VecDeque::new(),
            print_unwired: false,
            now: 0,
            image: None,
        }
    }

    /// Enable the real-thread parallel fast path (`par_map`/`par_filter` over
    /// pure named callbacks). Only source runs, which have the AST, set this.
    pub fn set_image(&mut self, image: std::sync::Arc<crate::parallel::ProgramImage>) {
        self.image = Some(image);
    }

    pub fn call_named(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let Some(&idx) = self.module.funs.get(name) else {
            return Err(VmError { msg: format!("no function `{name}`"), span: Span::default() });
        };
        let depth = self.frames.len();
        self.push_frame(idx, &args, 0, None)?;
        self.run(depth)
    }

    /// Re-entrantly call a top-level function by name (used by the ai tool host).
    fn call_fun_nested(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let Some(&idx) = self.module.funs.get(name) else {
            return Err(VmError { msg: format!("no function `{name}`"), span: Span::default() });
        };
        self.call_chunk_nested(idx, args, None)
    }

    /// Instantiate the app component, deliver `on start` to every instance in
    /// creation order, then drain the message queue to quiescence.
    pub fn run_app(&mut self, app: &str) -> Result<(), VmError> {
        let Some(&idx) = self.module.component_names.get(app) else {
            return Err(VmError { msg: format!("no component `{app}`"), span: Span::default() });
        };
        self.instantiate(idx, Vec::new())?;
        for id in 0..self.instances.len() {
            self.run_lifecycle(id, "@start")?;
            self.arm_timers(id);
        }
        self.drain()?;
        self.run_timers(100)?;
        Ok(())
    }

    fn run_lifecycle(&mut self, id: usize, key: &str) -> Result<(), VmError> {
        let meta = &self.module.components[self.instances[id].comp as usize];
        let handler = meta
            .handlers
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, chunk, has_param)| (*chunk, *has_param));
        if let Some((chunk, _)) = handler {
            self.call_chunk_nested(chunk, Vec::new(), Some(id))?;
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), VmError> {
        let mut processed: u64 = 0;
        while let Some((id, port, value)) = self.queue.pop_front() {
            processed += 1;
            if processed > crate::interp::MAX_COMPONENT_MESSAGES {
                return Err(VmError {
                    msg: format!(
                        "component message limit exceeded ({}) — a `wire` cycle?",
                        crate::interp::MAX_COMPONENT_MESSAGES
                    ),
                    span: Span::default(),
                });
            }
            let meta = &self.module.components[self.instances[id].comp as usize];
            let handler = meta
                .handlers
                .iter()
                .find(|(k, _, _)| *k == port)
                .map(|(_, chunk, has_param)| (*chunk, *has_param));
            if let Some((chunk, has_param)) = handler {
                let args = if has_param { vec![value] } else { Vec::new() };
                match self.call_chunk_nested(chunk, args, Some(id)) {
                    Ok(_) => {}
                    Err(e) if self.instances[id].restart_on_failure => {
                        self.restart(id, &e.msg)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    /// Supervision restart: reset state (restart chunk), re-run `on start`.
    fn restart(&mut self, id: usize, panic_msg: &str) -> Result<(), VmError> {
        let meta = &self.module.components[self.instances[id].comp as usize];
        let name = meta.name.clone();
        let restart_chunk = meta.restart_chunk;
        eprintln!("[supervise] {name} restarted after panic: {panic_msg}");
        self.call_chunk_nested(restart_chunk, Vec::new(), Some(id))?;
        self.run_lifecycle(id, "@start")?;
        self.arm_timers(id);
        Ok(())
    }

    /// Create an instance: fill props (running default chunks for gaps is the
    /// compiler's job — args arrive complete), zero the state, run the init chunk.
    fn instantiate(&mut self, comp_idx: u16, props: Vec<Value>) -> Result<usize, VmError> {
        let meta = &self.module.components[comp_idx as usize];
        let init = meta.init_chunk;
        let mut slots = props;
        slots.resize(meta.nslots as usize, Value::Unit);
        let id = self.instances.len();
        self.instances.push(VmInstance {
            comp: comp_idx,
            slots,
            wires: std::collections::HashMap::new(),
            restart_on_failure: false,
            timers: Vec::new(),
        });
        self.call_chunk_nested(init, Vec::new(), Some(id))?;
        Ok(id)
    }

    /// Arm the instance's timers relative to the current virtual time.
    fn arm_timers(&mut self, id: usize) {
        let now = self.now;
        let comp = self.instances[id].comp as usize;
        let timers: Vec<VmTimer> = self.module.components[comp]
            .timers
            .iter()
            .map(|t| VmTimer {
                chunk: t.chunk,
                every: t.every,
                interval: t.interval_ms,
                next_fire: now + t.interval_ms,
                active: true,
            })
            .collect();
        self.instances[id].timers = timers;
    }

    /// Advance the virtual clock, firing due timers in time order (ties broken
    /// by instance then declaration order) — identical semantics to the interp.
    pub fn advance(&mut self, dur: i64) -> Result<(), VmError> {
        if dur < 0 {
            return Err(VmError { msg: "cannot advance the clock by a negative duration".into(), span: Span::default() });
        }
        let target = self.now + dur;
        loop {
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
            let chunk = self.instances[iid].timers[ti].chunk;
            match self.call_chunk_nested(chunk, Vec::new(), Some(iid)) {
                Ok(_) => {}
                Err(e) if self.instances[iid].restart_on_failure => self.restart(iid, &e.msg)?,
                Err(e) => return Err(e),
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

    /// For `kupl run`: bounded timer firing (mirrors `Interp::run_timers`).
    pub fn run_timers(&mut self, max_fires: usize) -> Result<(), VmError> {
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

    /// Instantiate a component by name (props must be complete). Public for
    /// tests and future law-running on the VM.
    pub fn instantiate_named(&mut self, name: &str, props: Vec<Value>) -> Result<usize, VmError> {
        let Some(&idx) = self.module.component_names.get(name) else {
            return Err(VmError { msg: format!("no component `{name}`"), span: Span::default() });
        };
        let id = self.instantiate(idx, props)?;
        self.run_lifecycle(id, "@start")?;
        self.arm_timers(id);
        self.drain()?;
        Ok(id)
    }

    /// Call an exposed function on a live instance.
    pub fn call_expose(&mut self, id: usize, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let meta = &self.module.components[self.instances[id].comp as usize];
        let Some(&chunk) = meta.exposes.get(name) else {
            return Err(VmError {
                msg: format!("component `{}` does not expose `{name}`", meta.name),
                span: Span::default(),
            });
        };
        let v = self.call_chunk_nested(chunk, args, Some(id))?;
        self.drain()?;
        Ok(v)
    }

    /// Send a message to an instance's in port and drain to quiescence.
    pub fn send(&mut self, id: usize, port: &str, value: Value) -> Result<(), VmError> {
        self.queue.push_back((id, port.to_string(), value));
        self.drain()
    }

    /// Run a chunk to completion re-entrantly and return its value.
    fn call_chunk_nested(
        &mut self,
        chunk: u16,
        args: Vec<Value>,
        inst: Option<usize>,
    ) -> Result<Value, VmError> {
        let depth = self.frames.len();
        let stack_len = self.stack.len();
        self.push_frame(chunk, &args, 0, inst)?;
        match self.run(depth) {
            Ok(v) => Ok(v),
            Err(e) => {
                // unwind so supervision restarts leave the VM consistent
                self.frames.truncate(depth);
                self.stack.truncate(stack_len);
                Err(e)
            }
        }
    }

    fn chunk(&self, idx: u16) -> &'m Chunk {
        &self.module.chunks[idx as usize]
    }

    fn push_frame(
        &mut self,
        chunk_idx: u16,
        args: &[Value],
        dst: Reg,
        inst: Option<usize>,
    ) -> Result<(), VmError> {
        let chunk = self.chunk(chunk_idx);
        let expected = chunk.nparams as usize;
        if args.len() != expected {
            return Err(VmError {
                msg: format!("`{}` takes {expected} argument(s), {} given", chunk.name, args.len()),
                span: Span::default(),
            });
        }
        let base = self.stack.len();
        self.stack.resize(base + chunk.nregs as usize, Value::Unit);
        for (i, a) in args.iter().enumerate() {
            self.stack[base + chunk.ncaps as usize + i] = a.clone();
        }
        if self.frames.len() >= 10_000 {
            return Err(VmError { msg: "stack overflow (10000 frames)".into(), span: Span::default() });
        }
        self.frames.push(Frame { chunk: chunk_idx, ip: 0, base, dst, inst });
        Ok(())
    }

    fn push_closure_frame(
        &mut self,
        proto: u16,
        captures: &[Value],
        args: &[Value],
        dst: Reg,
        inst: Option<usize>,
    ) -> Result<(), VmError> {
        self.push_frame(proto, args, dst, inst)?;
        let base = self.frames.last().unwrap().base;
        for (i, c) in captures.iter().enumerate() {
            self.stack[base + i] = c.clone();
        }
        Ok(())
    }

    /// Call a callable value re-entrantly (used by Method callbacks).
    fn call_value_nested(&mut self, f: Value, args: Vec<Value>) -> Result<Value, String> {
        let depth = self.frames.len();
        let inst = self.frames.last().and_then(|f| f.inst);
        match f {
            Value::Fun(name) => {
                let Some(&idx) = self.module.funs.get(name.as_str()) else {
                    return Err(format!("no function `{name}`"));
                };
                self.push_frame(idx, &args, 0, None).map_err(|e| e.msg)?;
            }
            Value::VmClosure(proto, caps) => {
                self.push_closure_frame(proto, &caps, &args, 0, inst).map_err(|e| e.msg)?;
            }
            other => return Err(format!("{} is not callable", other.type_name())),
        }
        self.run(depth).map_err(|e| e.msg)
    }

    /// Execute until the frame stack returns to `stop_depth`; the final `Ret`
    /// value is returned to the caller.
    fn run(&mut self, stop_depth: usize) -> Result<Value, VmError> {
        macro_rules! frame {
            () => {
                self.frames.last_mut().unwrap()
            };
        }
        loop {
            let (chunk_idx, ip, base, cur_inst) = {
                let f = frame!();
                (f.chunk, f.ip, f.base, f.inst)
            };
            let chunk = self.chunk(chunk_idx);
            let op = chunk.code[ip].clone();
            let span = chunk.spans[ip];
            frame!().ip += 1;

            macro_rules! reg {
                ($r:expr) => {
                    self.stack[base + $r as usize].clone()
                };
            }
            macro_rules! set {
                ($r:expr, $v:expr) => {
                    self.stack[base + $r as usize] = $v
                };
            }
            macro_rules! bin {
                ($dst:expr, $a:expr, $b:expr, $op:expr) => {{
                    let l = reg!($a);
                    let r = reg!($b);
                    match raw_binary_op($op, &l, &r) {
                        Ok(v) => set!($dst, v),
                        Err(msg) => {
                            // operator overloading: a user value falls back to a
                            // top-level operator function (`a + b` -> `add(a, b)`)
                            let overload = matches!(l, Value::Ctor { .. })
                                .then(|| crate::interp::op_overload_name($op))
                                .flatten()
                                .filter(|f| self.module.funs.contains_key(*f));
                            match overload {
                                Some(fname) => {
                                    let f = Value::Fun(std::rc::Rc::new(fname.to_string()));
                                    match self.call_value_nested(f, vec![l, r]) {
                                        Ok(v) => set!($dst, v),
                                        Err(msg) => return Err(VmError { msg, span }),
                                    }
                                }
                                None => return Err(VmError { msg, span }),
                            }
                        }
                    }
                }};
            }

            use crate::ast::BinOp as B;
            match op {
                Op::Const(dst, idx) => set!(dst, chunk.consts[idx as usize].clone()),
                Op::Move(dst, src) => {
                    let v = reg!(src);
                    set!(dst, v);
                }
                Op::Add(d, a, b) => {
                    // Self-append fast path for `s = s + x` (compiled as Add(s, s, x)):
                    // append x's rendering to the uniquely-owned Str in place instead
                    // of reallocating — O(n^2) -> O(n). A shared string rebuilds, so
                    // value semantics hold. All non-(Str+Str) cases use the shared op.
                    let both_str = matches!(&self.stack[base + a as usize], Value::Str(_))
                        && matches!(&self.stack[base + b as usize], Value::Str(_));
                    if d == a && both_str {
                        let r = reg!(b);
                        let slot = &mut self.stack[base + a as usize];
                        if let Value::Str(rc) = slot {
                            if Rc::get_mut(rc).is_some() {
                                use std::fmt::Write as _;
                                let _ = write!(Rc::get_mut(rc).unwrap(), "{r}");
                            } else {
                                let l = rc.clone();
                                *slot = Value::str(format!("{l}{r}"));
                            }
                        }
                    } else {
                        bin!(d, a, b, B::Add);
                    }
                }
                Op::Sub(d, a, b) => bin!(d, a, b, B::Sub),
                Op::Mul(d, a, b) => bin!(d, a, b, B::Mul),
                Op::Div(d, a, b) => bin!(d, a, b, B::Div),
                Op::Rem(d, a, b) => bin!(d, a, b, B::Rem),
                Op::Eq(d, a, b) => bin!(d, a, b, B::Eq),
                Op::Ne(d, a, b) => bin!(d, a, b, B::Ne),
                Op::Lt(d, a, b) => bin!(d, a, b, B::Lt),
                Op::Le(d, a, b) => bin!(d, a, b, B::Le),
                Op::Gt(d, a, b) => bin!(d, a, b, B::Gt),
                Op::Ge(d, a, b) => bin!(d, a, b, B::Ge),
                Op::Neg(d, a) => match reg!(a) {
                    Value::Int(v) => match v.checked_neg() {
                        Some(n) => set!(d, Value::Int(n)),
                        None => return Err(VmError { msg: "integer overflow in negation".into(), span }),
                    },
                    Value::Float(v) => set!(d, Value::Float(-v)),
                    other => {
                        return Err(VmError {
                            msg: format!("cannot negate {}", other.type_name()),
                            span,
                        })
                    }
                },
                Op::Not(d, a) => match reg!(a) {
                    Value::Bool(v) => set!(d, Value::Bool(!v)),
                    other => {
                        return Err(VmError { msg: format!("cannot `!` {}", other.type_name()), span })
                    }
                },
                Op::Jump(t) => frame!().ip = t,
                Op::JumpIfFalse(r, t) => match reg!(r) {
                    Value::Bool(false) => frame!().ip = t,
                    Value::Bool(true) => {}
                    other => {
                        return Err(VmError {
                            msg: format!("condition must be Bool, found {}", other.type_name()),
                            span,
                        })
                    }
                },
                Op::JumpIfTrue(r, t) => match reg!(r) {
                    Value::Bool(true) => frame!().ip = t,
                    Value::Bool(false) => {}
                    other => {
                        return Err(VmError {
                            msg: format!("condition must be Bool, found {}", other.type_name()),
                            span,
                        })
                    }
                },
                Op::Call { dst, fun, start, argc } => {
                    let args: Vec<Value> =
                        (0..argc).map(|i| reg!(start + i)).collect();
                    self.push_frame(fun, &args, dst, None).map_err(|mut e| {
                        e.span = span;
                        e
                    })?;
                }
                Op::CallComp { dst, fun, start, argc } => {
                    let args: Vec<Value> =
                        (0..argc).map(|i| reg!(start + i)).collect();
                    self.push_frame(fun, &args, dst, cur_inst).map_err(|mut e| {
                        e.span = span;
                        e
                    })?;
                }
                Op::CallAi { dst, info, intent } => {
                    // `module` is &'m, independent of the &mut self we pass as
                    // the tool host below.
                    let module = self.module;
                    let Some(meta) = module.ai_funs.get(info as usize) else {
                        return Err(VmError { msg: "unknown ai fun".into(), span });
                    };
                    let intent_str = reg!(intent).to_string();
                    let args: Vec<Value> =
                        (0..chunk.nparams).map(|i| reg!(chunk.ncaps + i)).collect();
                    match crate::ai::ai_call(meta, &intent_str, &args, self) {
                        Ok(v) => set!(dst, v),
                        Err(msg) => return Err(VmError { msg, span }),
                    }
                }
                Op::CallBuiltin { dst, which, start, argc } => {
                    let args: Vec<Value> = (0..argc).map(|i| reg!(start + i)).collect();
                    match which {
                        BUILTIN_PRINT => {
                            println!("{}", args[0]);
                            set!(dst, Value::Unit);
                        }
                        BUILTIN_TO_STR => set!(dst, Value::str(args[0].to_string())),
                        BUILTIN_MAP_NEW => set!(dst, Value::Map(Rc::new(Vec::new()))),
                        BUILTIN_SET_NEW => set!(dst, Value::Set(Rc::new(Vec::new()))),
                        BUILTIN_SET_FROM => match crate::interp::set_from_list(&args[0]) {
                            Ok(v) => set!(dst, v),
                            Err(msg) => return Err(VmError { msg, span }),
                        },
                        BUILTIN_TENSOR | BUILTIN_ZEROS | BUILTIN_ARANGE => {
                            let name = match which {
                                BUILTIN_TENSOR => "tensor",
                                BUILTIN_ZEROS => "zeros",
                                _ => "arange",
                            };
                            match crate::interp::tensor_builtin(name, &args[0]) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_PANIC => {
                            return Err(VmError { msg: args[0].to_string(), span })
                        }
                        BUILTIN_READ_FILE | BUILTIN_WRITE_FILE | BUILTIN_APPEND_FILE
                        | BUILTIN_DELETE_FILE | BUILTIN_FILE_EXISTS | BUILTIN_LIST_DIR
                        | BUILTIN_MAKE_DIR | BUILTIN_REMOVE_DIR => {
                            let name = match which {
                                BUILTIN_READ_FILE => "read_file",
                                BUILTIN_WRITE_FILE => "write_file",
                                BUILTIN_APPEND_FILE => "append_file",
                                BUILTIN_DELETE_FILE => "delete_file",
                                BUILTIN_LIST_DIR => "list_dir",
                                BUILTIN_MAKE_DIR => "make_dir",
                                BUILTIN_REMOVE_DIR => "remove_dir",
                                _ => "file_exists",
                            };
                            match crate::interp::fs_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_JSON_PARSE => {
                            let s = match &args[0] {
                                Value::Str(s) => s.as_str().to_string(),
                                other => other.to_string(),
                            };
                            set!(dst, match crate::json::parse(&s) {
                                Ok(j) => Value::ok(j),
                                Err(e) => Value::err(Value::str(e)),
                            });
                        }
                        BUILTIN_JSON_STRINGIFY => match crate::json::stringify(&args[0]) {
                            Ok(s) => set!(dst, Value::str(s)),
                            Err(msg) => return Err(VmError { msg, span }),
                        },
                        BUILTIN_ENV_VAR | BUILTIN_ARGS | BUILTIN_EPRINT
                        | BUILTIN_READ_LINE | BUILTIN_READ_ALL => {
                            let name = match which {
                                BUILTIN_ENV_VAR => "env_var",
                                BUILTIN_ARGS => "args",
                                BUILTIN_READ_LINE => "read_line",
                                BUILTIN_READ_ALL => "read_all",
                                _ => "eprint",
                            };
                            match crate::interp::proc_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_EXIT => {
                            let code = match args.first() {
                                Some(Value::Int(n)) => *n as i32,
                                _ => 0,
                            };
                            use std::io::Write;
                            std::io::stdout().flush().ok();
                            std::process::exit(code);
                        }
                        BUILTIN_RANDOM_INTS | BUILTIN_RANDOM_FLOATS | BUILTIN_SHUFFLE => {
                            let name = match which {
                                BUILTIN_RANDOM_INTS => "random_ints",
                                BUILTIN_RANDOM_FLOATS => "random_floats",
                                _ => "shuffle",
                            };
                            match crate::interp::random_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_EXEC => match crate::interp::exec_builtin(&args) {
                            Ok(v) => set!(dst, v),
                            Err(msg) => return Err(VmError { msg, span }),
                        },
                        BUILTIN_HTTP_SERVE => {
                            let port = match &args[0] {
                                Value::Int(n) => *n,
                                _ => return Err(VmError { msg: "http_serve port must be an Int".into(), span }),
                            };
                            let handler = args[1].clone();
                            let mut call = |m: String, p: String| -> Result<String, String> {
                                self.call_value_nested(handler.clone(), vec![Value::str(m), Value::str(p)])
                                    .map(|v| v.to_string())
                            };
                            let v = match crate::interp::serve_http(port, &mut call) {
                                Ok(()) => Value::ok(Value::Unit),
                                Err(e) => Value::err(Value::str(e)),
                            };
                            set!(dst, v);
                        }
                        BUILTIN_HTTP_GET | BUILTIN_HTTP_POST => {
                            let name = if which == BUILTIN_HTTP_GET { "http_get" } else { "http_post" };
                            match crate::interp::http_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_RE_MATCH | BUILTIN_RE_FIND | BUILTIN_RE_FIND_ALL
                        | BUILTIN_RE_REPLACE => {
                            let name = match which {
                                BUILTIN_RE_MATCH => "re_match",
                                BUILTIN_RE_FIND => "re_find",
                                BUILTIN_RE_FIND_ALL => "re_find_all",
                                _ => "re_replace",
                            };
                            match crate::interp::regex_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_FORMAT_TIME | BUILTIN_YEAR_OF | BUILTIN_MONTH_OF
                        | BUILTIN_DAY_OF | BUILTIN_HOUR_OF | BUILTIN_MINUTE_OF
                        | BUILTIN_SECOND_OF | BUILTIN_WEEKDAY_OF | BUILTIN_YEARDAY_OF
                        | BUILTIN_DATE_ISO | BUILTIN_PARSE_ISO | BUILTIN_DATE_MAKE => {
                            let name = match which {
                                BUILTIN_FORMAT_TIME => "format_time",
                                BUILTIN_YEAR_OF => "year_of",
                                BUILTIN_MONTH_OF => "month_of",
                                BUILTIN_DAY_OF => "day_of",
                                BUILTIN_HOUR_OF => "hour_of",
                                BUILTIN_MINUTE_OF => "minute_of",
                                BUILTIN_SECOND_OF => "second_of",
                                BUILTIN_WEEKDAY_OF => "weekday_of",
                                BUILTIN_YEARDAY_OF => "yearday_of",
                                BUILTIN_DATE_ISO => "date_iso",
                                BUILTIN_PARSE_ISO => "parse_iso",
                                _ => "date_make",
                            };
                            match crate::interp::time_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_NOW => set!(dst, Value::Int(crate::interp::now_seconds())),
                        BUILTIN_BASE64_ENCODE | BUILTIN_BASE64_DECODE | BUILTIN_HEX_ENCODE
                        | BUILTIN_HEX_DECODE | BUILTIN_HASH_FNV => {
                            let name = match which {
                                BUILTIN_BASE64_ENCODE => "base64_encode",
                                BUILTIN_BASE64_DECODE => "base64_decode",
                                BUILTIN_HEX_ENCODE => "hex_encode",
                                BUILTIN_HEX_DECODE => "hex_decode",
                                _ => "hash_fnv",
                            };
                            match crate::interp::encoding_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_BIG => match crate::interp::big_builtin(&args[0]) {
                            Ok(v) => set!(dst, v),
                            Err(msg) => return Err(VmError { msg, span }),
                        },
                        BUILTIN_RAT => match crate::interp::rat_builtin(&args[0], &args[1]) {
                            Ok(v) => set!(dst, v),
                            Err(msg) => return Err(VmError { msg, span }),
                        },
                        BUILTIN_PATH_JOIN | BUILTIN_PATH_BASE | BUILTIN_PATH_DIR
                        | BUILTIN_PATH_EXT => {
                            let name = match which {
                                BUILTIN_PATH_JOIN => "path_join",
                                BUILTIN_PATH_BASE => "path_base",
                                BUILTIN_PATH_DIR => "path_dir",
                                _ => "path_ext",
                            };
                            match crate::interp::path_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_CSV_PARSE | BUILTIN_CSV_STRINGIFY => {
                            let name = if which == BUILTIN_CSV_PARSE { "csv_parse" } else { "csv_stringify" };
                            match crate::interp::csv_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        BUILTIN_URL_ENCODE | BUILTIN_URL_DECODE | BUILTIN_QUERY_PARSE
                        | BUILTIN_QUERY_BUILD => {
                            let name = match which {
                                BUILTIN_URL_ENCODE => "url_encode",
                                BUILTIN_URL_DECODE => "url_decode",
                                BUILTIN_QUERY_PARSE => "query_parse",
                                _ => "query_build",
                            };
                            match crate::interp::url_builtin(name, &args) {
                                Ok(v) => set!(dst, v),
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                        _ => return Err(VmError { msg: "unknown builtin".into(), span }),
                    }
                }
                Op::CallValue { dst, f, start, argc } => {
                    let callee = reg!(f);
                    let args: Vec<Value> = (0..argc).map(|i| reg!(start + i)).collect();
                    match callee {
                        Value::Fun(name) => {
                            let Some(&idx) = self.module.funs.get(name.as_str()) else {
                                return Err(VmError { msg: format!("no function `{name}`"), span });
                            };
                            self.push_frame(idx, &args, dst, None).map_err(|mut e| {
                                e.span = span;
                                e
                            })?;
                        }
                        Value::VmClosure(proto, caps) => {
                            self.push_closure_frame(proto, &caps, &args, dst, cur_inst)
                                .map_err(|mut e| {
                                    e.span = span;
                                    e
                                })?;
                        }
                        other => {
                            return Err(VmError {
                                msg: format!("{} is not callable", other.type_name()),
                                span,
                            })
                        }
                    }
                }
                Op::Method { dst, recv, name, start, argc } => {
                    let method = match &chunk.consts[name as usize] {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => return Err(VmError { msg: "bad method name".into(), span }),
                    };
                    // Self-push fast path (`xs = xs.push(x)`): the result overwrites
                    // the receiver register; push in place when the List is uniquely
                    // owned (O(n^2) -> O(n)). A shared list rebuilds a new one, so
                    // value semantics hold (an aliased list is never mutated).
                    if method == "push"
                        && argc == 1
                        && dst == recv
                        && matches!(&self.stack[base + recv as usize], Value::List(_))
                    {
                        let item = reg!(start);
                        if let Value::List(rc) = &mut self.stack[base + recv as usize] {
                            match Rc::get_mut(rc) {
                                Some(v) => v.push(item),
                                None => {
                                    let mut out = rc.as_ref().clone();
                                    out.push(item);
                                    *rc = Rc::new(out);
                                }
                            }
                        }
                        continue;
                    }
                    // Self-insert fast path (`m = m.insert(k, v)`): same in-place
                    // update of a uniquely-owned Map — O(n^2) build loop -> O(n).
                    if method == "insert"
                        && argc == 2
                        && dst == recv
                        && matches!(&self.stack[base + recv as usize], Value::Map(_))
                    {
                        let key = reg!(start);
                        let val = reg!(start + 1);
                        if let Value::Map(rc) = &mut self.stack[base + recv as usize] {
                            let pairs = match Rc::get_mut(rc) {
                                Some(p) => p,
                                None => {
                                    *rc = Rc::new(rc.as_ref().clone());
                                    Rc::get_mut(rc).unwrap()
                                }
                            };
                            match pairs.iter_mut().find(|(pk, _)| *pk == key) {
                                Some(pair) => pair.1 = val,
                                None => pairs.push((key, val)),
                            }
                        }
                        continue;
                    }
                    // Self-insert fast path (`s = s.insert(v)`, 1 arg): in-place dedup
                    // append on a uniquely-owned Set — O(n^2) build loop -> O(n).
                    if method == "insert"
                        && argc == 1
                        && dst == recv
                        && matches!(&self.stack[base + recv as usize], Value::Set(_))
                    {
                        let v = reg!(start);
                        if let Value::Set(rc) = &mut self.stack[base + recv as usize] {
                            let items = match Rc::get_mut(rc) {
                                Some(it) => it,
                                None => {
                                    *rc = Rc::new(rc.as_ref().clone());
                                    Rc::get_mut(rc).unwrap()
                                }
                            };
                            if !items.iter().any(|x| *x == v) {
                                items.push(v);
                            }
                        }
                        continue;
                    }
                    let r = reg!(recv);
                    let args: Vec<Value> = (0..argc).map(|i| reg!(start + i)).collect();
                    // expose call on a component instance
                    if let Value::Component(id) = r {
                        let meta = &self.module.components[self.instances[id].comp as usize];
                        let Some(&expose_chunk) = meta.exposes.get(&method) else {
                            return Err(VmError {
                                msg: format!("component `{}` does not expose `{method}`", meta.name),
                                span,
                            });
                        };
                        let v = self
                            .call_chunk_nested(expose_chunk, args, Some(id))
                            .map_err(|mut e| {
                                if e.span == Span::default() {
                                    e.span = span;
                                }
                                e
                            })?;
                        set!(dst, v);
                        continue;
                    }
                    // real-thread fast path (par_map/par_filter over pure named
                    // callbacks); falls through to sequential shared_method on
                    // any non-qualifying call. Same helper as the interpreter,
                    // so results are byte-identical by construction.
                    if let Some(image) = self.image.clone() {
                        if let Some(res) = crate::parallel::try_par_map(&r, &method, &args, &image)
                            .or_else(|| crate::parallel::try_par_filter(&r, &method, &args, &image))
                        {
                            match res {
                                Ok(v) => {
                                    set!(dst, v);
                                    continue;
                                }
                                Err(msg) => return Err(VmError { msg, span }),
                            }
                        }
                    }
                    // keep a copy for the UFCS fallback (only when a same-named
                    // top-level function exists — built-in methods win)
                    let ufcs = self.module.funs.get(&method).copied();
                    let backup = ufcs.map(|_| (r.clone(), args.clone()));
                    let mut call = |f: Value, args: Vec<Value>| self.call_value_nested(f, args);
                    match shared_method(&r, &method, args, &mut call) {
                        Ok(v) => set!(dst, v),
                        Err(msg) if backup.is_some() && msg.contains("has no method") => {
                            // UFCS: `recv.method(args)` -> `method(recv, args…)`
                            let (recv, margs) = backup.unwrap();
                            let mut full = Vec::with_capacity(margs.len() + 1);
                            full.push(recv);
                            full.extend(margs);
                            let v = self.call_chunk_nested(ufcs.unwrap(), full, None).map_err(|mut e| {
                                if e.span == Span::default() {
                                    e.span = span;
                                }
                                e
                            })?;
                            set!(dst, v);
                        }
                        Err(msg) => return Err(VmError { msg, span }),
                    }
                }
                Op::Ret(r) => {
                    let value = reg!(r);
                    let f = self.frames.pop().unwrap();
                    self.stack.truncate(f.base);
                    if self.frames.len() == stop_depth {
                        return Ok(value);
                    }
                    let caller = self.frames.last().unwrap();
                    let slot = caller.base + f.dst as usize;
                    self.stack[slot] = value;
                }
                Op::MakeList { dst, start, len } => {
                    let items: Vec<Value> = (0..len).map(|i| reg!(start + i)).collect();
                    set!(dst, Value::List(Rc::new(items)));
                }
                Op::MakeCtor { dst, ctor, start, len } => {
                    let meta = &self.module.ctors[ctor as usize];
                    let fields: Vec<Value> = (0..len).map(|i| reg!(start + i)).collect();
                    set!(
                        dst,
                        Value::Ctor {
                            ty: Rc::new(meta.type_name.clone()),
                            variant: Rc::new(meta.variant.clone()),
                            fields: Rc::new(fields),
                        }
                    );
                }
                Op::GetField { dst, obj, idx } => match reg!(obj) {
                    Value::Ctor { fields, .. } => match fields.get(idx as usize) {
                        Some(v) => set!(dst, v.clone()),
                        None => return Err(VmError { msg: "field index out of range".into(), span }),
                    },
                    other => {
                        return Err(VmError {
                            msg: format!("{} has no fields", other.type_name()),
                            span,
                        })
                    }
                },
                Op::GetFieldNamed { dst, obj, name } => {
                    let field = match &chunk.consts[name as usize] {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => return Err(VmError { msg: "bad field name".into(), span }),
                    };
                    match reg!(obj) {
                        Value::Ctor { variant, fields, ty } => {
                            let position = self
                                .module
                                .ctor_field_names
                                .get(variant.as_str())
                                .and_then(|fs| fs.iter().position(|f| f == &field));
                            match position.and_then(|i| fields.get(i)) {
                                Some(v) => set!(dst, v.clone()),
                                None => {
                                    return Err(VmError {
                                        msg: format!("`{ty}` value has no field `{field}`"),
                                        span,
                                    })
                                }
                            }
                        }
                        other => {
                            return Err(VmError {
                                msg: format!("{} has no fields", other.type_name()),
                                span,
                            })
                        }
                    }
                }
                Op::WithField { dst, obj, name, value } => {
                    let field = match &chunk.consts[name as usize] {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => return Err(VmError { msg: "bad field name".into(), span }),
                    };
                    match reg!(obj) {
                        Value::Ctor { ty, variant, fields } => {
                            let position = self
                                .module
                                .ctor_field_names
                                .get(variant.as_str())
                                .and_then(|fs| fs.iter().position(|f| f == &field));
                            match position {
                                Some(i) => {
                                    let mut new_fields = fields.as_ref().clone();
                                    new_fields[i] = reg!(value);
                                    set!(dst, Value::Ctor { ty, variant, fields: Rc::new(new_fields) });
                                }
                                None => {
                                    return Err(VmError {
                                        msg: format!("`{ty}` has no field `{field}`"),
                                        span,
                                    })
                                }
                            }
                        }
                        other => {
                            return Err(VmError {
                                msg: format!("{} has no fields to update", other.type_name()),
                                span,
                            })
                        }
                    }
                }
                Op::TagIs { dst, obj, ctor } => {
                    let meta = &self.module.ctors[ctor as usize];
                    let is = matches!(reg!(obj), Value::Ctor { variant, .. } if *variant == meta.variant);
                    set!(dst, Value::Bool(is));
                }
                Op::MakeClosure { dst, proto, start, ncaps } => {
                    let caps: Vec<Value> = (0..ncaps).map(|i| reg!(start + i)).collect();
                    set!(dst, Value::VmClosure(proto, Rc::new(caps)));
                }
                Op::MakeRange { dst, lo, hi, inclusive } => {
                    match (reg!(lo), reg!(hi)) {
                        (Value::Int(a), Value::Int(b)) => set!(dst, Value::Range(a, b, inclusive)),
                        _ => return Err(VmError { msg: "range bounds must be Int".into(), span }),
                    }
                }
                Op::IterLen(dst, x) => match reg!(x) {
                    Value::Range(a, b, incl) => {
                        let hi = if incl { b + 1 } else { b };
                        set!(dst, Value::Int((hi - a).max(0)));
                    }
                    Value::List(items) => set!(dst, Value::Int(items.len() as i64)),
                    other => {
                        return Err(VmError {
                            msg: format!("`for` needs a Range or List, found {}", other.type_name()),
                            span,
                        })
                    }
                },
                Op::IterGet { dst, iter, idx } => {
                    let i = match reg!(idx) {
                        Value::Int(i) => i,
                        _ => return Err(VmError { msg: "iterator index must be Int".into(), span }),
                    };
                    match reg!(iter) {
                        Value::Range(a, _, _) => set!(dst, Value::Int(a + i)),
                        Value::List(items) => match items.get(i as usize) {
                            Some(v) => set!(dst, v.clone()),
                            None => return Err(VmError { msg: "list index out of range".into(), span }),
                        },
                        other => {
                            return Err(VmError {
                                msg: format!("cannot iterate {}", other.type_name()),
                                span,
                            })
                        }
                    }
                }
                Op::ToStr(dst, src) => {
                    let v = reg!(src);
                    set!(dst, Value::str(v.to_string()));
                }
                Op::Concat(dst, a, b) => {
                    let (l, r) = (reg!(a), reg!(b));
                    set!(dst, Value::str(format!("{l}{r}")));
                }
                Op::StateGet(dst, slot) => {
                    let Some(id) = cur_inst else {
                        return Err(VmError { msg: "state access outside a component".into(), span });
                    };
                    let v = self.instances[id].slots[slot as usize].clone();
                    set!(dst, v);
                }
                Op::StateSet(slot, src) => {
                    let Some(id) = cur_inst else {
                        return Err(VmError { msg: "state access outside a component".into(), span });
                    };
                    let v = reg!(src);
                    self.instances[id].slots[slot as usize] = v;
                }
                Op::MakeInstance { dst, comp, start, argc, policy } => {
                    let props: Vec<Value> = (0..argc).map(|i| reg!(start + i)).collect();
                    let id = self.instantiate(comp, props).map_err(|mut e| {
                        if e.span == Span::default() {
                            e.span = span;
                        }
                        e
                    })?;
                    self.instances[id].restart_on_failure = policy == 1;
                    set!(dst, Value::Component(id));
                }
                Op::WireOp { from, out_port, to, in_port } => {
                    let (Value::Component(src), Value::Component(dst_id)) = (reg!(from), reg!(to))
                    else {
                        return Err(VmError { msg: "wire endpoints must be components".into(), span });
                    };
                    let out_name = chunk.consts[out_port as usize].to_string();
                    let in_name = chunk.consts[in_port as usize].to_string();
                    self.instances[src]
                        .wires
                        .entry(out_name)
                        .or_default()
                        .push((dst_id, in_name));
                }
                Op::EmitOp { port, payload } => {
                    let Some(id) = cur_inst else {
                        return Err(VmError { msg: "`emit` outside a component".into(), span });
                    };
                    let value = match payload {
                        Some(r) => reg!(r),
                        None => Value::Unit,
                    };
                    let port_name = chunk.consts[port as usize].to_string();
                    let targets = self.instances[id].wires.get(&port_name).cloned().unwrap_or_default();
                    if targets.is_empty() {
                        if self.print_unwired {
                            let comp = &self.module.components[self.instances[id].comp as usize].name;
                            println!("{comp}.{port_name} = {value}");
                        }
                    } else {
                        for (dst_id, dport) in targets {
                            self.queue.push_back((dst_id, dport, value.clone()));
                        }
                    }
                }
                Op::Panic(idx) => {
                    let msg = chunk.consts[idx as usize].to_string();
                    return Err(VmError { msg, span });
                }
            }
        }
    }
}

impl crate::ai::ToolHost for Vm<'_> {
    fn call_tool(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        self.call_fun_nested(name, args).map_err(|e| e.msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{Flow, Interp, ProgramDb};
    use crate::value::Value;

    /// Run `probe()` on both engines; assert both succeed with equal results.
    fn differential(src: &str) -> String {
        let compiled = crate::run::compile(src).expect("program must compile");

        // interpreter
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        let f = Value::Fun(std::rc::Rc::new("probe".to_string()));
        let iv = match interp.call_value(f, vec![], crate::diag::Span::default()) {
            Ok(v) => v.to_string(),
            Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
            Err(_) => "control-flow error".into(),
        };

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module must compile");
        let mut vm = Vm::new(&module);
        let vv = match vm.call_named("probe", vec![]) {
            Ok(v) => v.to_string(),
            Err(e) => format!("panic: {}", e.msg),
        };

        assert_eq!(iv, vv, "interpreter and KVM disagree on:\n{src}");
        iv
    }

    #[test]
    fn diff_arithmetic() {
        assert_eq!(differential("fun probe() -> Int {\n    (2 + 3) * 4 - 10 / 2 % 3\n}\n"), "18");
    }

    #[test]
    fn diff_path_builtins_edges() {
        // Pure path helpers agree on the fiddly cases: absolute-second-arg + empty
        // join, trailing-slash base/dir (-> "" / parent), root/no-slash dir, and
        // ext of a multi-dot name, a dotfile (none), a trailing dot, and a dot that
        // is in the directory not the base (none).
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{path_join(\"a\", \"/b\")}|{path_join(\"\", \"b\")}|{path_join(\"a\", \"\")}\"\n}\n"),
            "/b|b|a/"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{path_base(\"a/b/\")}|{path_base(\"/\")}|{path_base(\"noslash\")}\"\n}\n"),
            "||noslash"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{path_dir(\"a/b/c\")}|{path_dir(\"/a\")}|{path_dir(\"a/b/\")}\"\n}\n"),
            "a/b||a/b"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{path_ext(\"a.tar.gz\")}|{path_ext(\".hidden\")}|{path_ext(\"a.\")}|{path_ext(\"a.b/c\")}\"\n}\n"),
            ".gz||.|"
        );
    }

    #[test]
    fn diff_kvm_self_accumulate_in_place() {
        // The KVM compiles `x = x + e` / `x = x.push(e)` straight into x's register
        // and mutates a uniquely-owned Str/List in place (O(n^2) -> O(n)). differential
        // asserts interp == KVM, so this pins the KVM path's value semantics: aliases
        // stay frozen, and a build loop equals the allocating form.
        assert_eq!(
            differential("fun probe() -> Str {\n    var s = \"\"\n    var i = 0\n    while i < 6 { s = s + \"ab\"\n        i = i + 1 }\n    s\n}\n"),
            "abababababab"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    var xs = []\n    var i = 0\n    while i < 4 { xs = xs.push(i)\n        i = i + 1 }\n    let a = xs\n    xs = xs.push(99)\n    \"{xs}|{a}\"\n}\n"),
            "[0, 1, 2, 3, 99]|[0, 1, 2, 3]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    var s = \"x\"\n    let t = s\n    s = s + \"y\"\n    \"{s}|{t}\"\n}\n"),
            "xy|x"
        );
    }

    #[test]
    fn diff_list_self_push() {
        // `xs = xs.push(x)` pushes in place when xs is uniquely owned, but preserves
        // value semantics: an aliased list (and a mid-build snapshot) is never
        // mutated, and results match a normal push on both engines.
        assert_eq!(
            differential("fun probe() -> Str {\n    var xs = [1]\n    let a = xs\n    xs = xs.push(2)\n    xs = xs.push(3)\n    \"{xs}|{a}\"\n}\n"),
            "[1, 2, 3]|[1]"
        );
        // a snapshot taken mid-build stays frozen
        assert_eq!(
            differential("fun probe() -> Str {\n    var xs = [1, 2]\n    xs = xs.push(3)\n    let snap = xs\n    xs = xs.push(4)\n    \"{xs}|{snap}\"\n}\n"),
            "[1, 2, 3, 4]|[1, 2, 3]"
        );
        // a build loop yields the same list as an allocating push would
        assert_eq!(
            differential("fun probe() -> Str {\n    var xs = []\n    var i = 0\n    while i < 5 { xs = xs.push(i * i)\n        i = i + 1 }\n    \"{xs}\"\n}\n"),
            "[0, 1, 4, 9, 16]"
        );
    }

    #[test]
    fn diff_string_self_append() {
        // `s = s + x` is optimized to an in-place append when s is uniquely owned,
        // but MUST preserve value semantics: an aliased string is never mutated, and
        // the result is identical to a normal concat on both engines.
        assert_eq!(
            differential("fun probe() -> Str {\n    var s = \"ab\"\n    let a = s\n    s = s + \"cd\"\n    \"{s}|{a}\"\n}\n"),
            "abcd|ab"
        );
        // a build loop yields the same string as an allocating concat would
        assert_eq!(
            differential("fun probe() -> Str {\n    var s = \"\"\n    var i = 0\n    while i < 5 { s = s + \"ab\"\n        i = i + 1 }\n    s\n}\n"),
            "ababababab"
        );
        // multibyte suffix stays valid (NUL-free UTF-8)
        assert_eq!(
            differential("fun probe() -> Str {\n    var s = \"x\"\n    s = s + \"é\"\n    \"{s}|{s.len()}\"\n}\n"),
            "xé|2"
        );
    }

    #[test]
    fn diff_for_loop_lazy_semantics() {
        // The for loop iterates a Range lazily (no Vec materialization) and a List
        // over its shared Rc (no clone) — identical on both engines, and the List
        // iteration snapshots: a body that rebuilds the source list does not extend
        // the loop. break/continue still work.
        assert_eq!(differential("fun probe() -> Int {\n    var s = 0\n    for i in 0..1000 { s = s + i }\n    s\n}\n"), "499500");
        assert_eq!(differential("fun probe() -> Int {\n    var s = 0\n    for i in 0..10 { if i == 3 { continue }\n        if i == 7 { break }\n        s = s + i }\n    s\n}\n"), "18");
        assert_eq!(differential("fun probe() -> Str {\n    var xs = [1, 2, 3]\n    var seen = []\n    for x in xs {\n        seen = seen.push(x)\n        xs = xs.push(99)\n    }\n    \"{seen}|{xs}\"\n}\n"), "[1, 2, 3]|[1, 2, 3, 99, 99, 99]");
    }

    #[test]
    fn diff_seeded_rng_determinism() {
        // Seeded RNG (xorshift64*) is pure + deterministic: the same seed yields the
        // identical sequence on interp == KVM (and, per the native test, native too),
        // for positive, negative, and i64::MIN seeds. Reproducibility is a certified
        // invariant — these exact sequences are the reference and must never drift.
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{random_ints(42, 5)}\"\n}\n"),
            "[6255019084209693600, -4016670646968046118, -3871288216479333770, -1032231191467822881, -4346169525355410938]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{random_floats(42, 4)}\"\n}\n"),
            "[0.33908526400192196, 0.7822558479199243, 0.7901370452687786, 0.9440426349851643]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{shuffle(42, [1, 2, 3, 4, 5, 6, 7, 8])}\"\n}\n"),
            "[2, 5, 4, 6, 7, 3, 8, 1]"
        );
        // i64::MIN seed (built at runtime — the literal would overflow, K0004)
        assert_eq!(
            differential("fun probe() -> Str {\n    let s = (0 - 9223372036854775807) - 1\n    \"{random_ints(s, 3)}\"\n}\n"),
            "[-1079387622448562176, -6523166708701680128, -3755698650707786723]"
        );
    }

    #[test]
    fn diff_string_interpolation() {
        // Interpolation renders every value type identically on both engines, in a
        // single mixed string; literal `{{`/`}}` and nested interpolation work.
        assert_eq!(
            differential(r#"fun probe() -> Str {
    "i={42} f={3.0} b={true} l={[1, 2]} o={Some(5)} m={Map().insert("k", 1)}"
}
"#),
            "i=42 f=3.0 b=true l=[1, 2] o=Some(5) m=Map{\"k\": 1}"
        );
        // literal braces: {{ -> {, }} -> }, and {{{x}}} -> {value}
        assert_eq!(
            differential("fun probe() -> Str {\n    let x = 5\n    \"{{x}}={x} {{{x}}}\"\n}\n"),
            "{x}=5 {5}"
        );
        // BigInt / Rational / Tensor render in interpolation
        assert_eq!(
            differential("fun probe() -> Str {\n    \"b={big(2).pow(64)} r={rat(1, 3)} t={tensor([1.0, 2.0])}\"\n}\n"),
            "b=18446744073709551616 r=1/3 t=Tensor([1.0, 2.0])"
        );
        // nested method chain with an inner string literal (unescaped quotes inside {})
        assert_eq!(
            differential(r#"fun probe() -> Str {
    "r={["a", "bb", "ccc"].filter(fn s { s.len() > 1 })}"
}
"#),
            r#"r=["bb", "ccc"]"#
        );
    }

    #[test]
    fn diff_tensor_edges() {
        // Tensor edge cases are byte-identical on both engines: dot / elementwise
        // length mismatch panic WITH the two lengths, get out-of-range/negative
        // panics, NaN elements Display + propagate, zeros bounds. (Native dot/binop
        // mismatch messages gained the "(N vs M)" detail in it49.)
        assert_eq!(differential("fun probe() -> Str {\n    \"{tensor([1.0, 2.0]).dot(tensor([1.0, 2.0, 3.0]))}\"\n}\n"), "panic: dot: length mismatch (2 vs 3)");
        assert_eq!(differential("fun probe() -> Str {\n    \"{tensor([1.0, 2.0]) + tensor([1.0, 2.0, 3.0])}\"\n}\n"), "panic: tensor length mismatch (2 vs 3)");
        assert_eq!(differential("fun probe() -> Str {\n    \"{tensor([1.0, 2.0, 3.0]).get(5)}\"\n}\n"), "panic: tensor index 5 out of range for length 3");
        assert_eq!(differential("fun probe() -> Str {\n    \"{tensor([1.0, 0.0 / 0.0, 3.0])}\"\n}\n"), "Tensor([1.0, NaN, 3.0])");
        assert_eq!(differential("fun probe() -> Str {\n    \"{tensor([1.0, 2.0, 3.0]).dot(tensor([4.0, 5.0, 6.0]))}\"\n}\n"), "32.0");
        assert_eq!(differential("fun probe() -> Str {\n    \"{zeros(0 - 1)}\"\n}\n"), "panic: zeros() needs a non-negative size");
    }

    #[test]
    fn diff_bigint_rational_edges() {
        // BigInt/Rational arithmetic is byte-identical on both engines (the native C
        // bignum mirrors the Rust reference): negative div/mod use truncated-toward-
        // zero signs like Int, Rational normalizes the sign to the numerator and
        // reduces, div-by-zero panics cleanly.
        assert_eq!(differential("fun probe() -> Str {\n    \"{big(0 - 7) / big(2)}\"\n}\n"), "-3");
        assert_eq!(differential("fun probe() -> Str {\n    \"{big(0 - 7) % big(2)}\"\n}\n"), "-1");
        assert_eq!(differential("fun probe() -> Str {\n    \"{big(2).pow(100)}\"\n}\n"), "1267650600228229401496703205376");
        assert_eq!(differential("fun probe() -> Str {\n    \"{rat(2, 0 - 4)}\"\n}\n"), "-1/2");
        assert_eq!(differential("fun probe() -> Str {\n    \"{rat(0 - 2, 0 - 4)}\"\n}\n"), "1/2");
        assert_eq!(differential("fun probe() -> Str {\n    \"{rat(1, 3) + rat(1, 6)}\"\n}\n"), "1/2");
        assert_eq!(differential("fun probe() -> Str {\n    \"{big(5) / big(0)}\"\n}\n"), "panic: division by zero");
        assert_eq!(differential("fun probe() -> Str {\n    \"{rat(1, 0)}\"\n}\n"), "panic: division by zero");
    }

    #[test]
    fn diff_int_math_edges() {
        // clamp / gcd / isqrt / sign edge cases are byte-identical on both engines:
        // clamp with INVERTED bounds panics cleanly (no ICE — cf. the it28 slice
        // clamp bug), gcd handles 0/negative/i64::MIN, isqrt handles 0/negative/MAX.
        assert_eq!(differential("fun probe() -> Int {\n    15.clamp(0, 10)\n}\n"), "10");
        assert_eq!(differential("fun probe() -> Int {\n    5.clamp(10, 2)\n}\n"), "panic: `clamp`: lo must not exceed hi");
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 12).gcd(8)\n}\n"), "4");
        assert_eq!(differential("fun probe() -> Int {\n    let m = (0 - 9223372036854775807) - 1\n    m.gcd(2)\n}\n"), "2");
        assert_eq!(differential("fun probe() -> Int {\n    9223372036854775807.isqrt()\n}\n"), "3037000499");
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 4).isqrt()\n}\n"), "panic: `isqrt` of a negative Int");
    }

    #[test]
    fn diff_codec_decode_nul_rejected() {
        // hex_decode / base64_decode of bytes that include a NUL are REJECTED (a
        // NUL would violate NUL-free strings; interp embedded it, native truncated
        // — divergence, same class as the it45 url_decode fix). Valid decode and
        // round-trips are unchanged.
        assert_eq!(differential("fun probe() -> Str {\n    \"{hex_decode(\"610062\")}\"\n}\n"), "Err(\"decoded bytes contain a NUL byte\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{base64_decode(\"AA==\")}\"\n}\n"), "Err(\"decoded bytes contain a NUL byte\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{hex_decode(hex_encode(\"héllo\"))}\"\n}\n"), "Ok(\"héllo\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{base64_decode(base64_encode(\"data\"))}\"\n}\n"), "Ok(\"data\")");
    }

    #[test]
    fn diff_url_decode_nul_and_edges() {
        // url_decode of `%00` is REJECTED (a decoded NUL would violate KUPL's
        // NUL-free strings; interp used to embed it, native truncated at it —
        // divergence). Valid decode, `+`->space, and malformed escapes are
        // byte-identical on both engines.
        assert_eq!(differential("fun probe() -> Str {\n    \"{url_decode(\"a%00b\")}\"\n}\n"), "Err(\"invalid percent-encoding: decoded NUL byte\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{url_decode(\"a+b%20c\")}\"\n}\n"), "Ok(\"a b c\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{url_decode(\"abc%\")}\"\n}\n"), "Err(\"invalid percent-encoding: truncated escape\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{url_decode(\"%ZZ\")}\"\n}\n"), "Err(\"invalid percent-encoding: bad hex\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{url_decode(url_encode(\"a b/c?日\"))}\"\n}\n"), "Ok(\"a b/c?日\")");
    }

    #[test]
    fn diff_radix_formatting() {
        // to_hex/to_binary/to_octal/to_radix use SIGN-MAGNITUDE (a `-` prefix, not
        // two's-complement), handle i64::MIN without a negate-overflow, and panic
        // cleanly on an out-of-range base — all byte-identical on both engines.
        assert_eq!(differential("fun probe() -> Str {\n    (0 - 255).to_hex()\n}\n"), "-ff");
        assert_eq!(differential("fun probe() -> Str {\n    1295.to_radix(36)\n}\n"), "zz");
        assert_eq!(differential("fun probe() -> Str {\n    (0 - 5).to_radix(2)\n}\n"), "-101");
        assert_eq!(
            differential("fun probe() -> Str {\n    let m = (0 - 9223372036854775807) - 1\n    m.to_hex()\n}\n"),
            "-8000000000000000"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    (10).to_radix(37)\n}\n"),
            "panic: `to_radix` base must be in 2..=36"
        );
    }

    #[test]
    fn diff_csv_ops() {
        // csv_parse / csv_stringify (RFC 4180) — quoting/escaping of embedded commas,
        // quotes ("" escape), and newlines is byte-identical on both engines, and a
        // parse->stringify round-trip preserves the fields.
        assert_eq!(
            differential("fun probe() -> Str {\n    csv_stringify([[\"a\", \"b,c\"], [\"d\", \"e\"]])\n}\n"),
            "a,\"b,c\"\nd,e"
        );
        // a field containing a quote is quoted and the quote doubled
        assert_eq!(
            differential("fun probe() -> Str {\n    csv_stringify([[\"a\\\"b\", \"c\"]])\n}\n"),
            "\"a\"\"b\",c"
        );
        // round-trip: a comma-containing field survives stringify->parse
        assert_eq!(
            differential("fun probe() -> Str {\n    csv_parse(csv_stringify([[\"x,y\", \"z\"]])).get(0).unwrap_or([]).get(0).unwrap_or(\"?\")\n}\n"),
            "x,y"
        );
        // parse handles the "" escape inside a quoted field
        assert_eq!(
            differential(r#"fun probe() -> Str {
    csv_parse("x,\"say \"\"hi\"\"\"").get(0).unwrap_or([]).get(1).unwrap_or("?")
}
"#),
            "say \"hi\""
        );
    }

    #[test]
    fn diff_regex_ops() {
        // The shared regex engine (src/regex.rs) — match/find/find_all/replace and
        // invalid-pattern panics are byte-identical on both engines, and `.` matches
        // a full character (incl. multi-byte, after the it42 native fix).
        assert_eq!(differential("fun probe() -> Str {\n    \"{re_find_all(\"[0-9]+\", \"a1b22c333\")}\"\n}\n"), "[\"1\", \"22\", \"333\"]");
        assert_eq!(differential("fun probe() -> Str {\n    re_replace(\"[0-9]+\", \"a1b22c\", \"#\")\n}\n"), "a#b#c");
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"^a.c$\", \"abc\")\n}\n"), "true");
        assert_eq!(differential("fun probe() -> Str {\n    \"{re_find(\".\", \"日本\")}\"\n}\n"), "Some(\"日\")");
        assert_eq!(differential("fun probe() -> Str {\n    \"{re_find(\"a.*z\", \"a日本z\")}\"\n}\n"), "Some(\"a日本z\")");
        // invalid pattern -> identical clean panic
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"(abc\", \"abc\")\n}\n"), "panic: invalid regex: unclosed group `(`");
    }

    #[test]
    fn diff_par_determinism_and_panic() {
        // par_map / par_filter / par{} preserve INPUT order deterministically on both
        // engines (par_map runs branches on threads but joins in order), and a panic
        // inside a parallel branch propagates as the SAME clean panic (no ICE/hang/
        // partial result). Certifies the async axis.
        assert_eq!(differential("fun probe() -> Str {\n    \"{[5, 3, 8, 1, 9, 2].par_map(fn x { x * x })}\"\n}\n"), "[25, 9, 64, 1, 81, 4]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{[1, 2, 3, 4, 5, 6].par_filter(fn x { x % 2 == 0 })}\"\n}\n"), "[2, 4, 6]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{[1].drop(1).par_map(fn x { x + 1 })}\"\n}\n"), "[]");
        assert_eq!(differential("fun probe() -> Int {\n    let r = par {\n        3 * 3\n        4 * 4\n        5 * 5\n    }\n    r.sum()\n}\n"), "50");
        assert_eq!(differential("fun probe() -> Str {\n    \"{[1, 2, 0, 4].par_map(fn x { 10 / x })}\"\n}\n"), "panic: division by zero");
    }

    #[test]
    fn diff_nested_value_display() {
        // Display of complex/nested values (lists of lists, Option/Result nesting,
        // Map/Set with nested elements, reduced Rationals) is byte-identical on both
        // engines — programs that print/log structured values agree everywhere.
        assert_eq!(differential("fun probe() -> Str {\n    \"{[[1, 2], [3], []]}\"\n}\n"), "[[1, 2], [3], []]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{[Some(1), None, Some(3)]}\"\n}\n"), "[Some(1), None, Some(3)]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{Map().insert(\"a\", [1, 2]).insert(\"b\", [3])}\"\n}\n"), "Map{\"a\": [1, 2], \"b\": [3]}");
        assert_eq!(differential("fun probe() -> Str {\n    \"{Set([3, 1, 2])}\"\n}\n"), "Set{3, 1, 2}");
        assert_eq!(differential("fun probe() -> Str {\n    \"{[rat(1, 2), rat(2, 4)]}\"\n}\n"), "[1/2, 1/2]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{Map().insert(\"x\", Map().insert(\"y\", 1))}\"\n}\n"), "Map{\"x\": Map{\"y\": 1}}");
    }

    #[test]
    fn diff_datetime_format_and_parse() {
        // Deterministic UTC civil-calendar math — format/components/parse are
        // byte-identical across engines for fixed epochs incl. pre-1970 and extreme
        // values. parse_iso's Err message (a first-class Result VALUE the program
        // reads) is also identical — native used to return Err("") (a stack buffer
        // that dangled after return; PR-it36 heap-allocated it).
        assert_eq!(differential("fun probe() -> Str {\n    date_iso(0 - 1)\n}\n"), "1969-12-31T23:59:59Z");
        assert_eq!(differential("fun probe() -> Str {\n    date_iso(253402300799)\n}\n"), "9999-12-31T23:59:59Z");
        assert_eq!(differential("fun probe() -> Str {\n    date_iso(date_make(2000, 2, 29, 0, 0, 0))\n}\n"), "2000-02-29T00:00:00Z");
        assert_eq!(differential("fun probe() -> Int {\n    weekday_of(0)\n}\n"), "4");
        assert_eq!(
            differential("fun probe() -> Str {\n    match parse_iso(\"nope\") { Ok(t) => \"{t}\", Err(m) => m }\n}\n"),
            "invalid ISO-8601 timestamp: nope"
        );
    }

    #[test]
    fn diff_json_key_order_and_sort_stability() {
        // JSON object keys keep INPUT order through parse -> stringify (not sorted),
        // identically on both engines; duplicate keys collapse to the last value.
        assert_eq!(
            differential(r#"fun probe() -> Str {
    match json_parse("{{ \"b\": 1, \"a\": 2, \"c\": 3 }}") { Ok(j) => json_stringify(j), Err(e) => e }
}
"#),
            r#"{"b":1,"a":2,"c":3}"#
        );
        assert_eq!(
            differential(r#"fun probe() -> Str {
    match json_parse("{{ \"k\": 1, \"k\": 2 }}") { Ok(j) => json_stringify(j), Err(e) => e }
}
"#),
            r#"{"k":2}"#
        );
        // .sort_by is STABLE — equal keys keep their original relative order.
        assert_eq!(
            differential("type R = R(k: Int, t: Str)\nfun probe() -> Str {\n    var o = \"\"\n    for r in [R(2, \"a\"), R(1, \"b\"), R(2, \"c\"), R(1, \"d\"), R(3, \"e\"), R(1, \"f\")].sort_by(fn r { r.k }) { o = o + \"{r.t}\" }\n    o\n}\n"),
            "bdface"
        );
    }

    #[test]
    fn diff_map_set_insertion_order_deterministic() {
        // Map/Set iterate in INSERTION order — deterministic and identical on both
        // engines (no randomized-HashMap ordering). Order survives removal; Set
        // dedups keeping first occurrence; equality ignores insertion order.
        assert_eq!(
            differential("fun probe() -> Str {\n    let m = Map().insert(\"b\", 1).insert(\"a\", 2).insert(\"c\", 3)\n    \"{m.keys()}\"\n}\n"),
            "[\"b\", \"a\", \"c\"]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    let m = Map().insert(50, 0).insert(10, 0).insert(30, 0).insert(90, 0)\n    \"{m.remove(30).keys()}\"\n}\n"),
            "[50, 10, 90]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{Set([5, 1, 3, 9, 2, 7, 1, 5]).to_list()}\"\n}\n"),
            "[5, 1, 3, 9, 2, 7]"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    Map().insert(\"a\", 1).insert(\"b\", 2) == Map().insert(\"b\", 2).insert(\"a\", 1)\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_float_display_positional() {
        // f64 Display is positional shortest-round-trip on both engines — small
        // magnitudes are NOT scientific (native `%g` used to print "1e-05").
        assert_eq!(differential("fun probe() -> Str {\n    \"{0.00001}\"\n}\n"), "0.00001");
        assert_eq!(differential("fun probe() -> Str {\n    \"{0.000012345}\"\n}\n"), "0.000012345");
        assert_eq!(differential("fun probe() -> Str {\n    \"{0.1 + 0.2}\"\n}\n"), "0.30000000000000004");
        assert_eq!(differential("fun probe() -> Str {\n    \"{1e20}\"\n}\n"), "100000000000000000000.0");
        assert_eq!(differential("fun probe() -> Str {\n    \"{(0.0 - 5.0) * 0.0}\"\n}\n"), "-0.0");
    }

    #[test]
    fn diff_empty_separator_panics() {
        // An empty separator/pattern is a programming error: split/replace/
        // replace_first all raise the SAME clean panic on both engines (native too,
        // see cgen test) instead of the interpreter's old Rust-passthrough behavior
        // (which split into per-char pieces / inserted everywhere) diverging from
        // native's no-op/panic. Matches the existing `.count` non-empty rule.
        assert_eq!(differential("fun probe() -> Int {\n    \"abc\".split(\"\").len()\n}\n"), "panic: `split` needs a non-empty separator");
        assert_eq!(differential("fun probe() -> Str {\n    \"abc\".replace(\"\", \"x\")\n}\n"), "panic: `replace` needs a non-empty pattern");
        assert_eq!(differential("fun probe() -> Str {\n    \"abc\".replace_first(\"\", \"x\")\n}\n"), "panic: `replace_first` needs a non-empty pattern");
        // normal usage unaffected
        assert_eq!(differential("fun probe() -> Int {\n    \"a,b,c\".split(\",\").len()\n}\n"), "3");
        assert_eq!(differential("fun probe() -> Str {\n    \"aXbXc\".replace(\"X\", \"-\")\n}\n"), "a-b-c");
    }

    #[test]
    fn diff_string_slice_and_pad_edges() {
        // .slice with extreme/inverted indices must not panic (interp/KVM used to
        // ICE on slice(i64::MAX, i64::MAX) — a clamp with inverted bounds), and
        // char-indexed slicing over multibyte text agrees on both engines.
        assert_eq!(differential("fun probe() -> Str {\n    \"hello\".slice(9223372036854775807, 9223372036854775807)\n}\n"), "");
        assert_eq!(differential("fun probe() -> Str {\n    \"hello\".slice(9223372036854775807, 2)\n}\n"), "");
        assert_eq!(differential("fun probe() -> Str {\n    \"hello\".slice(3, 1)\n}\n"), "");
        assert_eq!(differential("fun probe() -> Str {\n    \"café\".slice(0, 4)\n}\n"), "café");
        assert_eq!(differential("fun probe() -> Int {\n    \"日本\".len()\n}\n"), "2");
        // .pad_* fills with the first CHAR (full codepoint) of the fill string.
        assert_eq!(differential("fun probe() -> Str {\n    \"é\".pad_right(3, \"日\")\n}\n"), "é日日");
        assert_eq!(differential("fun probe() -> Str {\n    \"é\".pad_left(3, \"日\")\n}\n"), "日日é");
    }

    #[test]
    fn diff_integer_overflow_panics_and_boundaries() {
        // KUPL uses CHECKED integer arithmetic: every operation panics (never wraps or
        // saturates) on i64 overflow, with a distinct per-op message — byte-identical on
        // interp/KVM, and (per the native test) native matches rather than wrapping via C's
        // signed-overflow UB (PR-it151).
        assert_eq!(differential("fun probe() -> Int { let mx = 9223372036854775807\n    mx + 1 }\n"), "panic: integer overflow in addition");
        assert_eq!(differential("fun probe() -> Int { let mn = 0 - 9223372036854775807 - 1\n    mn - 1 }\n"), "panic: integer overflow in subtraction");
        assert_eq!(differential("fun probe() -> Int { let mx = 9223372036854775807\n    mx * 2 }\n"), "panic: integer overflow in multiplication");
        // The classic MIN / -1 and MIN % -1 overflows are caught (not wrapped to a bogus
        // value); negating and abs of MIN overflow too.
        assert_eq!(differential("fun probe() -> Int { let mn = 0 - 9223372036854775807 - 1\n    mn / (0 - 1) }\n"), "panic: integer overflow in division");
        assert_eq!(differential("fun probe() -> Int { let mn = 0 - 9223372036854775807 - 1\n    mn % (0 - 1) }\n"), "panic: integer overflow in remainder");
        assert_eq!(differential("fun probe() -> Int { 5 / 0 }\n"), "panic: division by zero");
        // Boundary operations that do NOT overflow compute correctly (no false panic).
        assert_eq!(
            differential("fun probe() -> Str { let mx = 9223372036854775807\n    let mn = 0 - 9223372036854775807 - 1\n    \"{mx + 0}|{mx - 1}|{mn + 1}|{mn * 1}|{mn % 7}\" }\n"),
            "9223372036854775807|9223372036854775806|-9223372036854775807|-9223372036854775808|-1"
        );
    }

    #[test]
    fn diff_numeric_cast_and_overflow_panics() {
        // Sized-int narrowing that doesn't fit, integer .pow overflow, a negative
        // exponent, and i64::MIN.abs() all raise the SAME clean panic on both
        // engines (certified consistent — the native backend matches too, see the
        // cgen tests). No wrap, no UB, no ICE.
        assert_eq!(differential("fun probe() -> Str {\n    \"{300.to_i8()}\"\n}\n"), "panic: 300 out of range for `i8`");
        assert_eq!(differential("fun probe() -> Str {\n    \"{(0 - 1).to_u8()}\"\n}\n"), "panic: -1 out of range for `u8`");
        assert_eq!(differential("fun probe() -> Int {\n    2.pow(100)\n}\n"), "panic: integer overflow in pow");
        assert_eq!(differential("fun probe() -> Int {\n    2.pow(0 - 1)\n}\n"), "panic: `pow` needs a non-negative exponent");
        assert_eq!(differential("fun probe() -> Int {\n    ((0 - 9223372036854775807) - 1).abs()\n}\n"), "panic: integer overflow in abs");
        // in-range casts / pow are unchanged
        assert_eq!(differential("fun probe() -> Int {\n    127.to_i8().to_int()\n}\n"), "127");
        assert_eq!(differential("fun probe() -> Int {\n    2.pow(62)\n}\n"), "4611686018427387904");
    }

    #[test]
    fn diff_float_to_int_saturates() {
        // Float.to_int() is a saturating cast (Rust `as i64`): out-of-range floats
        // clamp to i64::MIN/MAX and NaN -> 0. Both engines must agree (the native
        // backend used a raw C cast which is UB out of range — fixed PR-it26).
        assert_eq!(differential("fun probe() -> Int {\n    (1e30).to_int()\n}\n"), "9223372036854775807");
        assert_eq!(differential("fun probe() -> Int {\n    (0.0 - 1e30).to_int()\n}\n"), "-9223372036854775808");
        assert_eq!(differential("fun probe() -> Int {\n    (0.0 / 0.0).to_int()\n}\n"), "0");
        assert_eq!(differential("fun probe() -> Int {\n    (1.0 / 0.0).to_int()\n}\n"), "9223372036854775807");
        assert_eq!(differential("fun probe() -> Int {\n    (3.7).to_int()\n}\n"), "3");
    }

    #[test]
    fn diff_shift_bounds() {
        // Shift methods panic identically on both engines for out-of-range amounts
        // (0..=63), and compute identical values in range (incl. sign handling).
        assert_eq!(differential("fun probe() -> Int {\n    (1).shl(63)\n}\n"), "-9223372036854775808");
        assert_eq!(differential("fun probe() -> Int {\n    (1).shl(64)\n}\n"), "panic: shift amount must be in 0..=63");
        assert_eq!(differential("fun probe() -> Int {\n    (1).shl(0 - 1)\n}\n"), "panic: shift amount must be in 0..=63");
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 1).ushr(4)\n}\n"), "1152921504606846975");
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 1).shr(4)\n}\n"), "-1");
    }

    #[test]
    fn diff_int_min_rem_overflow() {
        // i64::MIN % -1 overflows (the quotient overflows). It must be a clean
        // "integer overflow in remainder" panic on BOTH engines — a raw `%` used
        // to overflow-panic and escape as an ICE on the interpreter (PR-it25).
        // Matches how i64::MIN / -1 already reports division overflow.
        let src = "fun probe() -> Int {\n    let m = (0 - 9223372036854775807) - 1\n    m % (0 - 1)\n}\n";
        assert_eq!(differential(src), "panic: integer overflow in remainder");
        // and the division form, for good measure
        let d = "fun probe() -> Int {\n    let m = (0 - 9223372036854775807) - 1\n    m / (0 - 1)\n}\n";
        assert_eq!(differential(d), "panic: integer overflow in division");
        // normal remainder is unaffected (truncated-toward-zero sign convention)
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 17) % 5\n}\n"), "-2");
    }

    #[test]
    fn diff_huge_tensor_is_capped() {
        // A huge zeros()/arange() must panic cleanly (not hang / OOM), identically
        // on both engines (the native backend enforces the same cap).
        assert_eq!(
            differential("fun probe() -> Int {\n    arange(100000000000).len()\n}\n"),
            "panic: arange() size too large"
        );
        assert_eq!(
            differential("fun probe() -> Int {\n    zeros(100000000000).len()\n}\n"),
            "panic: zeros() size too large"
        );
    }

    #[test]
    fn diff_codec_and_csv_consistency() {
        // base64/hex/url decode (Ok values AND detailed Err messages), query_parse,
        // and csv round-trip are byte-identical across engines — locked in so the
        // native C mirrors can't drift from the Rust reference.
        let p = |body: &str| format!("fun probe() -> Str {{\n    {body}\n}}\n");
        assert_eq!(differential(&p("to_str(base64_decode(\"aGVsbG8=\"))")), "Ok(\"hello\")");
        assert_eq!(
            differential(&p("to_str(base64_decode(\"aGVsbG8\"))")),
            "Err(\"invalid base64: length not a multiple of 4\")"
        );
        assert_eq!(differential(&p("to_str(hex_decode(\"48454C4C4F\"))")), "Ok(\"HELLO\")");
        assert_eq!(differential(&p("to_str(hex_decode(\"abc\"))")), "Err(\"invalid hex: odd length\")");
        assert_eq!(differential(&p("to_str(url_decode(\"a%20b\"))")), "Ok(\"a b\")");
        assert_eq!(
            differential(&p("to_str(url_decode(\"a%ZZ\"))")),
            "Err(\"invalid percent-encoding: bad hex\")"
        );
        assert_eq!(
            differential(&p("to_str(query_parse(\"a=1&a=2\"))")),
            "[[\"a\", \"1\"], [\"a\", \"2\"]]"
        );
        // csv round-trip through a quoted field containing a comma
        assert_eq!(
            differential(&p(
                "let r = [[\"a,b\", \"c\"]]\n    to_str(csv_parse(csv_stringify(r)) == r)"
            )),
            "true"
        );
    }

    #[test]
    fn diff_parse_int_float_edges() {
        // parse_int/parse_float edge inputs are byte-identical across engines
        // (native strtoll/strtod were aligned to Rust: reject leading whitespace,
        // integer overflow is a failure not a saturated value).
        let p = |s: &str| format!("fun probe() -> Str {{\n    to_str({s})\n}}\n");
        assert_eq!(differential(&p("\"  12\".parse_int()")), "None");
        assert_eq!(differential(&p("\"99999999999999999999\".parse_int()")), "None");
        assert_eq!(differential(&p("\"-99999999999999999999\".parse_int()")), "None");
        assert_eq!(differential(&p("\"42\".parse_int()")), "Some(42)");
        assert_eq!(differential(&p("\"0x10\".parse_int()")), "None");
        assert_eq!(differential(&p("\"  1.5\".parse_float()")), "None");
        assert_eq!(differential(&p("\"1e999\".parse_float()")), "Some(inf)");
        assert_eq!(differential(&p("\"3.14\".parse_float()")), "Some(3.14)");
    }

    #[test]
    fn diff_utf8_string_ops() {
        // Multibyte UTF-8 string operations are byte-identical across engines:
        // len/slice/index are char-based; to_upper/to_lower are ASCII-only
        // (non-ASCII passes through unchanged — the native runtime can't carry
        // full Unicode case tables, so all engines agree on ASCII-only).
        assert_eq!(differential("fun probe() -> Int {\n    \"日本語\".len()\n}\n"), "3");
        assert_eq!(differential("fun probe() -> Int {\n    \"héllo\".len()\n}\n"), "5");
        assert_eq!(differential("fun probe() -> Str {\n    \"日本語\".slice(0, 2)\n}\n"), "日本");
        assert_eq!(differential("fun probe() -> Str {\n    \"héllo\".to_upper()\n}\n"), "HéLLO");
        assert_eq!(differential("fun probe() -> Str {\n    \"HÉLLO\".to_lower()\n}\n"), "hÉllo");
    }

    #[test]
    fn diff_nan_inf_display() {
        // NaN and infinities must Display identically on both engines (Rust's f64
        // Display: "NaN"/"inf"/"-inf"). The native backend matches too (PR-it5).
        let src = "fun probe() -> Str {\n    \
                   let n = 0.0 / 0.0\n    let p = 1.0 / 0.0\n    let m = -1.0 / 0.0\n    \
                   \"{n} {p} {m}\"\n}\n";
        assert_eq!(differential(src), "NaN inf -inf");
    }

    #[test]
    fn diff_deep_recursion_stack_overflow() {
        // Unbounded recursion must yield the SAME clean `stack overflow` panic on
        // both engines (the interpreter guards at MAX_CALL_DEPTH just like the KVM,
        // rather than exhausting the native stack and aborting uncatchably). The
        // interpreter needs the same large stack `main` gives it to reach the guard,
        // so run the differential on a big-stack thread.
        let src = "fun rec(n: Int) -> Int {\n    if n == 0 { 0 } else { rec(n - 1) }\n}\n\
                   fun probe() -> Int {\n    rec(50000)\n}\n";
        let out = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(move || differential(src))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(out, "panic: stack overflow (10000 frames)");
    }

    #[test]
    fn diff_recursion_fib() {
        let src = "fun fib(n: Int) -> Int {\n    if n < 2 {\n        n\n    } else {\n        fib(n - 1) + fib(n - 2)\n    }\n}\nfun probe() -> Int {\n    fib(15)\n}\n";
        assert_eq!(differential(src), "610");
    }

    #[test]
    fn diff_adt_match() {
        let src = "type Shape = Circle(r: Float) | Rect(w: Float, h: Float)\nfun area(s: Shape) -> Float {\n    match s {\n        Circle(r) => 3.0 * r * r\n        Rect(w, h) => w * h\n    }\n}\nfun probe() -> Float {\n    area(Circle(r: 2.0)) + area(Rect(w: 3.0, h: 4.0))\n}\n";
        assert_eq!(differential(src), "24.0");
    }

    #[test]
    fn diff_lists_lambdas() {
        let src = "fun probe() -> Int {\n    let xs = [1, 2, 3, 4, 5, 6]\n    xs.filter(fn n { n % 2 == 0 }).map(fn n { n * 10 }).sum()\n}\n";
        assert_eq!(differential(src), "120");
    }

    #[test]
    fn diff_closure_capture() {
        let src = "fun probe() -> Int {\n    let base = 100\n    let add = fn n { n + base }\n    [1, 2, 3].map(add).sum()\n}\n";
        assert_eq!(differential(src), "306");
    }

    #[test]
    fn diff_string_interp() {
        let src = "fun probe() -> Str {\n    let n = 6 * 7\n    \"answer is {n}!\"\n}\n";
        assert_eq!(differential(src), "answer is 42!");
    }

    #[test]
    fn diff_result_try() {
        let src = "fun half(n: Int) -> Result[Int, Str] {\n    if n % 2 == 0 {\n        Ok(n / 2)\n    } else {\n        Err(\"odd: {n}\")\n    }\n}\nfun sum_halves(a: Int, b: Int) -> Result[Int, Str] {\n    let x = half(a)?\n    let y = half(b)?\n    Ok(x + y)\n}\nfun probe() -> Str {\n    let good = sum_halves(10, 4)\n    let bad = sum_halves(10, 3)\n    \"{good} then {bad}\"\n}\n";
        assert_eq!(differential(src), "Ok(7) then Err(\"odd: 3\")");
    }

    #[test]
    fn diff_loops_break_continue() {
        let src = "fun probe() -> Int {\n    var total = 0\n    for i in 0..20 {\n        if i % 3 == 0 {\n            continue\n        }\n        if i > 10 {\n            break\n        }\n        total += i\n    }\n    var w = 0\n    while w < 5 {\n        total += 100\n        w += 1\n    }\n    total\n}\n";
        assert_eq!(differential(src), "537");
    }

    #[test]
    fn diff_records_and_options() {
        let src = "type User = { name: Str, age: Int }\nfun probe() -> Str {\n    let users = [User(name: \"Ada\", age: 36), User(name: \"Alan\", age: 41)]\n    let found = users.find(fn u { u.age > 40 })\n    match found {\n        Some(u) => u.name\n        None => \"nobody\"\n    }\n}\n";
        assert_eq!(differential(src), "Alan");
    }

    #[test]
    fn diff_overflow_panics_identically() {
        let src = "fun probe() -> Int {\n    9223372036854775807 + 1\n}\n";
        assert_eq!(differential(src), "panic: integer overflow in addition");
    }

    #[test]
    fn diff_higher_order_funs() {
        let src = "fun twice(f: fn(Int) -> Int, x: Int) -> Int {\n    f(f(x))\n}\nfun inc(n: Int) -> Int {\n    n + 1\n}\nfun probe() -> Int {\n    twice(inc, 5) + twice(fn n { n * 2 }, 3)\n}\n";
        assert_eq!(differential(src), "19");
    }

    #[test]
    fn diff_par_result_order_is_deterministic() {
        // par_map/par_filter return results in INPUT order (not completion order),
        // deterministically and identically on interp and KVM — parallelism must not
        // leak into observable ordering.
        let src = "fun probe() -> Str {\n    let r = [5, 3, 8, 1, 9, 2].par_map(fn n { n * n })\n    \
                   let f = [1, 2, 3, 4, 5, 6, 7, 8].par_filter(fn n { n % 2 == 0 })\n    \
                   let e: List[Int] = []\n    \"{r}|{f}|{e.par_map(fn n { n + 1 })}|{[42].par_map(fn n { n * 2 })}\"\n}\n";
        assert_eq!(differential(src), "[25, 9, 64, 1, 81, 4]|[2, 4, 6, 8]|[]|[84]");
    }

    #[test]
    fn diff_tensor_ops_and_empty_edges() {
        // Tensor construction, reductions, dot/scale/map and element access are
        // byte-identical on interp and KVM — including float formatting.
        let src = "fun probe() -> Str {\n    let a = tensor([1.0, 2.0, 3.0, 4.0])\n    \
                   let b = tensor([2.0, 0.0, 1.0, 3.0])\n    \
                   \"{a.sum()}|{a.mean()}|{a.max()}|{a.min()}|{a.dot(b)}|{a.scale(0.5).to_list()}|\
                   {a.map(fn(x) { x * x }).to_list()}|{a.get(2)}|{arange(4).to_list()}\"\n}\n";
        assert_eq!(
            differential(src),
            "10.0|2.5|4.0|1.0|17.0|[0.5, 1.0, 1.5, 2.0]|[1.0, 4.0, 9.0, 16.0]|3.0|[0.0, 1.0, 2.0, 3.0]"
        );
        // Empty-tensor edges: sum is +0.0 (PR-it101 fixed interp's Rust -0.0 identity to
        // match native's 0.0), mean/max/min are clean per-op panics, out-of-range get.
        assert_eq!(differential("fun probe() -> Str { \"{zeros(0).sum()}\" }\n"), "0.0");
        assert_eq!(differential("fun probe() -> Str { \"{zeros(0).mean()}\" }\n"), "panic: mean of an empty tensor");
        assert_eq!(differential("fun probe() -> Str { \"{zeros(0).max()}\" }\n"), "panic: max of an empty tensor");
        assert_eq!(differential("fun probe() -> Str { \"{zeros(0).min()}\" }\n"), "panic: min of an empty tensor");
        assert_eq!(
            differential("fun probe() -> Str { \"{tensor([1.0]).get(5)}\" }\n"),
            "panic: tensor index 5 out of range for length 1"
        );
        // -0.0 bug class (PR-it102): dot of empty tensors and any dot/mean that sums to
        // zero must be +0.0 (matching native), not Rust `Iterator::sum`'s -0.0 identity.
        assert_eq!(differential("fun probe() -> Str { \"{zeros(0).dot(zeros(0))}\" }\n"), "0.0");
        assert_eq!(
            differential("fun probe() -> Str { \"{tensor([1.0, 0.0]).dot(tensor([0.0, 1.0]))}\" }\n"),
            "0.0"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{tensor([1.0, 0.0 - 1.0]).mean()}\" }\n"),
            "0.0"
        );
    }

    #[test]
    fn diff_codec_decode_error_messages() {
        // PR-it117: the codec decoders (hex/base64/url) already give specific, matching
        // error messages on interp/KVM (native verified separately) — the generic-message
        // class was JSON-only (fixed it116).
        let src = r#"fun e(r: Result[Str, Str]) -> Str { match r { Ok(_) => "ok"
        Err(m) => m } }
fun probe() -> Str { "{e(hex_decode("abc"))}|{e(hex_decode("zz"))}|{e(base64_decode("ab@d"))}|{e(base64_decode("ab=c"))}|{e(url_decode("%zz"))}|{e(url_decode("%a"))}" }
"#;
        assert_eq!(
            differential(src),
            "invalid hex: odd length|invalid hex: bad digit|invalid base64: bad character|invalid base64: misplaced padding|invalid percent-encoding: bad hex|invalid percent-encoding: truncated escape"
        );
    }

    #[test]
    fn diff_json_parse_error_messages() {
        // PR-it116: malformed JSON gives the SAME specific, positioned error message on
        // interp/KVM (and native) — not a generic "invalid JSON".
        let src = r#"fun e(j: Str) -> Str { match json_parse(j) { Ok(_) => "ok"
        Err(m) => m } }
fun probe() -> Str { "{e("NaN")}|{e("[1,2")}|{e("1.2.3")}|{e("")}|{e("tru")}|{e("[1,2] x")}" }
"#;
        assert_eq!(
            differential(src),
            "unexpected character `N` at position 0|expected `,` or `]` in array|invalid number `1.2.3`|unexpected end of input|invalid literal (expected `true`)|unexpected trailing characters at position 6"
        );
    }

    #[test]
    fn diff_json_surrogate_pair_parsing() {
        // PR-it115: a `🎉` surrogate pair decodes to the single astral code
        // point (🎉), a BMP escape is unchanged (é), and a lone surrogate becomes
        // U+FFFD — byte-identical on interp/KVM.
        let src = r#"fun d(j: Str) -> Str { match json_parse(j) { Ok(JStr(s)) => "{s}:{s.len()}"
        _ => "ERR" } }
fun probe() -> Str { "{d("\"\\uD83C\\uDF89\"")}|{d("\"caf\\u00e9\"")}|{d("\"\\uD83C\"")}|{d("\"a\\uD83C\\uDF89b\"")}" }
"#;
        assert_eq!(differential(src), "🎉:1|café:4|\u{FFFD}:1|a🎉b:3");
    }

    #[test]
    fn diff_json_number_and_string_fidelity() {
        // PR-it114: JSON numbers format positionally (never scientific), byte-identical
        // on interp/KVM — a large integer-valued float is "100000000000000000000", not
        // "1e+20"; whole numbers drop the ".0"; precision is shortest-round-trip.
        assert_eq!(
            differential("fun probe() -> Str { \"{json_stringify(JNum(0.1 + 0.2))}|{json_stringify(JNum(1e20))}|{json_stringify(JNum(1.0 / 3.0))}|{json_stringify(JNum(1e-10))}|{json_stringify(JNum(42.0))}|{json_stringify(JNum(1.5))}\" }\n"),
            "0.30000000000000004|100000000000000000000|0.3333333333333333|0.0000000001|42|1.5"
        );
        // string escaping: quote/backslash/newline/tab become JSON escapes.
        assert_eq!(
            differential("fun probe() -> Str { json_stringify(JStr(\"tab\\tnl\\nq\\\"end\")) }\n"),
            "\"tab\\tnl\\nq\\\"end\""
        );
        // a value survives json_parse(json_stringify(x)) round-trip.
        assert_eq!(
            differential("fun probe() -> Str { let d = JObj(Map().insert(\"a\", JNum(1e20)).insert(\"b\", JArr([JBool(true), JNull])))\n    match json_parse(json_stringify(d)) { Ok(j) => json_stringify(j)\n        _ => \"ERR\" } }\n"),
            "{\"a\":100000000000000000000,\"b\":[true,null]}"
        );
    }

    #[test]
    fn diff_list_higher_order_ordering() {
        // sort_by is STABLE: elements with equal keys keep their original relative order.
        // Sorting [[3,1],[1,2],[3,3],[1,4],[2,5]] by the first field yields second fields
        // [2,4,5,1,3] (the two key-1 rows stay 2 before 4; the two key-3 rows stay 1 before 3).
        let sortby = "fun probe() -> Str { let xs = [[3, 1], [1, 2], [3, 3], [1, 4], [2, 5]]\n    \
                      \"{xs.sort_by(fn p { p.get(0).unwrap_or(0) }).map(fn p { p.get(1).unwrap_or(0) })}\" }\n";
        assert_eq!(differential(sortby), "[2, 4, 5, 1, 3]");
        // group_by keys by first-seen bucket order, elements within a bucket in original order.
        assert_eq!(
            differential("fun probe() -> Str { \"{[1, 2, 3, 4, 5, 6, 7].group_by(fn x { x % 3 })}\" }\n"),
            "Map{1: [1, 4, 7], 2: [2, 5], 0: [3, 6]}"
        );
        // zip_with truncates to the shorter list.
        assert_eq!(
            differential("fun probe() -> Str { \"{[1, 2, 3, 4].zip_with([10, 20], fn(a, b) { a + b })}|{[1].zip_with([10, 20, 30], fn(a, b) { a * b })}\" }\n"),
            "[11, 22]|[10]"
        );
        // flat_map preserves order (and an empty result filters); take_while/drop_while act on
        // the leading run; partition keeps order in both halves; max_by breaks ties to the first.
        assert_eq!(
            differential("fun probe() -> Str { \"{[1, 2, 3].flat_map(fn x { [x, x * 10] })}|{[1, 2, 3, 4].flat_map(fn x { if x % 2 == 0 { [x] } else { [] } })}\" }\n"),
            "[1, 10, 2, 20, 3, 30]|[2, 4]"
        );
        assert_eq!(
            differential("fun probe() -> Str { let xs = [1, 2, 3, 4, 1, 2]\n    \"{xs.take_while(fn x { x < 3 })}|{xs.drop_while(fn x { x < 3 })}|{xs.partition(fn x { x % 2 == 0 })}\" }\n"),
            "[1, 2]|[3, 4, 1, 2]|[[2, 4, 2], [1, 3, 1]]"
        );
        assert_eq!(
            differential("fun probe() -> Str { let xs = [[1, 5], [2, 5], [3, 1]]\n    \"{xs.min_by(fn p { p.get(1).unwrap_or(0) })}|{xs.max_by(fn p { p.get(1).unwrap_or(0) })}\" }\n"),
            "Some([3, 1])|Some([1, 5])"
        );
    }

    #[test]
    fn diff_list_scan_prefix_accumulation() {
        // PR-it113: `scan` is `fold` that keeps every running accumulator, byte-identical
        // on interp/KVM. Prefix sums, running max, empty list, and non-numeric accumulators.
        assert_eq!(
            differential("fun probe() -> Str { \"{[1, 2, 3, 4].scan(0, fn(a, x) { a + x })}\" }\n"),
            "[1, 3, 6, 10]"
        );
        assert_eq!(
            differential("fun probe() -> Str { let xs = [3, 1, 4, 1, 5, 9, 2]\n    \"{xs.scan(0, fn(m, x) { if x > m { x } else { m } })}\" }\n"),
            "[3, 3, 4, 4, 5, 9, 9]"
        );
        assert_eq!(differential("fun probe() -> Str { \"{[].scan(0, fn(a, x) { a + x })}\" }\n"), "[]");
        assert_eq!(
            differential("fun probe() -> Str { \"{[\"a\", \"b\", \"c\"].scan(\"\", fn(a, x) { \"{a}{x}\" })}\" }\n"),
            "[\"a\", \"ab\", \"abc\"]"
        );
    }

    #[test]
    fn diff_records_and_with_update() {
        // `with` is an IMMUTABLE update: q/r are new values; the original p is unchanged
        // (single field, multiple fields, and a field derived from the original) — PR-it126.
        let upd = "type Point = Point(x: Int, y: Int)\nfun probe() -> Str {\n    let p = Point(x: 3, y: 4)\n    \
                   let q = p with x: 5\n    let r = p with x: p.x + 10, y: p.y * 2\n    \
                   \"{q.x},{q.y}|{r.x},{r.y}|orig={p.x},{p.y}\"\n}\n";
        assert_eq!(differential(upd), "5,4|13,8|orig=3,4");
        // Anonymous record type: field access, structural equality, display in decl order.
        let anon = "type Entry = { key: Str, value: Int }\nfun probe() -> Str {\n    let e = Entry(key: \"k\", value: 10)\n    \
                    let e2 = e with value: 20\n    \
                    \"{e2.value}|orig={e.value}|{e}|{e == Entry(key: \"k\", value: 10)}|{e == e2}\"\n}\n";
        assert_eq!(differential(anon), "20|orig=10|Entry(\"k\", 10)|true|false");
        // Nested record: a deep `with` update leaves the original's inner record intact.
        let nest = "type Inner = Inner(v: Int)\ntype Outer = Outer(name: Str, inner: Inner)\n\
                    fun probe() -> Str {\n    let o = Outer(name: \"a\", inner: Inner(v: 1))\n    \
                    let o2 = o with inner: (o.inner with v: 99)\n    \"{o.inner.v}|{o2.inner.v}|{o}\"\n}\n";
        assert_eq!(differential(nest), "1|99|Outer(\"a\", Inner(1))");
    }

    #[test]
    fn diff_deeply_nested_generic_containers() {
        // Deeply-nested parametric containers display, access, and compare consistently on
        // interp/KVM — the nested Display uses the right brackets/braces/parens/quotes at
        // every level, including three levels deep (PR-it140).
        let display = "fun probe() -> Str { let a: Option[List[Int]] = Some([1, 2, 3])\n    \
                       let b: List[Option[Int]] = [Some(1), None, Some(3)]\n    \
                       let e: Option[List[Map[Str, List[Int]]]] = Some([Map().insert(\"k\", [9])])\n    \
                       \"{a}|{b}|{Map().insert(\"x\", [1, 2]).insert(\"y\", [3])}|{e}\" }\n";
        assert_eq!(differential(display), "Some([1, 2, 3])|[Some(1), None, Some(3)]|Map{\"x\": [1, 2], \"y\": [3]}|Some([Map{\"k\": [9]}])");
        // Access chains through nested containers, and Result/Set nesting Display.
        let access = "fun probe() -> Str { let m = Map().insert(\"k\", [10, 20, 30])\n    \
                      let r: List[Result[Int, Str]] = [Ok(1), Err(\"bad\"), Ok(3)]\n    \
                      let s: Map[Str, Set[Int]] = Map().insert(\"a\", Set([1, 1, 2]))\n    \
                      \"{m.get(\"k\").unwrap_or([]).get(1)}|{m.get(\"z\").unwrap_or([]).get(0)}|{r}|{s}\" }\n";
        assert_eq!(differential(access), "Some(20)|None|[Ok(1), Err(\"bad\"), Ok(3)]|Map{\"a\": Set{1, 2}}");
        // HOFs over nested collections + structural equality + empty containers at depth.
        let hof = "fun probe() -> Str { let xs: List[Map[Str, Int]] = [Map().insert(\"a\", 1), Map().insert(\"a\", 5)]\n    \
                   let nested = [[1, 2], [3], [4, 5, 6]]\n    let x = Some([1, 2])\n    let y = Some([1, 2])\n    let empty: Option[List[Int]] = Some([])\n    \
                   \"{xs.map(fn m { m.get(\"a\").unwrap_or(0) })}|{nested.flatten()}|{nested.map(fn ys { ys.sum() })}|{x == y}|{empty}\" }\n";
        assert_eq!(differential(hof), "[1, 5]|[1, 2, 3, 4, 5, 6]|[3, 3, 15]|true|Some([])");
    }

    #[test]
    fn diff_generics_depth() {
        // A generic fun used at several types, byte-identical on interp/KVM.
        let id = "fun id[T](x: T) -> T { x }\nfun probe() -> Str { \"{id(5)}|{id(\"hi\")}|{id([1, 2, 3])}|{id(true)}\" }\n";
        assert_eq!(differential(id), "5|hi|[1, 2, 3]|true");
        // Generic ADT: construct/unbox at two types + nested Box(Box(x)) display.
        let boxed = "type Box[T] = Box(v: T)\nfun unbox[T](b: Box[T]) -> T { match b { Box(x) => x } }\n\
                     fun probe() -> Str { \"{unbox(Box(42))}|{unbox(Box(\"hi\"))}|{Box(42)}|{Box(\"hi\")}|{Box(Box(7))}\" }\n";
        assert_eq!(differential(boxed), "42|hi|Box(42)|Box(\"hi\")|Box(Box(7))");
        // Two type params, a generic higher-order fun, and a generic tree fold.
        let pair = "type Pair[A, B] = Pair(fst: A, snd: B)\nfun probe() -> Str { match Pair(1, \"x\") { Pair(a, b) => \"{a}:{b}\" } }\n";
        assert_eq!(differential(pair), "1:x");
        let hof = "fun twice[T](f: fn(T) -> T, x: T) -> T { f(f(x)) }\nfun probe() -> Str { \"{twice(fn n { n + 1 }, 10)}|{twice(fn s { \"{s}!\" }, \"hi\")}\" }\n";
        assert_eq!(differential(hof), "12|hi!!");
        let tree = "type Tree[T] = Leaf(v: T) | Node(l: Tree[T], r: Tree[T])\n\
                    fun sum(t: Tree[Int]) -> Int { match t { Leaf(v) => v\n        Node(l, r) => sum(l) + sum(r) } }\n\
                    fun probe() -> Str { \"{sum(Node(Leaf(1), Node(Leaf(2), Leaf(3))))}\" }\n";
        assert_eq!(differential(tree), "6");
    }

    #[test]
    fn diff_float_fmt_vs_format_rounding_modes() {
        // A bug-hunt-17 lock (it244): KUPL has TWO fixed-precision float formatters with DIFFERENT
        // half-way rounding, and both must be byte-identical across engines. `fmt(n)` rounds
        // half-AWAY-from-zero (0.5->1, 2.5->3); `format(n)` rounds half-to-EVEN / banker's
        // (0.5->0, 2.5->2, locked at it5120). On the exact .5 ties they diverge from each other but
        // agree across interp/KVM (and native per the sweep). Pinning them side-by-side documents
        // that the two methods are NOT interchangeable — picking the wrong one silently shifts
        // rounding on financial/statistical output.
        let src = r#"fun probe() -> Str {
    "{(0.5).fmt(0)}/{(0.5).format(0)}|{(1.5).fmt(0)}/{(1.5).format(0)}|{(2.5).fmt(0)}/{(2.5).format(0)}|{(3.5).fmt(0)}/{(3.5).format(0)}"
}
"#;
        assert_eq!(differential(src), "1/0|2/2|3/2|4/4");
        // A few non-tie cases: fmt keeps n decimals, rounds the rest, negatives keep sign.
        assert_eq!(
            differential("fun probe() -> Str { let n = (0.0) - 3.14159\n    \"{(3.14159).fmt(4)}|{n.fmt(3)}|{(100.0).fmt(2)}|{(3.14159).fmt(0)}\" }\n"),
            "3.1416|-3.142|100.00|3"
        );
    }

    #[test]
    fn diff_set_algebra_on_records_structural() {
        // it219 locked a single Set of records dedup + `contains` by STRUCTURAL equality; this
        // extends it to set ALGEBRA between two record-Sets (it242). union/intersect/difference must
        // compare records field-by-field, not by identity — so intersect finds the shared
        // Point(3, 4), difference finds the Point(1, 2) present only in `a`, and the duplicate
        // Point(1, 2) in `a`'s constructor already deduped to one element. Byte-identical on
        // interp/KVM (native per the sweep); a certification lock, the rest of the batch (Set-of-
        // records via bug-hunt) was consistent.
        let src = r#"type Point = { x: Int, y: Int }
fun probe() -> Str {
    let a = Set([Point(x: 1, y: 2), Point(x: 3, y: 4), Point(x: 1, y: 2)])
    let b = Set([Point(x: 3, y: 4), Point(x: 5, y: 6)])
    "{a.len()}|{a.union(b).len()}|{a.intersect(b).len()}|{a.difference(b).len()}|{a.contains(Point(x: 1, y: 2))}|{a.intersect(b).to_list()}"
}
"#;
        assert_eq!(differential(src), "2|3|1|1|true|[Point(3, 4)]");
    }

    #[test]
    fn diff_map_merge_with_combiner_fold() {
        // A bug-hunt-16 lock (it241): plain `merge` (last-wins) is certified in it160/it4097; KUPL
        // has no builtin merge-with-fn, so the common "combine two count maps by SUMMING shared
        // keys" idiom is written by hand — fold one map's keys into the other, adding the values.
        // This exercises the ordering subtlety: the fold starts from `counts2` ({b, c}), updating a
        // shared key ("b") IN PLACE keeps its slot, and a new key ("a") appends — so keys iterate
        // [b, c, a], values [7, 7, 3]. Byte-identical on interp/KVM (native per the sweep); the rest
        // of the batch-16 sweep (List partition/zip_with, Option<->Result ok_or/ok, tensor
        // reductions) was already locked/consistent.
        let src = r#"fun probe() -> Str {
    let counts1 = Map().insert("a", 3).insert("b", 5)
    let counts2 = Map().insert("b", 2).insert("c", 7)
    let combined = counts1.keys().fold(counts2, fn(acc, k) {
        acc.insert(k, acc.get(k).unwrap_or(0) + counts1.get(k).unwrap_or(0))
    })
    "{combined}|{combined.get("b")}|{combined.keys()}"
}
"#;
        assert_eq!(
            differential(src),
            r#"Map{"b": 7, "c": 7, "a": 3}|Some(7)|["b", "c", "a"]"#
        );
    }

    #[test]
    fn diff_string_builder_report_in_loop() {
        // Extends diff_string_self_append (it202, the `s = s + x` concat form) to the report-builder
        // idiom an AI actually generates: accumulate a multiline string by INTERPOLATING the
        // accumulator back into itself each iteration — `report = "{report}  - {name}: {qty}\n"` —
        // while a parallel Int total accumulates. This composes string-interpolation accumulation
        // (a different code path from `+`), newline escapes, get-with-default, and parse_int, and is
        // byte-identical on interp/KVM (and native, per the sweep). The final `\n`-joined report reads
        // back exactly (PR-it240).
        let src = r#"fun probe() -> Str {
    let items = [["apple", "3"], ["banana", "12"], ["cherry", "7"]]
    var report = "Inventory:\n"
    var total = 0
    for row in items {
        let name = row.get(0).unwrap_or("?")
        let qty = row.get(1).unwrap_or("0").parse_int().unwrap_or(0)
        report = "{report}  - {name}: {qty}\n"
        total = total + qty
    }
    "{report}Total: {total}"
}
"#;
        assert_eq!(
            differential(src),
            "Inventory:\n  - apple: 3\n  - banana: 12\n  - cherry: 7\nTotal: 22"
        );
    }

    #[test]
    fn diff_json_nested_mixed_roundtrip() {
        // Extends diff_json_key_order_and_sort_stability (it162, a FLAT object's key order) to a
        // NESTED object with MIXED value types: a string, an array of numbers, and a nested object
        // with a bool. parse -> stringify round-trips the whole structure — key order preserved at
        // each level, compact formatting (no spaces), numbers without trailing `.0`, and the nested
        // object and array reproduced exactly. Byte-identical on interp/KVM (a bug-hunt-15 lock;
        // the rest of the batch — List chunk/window/scan, date leap/century-nonleap, tensor
        // reductions — was already locked/consistent) (PR-it238).
        let src = r#"fun probe() -> Str {
    let data = "\{\"name\": \"kupl\", \"nums\": [1, 2, 3], \"nested\": \{\"ok\": true\}\}"
    match json_parse(data) {
        Ok(v) => json_stringify(v)
        Err(e) => "err:{e}"
    }
}
"#;
        assert_eq!(differential(src), r#"{"name":"kupl","nums":[1,2,3],"nested":{"ok":true}}"#);
    }

    #[test]
    fn diff_map_build_from_pairs_and_group_by() {
        // A common AI-generated data-processing idiom, byte-identical on interp/KVM (and native, per
        // the sweep): (a) build a Map by folding a list of [key, value] pairs in a for-loop; (b) the
        // canonical GROUP-BY — accumulate a Map[Str, List] where each key maps to the list of items
        // sharing it, via get-or-empty-list, push, re-insert. This is a step beyond the scalar
        // Map[Str, Int] word-count (it217): the value is a growing List, so it exercises the empty
        // collection default `unwrap_or([])`, a List push that builds a new list, and a List-valued
        // re-insert (which keeps the key's position per it224). Keys iterate in first-seen order and
        // each bucket in encounter order (PR-it237).
        let src = r#"fun probe() -> Str {
    let pairs = [["a", "1"], ["b", "2"], ["c", "3"]]
    var m = Map()
    for p in pairs {
        m = m.insert(p.get(0).unwrap_or(""), p.get(1).unwrap_or(""))
    }
    var groups = Map()
    for w in ["apple", "ant", "bear", "bee", "cat"] {
        let key = w.chars().get(0).unwrap_or("?")
        groups = groups.insert(key, groups.get(key).unwrap_or([]).push(w))
    }
    "{m}|{m.len()}|{groups.get("a")}|{groups.get("b")}|{groups.get("c")}|{groups.keys()}"
}
"#;
        assert_eq!(
            differential(src),
            r#"Map{"a": "1", "b": "2", "c": "3"}|3|Some(["apple", "ant"])|Some(["bear", "bee"])|Some(["cat"])|["a", "b", "c"]"#
        );
    }

    #[test]
    fn diff_set_algebra_composed_chain() {
        // A bug-hunt-14 lock (it235): it161/it123 certified each set operation in ISOLATION on a
        // freshly-constructed set; this certifies a COMPOSED chain where each op's result (a new set)
        // feeds the next — a.union(b).difference(c).intersect(d). Insertion order must be preserved
        // THROUGH the composition, not just for single ops, so the intermediate sets carry their
        // order into the next operation. Byte-identical on interp/KVM (and native, per the sweep):
        // {1,2,3,4,5} ∪ {4,5,6,7} = {1..7}, ∖ {2,4,6} = {1,3,5,7}, ∩ {1,3,5,7,9} = {1,3,5,7}. The rest
        // of the batch-14 sweep (recursive-ADT tree sum/depth, Map values fold/map_values, Result `?`
        // type-matching K0238) was already locked/consistent.
        let src = r#"fun probe() -> Str {
    let a = Set([1, 2, 3, 4, 5])
    let b = Set([4, 5, 6, 7])
    let c = Set([2, 4, 6])
    let chain = a.union(b).difference(c).intersect(Set([1, 3, 5, 7, 9]))
    let chain2 = b.union(c).symmetric_difference(a).difference(Set([7]))
    "{chain}|{chain2}|{chain.len()}"
}
"#;
        assert_eq!(differential(src), "Set{1, 3, 5, 7}|Set{6, 1, 3}|4");
    }

    #[test]
    fn diff_nested_closure_loop_capture_and_currying() {
        // Extends diff_higher_order_and_closure_depth (it202/it3415, a 1-level returned closure and
        // a no-arg loop-capture) with two deeper cases, byte-identical on interp/KVM (and native, per
        // the sweep):
        //   (a) Closures built in a loop each capture the loop variable BY VALUE at creation AND take
        //       a call argument, then are applied AFTER the loop with the same arg — so the captured
        //       value composes with the argument: maker_k(7) = k*100 + 7, giving 7, 107, 207 (not
        //       the final loop value 2 three times).
        //   (b) THREE-level currying: adder(base) returns a closure over base that returns a closure
        //       over mid that returns a closure over inner, so adder(100)(20)(3) = 123 and each
        //       nesting level's binding survives to the innermost call (PR-it234).
        let loopcap = r#"fun probe() -> Str {
    var makers: List[fn(Int) -> Int] = []
    var i = 0
    while i < 3 {
        let captured = i
        makers = makers.push(fn x { captured * 100 + x })
        i = i + 1
    }
    let m0 = makers.get(0).unwrap_or(fn x { 0 - 1 })
    let m1 = makers.get(1).unwrap_or(fn x { 0 - 1 })
    let m2 = makers.get(2).unwrap_or(fn x { 0 - 1 })
    "{m0(7)}|{m1(7)}|{m2(7)}"
}
"#;
        assert_eq!(differential(loopcap), "7|107|207");
        let curry = r#"fun adder(base: Int) -> fn(Int) -> fn(Int) -> Int {
    fn mid { fn inner { base + mid + inner } }
}
fun probe() -> Str {
    let a = adder(100)
    let b = a(20)
    "{b(3)}|{adder(1)(2)(3)}|{a(0)(5)}"
}
"#;
        assert_eq!(differential(curry), "123|6|105");
    }

    #[test]
    fn diff_string_lines_split_semantics() {
        // A bug-hunt-13 lock (it233): `Str.lines()` follows Rust's `.lines()` semantics, which differ
        // from `split("\n")` in two ways that are easy to get wrong on one engine — an EMPTY string
        // yields an EMPTY list (`[]`, not `[""]`), and a TRAILING newline produces NO trailing empty
        // element (`"a\nb\n"` -> `["a", "b"]`, whereas split would give `["a", "b", ""]`). Byte-
        // identical on interp/KVM (and native, per the sweep). The rest of the batch-13 sweep (float
        // inf/nan/-0 Display, Str split/join, ADT match guards, Option map/and_then/filter) was
        // already locked/consistent.
        let src = r#"fun probe() -> Str {
    let s = "line1\nline2\nline3"
    let empty = ""
    let trailing = "a\nb\n"
    "{s.lines()}|{s.lines().len()}|{empty.lines()}|{trailing.lines()}|{"abc".chars()}|{"abc".chars().len()}"
}
"#;
        assert_eq!(
            differential(src),
            r#"["line1", "line2", "line3"]|3|[]|["a", "b"]|["a", "b", "c"]|3"#
        );
    }

    #[test]
    fn diff_rational_harmonic_accumulation() {
        // The exact-fraction parallel to diff_bigint_fibonacci_accumulation (it230): the numeric
        // tower (it169) certified single Rational ops (1/3 + 1/6 = 1/2) but not ACCUMULATION across
        // a loop, where each addition of 1/i grows the common denominator and the running result must
        // stay GCD-reduced. A harmonic-series generator sums 1/1 + 1/2 + ... + 1/n as an exact
        // Rational: H(1)=1, H(4)=25/12, H(5)=137/60, H(10)=7381/2520 (the true harmonic numbers).
        // Byte-identical on interp/KVM (and native, per the native test) — the native reduction must
        // keep the same numerator/denominator at every step (PR-it231).
        let src = r#"fun harmonic(n: Int) -> Rational {
    var acc = rat(0, 1)
    var i = 1
    while i <= n {
        acc = acc + rat(1, i)
        i = i + 1
    }
    acc
}
fun probe() -> Str {
    "{harmonic(1)}|{harmonic(4)}|{harmonic(5)}|{harmonic(10)}"
}
"#;
        assert_eq!(differential(src), "1|25/12|137/60|7381/2520");
    }

    #[test]
    fn diff_bigint_fibonacci_accumulation() {
        // A bug-hunt-12 lock (it230): the numeric tower (it169) certified BigInt pow (2^70) and
        // factorial (25!) — multiplicative growth — but not ADDITIVE accumulation across a loop.
        // A Fibonacci generator adds two BigInts each turn for n turns; fib(100) is a 21-digit number
        // (354224848179261915075) that far exceeds i64::MAX, so every intermediate carry must be
        // exact. Byte-identical on interp/KVM (and native, per the native test) — the native C bignum
        // must produce the same digits (PR-it230). The rest of the batch-12 sweep (nested
        // Option/Result Display, string replace/repeat, negative modulo) was already locked/consistent.
        let src = r#"fun fib_big(n: Int) -> BigInt {
    var a = big(0)
    var b = big(1)
    var i = 0
    while i < n {
        let t = a + b
        a = b
        b = t
        i = i + 1
    }
    a
}
fun probe() -> Str {
    "{fib_big(10)}|{fib_big(50)}|{fib_big(100)}"
}
"#;
        assert_eq!(differential(src), "55|12586269025|354224848179261915075");
    }

    #[test]
    fn diff_hof_comprehension_pipeline() {
        // A composed higher-order pipeline — filter -> sort_by -> map -> fold, plus position/all/any
        // and sum — is byte-identical on interp/KVM (and native, per the sweep). This chains the
        // individual list HOFs certified in it127/165/212 into the kind of comprehension an AI would
        // generate, and critically exercises STABLE sort_by across ties: two people aged 25 (Bob,
        // Di) keep insertion order, and two names of length 3 (Ann, Bob) do too (PR-it229).
        let recs = r#"type P = { name: Str, age: Int }
fun probe() -> Str {
    let people = [P(name: "Ann", age: 30), P(name: "Bob", age: 25), P(name: "Cy", age: 35), P(name: "Di", age: 25)]
    let adults = people.filter(fn p { p.age >= 25 }).sort_by(fn p { p.age }).map(fn p { p.name })
    let total = people.map(fn p { p.age }).filter(fn a { a > 24 }).fold(0, fn(acc, a) { acc + a })
    let names = people.map(fn p { p.name }).sort_by(fn s { s.len() })
    "{adults}|{total}|{names}|{people.map(fn p { p.age }).sum()}"
}
"#;
        assert_eq!(
            differential(recs),
            r#"["Bob", "Di", "Ann", "Cy"]|115|["Cy", "Di", "Ann", "Bob"]|115"#
        );
        let ints = r#"fun probe() -> Str {
    let xs = [5, 3, 8, 1, 9, 2, 7]
    let evens = xs.filter(fn x { x % 2 == 0 })
    let sorted = xs.sort_by(fn x { x })
    let firstBig = xs.position(fn x { x > 6 })
    "{evens}|{sorted}|{firstBig}|{xs.all(fn x { x > 0 })}|{xs.any(fn x { x > 8 })}|{xs.map(fn x { x * x }).fold(0, fn(a, x) { a + x })}"
}
"#;
        assert_eq!(differential(ints), "[8, 2]|[1, 2, 3, 5, 7, 8, 9]|Some(2)|true|true|233");
    }

    #[test]
    fn diff_ackermann_nonprimitive_recursion() {
        // A bug-hunt-11 lock (it227): the Ackermann function is the canonical NON-primitive-recursive
        // function — its `ackermann(m - 1, ackermann(m, n - 1))` arm nests a recursive call inside
        // another call's argument, so both the recursion depth AND the inner argument are themselves
        // computed by recursion. This stresses call-stack management and argument-evaluation order far
        // beyond simple or mutual recursion (it139), and is byte-identical on interp/KVM: A(1,5)=7,
        // A(2,2)=7, A(2,4)=11, A(2,5)=13. (The deeper m=3 cases A(3,3)=61 and A(3,4)=125 are
        // exercised by the native test in cgen.rs, which runs in a subprocess with a full stack.
        // Ackermann nests a recursive call in argument position, holding several eval frames open per
        // KUPL level, so even A(3,2) overflows the 2 MB test-thread stack in a debug build — hence the
        // in-process differential stays at m<=2.) The rest of the batch-11 sweep (List.get OOB->None,
        // flatten, try `?` chains, mutual recursion) was already locked and consistent.
        let src = r#"fun ackermann(m: Int, n: Int) -> Int {
    if m == 0 { n + 1 }
    else { if n == 0 { ackermann(m - 1, 1) } else { ackermann(m - 1, ackermann(m, n - 1)) } }
}
fun probe() -> Str {
    "{ackermann(0, 0)}|{ackermann(1, 5)}|{ackermann(2, 2)}|{ackermann(2, 4)}|{ackermann(2, 5)}"
}
"#;
        assert_eq!(differential(src), "1|7|7|11|13");
    }

    #[test]
    fn diff_mutual_recursion() {
        // Mutually-recursive functions (a calls b, b calls a) work regardless of definition
        // order — is_odd is defined AFTER is_even yet each calls the other. Byte-identical on
        // interp/KVM; native must forward-declare every function for this to compile (PR-it139).
        // The even/odd depth (1000) needs the interpreter's full stack, so run on a big-stack
        // thread like the other deep-recursion differential tests.
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(|| {
                let evenodd = "fun is_even(n: Int) -> Bool { if n == 0 { true } else { is_odd(n - 1) } }\n\
                               fun is_odd(n: Int) -> Bool { if n == 0 { false } else { is_even(n - 1) } }\n\
                               fun probe() -> Str { \"{is_even(10)}|{is_odd(7)}|{is_even(1000)}|{is_odd(0)}\" }\n";
                assert_eq!(differential(evenodd), "true|true|true|false");
                // A three-way cycle a -> b -> c -> a terminates and cycles deterministically.
                let cycle = "fun a(n: Int) -> Str { if n <= 0 { \"a\" } else { b(n - 1) } }\n\
                             fun b(n: Int) -> Str { if n <= 0 { \"b\" } else { c(n - 1) } }\n\
                             fun c(n: Int) -> Str { if n <= 0 { \"c\" } else { a(n - 1) } }\n\
                             fun probe() -> Str { \"{a(0)}{a(1)}{a(2)}{a(3)}{a(9)}\" }\n";
                assert_eq!(differential(cycle), "abcaa");
                // Mutual recursion with mixed return types (Int and Str), the Str fn defined
                // between the two Int fns (a backward and a forward reference in one program).
                let mixed = "fun ping(n: Int) -> Int { if n <= 0 { 0 } else { pong(n - 1) + 1 } }\n\
                             fun label(n: Int) -> Str { if ping(n) > 2 { \"big\" } else { \"small\" } }\n\
                             fun pong(n: Int) -> Int { if n <= 0 { 0 } else { ping(n - 1) + 1 } }\n\
                             fun probe() -> Str { \"{ping(6)}|{label(5)}|{label(1)}\" }\n";
                assert_eq!(differential(mixed), "6|big|small");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn diff_closures_as_first_class_values() {
        // Closures are first-class: stored in a List and applied; RETURNED from a function
        // capturing its argument (escaping the creating call) so add3 and add10 keep
        // independent captures; composed; and curried three deep — byte-identical on
        // interp/KVM (PR-it147).
        let ret = "fun adder(n: Int) -> fn(Int) -> Int { fn x { x + n } }\n\
                   fun compose(f: fn(Int) -> Int, g: fn(Int) -> Int) -> fn(Int) -> Int { fn x { f(g(x)) } }\n\
                   fun add(a: Int) -> fn(Int) -> fn(Int) -> Int { fn b { fn c { a + b + c } } }\n\
                   fun probe() -> Str {\n    let fns = [fn x { x + 1 }, fn x { x * 2 }, fn x { x * x }]\n    \
                   let add3 = adder(3)\n    let add10 = adder(10)\n    \
                   \"{fns.map(fn f { f(10) })}|{add3(4)}|{add10(4)}|{add3(100)}|{compose(add3, add10)(1)}|{add(1)(2)(3)}\"\n}\n";
        assert_eq!(differential(ret), "[11, 20, 100]|7|14|103|14|6");
        // Value-capture (it76) survives storage: each loop iteration's closure captures its
        // own `i` by VALUE, so applying them gives [1, 2, 3], not [3, 3, 3].
        let cap = "fun probe() -> Str {\n    var fns: List[fn() -> Int] = []\n    for i in 1..4 {\n        fns = fns.push(fn() { i })\n    }\n    \"{fns.map(fn f { f() })}\"\n}\n";
        assert_eq!(differential(cap), "[1, 2, 3]");
    }

    #[test]
    fn diff_int_leading_trailing_zeros() {
        // The NEW leading_zeros/trailing_zeros are byte-identical on interp/KVM over the 64-bit
        // pattern. The critical case is 0 -> 64 (both), which the native backend must guard
        // because C clz/ctz of 0 is undefined behavior (PR-it188).
        let src = r#"fun probe() -> Str {
    let lz = "{(0).leading_zeros()}|{(1).leading_zeros()}|{(0 - 1).leading_zeros()}|{(9223372036854775807).leading_zeros()}|{(1024).leading_zeros()}"
    let tz = "{(0).trailing_zeros()}|{(1).trailing_zeros()}|{(8).trailing_zeros()}|{(0 - 9223372036854775807 - 1).trailing_zeros()}|{(1024).trailing_zeros()}"
    "{lz}#{tz}"
}
"#;
        assert_eq!(differential(src), "64|63|0|1|53#64|0|3|63|10");
    }

    #[test]
    fn diff_int_count_ones() {
        // The NEW count_ones (popcount) is byte-identical on interp/KVM over the 64-bit two's-
        // complement pattern: (-1) has all 64 bits set, i64::MAX has 63, i64::MIN has 1
        // (PR-it186).
        let src = r#"fun probe() -> Str {
    "{(0).count_ones()}|{(7).count_ones()}|{(255).count_ones()}|{(0 - 1).count_ones()}|{(1024).count_ones()}|{(9223372036854775807).count_ones()}|{(0 - 9223372036854775807 - 1).count_ones()}"
}
"#;
        assert_eq!(differential(src), "0|3|8|64|1|63|1");
    }

    #[test]
    fn diff_int_factorial() {
        // The NEW factorial() is byte-identical on interp/KVM: 0!=1!=1, 20! is the largest that
        // fits i64, 21! is a checked-overflow panic, and a negative is a clean panic (PR-it185).
        let src = r#"fun probe() -> Str {
    "{(0).factorial()}|{(1).factorial()}|{(5).factorial()}|{(10).factorial()}|{(20).factorial()}"
}
"#;
        assert_eq!(differential(src), "1|1|120|3628800|2432902008176640000");
        assert_eq!(
            differential("fun probe() -> Str { \"{(21).factorial()}\" }\n"),
            "panic: integer overflow in `factorial`"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{(0 - 3).factorial()}\" }\n"),
            "panic: `factorial` of a negative Int"
        );
    }

    #[test]
    fn diff_int_digits() {
        // The NEW Int.digits() returns the base-10 digits of |n| most-significant first,
        // byte-identical on interp/KVM. Edges: 0 -> [0], negatives use the magnitude, and i64::MIN
        // works because the impl takes unsigned_abs (2^63) rather than .abs() (which overflows)
        // (PR-it209).
        let src = r#"fun probe() -> Str {
    let imin = (0 - 9223372036854775807) - 1
    "{(12345).digits()}|{(0).digits()}|{(0 - 12345).digits()}|{(100).digits()}|{(9223372036854775807).digits()}|{imin.digits()}"
}
"#;
        assert_eq!(
            differential(src),
            "[1, 2, 3, 4, 5]|[0]|[1, 2, 3, 4, 5]|[1, 0, 0]|[9, 2, 2, 3, 3, 7, 2, 0, 3, 6, 8, 5, 4, 7, 7, 5, 8, 0, 7]|[9, 2, 2, 3, 3, 7, 2, 0, 3, 6, 8, 5, 4, 7, 7, 5, 8, 0, 8]"
        );
    }

    #[test]
    fn diff_list_intersperse() {
        // The NEW intersperse(sep) inserts sep between each pair of adjacent elements,
        // byte-identical on interp/KVM. Empty and singleton lists are unchanged (no separator to
        // place), and it works for any element type (PR-it212).
        let src = r#"fun probe() -> Str {
    let a = [1, 2, 3, 4]
    let empty: List[Int] = []
    "{a.intersperse(0)}|{[9].intersperse(0)}|{empty.intersperse(0)}|{["a", "b", "c"].intersperse("-")}|{[1, 2].intersperse(99)}"
}
"#;
        assert_eq!(
            differential(src),
            r#"[1, 0, 2, 0, 3, 0, 4]|[9]|[]|["a", "-", "b", "-", "c"]|[1, 99, 2]"#
        );
    }

    #[test]
    fn diff_list_rotate() {
        // The NEW rotate_left(n)/rotate_right(n) cyclically shift a list, byte-identical on
        // interp/KVM. n is floor-modded by the length so a full rotation (n==len) and n>len are
        // identities-mod-len, and a NEGATIVE n reverses direction (rotate_right(-1) == left-1).
        // Empty and singleton lists are unchanged (PR-it208).
        let src = r#"fun probe() -> Str {
    let a = [1, 2, 3, 4, 5]
    let empty: List[Int] = []
    "{a.rotate_left(2)}|{a.rotate_right(1)}|{a.rotate_left(0)}|{a.rotate_left(5)}|{a.rotate_left(7)}|{a.rotate_right(0 - 1)}|{empty.rotate_left(3)}|{[9].rotate_left(1)}"
}
"#;
        assert_eq!(
            differential(src),
            "[3, 4, 5, 1, 2]|[5, 1, 2, 3, 4]|[1, 2, 3, 4, 5]|[1, 2, 3, 4, 5]|[3, 4, 5, 1, 2]|[2, 3, 4, 5, 1]|[]|[9]"
        );
    }

    #[test]
    fn diff_list_dedup() {
        // The NEW List.dedup collapses only CONSECUTIVE equal runs (Unix `uniq`), byte-identical
        // on interp/KVM — and is distinct from unique(): the trailing run of 1s reappears in dedup
        // but not in unique (PR-it203).
        let src = r#"fun probe() -> Str {
    let a = [1, 1, 2, 2, 2, 3, 1, 1]
    let empty: List[Int] = []
    "{a.dedup()}|{a.unique()}|{[5].dedup()}|{empty.dedup()}|{["x", "x", "y", "x"].dedup()}"
}
"#;
        assert_eq!(
            differential(src),
            r#"[1, 2, 3, 1]|[1, 2, 3]|[5]|[]|["x", "y", "x"]"#
        );
    }

    #[test]
    fn diff_shadowing_and_closure_value_capture() {
        // Lexical scoping is byte-identical across interp/KVM: a `let` inside a block shadows an
        // outer binding of the same name and the outer value is RESTORED after the block; and a
        // closure captures the VALUE of a variable at creation, not a later mutation (PR-it202).
        let shadow = r#"fun probe() -> Str {
    let x = 1
    let a = x
    let inner = {
        let x = 100
        x + 5
    }
    let b = x
    "{a}|{inner}|{b}"
}
"#;
        assert_eq!(differential(shadow), "1|105|1");
        // Curried closures each capture their own n; and mutating n after capture doesn't change
        // an already-created closure (capture is by value: f(1) is 1+10, not 1+99).
        let cap = r#"fun make_adder(n: Int) -> fn(Int) -> Int {
    fn x { x + n }
}
fun probe() -> Str {
    let add5 = make_adder(5)
    let add10 = make_adder(10)
    var n = 10
    let f = fn x { x + n }
    n = 99
    "{add5(1)}|{add10(1)}|{add5(add10(0))}|{f(1)}"
}
"#;
        assert_eq!(differential(cap), "6|11|15|11");
    }

    #[test]
    fn diff_sort_stability_json_numbers_regex() {
        // A sixth bug-hunt sweep (it211, ~10 probes) found no divergence; this locks its subtlest
        // deterministic edges, byte-identical on interp/KVM: sort_by is STABLE (records with equal
        // keys keep their original order), JSON serialization prints integer-valued floats without
        // a trailing .0 while keeping fractional precision, and regex classes/anchors/alternation
        // plus find_all agree (PR-it211).
        let stable = r#"type P = { name: Str, age: Int }
fun probe() -> Str {
    let ps = [P(name: "a", age: 30), P(name: "b", age: 20), P(name: "c", age: 30), P(name: "d", age: 20)]
    "{ps.sort_by(fn p { p.age }).map(fn p { p.name }).join(",")}"
}
"#;
        assert_eq!(differential(stable), "b,d,a,c");
        let js = r#"fun probe() -> Str {
    let j = JArr([JNum(0.0 - 3.5), JNum(1000000.0), JNum(0.001), JNum(42.0)])
    match json_parse(json_stringify(j)) {
        Ok(v) => "{json_stringify(v)}"
        Err(e) => "err:{e}"
    }
}
"#;
        assert_eq!(differential(js), "[-3.5,1000000,0.001,42]");
        let rx = r#"fun probe() -> Str {
    "{re_match("[a-z]+", "hello")}|{re_match("^\\d+$", "12345")}|{re_match("cat|dog", "dog")}|{re_find_all("\\d+", "a1b22c333")}"
}
"#;
        assert_eq!(differential(rx), r#"true|true|true|["1", "22", "333"]"#);
    }

    #[test]
    fn diff_operator_precedence_and_short_circuit() {
        // The operator-precedence ladder and left-associativity evaluate identically on
        // interp/KVM, and && / || short-circuit — a RHS that would PANIC (1/0) is never reached
        // when the LHS already decides the result (PR-it201).
        let prec = r#"fun probe() -> Str {
    "{2 + 3 * 4}|{(2 + 3) * 4}|{10 - 2 - 3}|{2 * 3 + 4 * 5}|{20 / 4 / 5}|{7 % 3 + 1}|{1 + 2 < 4 && 5 > 3}|{true || false && false}|{2 + 3 == 5 && 4 * 2 > 7}|{0 - 2 + 3}"
}
"#;
        assert_eq!(differential(prec), "14|20|5|26|1|2|true|true|true|1");
        // Short-circuit: boom() divides by zero, so if it were evaluated the program would panic.
        let sc = r#"fun boom() -> Bool {
    let x = 1 / 0
    x > 0
}
fun probe() -> Str {
    "{false && boom()}|{true || boom()}"
}
"#;
        assert_eq!(differential(sc), "false|true");
    }

    #[test]
    fn diff_numeric_literal_forms() {
        // Hex/binary/underscore int literals and scientific/underscore float literals all lex to
        // the SAME runtime value on interp/KVM, and the hex i64 boundary wraps as two's-complement
        // (0xFFFF_FFFF_FFFF_FFFF = -1, 0x8000...0 = i64::MIN) — a native test pins the float forms
        // against native's own printer (PR-it207).
        let ints = r#"fun probe() -> Str {
    "{0xFF}|{0xff}|{0b1010}|{1_000_000}|{0x00FF_FF00}|{0b1111_0000}"
}
"#;
        assert_eq!(differential(ints), "255|255|10|1000000|16776960|240");
        let hexb = r#"fun probe() -> Str {
    "{0xFFFFFFFFFFFFFFFF}|{0x7FFFFFFFFFFFFFFF}|{0x8000000000000000}"
}
"#;
        assert_eq!(differential(hexb), "-1|9223372036854775807|-9223372036854775808");
        let floats = r#"fun probe() -> Str {
    "{1e3}|{1.5e2}|{2.5e-3}|{1_000.5}|{6.022e23}|{1E4}"
}
"#;
        assert_eq!(
            differential(floats),
            "1000.0|150.0|0.0025|1000.5|602200000000000027262976.0|10000.0"
        );
    }

    #[test]
    fn diff_nested_match_shadow_map_wordcount_curry() {
        // An eighth bug-hunt sweep (it217, ~10 probes) found no divergence; this locks its subtlest
        // deterministic edges, byte-identical on interp/KVM: a match-arm binding shadows an outer
        // binding of the same name and the outer is restored after (nested even within another
        // match), a mutable Map accumulated in a for-loop counts occurrences in insertion order,
        // and a curried closure captures each level's argument (PR-it217).
        let shadow = r#"type Box = B(v: Int)
fun probe() -> Str {
    let v = 100
    let r = match B(5) { B(v) => match B(v * 2) { B(v) => v } }
    "{r}|{v}"
}
"#;
        assert_eq!(differential(shadow), "10|100");
        let wc = r#"fun probe() -> Str {
    var m = Map()
    for w in ["a", "b", "a", "c", "a", "b"] {
        m = m.insert(w, m.get(w).unwrap_or(0) + 1)
    }
    "{m.get("a")}|{m.get("b")}|{m.get("c")}|{m.keys()}"
}
"#;
        assert_eq!(differential(wc), r#"Some(3)|Some(2)|Some(1)|["a", "b", "c"]"#);
        let curry = r#"fun adder(n: Int) -> fn(Int) -> fn(Int) -> Int {
    fn a { fn b { n + a + b } }
}
fun probe() -> Str {
    "{adder(100)(20)(3)}|{adder(1)(2)(3)}"
}
"#;
        assert_eq!(differential(curry), "123|6");
    }

    #[test]
    fn diff_string_escape_len_brace_split() {
        // Extends diff_string_escape_sequences (it145) with three further guarantees, byte-identical
        // on interp/KVM: (a) \r also decodes to a single char (so "a\rb".len() == 3, giving the
        // 33333 run across \t \n \r \\ \"); (b) the BACKSLASH-brace escape "\{x\}" yields the literal
        // text "{x}" with no interpolation — a mechanism distinct from the {{ }} double-brace escape
        // certified in it144; (c) splitting on a real "\t"/"\n"/"\\" confirms each escape decoded to
        // the actual control/backslash character rather than a two-char sequence (PR-it223).
        let src = r#"fun probe() -> Str {
    let sep = "\t"
    "{"a\tb".len()}{"a\nb".len()}{"a\rb".len()}{"a\\b".len()}{"a\"b".len()}|{"\{x\}"}|{"x\ty\tz".split(sep).len()}|{"line1\nline2".split("\n").len()}|{"a\\b".split("\\").len()}"
}
"#;
        assert_eq!(differential(src), "33333|{x}|3|2|2");
    }

    #[test]
    fn diff_generic_monomorphization_across_types() {
        // A single generic function instantiated at MANY types (Int, Str, Bool, record, List,
        // Option) produces byte-identical results on interp/KVM — the native backend must
        // monomorphize/dispatch each instantiation the same way the interpreter does, including
        // nested generic instantiations like first(dup(record)) and unwrap_or(wrap(list)) (PR-it222).
        let g = r#"type Pt = { x: Int, y: Int }
fun id[T](x: T) -> T { x }
fun dup[T](x: T) -> List[T] { [x, x] }
fun first[T](xs: List[T]) -> Option[T] { xs.first() }
fun probe() -> Str {
    "{id(5)}|{id("hi")}|{id(true)}|{id(Pt(x: 1, y: 2))}|{dup(3)}|{dup("z")}|{first([10, 20])}|{first(dup(Pt(x: 9, y: 8)))}"
}
"#;
        assert_eq!(
            differential(g),
            r#"5|hi|true|Pt(1, 2)|[3, 3]|["z", "z"]|Some(10)|Some(Pt(9, 8))"#
        );
        let opt = r#"fun wrap[T](x: T) -> Option[T] { Some(x) }
fun unwrap_or[T](o: Option[T], d: T) -> T {
    match o {
        Some(v) => v
        None => d
    }
}
fun probe() -> Str {
    "{wrap(42)}|{wrap("txt")}|{unwrap_or(wrap(1), 0)}|{unwrap_or(wrap([1, 2]), [])}|{unwrap_or(None, 99)}"
}
"#;
        assert_eq!(differential(opt), r#"Some(42)|Some("txt")|1|[1, 2]|99"#);
    }

    #[test]
    fn diff_record_equality_set_dedup_emoji() {
        // A ninth bug-hunt sweep (it219, ~10 probes) found no divergence; this locks its subtlest
        // deterministic edges, byte-identical on interp/KVM: record equality is STRUCTURAL (deep,
        // so a Set of records dedups by value and nested records compare field-by-field), and
        // string length / casing / reversal are char-indexed and unicode-correct — an emoji counts
        // as one char and survives to_upper and a double reverse (PR-it219).
        let rec = r#"type Pt = { x: Int, y: Int }
type Line = { a: Pt, b: Pt }
fun probe() -> Str {
    let s = Set([Pt(x: 1, y: 2), Pt(x: 1, y: 2), Pt(x: 3, y: 4)])
    let l1 = Line(a: Pt(x: 0, y: 0), b: Pt(x: 1, y: 1))
    let l2 = Line(a: Pt(x: 0, y: 0), b: Pt(x: 1, y: 1))
    let l3 = Line(a: Pt(x: 0, y: 0), b: Pt(x: 2, y: 2))
    "{s.len()}|{s.contains(Pt(x: 1, y: 2))}|{l1 == l2}|{l1 == l3}|{l1}"
}
"#;
        assert_eq!(
            differential(rec),
            "2|true|true|false|Line(Pt(0, 0), Pt(1, 1))"
        );
        let emoji = r#"fun probe() -> Str {
    let s = "héllo 🎉 wörld"
    "{s.len()}|{s.chars().len()}|{s.to_upper()}|{s.reverse().reverse()}"
}
"#;
        assert_eq!(differential(emoji), "13|13|HéLLO 🎉 WöRLD|héllo 🎉 wörld");
    }

    #[test]
    fn diff_tensor_elementwise_reductions_dot() {
        // Tensor elementwise ops, reductions, dot product, and scalar scaling are byte-identical
        // across interp/KVM — including the IEEE subtlety that scaling a +0.0 element by -1.0
        // yields -0.0, and that chained arange/scale/add compose exactly (PR-it218).
        let ops = r#"fun probe() -> Str {
    let a = tensor([1.0, 2.0, 3.0, 4.0])
    let b = tensor([10.0, 20.0, 30.0, 40.0])
    "{a + b}|{a * b}|{a - b}|{a.scale(2.0)}|{a.sum()}|{a.mean()}|{a.max()}|{a.min()}|{a.dot(b)}"
}
"#;
        assert_eq!(
            differential(ops),
            "Tensor([11.0, 22.0, 33.0, 44.0])|Tensor([10.0, 40.0, 90.0, 160.0])|Tensor([-9.0, -18.0, -27.0, -36.0])|Tensor([2.0, 4.0, 6.0, 8.0])|10.0|2.5|4.0|1.0|300.0"
        );
        let negzero = r#"fun probe() -> Str {
    let a = tensor([0.0 - 3.0, 0.0, 1.5, 0.0 - 0.5])
    "{a.scale(0.0 - 1.0)}|{a.sum()}|{a.mean()}"
}
"#;
        assert_eq!(differential(negzero), "Tensor([3.0, -0.0, -1.5, 0.5])|-2.0|-0.5");
        let chained = r#"fun probe() -> Str {
    let a = arange(5).scale(1.5)
    let b = a + arange(5)
    "{b}|{b.sum()}|{b.dot(arange(5))}"
}
"#;
        assert_eq!(differential(chained), "Tensor([0.0, 2.5, 5.0, 7.5, 10.0])|25.0|75.0");
    }

    #[test]
    fn diff_map_set_iteration_order_after_mutation() {
        // Map and Set iterate in INSERTION order deterministically across interp/KVM, and the
        // order is stable through a mixed mutation sequence: a removed-then-reinserted key lands at
        // the END (a fresh insertion, not its old slot), and reinserting an already-present element
        // is a no-op that keeps its position (PR-it216).
        let m = r#"fun probe() -> Str {
    let m = Map().insert("a", 1).insert("b", 2).insert("c", 3).insert("d", 4)
    let m2 = m.remove("b").remove("d").insert("b", 20).insert("e", 5)
    "{m2.keys()}|{m2.values()}|{m2}"
}
"#;
        assert_eq!(
            differential(m),
            r#"["a", "c", "b", "e"]|[1, 3, 20, 5]|Map{"a": 1, "c": 3, "b": 20, "e": 5}"#
        );
        let s = r#"fun probe() -> Str {
    let s: Set[Int] = Set([3, 1, 4, 1, 5, 9, 2, 6])
    let s2 = s.remove(4).insert(7).insert(1)
    "{s2}|{s2.len()}"
}
"#;
        assert_eq!(differential(s), "Set{3, 1, 5, 9, 2, 6, 7}|7");
    }

    #[test]
    fn diff_map_update_existing_key_keeps_position() {
        // Complements diff_map_set_iteration_order_after_mutation (it216, which covers
        // remove-then-reinsert landing at the END): re-inserting an already-present key WITHOUT
        // removing it first UPDATES the value in place and KEEPS the key's original insertion
        // position — it does not move to the end. So inserting x,y,z then insert("y", 20) leaves
        // y in slot 2 with value 20; after remove("x") the order is [y, z]. Byte-identical on
        // interp/KVM (PR-it224, a tenth bug-hunt sweep that otherwise found every probed area — JSON
        // nesting, Map merge, sized-int overflow panic/wrap/saturate, RNG determinism — already
        // locked and consistent).
        let src = r#"fun probe() -> Str {
    let m = Map().insert("x", 1).insert("y", 2).insert("z", 3).insert("y", 20).remove("x")
    "{m.keys()}|{m.values()}|{m}|{m.contains_key("x")}|{m.contains_key("y")}"
}
"#;
        assert_eq!(
            differential(src),
            r#"["y", "z"]|[20, 3]|Map{"y": 20, "z": 3}|false|true"#
        );
    }

    #[test]
    fn diff_match_first_match_wins_and_guards() {
        // `match` evaluates arms top-to-bottom and takes the FIRST whose pattern matches AND whose
        // guard holds; a guard that fails falls through to later arms. This is byte-identical on
        // interp/KVM and holds even when a later arm would ALSO match (PR-it206).
        let g = r#"fun classify(n: Int) -> Str {
    match n {
        0 => "zero"
        x if x < 0 => "negative"
        x if x % 2 == 0 => "even-positive"
        _ => "odd-positive"
    }
}
fun probe() -> Str {
    "{classify(0)}|{classify(0 - 5)}|{classify(4)}|{classify(7)}"
}
"#;
        assert_eq!(differential(g), "zero|negative|even-positive|odd-positive");
        // A guarded arm placed BEFORE a literal that would also match still wins (order, not
        // specificity, decides): f(1) is "pos-guard", never "one".
        let o = r#"fun f(n: Int) -> Str {
    match n {
        x if x > 0 => "pos-guard"
        1 => "one"
        _ => "other"
    }
}
fun probe() -> Str { "{f(1)}|{f(5)}|{f(0)}" }
"#;
        assert_eq!(differential(o), "pos-guard|pos-guard|other");
        // Same for ADT variants: two arms on the same constructor resolve first-guard-wins.
        let a = r#"type Shape = Circle(r: Int) | Rect(w: Int, h: Int)
fun describe(s: Shape) -> Str {
    match s {
        Circle(r) if r == 0 => "point"
        Circle(r) => "circle-{r}"
        Rect(w, h) if w == h => "square-{w}"
        Rect(w, h) => "rect-{w}x{h}"
    }
}
fun probe() -> Str {
    "{describe(Circle(0))}|{describe(Circle(5))}|{describe(Rect(3, 3))}|{describe(Rect(2, 4))}"
}
"#;
        assert_eq!(differential(a), "point|circle-5|square-3|rect-2x4");
    }

    #[test]
    fn diff_loopvar_capture_negmod_try_propagation() {
        // A fifth bug-hunt sweep (it205, ~10 probes) found no divergence; this locks its subtlest
        // deterministic edges, byte-identical on interp/KVM. Each closure made in a loop captures
        // that iteration's value of i (not the final i — the classic late-binding footgun), so
        // adding 10 yields 10/11/12; negative % and / truncate toward zero with the remainder's
        // sign following the dividend; and `?` on a Result short-circuits the whole function on
        // the FIRST Err, propagating it unchanged (PR-it205).
        let cap = r#"fun probe() -> Str {
    var fns: List[fn(Int) -> Int] = []
    for i in 0..3 { fns = fns.push(fn x { x + i }) }
    "{fns.map(fn f { f(10) })}"
}
"#;
        assert_eq!(differential(cap), "[10, 11, 12]");
        let nm = r#"fun probe() -> Str {
    "{(0 - 7) % 3}|{7 % (0 - 3)}|{(0 - 7) / 3}|{(0 - 7) / (0 - 3)}|{(0 - 8) % 4}"
}
"#;
        assert_eq!(differential(nm), "-1|1|-2|2|0");
        let tq = r#"fun parse_pos(s: Str) -> Result[Int, Str] {
    match s.parse_int() {
        Some(n) => if n > 0 { Ok(n) } else { Err("not positive") }
        None => Err("not a number")
    }
}
fun sum_two(a: Str, b: Str) -> Result[Int, Str] {
    let x = parse_pos(a)?
    let y = parse_pos(b)?
    Ok(x + y)
}
fun probe() -> Str {
    "{sum_two("3", "4")}|{sum_two("x", "4")}"
}
"#;
        assert_eq!(differential(tq), r#"Ok(7)|Err("not a number")"#);
    }

    #[test]
    fn diff_replace_json_largelist_edges() {
        // A second bug-hunt sweep (it200, ~10 probes) found no divergence; this locks the
        // subtlest deterministic edges it exercised, byte-identical on interp/KVM: str replace
        // consumes non-overlapping left-to-right (so "aaaa".replace("aa","b") = "bb" not "ba"),
        // a nested JSON value built from the ADT stringifies with unicode intact and no spaces,
        // and a 100-element fold/filter/map composition (PR-it200).
        let src = r#"fun probe() -> Str {
    let rep = "{"aaaa".replace("aa", "b")}|{"ababab".replace("ab", "X")}|{"xyx".replace("x", "xx")}"
    let j = JArr([JNum(1.0), JObj(Map().insert("b", JStr("héllo"))), JNull])
    var xs: List[Int] = []
    var i = 1
    while i <= 100 { xs = xs.push(i)
        i = i + 1 }
    let big = "{xs.fold(0, fn(a, x) { a + x })}|{xs.filter(fn x { x % 7 == 0 }).len()}|{xs.map(fn x { x * x }).last()}"
    "{rep}#{json_stringify(j)}#{big}"
}
"#;
        assert_eq!(
            differential(src),
            "bb|XXX|xxyxx#[1,{\"b\":\"héllo\"},null]#5050|14|Some(10000)"
        );
    }

    #[test]
    fn diff_deep_feature_composition() {
        // A bug-hunt sweep (it198, ~10 probes) found no divergence; this locks the deepest
        // cross-feature compositions it exercised as a regression guard, all byte-identical on
        // interp/KVM: a recursive Tree ADT folded via match, a Map of records accessed through
        // Option, arbitrary-precision big(2)^100, and exact rational reduction (PR-it198).
        let src = r#"type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)
type P = { x: Int, y: Int }
fun tsum(t: Tree) -> Int {
    match t {
        Leaf(v) => v
        Node(l, r) => tsum(l) + tsum(r)
    }
}
fun probe() -> Str {
    let t = Node(Node(Leaf(1), Leaf(2)), Node(Leaf(3), Leaf(4)))
    let m = Map().insert("a", P(x: 1, y: 2)).insert("b", P(x: 3, y: 4))
    let rec = match m.get("b") { Some(p) => "{p.x},{p.y}"
        None => "none" }
    "{tsum(t)}#{rec}#{big(2).pow(100)}#{rat(1, 3) + rat(1, 3) + rat(1, 3)}#{rat(10, 4) * rat(2, 5)}"
}
"#;
        assert_eq!(
            differential(src),
            "10#3,4#1267650600228229401496703205376#1#1"
        );
    }

    #[test]
    fn diff_numeric_collection_edges() {
        // Subtle numeric/collection edges surfaced by a bug-hunt sweep, all byte-identical on
        // interp/KVM (PR-it192): a NaN can be STORED in a Set (len grows) but contains(nan) is
        // false since NaN != NaN (IEEE); a rational normalizes the sign onto the numerator and
        // reduces; and the classic i64::MIN / -1 and x / 0 cases panic rather than wrap.
        let src = r#"fun probe() -> Str {
    let nan = 0.0 / 0.0
    let s = Set([1.0, 2.0, nan])
    let setnan = "{s.len()}|{s.contains(nan)}|{s.contains(1.0)}"
    let rats = "{rat(3, 0 - 6)}|{rat(0 - 3, 0 - 6)}|{rat(0 - 4, 8)}"
    "{setnan}#{rats}"
}
"#;
        assert_eq!(differential(src), "3|false|true#-1/2|1/2|-1/2");
        // Division edges are clean panics (not wrapped values), identical across engines.
        assert_eq!(differential("fun probe() -> Str { \"{10 / 0}\" }\n"), "panic: division by zero");
        assert_eq!(
            differential("fun probe() -> Str { \"{(0 - 9223372036854775807 - 1) / (0 - 1)}\" }\n"),
            "panic: integer overflow in division"
        );
        assert_eq!(differential("fun probe() -> Str { \"{rat(5, 0)}\" }\n"), "panic: division by zero");
    }

    #[test]
    fn diff_float_to_degrees_radians() {
        // The NEW to_degrees/to_radians complete the trig surface, byte-identical on interp/KVM
        // down to the last bit — the native impl uses M_PI (== Rust consts::PI) so the constant
        // fold matches: 180deg = pi rad, 1 rad = 57.29577951308232 deg (PR-it194).
        let src = r#"fun probe() -> Str {
    let pi = 3.141592653589793
    "{(180.0).to_radians()}|{pi.to_degrees()}|{(90.0).to_radians()}|{(1.0).to_degrees()}|{(0.0 - 45.0).to_radians()}"
}
"#;
        assert_eq!(
            differential(src),
            "3.141592653589793|180.0|1.5707963267948966|57.29577951308232|-0.7853981633974483"
        );
    }

    #[test]
    fn diff_float_mul_add() {
        // The NEW mul_add is a FUSED multiply-add (self*a + b, single rounding), byte-identical
        // on interp/KVM. The key case proves the fusion is real: (0.1).mul_add(0.1, -0.01) differs
        // from 0.1*0.1 - 0.01 in the last bits because the product isn't doubly rounded — and all
        // engines agree on the fused result, so the native fma() is genuinely fused (PR-it199).
        let src = r#"fun probe() -> Str {
    let fused = (0.1).mul_add(0.1, 0.0 - 0.01)
    let unfused = 0.1 * 0.1 - 0.01
    "{fused}|{unfused}|{fused == unfused}|{(2.0).mul_add(0.0 - 3.0, 1.0)}|{(1000000.0).mul_add(1000000.0, 1.0)}"
}
"#;
        assert_eq!(
            differential(src),
            "0.0000000000000000009020562075079397|0.000000000000000001734723475976807|false|-5.0|1000000000001.0"
        );
    }

    #[test]
    fn diff_float_copysign() {
        // The NEW copysign(x, y) is byte-identical on interp/KVM: magnitude of the receiver with
        // the SIGN BIT of the argument. Crucially the sign comes from the bit, so a genuine -0.0
        // argument (from (0.0-1.0)*0.0) yields a negative result, and infinities carry through
        // (PR-it191).
        let src = r#"fun probe() -> Str {
    let nz = (0.0 - 1.0) * 0.0
    let a = "{(3.0).copysign(1.0)}|{(3.0).copysign(0.0 - 1.0)}|{(0.0 - 3.0).copysign(5.0)}"
    let b = "{(3.0).copysign(nz)}|{(1.0 / 0.0).copysign(0.0 - 1.0)}|{(3.0).copysign(1.0 / 0.0)}"
    "{a}#{b}"
}
"#;
        assert_eq!(differential(src), "3.0|-3.0|3.0#-3.0|-inf|3.0");
    }

    #[test]
    fn diff_float_trunc_fract() {
        // The NEW trunc/fract complete the floor/ceil/round rounding family, byte-identical on
        // interp/KVM: trunc rounds toward zero, fract is the signed fractional part (x-trunc(x)),
        // and IEEE specials hold (inf.trunc()=inf, inf.fract()=NaN) (PR-it184).
        let src = r#"fun probe() -> Str {
    let t = "{(3.7).trunc()}|{(0.0 - 3.7).trunc()}|{(3.0).trunc()}|{(0.99).trunc()}"
    let f = "{(3.75).fract()}|{(0.0 - 3.75).fract()}|{(3.0).fract()}"
    let inf = 1.0 / 0.0
    let sp = "{inf.trunc()}|{(0.0 / 0.0).trunc()}|{inf.fract()}"
    "{t}#{f}#{sp}"
}
"#;
        assert_eq!(differential(src), "3.0|-3.0|3.0|0.0#0.75|-0.75|0.0#inf|NaN|NaN");
    }

    #[test]
    fn diff_set_is_superset() {
        // The NEW is_superset() is the mirror of is_subset, byte-identical on interp/KVM: a is a
        // superset of b iff every element of b is in a; every set is a superset of the empty set
        // and of itself; disjoint/partial overlap is false (PR-it183).
        let src = r#"fun probe() -> Str {
    let big = Set([1, 2, 3, 4])
    let el: List[Int] = []
    let empty = Set(el)
    "{big.is_superset(Set([2, 3]))}|{Set([2, 3]).is_superset(big)}|{big.is_superset(Set([2, 5]))}|{big.is_superset(empty)}|{big.is_superset(big)}|{empty.is_superset(empty)}"
}
"#;
        assert_eq!(differential(src), "true|false|false|true|true|true");
    }

    #[test]
    fn diff_string_swapcase() {
        // The NEW swapcase() completes the ASCII casing family, byte-identical on interp/KVM:
        // each ASCII letter flips case, digits/punctuation/non-ASCII are unchanged, and a double
        // swap is the identity (PR-it189).
        let src = r#"fun probe() -> Str {
    "[{"Hello, WORLD".swapcase()}]|[{"MixEd 123".swapcase()}]|[{"".swapcase()}]|[{"héllo WÖRLD".swapcase()}]|[{"ALLCAPS".swapcase().swapcase()}]"
}
"#;
        assert_eq!(
            differential(src),
            "[hELLO, world]|[mIXeD 123]|[]|[HéLLO wÖrld]|[ALLCAPS]"
        );
    }

    #[test]
    fn diff_string_capitalize() {
        // The NEW capitalize() is ASCII-cased like to_upper/to_lower, byte-identical on
        // interp/KVM: first char up + rest down, empty stays empty, a digit or non-ASCII first
        // char is left unchanged (PR-it182).
        let src = r#"fun probe() -> Str {
    "[{"hELLO world".capitalize()}]|[{"HELLO".capitalize()}]|[{"".capitalize()}]|[{"already Cap".capitalize()}]|[{"123abc".capitalize()}]|[{"élan".capitalize()}]|[{"a".capitalize()}]"
}
"#;
        assert_eq!(
            differential(src),
            "[Hello world]|[Hello]|[]|[Already cap]|[123abc]|[élan]|[A]"
        );
    }

    #[test]
    fn diff_int_abs_diff() {
        // The NEW abs_diff is byte-identical on interp/KVM: symmetric |a-b| computed without
        // intermediate overflow, and a result past i64::MAX is a checked panic — including the
        // subtle abs_diff(i64::MIN, 0) = 2^63, one past i64::MAX (PR-it196).
        let src = r#"fun probe() -> Str {
    "{(5).abs_diff(3)}|{(3).abs_diff(5)}|{(0 - 5).abs_diff(3)}|{(0 - 5).abs_diff(0 - 8)}|{(9223372036854775807).abs_diff(0)}"
}
"#;
        assert_eq!(differential(src), "2|2|8|3|9223372036854775807");
        assert_eq!(
            differential("fun probe() -> Str { \"{(9223372036854775807).abs_diff(0 - 9223372036854775807 - 1)}\" }\n"),
            "panic: integer overflow in `abs_diff`"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{(0 - 9223372036854775807 - 1).abs_diff(0)}\" }\n"),
            "panic: integer overflow in `abs_diff`"
        );
    }

    #[test]
    fn diff_int_rem_div_euclid() {
        // The NEW rem_euclid/div_euclid are byte-identical on interp/KVM and genuinely differ
        // from % / /: rem_euclid is ALWAYS non-negative ((-7).rem_euclid(3)=2, not -1) and
        // div_euclid rounds toward negative infinity ((-7).div_euclid(3)=-3, not -2) (PR-it195).
        let src = r#"fun probe() -> Str {
    let r = "{(7).rem_euclid(3)}|{(0 - 7).rem_euclid(3)}|{(7).rem_euclid(0 - 3)}|{(0 - 7).rem_euclid(0 - 3)}"
    let d = "{(7).div_euclid(3)}|{(0 - 7).div_euclid(3)}|{(7).div_euclid(0 - 3)}|{(0 - 7).div_euclid(0 - 3)}"
    let cmp = "{(0 - 7) % 3}|{(0 - 7) / 3}"
    "{r}#{d}#{cmp}"
}
"#;
        assert_eq!(differential(src), "1|2|1|2#2|-3|-2|3#-1|-2");
        // Zero divisor and the i64::MIN / -1 overflow are clean panics on both operations.
        assert_eq!(differential("fun probe() -> Str { \"{(5).rem_euclid(0)}\" }\n"), "panic: division by zero");
        assert_eq!(
            differential("fun probe() -> Str { \"{(0 - 9223372036854775807 - 1).div_euclid(0 - 1)}\" }\n"),
            "panic: integer overflow in `div_euclid`"
        );
    }

    #[test]
    fn diff_int_lcm() {
        // The NEW lcm() is the natural companion to gcd, byte-identical on interp/KVM:
        // |v|/gcd*|w|, always non-negative, lcm(0,_)=0, and an out-of-i64 result panics
        // (PR-it181).
        let src = r#"fun probe() -> Str {
    let basic = "{(4).lcm(6)}|{(21).lcm(6)}|{(12).lcm(12)}"
    let zero = "{(0).lcm(5)}|{(5).lcm(0)}"
    let neg = "{(0 - 4).lcm(6)}|{(4).lcm(0 - 6)}|{(0 - 4).lcm(0 - 6)}"
    "{basic}#{zero}#{neg}"
}
"#;
        assert_eq!(differential(src), "12|42|12#0|0#12|12|12");
        // A result that overflows i64 is a clean panic, identical across engines.
        assert_eq!(
            differential("fun probe() -> Str { \"{(9223372036854775807).lcm(2)}\" }\n"),
            "panic: integer overflow in `lcm`"
        );
    }

    #[test]
    fn diff_string_center_alignment() {
        // The NEW center() completes the pad_left/pad_right trio, byte-identical on interp/KVM:
        // char-aware width, extra padding on the RIGHT when odd, width <= length is a no-op, and
        // a multibyte fill is placed as a full codepoint (PR-it180).
        let src = r#"fun probe() -> Str {
    let even = "hi".center(6, "-")
    let odd = "hi".center(7, "-")
    let noop = "hello".center(3, "-")
    let uni = "é".center(5, "*")
    let unifill = "x".center(4, "日")
    "[{even}]|[{odd}]|[{noop}]|[{uni}]|[{unifill}]"
}
"#;
        assert_eq!(differential(src), "[--hi--]|[--hi---]|[hello]|[**é**]|[日x日日]");
    }

    #[test]
    fn diff_radix_to_and_from_base() {
        // to_hex/to_binary/to_octal/to_radix and the NEW inverse parse_radix are byte-identical
        // on interp/KVM: lowercase digits, sign-prefixed negatives (not two's complement), 0 ->
        // "0", parse_radix is case-insensitive and rejects 0x/whitespace/invalid-digit as None,
        // and the pair round-trips (PR-it179).
        let src = r#"fun probe() -> Str {
    let to = "{(255).to_hex()}|{(8).to_binary()}|{(64).to_octal()}|{(35).to_radix(36)}|{(0 - 255).to_hex()}|{(0).to_radix(2)}"
    let from = "{"ff".parse_radix(16)}|{"1010".parse_radix(2)}|{"-ff".parse_radix(16)}|{"FF".parse_radix(16)}"
    let bad = "{"9".parse_radix(8)}|{"0xff".parse_radix(16)}|{" ff".parse_radix(16)}|{"".parse_radix(16)}"
    let rt = "{(255).to_radix(16).parse_radix(16)}|{(0 - 42).to_radix(2).parse_radix(2)}"
    "{to}#{from}#{bad}#{rt}"
}
"#;
        assert_eq!(
            differential(src),
            "ff|1000|100|z|-ff|0#Some(255)|Some(10)|Some(-255)|Some(255)#None|None|None|None#Some(255)|Some(-42)"
        );
        // Out-of-range base is a clean panic on both to_radix and parse_radix.
        assert_eq!(differential("fun probe() -> Str { \"{(10).to_radix(37)}\" }\n"), "panic: `to_radix` base must be in 2..=36");
        assert_eq!(differential("fun probe() -> Str { \"{\"10\".parse_radix(1)}\" }\n"), "panic: `parse_radix` base must be in 2..=36");
    }

    #[test]
    fn diff_csv_parse_stringify_quoting() {
        // csv_parse/csv_stringify follow RFC-4180-style quoting byte-identically on interp/KVM:
        // an embedded comma keeps a quoted field as ONE field, a doubled "" un-doubles to a
        // single " on parse, stringify quotes comma/quote fields (doubling embedded quotes), an
        // empty field is preserved, and a parse->stringify round-trip is stable (PR-it178).
        let src = r#"fun probe() -> Str {
    let basic = csv_parse("a,b,c\nd,e,f")
    let quoted = csv_parse("x,\"b,c\",z")
    let emptyf = csv_parse("a,,c")
    let dq = csv_parse("p,\"he said \"\"hi\"\"\",q")
    let w1 = csv_stringify([["a", "b,c", "d"]])
    let w2 = csv_stringify([["x", "say \"hi\"", "z"]])
    let rt = csv_stringify(csv_parse("1,\"a,b\",3"))
    "{basic}#{quoted}#{emptyf}#{dq}#{w1}#{w2}#{rt}"
}
"#;
        assert_eq!(
            differential(src),
            "[[\"a\", \"b\", \"c\"], [\"d\", \"e\", \"f\"]]#[[\"x\", \"b,c\", \"z\"]]#[[\"a\", \"\", \"c\"]]#[[\"p\", \"he said \"hi\"\", \"q\"]]#a,\"b,c\",d#x,\"say \"\"hi\"\"\",z#1,\"a,b\",3"
        );
    }

    #[test]
    fn diff_string_codec_roundtrip() {
        // base64/hex/url codecs are byte-identical on interp/KVM: encode produces standard
        // output (base64 padding, hex byte values, url percent-encoding), decode returns a
        // Result, round-trips preserve unicode, and malformed input decodes to Err not a panic
        // (PR-it177).
        let src = r#"fun probe() -> Str {
    let enc = "{base64_encode("Hello")}|{hex_encode("AB")}|{url_encode("a b&c")}"
    let rt = "{base64_decode(base64_encode("héllo café"))}|{hex_decode(hex_encode("héllo"))}|{url_decode(url_encode("a+b/c=d &e"))}"
    let empty = "{base64_encode("")}|{hex_encode("")}"
    let bad = match base64_decode("not!valid!") { Ok(s) => "ok"
        Err(e) => "err" }
    let badhex = match hex_decode("xyz") { Ok(s) => "ok"
        Err(e) => "err" }
    "{enc}#{rt}#{empty}#{bad}|{badhex}"
}
"#;
        assert_eq!(
            differential(src),
            "SGVsbG8=|4142|a%20b%26c#Ok(\"héllo café\")|Ok(\"héllo\")|Ok(\"a+b/c=d &e\")#|#err|err"
        );
    }

    #[test]
    fn diff_regex_match_find_replace() {
        // The regex builtins (re_match/re_find/re_find_all/re_replace) are byte-identical on
        // interp/KVM: match is a bool with anchors, find returns the first Some/None, find_all a
        // list, replace substitutes ALL matches with LITERAL text (no `$1` backrefs), `.` is
        // char-aware (multibyte), and an invalid pattern is a clean panic (PR-it176).
        let src = r##"fun probe() -> Str {
    let m = "{re_match("[0-9]+", "hello123")}|{re_match("^[a-z]+$", "hello123")}"
    let f = "{re_find("[0-9]+", "abc123def456")}|{re_find("[0-9]", "abc")}"
    let fa = "{re_find_all("[0-9]+", "a1b22c333")}|{re_find_all("[0-9]+", "abcdef")}"
    let r = "{re_replace("[0-9]", "abc123", "#")}"
    let u = "{re_find_all(".", "héllo")}"
    "{m}#{f}#{fa}#{r}#{u}"
}
"##;
        assert_eq!(
            differential(src),
            "true|false#Some(\"123\")|None#[\"1\", \"22\", \"333\"]|[]#abc####[\"h\", \"é\", \"l\", \"l\", \"o\"]"
        );
        // An invalid regex is a clean panic, identical across engines.
        assert_eq!(differential("fun probe() -> Str { \"{re_match(\"[unclosed\", \"x\")}\" }\n"), "panic: invalid regex: unclosed character class `[`");
    }

    #[test]
    fn diff_parallel_hof_is_deterministic_and_input_ordered() {
        // par_map/par_filter are DETERMINISTIC and preserve INPUT order (not completion order)
        // despite parallel evaluation — byte-identical on interp/KVM, and par_map produces the
        // SAME result as a sequential map (PR-it175).
        let src = r#"fun probe() -> Str {
    let sq = [1, 2, 3, 4, 5].par_map(fn x { x * x })
    let ev = [1, 2, 3, 4, 5, 6, 7, 8].par_filter(fn x { x % 2 == 0 })
    var big: List[Int] = []
    var i = 0
    while i < 50 { big = big.push(i)
        i = i + 1 }
    let pm = big.par_map(fn x { x * 2 })
    let seq = big.map(fn x { x * 2 })
    "{sq}|{ev}#{pm == seq}|{pm.get(49)}|{pm.get(0)}"
}
"#;
        assert_eq!(differential(src), "[1, 4, 9, 16, 25]|[2, 4, 6, 8]#true|Some(98)|Some(0)");
    }

    #[test]
    fn diff_tensor_ops_and_fp_accumulation() {
        // The 1D-float-vector tensor surface (elementwise +/*, scale, dot, sum/mean/max/min/
        // get/len) is byte-identical on interp/KVM, INCLUDING the floating-point accumulation
        // order of reductions — sum of [1.0, 1e-7, 1e-7, 1e-7] is exactly 1.0000003000000002
        // on every engine, and a 100k-element reduction agrees (PR-it173).
        let src = r#"fun probe() -> Str {
    let a = tensor([1.0, 2.0, 3.0, 4.0])
    let b = tensor([10.0, 20.0, 30.0, 40.0])
    let fp = tensor([1.0, 0.0000001, 0.0000001, 0.0000001])
    let big = arange(100000)
    "{a + b}|{a * b}|{a.scale(2.0)}|{a.dot(b)}#{a.sum()}|{a.mean()}|{a.max()}|{a.min()}|{a.get(2)}|{a.len()}#{fp.sum()}|{big.sum()}|{big.mean()}"
}
"#;
        assert_eq!(
            differential(src),
            "Tensor([11.0, 22.0, 33.0, 44.0])|Tensor([10.0, 40.0, 90.0, 160.0])|Tensor([2.0, 4.0, 6.0, 8.0])|300.0#10.0|2.5|4.0|1.0|3.0|4#1.0000003000000002|4999950000.0|49999.5"
        );
        // Shape mismatch and out-of-bounds index are clean panics, not bogus values.
        assert_eq!(differential("fun probe() -> Str { \"{tensor([1.0, 2.0]) + tensor([1.0, 2.0, 3.0])}\" }\n"), "panic: tensor length mismatch (2 vs 3)");
        assert_eq!(differential("fun probe() -> Str { \"{tensor([1.0, 2.0]).get(5)}\" }\n"), "panic: tensor index 5 out of range for length 2");
    }

    #[test]
    fn diff_component_state_isolation_and_composition() {
        // Two instances of a stateful component keep INDEPENDENT state (a=5, b=2), and a
        // component that holds another component as state delegates to it correctly (an
        // Aggregator holding a Counter reaches 3 after 3 bumps) — byte-identical on interp/KVM
        // (PR-it171).
        let src = r#"contract Count { intent "c"
    expose fun inc() -> Int
    expose fun get() -> Int }
component Counter fulfills Count { intent "ctr"
    state n: Int = 0
    expose fun inc() -> Int { n = n + 1
        n }
    expose fun get() -> Int { n } }
contract Agg { intent "a"
    expose fun bump() -> Int
    expose fun total() -> Int }
component Aggregator fulfills Agg { intent "holds a counter"
    state inner: Counter = Counter()
    state calls: Int = 0
    expose fun bump() -> Int { calls = calls + 1
        inner.inc() }
    expose fun total() -> Int { inner.get() } }
fun probe() -> Str {
    var a = Counter()
    var b = Counter()
    var i = 0
    while i < 5 { a.inc()
        i = i + 1 }
    b.inc()
    b.inc()
    var agg = Aggregator()
    agg.bump()
    agg.bump()
    agg.bump()
    "a={a.get()} b={b.get()} agg={agg.total()}"
}
"#;
        assert_eq!(differential(src), "a=5 b=2 agg=3");
    }

    #[test]
    fn diff_records_depth_nested_with_and_equality() {
        // Nested records, chained field access, a NESTED `with` update (updating an inner field
        // while preserving the outer's other fields), STRUCTURAL equality (shallow and deeply
        // nested), nested record destructuring in match, and structural equality inside
        // `.contains` are all byte-identical on interp/KVM (PR-it170).
        let src = r#"type Inner = Inner(v: Int)
type Outer = Outer(name: Str, inner: Inner)
type P = P(x: Int, y: Int)
fun probe() -> Str {
    let o = Outer(name: "x", inner: Inner(v: 5))
    let o2 = o with inner: (o.inner with v: 99)
    let eq1 = P(x: 1, y: 2) == P(x: 1, y: 2)
    let eq2 = P(x: 1, y: 2) == P(x: 1, y: 3)
    let eqn = Outer(name: "a", inner: Inner(v: 1)) == Outer(name: "a", inner: Inner(v: 1))
    let matched = match o2 { Outer(nm, Inner(vv)) => "{nm}:{vv}" }
    let inList = [P(x: 1, y: 1), P(x: 2, y: 2)].contains(P(x: 2, y: 2))
    "{o.inner.v}|{o2.inner.v}|{o2.name}#{eq1}|{eq2}|{eqn}#{matched}#{inList}#{o2}"
}
"#;
        assert_eq!(differential(src), "5|99|x#true|false|true#x:99#true#Outer(\"x\", Inner(99))");
    }

    #[test]
    fn diff_deeply_nested_with_update_three_levels() {
        // Extends diff_records_depth_nested_with_and_equality (it170, a 2-level `with`) to THREE
        // levels: Team > Person > Addr. A deep immutable update `t with lead: (t.lead with addr:
        // (t.lead.addr with zip: N))` rebuilds all three record layers while preserving EVERY
        // sibling field at each level (name at level 2, city at level 3, size at the top), and a
        // separate top-level `with` on the result composes cleanly. The original `t` is untouched
        // throughout (t.lead.addr.zip stays 10001, t.size stays 5). Byte-identical on interp/KVM,
        // including the nested Display Team(Person("Ann", Addr("NYC", 20002)), 5) (PR-it226).
        let src = r#"type Addr = Addr(city: Str, zip: Int)
type Person = Person(name: Str, addr: Addr)
type Team = Team(lead: Person, size: Int)
fun probe() -> Str {
    let t = Team(lead: Person(name: "Ann", addr: Addr(city: "NYC", zip: 10001)), size: 5)
    let t2 = t with lead: (t.lead with addr: (t.lead.addr with zip: 20002))
    let t3 = t2 with size: 6
    "{t.lead.addr.zip}|{t2.lead.addr.zip}|{t2.lead.name}|{t2.lead.addr.city}|{t3.size}|{t.size}|{t2}"
}
"#;
        assert_eq!(
            differential(src),
            r#"10001|20002|Ann|NYC|6|5|Team(Person("Ann", Addr("NYC", 20002)), 5)"#
        );
    }

    #[test]
    fn diff_numeric_tower_precision_and_conversions() {
        // The numeric tower is byte-identical on interp/KVM (and native, per the native test):
        // BigInt is arbitrary-precision (2^70, 2^64, 25! all exact and exceeding i64), Rational
        // is exact and auto-reduces (1/3+1/6=1/2, 2/4->1/2) with num/den and to_float, and
        // Int/Float conversions (to_float, to_int truncating toward zero, to_str) agree (PR-it169).
        let src = r#"fun probe() -> Str {
    let bignum = big(2).pow(70)
    var f = big(1)
    var i = 1
    while i <= 25 { f = f * big(i)
        i = i + 1 }
    let third = rat(1, 3)
    let half = third + rat(1, 6)
    "{bignum}|{big(2).pow(64)}|{f}#{third}|{half}|{half.num()}/{half.den()}|{rat(2, 4)}|{third.to_float()}#{(5).to_float()}|{(2.9).to_int()}|{(0.0 - 2.9).to_int()}|{(7).to_str()}"
}
"#;
        assert_eq!(
            differential(src),
            "1180591620717411303424|18446744073709551616|15511210043330985984000000#1/3|1/2|1/2|1/2|0.3333333333333333#5.0|2|-2|7"
        );
        // A zero-denominator Rational is a clean panic, not a bogus value.
        assert_eq!(differential("fun probe() -> Str { \"{rat(1, 0)}\" }\n"), "panic: division by zero");
    }

    #[test]
    fn diff_generics_multiparam_and_adt() {
        // Multi-parameter generic funs, generic ADTs instantiated at varied types
        // (record/list/nested), generic-over-collection, and a multi-param generic ADT swap are
        // byte-identical on interp/KVM — the native monomorphization must agree with the
        // interpreter's uniform representation at every instantiation (PR-it167).
        let src = r#"type Box[T] = Box(v: T)
type P = P(x: Int, y: Int)
type Pair[A, B] = Pair(fst: A, snd: B)
fun both[A, B](a: A, b: B) -> Str { "{a},{b}" }
fun unbox[T](b: Box[T]) -> T { match b { Box(v) => v } }
fun id[T](x: T) -> T { x }
fun firstOf[T](xs: List[T]) -> Option[T] { xs.get(0) }
fun swap[A, B](p: Pair[A, B]) -> Pair[B, A] { match p { Pair(a, b) => Pair(fst: b, snd: a) } }
fun probe() -> Str {
    let empty: List[Int] = []
    let sw = match swap(Pair(fst: 1, snd: "hi")) { Pair(a, b) => "{a}/{b}" }
    "{both(1, "hi")}|{both("x", true)}#{unbox(Box(P(x: 3, y: 4))).x}|{unbox(Box([1, 2, 3]))}|{unbox(unbox(Box(Box(9))))}#{id(id(42))}|{firstOf([10, 20])}|{firstOf(empty)}#{sw}"
}
"#;
        assert_eq!(differential(src), "1,hi|x,true#3|[1, 2, 3]|9#42|Some(10)|None#hi/1");
    }

    #[test]
    fn diff_pattern_match_depth() {
        // Guards, nested destructuring, guard-on-binding, literal, and wildcard-in-constructor
        // patterns are byte-identical on interp/KVM: a failed guard falls through to the next
        // arm in SOURCE order, nested ADT/record patterns bind the inner fields, and a guard on
        // a destructured binding falls through to the un-guarded arm of the same shape (PR-it166).
        let src = r#"type P = P(x: Int, y: Int)
type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)
fun cls(n: Int) -> Str { match n { x if x > 10 => "big"
    x if x > 0 => "small"
    _ => "neg" } }
fun sumt(t: Tree) -> Int { match t { Leaf(v) => v
    Node(Leaf(a), Leaf(b)) => a + b + 1000
    Node(l, r) => sumt(l) + sumt(r) } }
fun opt(o: Option[Int]) -> Str { match o { Some(x) if x > 5 => "big:{x}"
    Some(_) => "small"
    None => "none" } }
fun lit(n: Int) -> Str { match n { 0 => "zero"
    1 => "one"
    _ => "many" } }
fun wild(p: P) -> Int { match p { P(_, y) => y } }
fun probe() -> Str {
    let px = match Some(P(x: 3, y: 7)) { Some(P(a, b)) => a + b
        None => 0 - 1 }
    "{cls(20)}|{cls(5)}|{cls(0 - 3)}#{sumt(Node(Leaf(2), Leaf(3)))}|{sumt(Node(Node(Leaf(1), Leaf(1)), Leaf(5)))}#{opt(Some(9))}|{opt(Some(2))}|{opt(None)}#{lit(0)}|{lit(1)}|{lit(9)}|{wild(P(x: 100, y: 42))}#{px}"
}
"#;
        assert_eq!(
            differential(src),
            "big|small|neg#1005|1007#big:9|small|none#zero|one|many|42#10"
        );
    }

    #[test]
    fn diff_list_transformation_surface() {
        // The structural list transforms are byte-identical on interp/KVM with correct edge
        // semantics: take/drop CLAMP past the length (no error), take_while/drop_while split at
        // the predicate boundary, chunk yields a partial final group, window slides, flatten
        // drops empty inner lists, zip_with stops at the shorter list, partition returns
        // (matching, non-matching), and scan emits running accumulations (PR-it165).
        let src = r#"fun probe() -> Str {
    let xs = [1, 2, 3, 4, 5]
    let td = "{xs.take(2)}|{xs.take(10)}|{xs.drop(2)}|{xs.drop(10)}"
    let tw = "{[1, 2, 3, 4, 1].take_while(fn x { x < 3 })}|{[1, 2, 3, 4, 1].drop_while(fn x { x < 3 })}"
    let cw = "{xs.chunk(2)}|{xs.window(2)}"
    let ff = "{[[1, 2], [3], []].flatten()}|{[1, 2, 3].flat_map(fn x { [x, x * 10] })}"
    let pz = "{[1, 2, 3, 4].partition(fn x { x % 2 == 0 })}|{[1, 2, 3].zip_with([10, 20], fn(a, b) { a + b })}|{[1, 2, 3].scan(0, fn(a, x) { a + x })}"
    "{td}#{tw}#{cw}#{ff}#{pz}"
}
"#;
        assert_eq!(
            differential(src),
            "[1, 2]|[1, 2, 3, 4, 5]|[3, 4, 5]|[]#[1, 2]|[3, 4, 1]#[[1, 2], [3, 4], [5]]|[[1, 2], [2, 3], [3, 4], [4, 5]]#[1, 2, 3]|[1, 10, 2, 20, 3, 30]#[[2, 4], [1, 3]]|[11, 22]|[1, 3, 6]"
        );
    }

    #[test]
    fn diff_option_result_combinators() {
        // The Option/Result combinator surface (map/filter/and_then/ok_or/unwrap_or/map_err/ok)
        // is byte-identical on interp/KVM with correct short-circuiting: map/filter do NOT call
        // the closure on None/Err, ok_or converts Option->Result, and a chain short-circuits
        // once it hits None (PR-it164).
        let src = r#"fun probe() -> Str {
    let n: Option[Int] = None
    let opt = "{Some(3).map(fn x { x * 2 })}|{n.map(fn x { x * 2 })}|{Some(4).filter(fn x { x > 2 })}|{Some(1).filter(fn x { x > 2 })}"
    let oc = "{Some(5).unwrap_or(0)}|{n.unwrap_or(0)}|{Some(3).ok_or("e")}|{n.ok_or("e")}"
    let ok: Result[Int, Str] = Ok(3)
    let er: Result[Int, Str] = Err("boom")
    let res = "{ok.map(fn x { x + 1 })}|{er.map(fn x { x + 1 })}|{er.map_err(fn e { "w: {e}" })}|{er.unwrap_or(0)}|{ok.ok()}|{er.ok()}"
    let chain = "{Some(10).map(fn x { x + 1 }).filter(fn x { x > 100 }).map(fn x { x * 2 })}"
    "{opt}#{oc}#{res}#{chain}"
}
"#;
        assert_eq!(
            differential(src),
            "Some(6)|None|Some(4)|None#5|0|Ok(3)|Err(\"e\")#Ok(4)|Err(\"boom\")|Err(\"w: boom\")|0|Some(3)|None#None"
        );
    }

    #[test]
    fn diff_json_nested_roundtrip_and_key_order() {
        // JSON serialize/parse of nested structures is byte-identical on interp/KVM: JObj keys
        // stringify in insertion order (matching the map cert), a whole JNum renders as an int
        // and a fractional one keeps its decimal, parse preserves nested structure and key
        // order, duplicate object keys are last-wins, and empty object/array round-trip
        // (PR-it162).
        let src = r#"fun probe() -> Str {
    let inner = JObj(Map().insert("x", JNum(1.0)).insert("y", JBool(true)))
    let doc = JObj(Map().insert("name", JStr("kupl")).insert("items", JArr([JNum(1.0), JNull])).insert("nested", inner))
    let s1 = json_stringify(doc)
    let rt = match json_parse("\{\"a\": 1, \"b\": [true, null], \"c\": \{\"d\": 2.5\}\}") {
        Ok(j) => json_stringify(j)
        Err(e) => "err"
    }
    let dup = match json_parse("\{\"k\": 1, \"k\": 2\}") {
        Ok(j) => json_stringify(j)
        Err(e) => "err"
    }
    "{s1}#{rt}#{dup}#{json_stringify(JObj(Map()))}|{json_stringify(JArr([]))}"
}
"#;
        assert_eq!(
            differential(src),
            "{\"name\":\"kupl\",\"items\":[1,null],\"nested\":{\"x\":1,\"y\":true}}#{\"a\":1,\"b\":[true,null],\"c\":{\"d\":2.5}}#{\"k\":2}#{}|[]"
        );
    }

    #[test]
    fn diff_set_ops_preserve_insertion_order() {
        // Sets are insertion-ordered and stable through mutation, byte-identical on interp/KVM
        // (parallel to the map cert in PR-it160): insert of an existing element is a no-op that
        // keeps order, remove preserves the rest's order, remove-then-reinsert moves to the
        // end, the constructor dedups in first-occurrence order, and the algebra ops
        // (union/intersect/difference/symmetric_difference) have deterministic order (PR-it161).
        let src = r#"fun probe() -> Str {
    let s = Set([1, 2, 3])
    let ins = "{s.insert(4)}|{s.insert(2)}"
    let rem = "{s.remove(2)}|{s.remove(9)}|{s.remove(1).insert(1)}"
    let dedup = "{Set([3, 1, 2, 1, 3])}"
    let a = Set([1, 2, 3])
    let b = Set([3, 4, 2])
    let alg = "{a.union(b)}|{a.intersect(b)}|{a.difference(b)}|{a.symmetric_difference(b)}"
    let sub = "{Set([1, 2]).is_subset(b)}|{b.is_subset(Set([1, 2]))}"
    "{ins}#{rem}#{dedup}#{alg}#{sub}"
}
"#;
        assert_eq!(
            differential(src),
            "Set{1, 2, 3, 4}|Set{1, 2, 3}#Set{1, 3}|Set{1, 2, 3}|Set{2, 3, 1}#Set{3, 1, 2}#Set{1, 2, 3, 4}|Set{2, 3}|Set{1}|Set{1, 4}#false|false"
        );
    }

    #[test]
    fn diff_map_ops_preserve_insertion_order() {
        // Maps are insertion-ordered and that order is stable through mutation, byte-identical
        // on interp/KVM: updating an existing key keeps its position, remove preserves the
        // rest's order, remove-then-reinsert moves to the end, remove-missing is a no-op, and
        // merge is left-order-first with the right map winning on key conflicts (PR-it160).
        let src = r#"fun probe() -> Str {
    let upd = Map().insert("a", 1).insert("b", 2).insert("c", 3).insert("b", 20)
    let rem = Map().insert("a", 1).insert("b", 2).insert("c", 3).remove("b")
    let reins = Map().insert("a", 1).insert("b", 2).insert("c", 3).remove("a").insert("a", 9)
    let miss = Map().insert("a", 1).insert("b", 2).remove("z")
    let l = Map().insert("a", 1).insert("b", 2)
    let mg = l.merge(Map().insert("b", 20).insert("c", 3))
    "{upd.keys()} {upd.values()}#{rem.keys()} {rem.values()}#{reins.keys()}#{miss.keys()} {miss.len()}#{mg.keys()} {mg.values()}"
}
"#;
        assert_eq!(
            differential(src),
            "[\"a\", \"b\", \"c\"] [1, 20, 3]#[\"a\", \"c\"] [1, 3]#[\"b\", \"c\", \"a\"]#[\"a\", \"b\"] 2#[\"a\", \"b\", \"c\"] [1, 20, 3]"
        );
    }

    #[test]
    fn diff_date_time_arithmetic_and_components() {
        // The timestamp-based date API (date_make/date_iso/year_of/.../weekday_of) is
        // byte-identical on interp/KVM with correct Gregorian semantics: components, ISO
        // round-trip, month-boundary rollover, leap-day arithmetic (2024 leap, 1900 not — the
        // century rule), and normalization of an out-of-range day (PR-it159).
        let src = r#"fun probe() -> Str {
    let t = date_make(2024, 2, 29, 12, 30, 45)
    let comp = "{year_of(t)}-{month_of(t)}-{day_of(t)} wd={weekday_of(t)} yd={yearday_of(t)}"
    let rt = date_iso(date_make(2024, 12, 31, 23, 59, 59))
    let mb = date_make(2024, 1, 31, 0, 0, 0) + 86400
    let leap = date_make(2024, 2, 28, 0, 0, 0) + 86400
    let noleap = date_make(1900, 2, 28, 0, 0, 0) + 86400
    let norm = date_iso(date_make(2023, 2, 29, 0, 0, 0))
    "{comp}#{rt}#{month_of(mb)}-{day_of(mb)}#{month_of(leap)}-{day_of(leap)}#{month_of(noleap)}-{day_of(noleap)}#{norm}"
}
"#;
        assert_eq!(
            differential(src),
            "2024-2-29 wd=4 yd=60#2024-12-31T23:59:59Z#2-1#2-29#3-1#2023-03-01T00:00:00Z"
        );
    }

    #[test]
    fn diff_string_method_surface_is_char_aware() {
        // The string-method surface is byte-identical on interp/KVM and consistently UTF-8
        // CHAR-aware (not byte-based): reverse reverses by char, index_of/rfind return char
        // indices, pad counts chars, case is ASCII-only (PR-it158).
        let src = r#"fun probe() -> Str {
    let a = "[{"  hi  ".trim()}]|[{"x  ".trim_end()}]|[{"   ".trim()}]"
    let b = "{"hello".starts_with("he")}|{"hello".starts_with("")}|{"hi".starts_with("hello")}"
    let c = "[{"ab".repeat(3)}]|[{"ab".repeat(0)}]"
    let d = "[{"abé".reverse()}]"
    let e = "{"héllo".index_of("llo")}|{"hello".index_of("z")}|{"héllo".rfind("l")}"
    let f = "{"café".to_upper()}|[{"é".pad_left(4, "*")}]"
    "{a}#{b}#{c}#{d}#{e}#{f}"
}
"#;
        assert_eq!(
            differential(src),
            "[hi]|[x]|[]#true|true|false#[ababab]|[]#[éba]#Some(2)|None|Some(3)#CAFé|[***é]"
        );
    }

    #[test]
    fn diff_sized_int_arithmetic_overflow_panics() {
        // Sized-int arithmetic is CHECKED at the type width (like the default i64 in PR-it151):
        // it panics on overflow/underflow rather than wrapping — byte-identical on interp/KVM,
        // and (per the native test) native does NOT wrap despite C's silent sized overflow.
        assert_eq!(differential("fun probe() -> Str { \"{(255u8) + (1u8)}\" }\n"), "panic: integer overflow in addition");
        assert_eq!(differential("fun probe() -> Str { \"{(200u8) + (100u8)}\" }\n"), "panic: integer overflow in addition");
        assert_eq!(differential("fun probe() -> Str { \"{(127i8) + (1i8)}\" }\n"), "panic: integer overflow in addition");
        // u8 cannot go negative, so subtracting below zero is an overflow panic, not a wrap.
        assert_eq!(differential("fun probe() -> Str { \"{(0u8) - (1u8)}\" }\n"), "panic: integer overflow in subtraction");
        assert_eq!(differential("fun probe() -> Str { \"{(16u8) * (16u8)}\" }\n"), "panic: integer overflow in multiplication");
        // A result that fits the width computes normally (no false panic).
        assert_eq!(differential("fun probe() -> Str { \"{(200u8) + (55u8)}\" }\n"), "255");
    }

    #[test]
    fn diff_sized_int_bitwise_width_semantics() {
        // Sized-int bitwise ops respect the operand WIDTH (unlike the default i64 Int): bnot is
        // a width-wide complement, shifts wrap at the width, i8 shr is arithmetic (sign-
        // extends), and a shift >= the width panics rather than hitting C's UB. Byte-identical
        // on interp/KVM, and (per the native test) native masks to width too (PR-it155).
        assert_eq!(
            differential("fun probe() -> Str { \"{(0u8).bnot()}|{(0u16).bnot()}|{(5i8).bnot()}|{(255u8).bnot()}\" }\n"),
            "255|65535|-6|0"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{(1u8).shl(7)}|{(255u8).shl(1)}|{(128u8).shr(1)}|{(1u16).shl(15)}\" }\n"),
            "128|254|64|32768"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{(12u8).band(10u8)}|{(12u8).bor(10u8)}|{(12u8).bxor(10u8)}\" }\n"),
            "8|14|6"
        );
        // i8 shr sign-extends; a shift amount at/above the width is a clean panic, not UB.
        assert_eq!(differential("fun probe() -> Str { let neg = (0i8 - 8i8)\n    \"{neg.shr(1)}|{neg.bnot()}\" }\n"), "-4|7");
        assert_eq!(differential("fun probe() -> Str { \"{(1u8).shl(8)}\" }\n"), "panic: shift amount must be in 0..=7");
    }

    #[test]
    fn diff_while_loop_and_break_continue() {
        // while runs while its condition holds (false-initial => zero iterations); break exits
        // the innermost loop; continue skips the rest of the current iteration; in nested loops
        // break/continue affect only the INNER loop — byte-identical on interp/KVM (PR-it153).
        let src = "fun probe() -> Str {\n    var i = 0\n    while i < 5 { i = i + 1 }\n    \
                   var j = 0\n    while false { j = j + 1 }\n    \
                   var a = 48\n    var b = 36\n    while b != 0 { let t = b\n        b = a % b\n        a = t }\n    \
                   var w = 0\n    while w < 100 { if w == 7 { break }\n        w = w + 1 }\n    \
                   var s = 0\n    for x in 1..10 { if x % 2 == 0 { continue }\n        s = s + x }\n    \
                   var out: List[Int] = []\n    for p in 1..4 {\n        for q in 1..4 {\n            if q == 2 { continue }\n            if q == 3 { break }\n            out = out.push(p * 10 + q)\n        }\n    }\n    \
                   \"{i}|{j}|{a}|{w}|{s}|{out}\"\n}\n";
        assert_eq!(differential(src), "5|0|12|7|25|[11, 21, 31]");
    }

    #[test]
    fn diff_while_data_dependent_termination_and_nested_break() {
        // Complements diff_while_loop_and_break_continue (it153, fixed-bound loops + Euclid GCD)
        // with loops whose iteration count is DATA-DEPENDENT — determined by runtime-computed values,
        // not a static bound. A Collatz step-counter mutates n by a branch each turn and terminates
        // only when n reaches 1 (27 famously takes 111 steps; a start of 1 takes 0). A trial-division
        // prime counter nests a `while d*d <= i` inside an outer `while i < 100`, where an inner
        // `break` on the first divisor must exit ONLY the inner loop and let the outer continue —
        // 25 primes below 100. Byte-identical on interp/KVM (PR-it225).
        let src = r#"fun collatz_steps(n0: Int) -> Int {
    var n = n0
    var steps = 0
    while n != 1 {
        if n % 2 == 0 { n = n / 2 } else { n = 3 * n + 1 }
        steps = steps + 1
    }
    steps
}
fun count_primes_below(limit: Int) -> Int {
    var found = 0
    var i = 2
    while i < limit {
        var is_prime = true
        var d = 2
        while d * d <= i {
            if i % d == 0 { is_prime = false
                break }
            d = d + 1
        }
        if is_prime { found = found + 1 }
        i = i + 1
    }
    found
}
fun probe() -> Str {
    var sum = 0
    var i = 1
    while i <= 10 { sum = sum + i
        i = i + 1 }
    "{sum}|{collatz_steps(27)}|{collatz_steps(1)}|{count_primes_below(100)}"
}
"#;
        assert_eq!(differential(src), "55|111|0|25");
    }

    #[test]
    fn diff_range_and_for_loop_edges() {
        // Ranges are hi-EXCLUSIVE; an empty range (lo == hi) and a reversed range (lo > hi)
        // both iterate zero times; negative bounds work. `for` over a List preserves order;
        // an empty iterable runs the body zero times; nested loops compose (PR-it152).
        let src = "fun probe() -> Str {\n    var a = 0\n    for i in 1..4 { a = a + i }\n    \
                   var b = 0\n    for i in 5..5 { b = b + 1 }\n    var c = 0\n    for i in 5..3 { c = c + 1 }\n    \
                   var d = 0\n    for i in (0 - 3)..0 { d = d + i }\n    \
                   var s = \"\"\n    for x in [3, 1, 2] { s = \"{s}{x}\" }\n    \
                   var t = 0\n    for x in [] { t = t + 1 }\n    \
                   var out: List[Int] = []\n    for i in 1..3 {\n        for j in 1..3 {\n            out = out.push(i * j)\n        }\n    }\n    \
                   \"{a}|{b}|{c}|{d}|{s}|{t}|{out}\"\n}\n";
        assert_eq!(differential(src), "6|0|0|-6|312|0|[1, 2, 2, 4]");
    }

    #[test]
    fn diff_higher_order_and_closure_depth() {
        // A returned closure keeps its own captured environment; two are independent.
        let ret = "fun adder(n: Int) -> fn(Int) -> Int { fn x { x + n } }\n\
                   fun probe() -> Str { let a3 = adder(3)\n    let a10 = adder(10)\n    \"{a3(1)}|{a10(1)}|{a3(100)}\" }\n";
        assert_eq!(differential(ret), "4|11|103");
        // Loop-variable capture is VALUE-at-creation (PR-it76), not the final value:
        // the three closures return 0,1,2 — not 3,3,3.
        let loopcap = "fun probe() -> Str { var fs: List[fn() -> Int] = []\n    var i = 0\n    \
                       while i < 3 {\n        let captured = i\n        fs = fs.push(fn { captured })\n        i = i + 1\n    }\n    \
                       let g0 = fs.get(0).unwrap_or(fn { 0 - 1 })\n    let g1 = fs.get(1).unwrap_or(fn { 0 - 1 })\n    \
                       let g2 = fs.get(2).unwrap_or(fn { 0 - 1 })\n    \"{g0()}|{g1()}|{g2()}\" }\n";
        assert_eq!(differential(loopcap), "0|1|2");
        // Value-capture also means a later mutation of the captured var is not seen.
        assert_eq!(
            differential("fun probe() -> Str { var x = 1\n    let f = fn { x }\n    x = 99\n    \"{f()}\" }\n"),
            "1"
        );
        // Composition (higher-order taking + returning funs) and closures held in a list.
        let comp = "fun compose(f: fn(Int) -> Int, g: fn(Int) -> Int) -> fn(Int) -> Int { fn x { f(g(x)) } }\n\
                    fun probe() -> Str { let inc = fn x { x + 1 }\n    let dbl = fn x { x * 2 }\n    \
                    let h = compose(inc, dbl)\n    let fs = [inc, dbl]\n    \"{h(5)}|{fs.map(fn f { f(5) })}\" }\n";
        assert_eq!(differential(comp), "11|[6, 10]");
    }

    #[test]
    fn diff_component_state_persists_and_isolates() {
        // A component's `state` persists across expose-fun calls on the same instance, and
        // separate instances are isolated — byte-identical on interp/KVM (PR-it132).
        let counter = "component Counter {\n    intent \"c\"\n    state n: Int = 0\n    expose fun bump() -> Int { n = n + 1\n        n }\n}\n\
                       fun probe() -> Str {\n    let c = Counter()\n    let d = Counter()\n    \"{c.bump()},{c.bump()},{c.bump()}|iso {d.bump()}\"\n}\n";
        assert_eq!(differential(counter), "1,2,3|iso 1");
        // Multiple state fields (an Int and a growing List) track independently.
        let multi = "component Store {\n    intent \"s\"\n    state count: Int = 0\n    state items: List[Str] = []\n    \
                     expose fun add(x: Str) -> Str {\n        count = count + 1\n        items = items.push(x)\n        \"{count}:{items}\"\n    }\n}\n\
                     fun probe() -> Str {\n    let s = Store()\n    \"{s.add(\"a\")}|{s.add(\"b\")}\"\n}\n";
        assert_eq!(differential(multi), "1:[\"a\"]|2:[\"a\", \"b\"]");
        // Record-valued state updated via `with`; a Map-valued state that accumulates.
        let rec = "type Pos = Pos(x: Int, y: Int)\ncomponent Robot {\n    intent \"r\"\n    state pos: Pos = Pos(x: 0, y: 0)\n    \
                   expose fun move(dx: Int, dy: Int) -> Str {\n        pos = pos with x: pos.x + dx, y: pos.y + dy\n        \"({pos.x},{pos.y})\"\n    }\n}\n\
                   fun probe() -> Str {\n    let r = Robot()\n    \"{r.move(1, 2)}|{r.move(3, 0 - 1)}\"\n}\n";
        assert_eq!(differential(rec), "(1,2)|(4,1)");
    }

    #[test]
    fn diff_if_let_and_while_let() {
        // `if let` as an EXPRESSION (both branches yield a value), with a nested pattern
        // and a Result scrutinee — byte-identical on interp/KVM (PR-it125).
        let iflet = "type Pt = Pt(x: Int, y: Int)\nfun probe() -> Str {\n    \
                     let a: Option[Int] = Some(7)\n    let b: Option[Int] = None\n    \
                     let p: Option[Pt] = Some(Pt(3, 4))\n    let res: Result[Int, Str] = Ok(9)\n    \
                     \"{if let Some(x) = a { x * 2 } else { 0 - 1 }}|{if let Some(x) = b { x } else { 0 - 1 }}|\
                     {if let Some(Pt(x, y)) = p { x + y } else { 0 }}|{if let Ok(v) = res { v } else { 0 - 1 }}\"\n}\n";
        assert_eq!(differential(iflet), "14|-1|7|9");
        // `if let` as a STATEMENT (no else -> does nothing on a failed match) + binding
        // scope: the inner binding does not leak or mutate an outer variable of the same name.
        let stmt = "fun probe() -> Str {\n    var log = \"\"\n    let x = 100\n    let a: Option[Int] = Some(5)\n    let b: Option[Int] = None\n    \
                    if let Some(x) = a { log = \"{log}got{x}\" }\n    if let Some(x) = b { log = \"{log}never\" }\n    \"{log}|{x}\"\n}\n";
        assert_eq!(differential(stmt), "got5|100");
        // `while let` iterates until the pattern fails, building a result, then terminates.
        let whilelet = "fun step(n: Int) -> Option[Int] { if n > 0 { Some(n * n) } else { None } }\n\
                        fun probe() -> Str {\n    var n = 3\n    var acc: List[Int] = []\n    \
                        while let Some(sq) = step(n) {\n        acc = acc.push(sq)\n        n = n - 1\n    }\n    \"{acc}\"\n}\n";
        assert_eq!(differential(whilelet), "[9, 4, 1]");
    }

    #[test]
    fn diff_recursive_adt_trees() {
        // Recursive ADTs (self-referential ctor payloads) build, traverse, map, display
        // nested, compare structurally, and recurse deeply — byte-identical on interp/KVM
        // (PR-it137). A depth-12 tree is 4096 leaves; native heap-alloc + recursion holds.
        let tree = "type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)\n\
                    fun sum(t: Tree) -> Int { match t {\n        Leaf(v) => v\n        Node(l, r) => sum(l) + sum(r)\n    } }\n\
                    fun mapt(t: Tree, f: fn(Int) -> Int) -> Tree { match t {\n        Leaf(v) => Leaf(f(v))\n        Node(l, r) => Node(l: mapt(l, f), r: mapt(r, f))\n    } }\n\
                    fun build(n: Int) -> Tree { if n <= 0 { Leaf(1) } else { Node(l: build(n - 1), r: build(n - 1)) } }\n\
                    fun probe() -> Str {\n    let t = Node(l: Node(l: Leaf(1), r: Leaf(2)), r: Leaf(3))\n    \
                    \"{sum(t)}|{t}|{mapt(t, fn x { x * 10 })}|{sum(build(12))}\" }\n";
        assert_eq!(differential(tree), "6|Node(Node(Leaf(1), Leaf(2)), Leaf(3))|Node(Node(Leaf(10), Leaf(20)), Leaf(30))|4096");
        // An expression-tree evaluator + nested Display.
        let expr = "type Expr = Num(n: Int) | Add(a: Expr, b: Expr) | Mul(a: Expr, b: Expr)\n\
                    fun eval(e: Expr) -> Int { match e {\n        Num(n) => n\n        Add(a, b) => eval(a) + eval(b)\n        Mul(a, b) => eval(a) * eval(b)\n    } }\n\
                    fun probe() -> Str {\n    let e = Mul(a: Add(a: Num(2), b: Num(3)), b: Num(4))\n    \"{eval(e)}|{e}\" }\n";
        assert_eq!(differential(expr), "20|Mul(Add(Num(2), Num(3)), Num(4))");
        // A cons-list (nullary Nil ctor displays without parens) + structural equality.
        let cons = "type IntList = Nil | Cons(head: Int, tail: IntList)\n\
                    fun rev(xs: IntList, acc: IntList) -> IntList { match xs {\n        Nil => acc\n        Cons(h, tail) => rev(tail, Cons(head: h, tail: acc))\n    } }\n\
                    fun probe() -> Str {\n    let xs = Cons(head: 1, tail: Cons(head: 2, tail: Cons(head: 3, tail: Nil)))\n    \
                    \"{xs}|{rev(xs, Nil)}|{xs == xs}\" }\n";
        assert_eq!(differential(cons), "Cons(1, Cons(2, Cons(3, Nil)))|Cons(3, Cons(2, Cons(1, Nil)))|true");
    }

    #[test]
    fn diff_pattern_matching_depth() {
        // Guards (first-match-wins, may reference the bound variable), byte-identical.
        let guard = "fun cls(n: Int) -> Str { match n {\n        x if x > 10 => \"big\"\n        \
                     x if x > 0 => \"small\"\n        _ => \"nonpos\"\n    } }\n\
                     fun probe() -> Str { \"{cls(50)}|{cls(5)}|{cls(-1)}\" }\n";
        assert_eq!(differential(guard), "big|small|nonpos");
        // Or-patterns (non-binding) and range patterns (lo..hi exclusive).
        assert_eq!(
            differential("fun f(n: Int) -> Str { match n {\n        1 | 2 | 3 => \"low\"\n        _ => \"other\"\n    } }\nfun probe() -> Str { \"{f(2)}|{f(9)}\" }\n"),
            "low|other"
        );
        assert_eq!(
            differential("fun g(n: Int) -> Str { match n {\n        0..60 => \"F\"\n        60..90 => \"B\"\n        _ => \"A\"\n    } }\nfun probe() -> Str { \"{g(50)}|{g(75)}|{g(95)}\" }\n"),
            "F|B|A"
        );
        // Nested constructor destructuring binds inner fields.
        let nested = "type Pt = Pt(x: Int, y: Int)\ntype Seg = Seg(a: Pt, b: Pt)\n\
                      fun mid(s: Seg) -> Str { match s {\n        Seg(Pt(x1, y1), Pt(x2, y2)) => \"{(x1 + x2) / 2},{(y1 + y2) / 2}\"\n    } }\n\
                      fun probe() -> Str { \"{mid(Seg(Pt(0, 0), Pt(10, 4)))}\" }\n";
        assert_eq!(differential(nested), "5,2");
    }

    #[test]
    fn diff_try_operator_on_option() {
        // `?` works on Option like it does on Result: Some(x) unwraps to x, None
        // short-circuits the enclosing Option-returning function (PR-it135). Chained `?`
        // and a None in the middle both short-circuit. Byte-identical on interp/KVM.
        let src = "fun lookup(m: Map[Str, Int], k: Str) -> Option[Int] { let v = m.get(k)?\n    Some(v * 2) }\n\
                   fun chain(m: Map[Str, Int]) -> Option[Int] { let a = lookup(m, \"a\")?\n    let b = lookup(m, \"b\")?\n    Some(a + b) }\n\
                   fun probe() -> Str {\n    let m = Map().insert(\"a\", 5).insert(\"b\", 3)\n    \
                   \"{lookup(m, \"a\")}|{lookup(m, \"missing\")}|{chain(m)}|{chain(Map().insert(\"a\", 1))}\" }\n";
        assert_eq!(differential(src), "Some(10)|None|Some(16)|None");
    }

    #[test]
    fn diff_option_result_and_try_operator() {
        // Option/Result methods behave identically on interp/KVM.
        let opt = "fun probe() -> Str {\n    let s: Option[Int] = Some(2)\n    let n: Option[Int] = None\n    \
                   \"{s.map(fn x { x + 1 })}|{n.map(fn x { x + 1 })}|{s.unwrap_or(0)}|{n.unwrap_or(0)}|\
                   {s.and_then(fn x { Some(x * 10) })}|{s.ok_or(\"e\")}|{n.ok_or(\"e\")}|{s.filter(fn x { x > 9 })}\"\n}\n";
        assert_eq!(differential(opt), "Some(3)|None|2|0|Some(20)|Ok(2)|Err(\"e\")|None");
        let res = "fun probe() -> Str {\n    let o: Result[Int, Str] = Ok(2)\n    let e: Result[Int, Str] = Err(\"bad\")\n    \
                   \"{o.map(fn x { x + 1 })}|{e.map(fn x { x + 1 })}|{o.is_ok()}|{e.is_err()}|{o.ok()}|{e.ok()}|{e.unwrap_or(0)}\"\n}\n";
        assert_eq!(differential(res), "Ok(3)|Err(\"bad\")|true|true|Some(2)|None|0");
        // the `?` operator unwraps Ok and early-returns Err from the enclosing fun.
        let try_op = "fun half(n: Int) -> Result[Int, Str] { if n % 2 == 0 { Ok(n / 2) } else { Err(\"odd\") } }\n\
                      fun chain(n: Int) -> Result[Int, Str] { let a = half(n)?\n    let b = half(a)?\n    Ok(b) }\n\
                      fun probe() -> Str { \"{chain(8)}|{chain(4)}|{chain(6)}\" }\n";
        assert_eq!(differential(try_op), "Ok(2)|Ok(1)|Err(\"odd\")");
        // equality and nested Option display.
        assert_eq!(
            differential("fun probe() -> Str { let a: Option[Int] = None\n    let b: Option[Int] = None\n    \"{a == b}|{Some(1) == Some(1)}|{Some(1) == Some(2)}|{Some(Some(7))}\" }\n"),
            "true|true|false|Some(Some(7))"
        );
    }

    #[test]
    fn diff_slice_and_index_edges() {
        // Str.slice is char-indexed with a hi-EXCLUSIVE bound; out-of-bounds clamps, a
        // reversed range (lo > hi) is empty, and it never splits a multibyte char (PR-it136).
        assert_eq!(
            differential("fun probe() -> Str { let s = \"abcde\"\n    \"{s.slice(1, 3)}|{s.slice(2, 2)}|{s.slice(1, 99)}|{s.slice(99, 100)}|{s.slice(3, 2)}\" }\n"),
            "bc||bcde||"
        );
        assert_eq!(
            differential("fun probe() -> Str { let s = \"aé世b\"\n    \"{s.slice(1, 3)}|{s.slice(2, 99)}|{\"\".slice(0, 5)}\" }\n"),
            "é世|世b|"
        );
        // List.get returns an Option (None out of bounds); take/drop clamp to the length.
        assert_eq!(
            differential("fun probe() -> Str { let xs = [10, 20, 30]\n    \"{xs.get(0)}|{xs.get(3)}|{[].get(0)}|{xs.take(2)}|{xs.take(99)}|{xs.drop(2)}|{xs.drop(99)}\" }\n"),
            "Some(10)|None|None|[10, 20]|[10, 20, 30]|[30]|[]"
        );
        // window slides; chunk splits into non-overlapping groups with a partial tail.
        assert_eq!(
            differential("fun probe() -> Str { let xs = [1, 2, 3, 4, 5]\n    \"{xs.window(2)}|{xs.chunk(2)}\" }\n"),
            "[[1, 2], [2, 3], [3, 4], [4, 5]]|[[1, 2], [3, 4], [5]]"
        );
    }

    #[test]
    fn diff_string_escape_sequences() {
        // Each escape (\n \t \r \\ \") decodes to a SINGLE character: "a\nb".len() == 3, and
        // splitting on the decoded control char works — byte-identical on interp/KVM (PR-it145).
        let src = "fun probe() -> Str {\n    let nl = \"a\\nb\"\n    let tb = \"a\\tb\"\n    let bs = \"a\\\\b\"\n    let qt = \"a\\\"b\"\n    \
                   \"{nl.len()}|{tb.len()}|{bs.len()}|{qt.len()}|{nl.split(\"\\n\")}|{qt}\"\n}\n";
        assert_eq!(differential(src), "3|3|3|3|[\"a\", \"b\"]|a\"b");
    }

    #[test]
    fn diff_string_interpolation_edges() {
        // Interpolation holds arbitrary expressions, method/function calls, and if-exprs;
        // adjacent and interleaved interps concatenate cleanly (PR-it144).
        let exprs = "fun dbl(x: Int) -> Int { x * 2 }\nfun probe() -> Str { let a = 3\n    let b = 4\n    let xs = [1, 2, 3]\n    \
                     \"{a + b}|{xs.len()}|{dbl(a)}|{if a > b { \"hi\" } else { \"lo\" }}|{a}{b}|x{a}y{b}z\" }\n";
        assert_eq!(differential(exprs), "7|3|6|lo|34|x3y4z");
        // Brace escaping: {{ -> a literal {, }} -> a literal } — so a JSON-like string with an
        // interpolation reads naturally.
        let braces = r##"fun probe() -> Str { let a = 5
    "{{|}}|{{{a}}}|a {{ b }} c|{{\"k\": {a}}}" }
"##;
        assert_eq!(differential(braces), "{|}|{5}|a { b } c|{\"k\": 5}");
        // A string literal inside an interpolation (unescaped quotes), a function call with a
        // string argument, and a NESTED interpolation all parse and evaluate correctly.
        let nested = r##"fun greet(name: Str) -> Str { "hi {name}" }
fun probe() -> Str { "{"inner"}|{greet("Ada")}|{"a{1 + 1}b"}" }
"##;
        assert_eq!(differential(nested), "inner|hi Ada|a2b");
    }

    #[test]
    fn diff_string_split_replace_search_char_indexed() {
        // split_once splits at the FIRST match (preserving an empty left part); a
        // no-match yields None (PR-it130, extending the char-indexed guarantees of it105).
        assert_eq!(
            differential("fun probe() -> Str { \"{\"a=b=c\".split_once(\"=\")}|{\"nope\".split_once(\"=\")}|{\"=lead\".split_once(\"=\")}\" }\n"),
            "Some([\"a\", \"b=c\"])|None|Some([\"\", \"lead\"])"
        );
        // replace is non-overlapping left-to-right ("aaaa" -> "bb"); replace_first hits once.
        assert_eq!(
            differential("fun probe() -> Str { \"{\"aaaa\".replace(\"aa\", \"b\")}|{\"aXbXc\".replace_first(\"X\", \"-\")}|{\"aXbXc\".replace(\"X\", \"\")}\" }\n"),
            "bb|a-bXc|abc"
        );
        // index_of/rfind return CHAR indices (é is one char, so "llo" starts at 2), count is
        // non-overlapping, split preserves empty fields — all unicode-aware.
        assert_eq!(
            differential("fun probe() -> Str { \"{\"abcabc\".index_of(\"bc\")}|{\"abcabc\".rfind(\"bc\")}|{\"héllo\".index_of(\"llo\")}|{\"aaa\".count(\"aa\")}\" }\n"),
            "Some(1)|Some(4)|Some(2)|1"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{\"a,b,,c\".split(\",\")}|{\"aXXbXXc\".split(\"XX\")}|{\"héllo\".split(\"l\")}\" }\n"),
            "[\"a\", \"b\", \"\", \"c\"]|[\"a\", \"b\", \"c\"]|[\"hé\", \"\", \"o\"]"
        );
        // pad counts characters (not bytes); reverse is char-aware.
        assert_eq!(
            differential("fun probe() -> Str { \"{\"hé\".pad_right(5, \"*\")}|{\"hé\".pad_left(5, \"*\")}|{\"héllo\".reverse()}\" }\n"),
            "hé***|***hé|olléh"
        );
    }

    #[test]
    fn diff_string_unicode_is_char_indexed() {
        // Every string op is CHAR-indexed (never byte-indexed), byte-identical on
        // interp/KVM, across 2-byte (é), 3-byte (世) and 4-byte (🎉) characters.
        let src = "fun probe() -> Str {\n    let s = \"aé世b\"\n    \
                   \"{\"aé世🎉\".len()}|{s.slice(1, 3)}|{s.index_of(\"世\")}|{s.slice(1, 99)}|\
                   {\"a世b🎉\".reverse()}|{\"éxéxé\".count(\"é\")}|{\"éé世\".replace(\"é\", \"x\")}|\
                   {\"世\".pad_left(3, \"-\")}\"\n}\n";
        assert_eq!(
            differential(src),
            "4|é世|Some(2)|é世b|🎉b世a|3|xx世|--世"
        );
        // chars() yields whole characters; split keeps multibyte parts intact.
        assert_eq!(
            differential("fun probe() -> Str { let c = \"a世🎉\".chars()\n    \"{c.len()}|{c.get(1)}|{c.get(2)}\" }\n"),
            "3|Some(\"世\")|Some(\"🎉\")"
        );
    }

    #[test]
    fn diff_parse_iso_rejects_impossible_dates() {
        // parse_iso validates the day against the actual month length (leap-year aware),
        // byte-identical on interp/KVM — an impossible calendar date is Err, not silently
        // normalized into the next month (PR-it111).
        let src = "fun probe() -> Str {\n    \
                   \"{parse_iso(\"2023-02-29\").is_ok()}|{parse_iso(\"2024-02-29\").is_ok()}|\
                   {parse_iso(\"1900-02-29\").is_ok()}|{parse_iso(\"2000-02-29\").is_ok()}|\
                   {parse_iso(\"2024-04-31\").is_ok()}|{parse_iso(\"2024-04-30\").is_ok()}|\
                   {parse_iso(\"2024-02-30\").is_ok()}\"\n}\n";
        // 2023-02 has 28 days; 2024 (leap) has 29; 1900 is NOT leap (century, not /400);
        // 2000 IS leap (/400); April has 30 days.
        assert_eq!(differential(src), "false|true|false|true|false|true|false");
        // a valid leap-day round-trips through the calendar accessors.
        assert_eq!(
            differential("fun probe() -> Str { let t = parse_iso(\"2024-02-29T00:00:00Z\").unwrap_or(0)\n    \"{year_of(t)}-{month_of(t)}-{day_of(t)}\" }\n"),
            "2024-2-29"
        );
    }

    #[test]
    fn diff_transcendental_math() {
        // sqrt/cbrt/hypot are correctly-rounded (IEEE); sin/cos/tan/exp/log/pow share the
        // platform libm, so interp and KVM (both Rust f64, which delegates to libm) agree
        // exactly (PR-it143). The special-value edges are IEEE-defined and platform-stable.
        assert_eq!(
            differential("fun probe() -> Str { \"{(2.0).sqrt()}|{(27.0).cbrt()}|{(3.0).hypot(4.0)}|{(2.0).pow(10.0)}|{(0.0 - 8.0).cbrt()}|{(9.0).pow(0.5)}\" }\n"),
            "1.4142135623730951|3.0|5.0|1024.0|-2.0|3.0"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{(1.0).sin()}|{(1.0).cos()}|{(1.0).exp()}|{(2.718281828459045).log()}|{(0.5).sin()}\" }\n"),
            "0.8414709848078965|0.5403023058681398|2.718281828459045|1.0|0.479425538604203"
        );
        // Special values: sqrt of a negative and log of <= 0 give NaN / -inf, pow(0,0) = 1,
        // pow of a negative base to a fractional exp is NaN, exp overflow is +inf.
        assert_eq!(
            differential("fun probe() -> Str { \"{(0.0).sqrt()}|{(0.0 - 1.0).sqrt()}|{(0.0).log()}|{(0.0 - 1.0).log()}|{(0.0).pow(0.0)}|{(0.0 - 2.0).pow(0.5)}|{(1000.0).exp()}\" }\n"),
            "0.0|NaN|-inf|NaN|1.0|NaN|inf"
        );
    }

    #[test]
    fn diff_float_int_conversions() {
        // round is half-away-from-zero and returns a Float; floor/ceil return Float and are
        // correct on negatives (PR-it142).
        assert_eq!(
            differential("fun probe() -> Str { let n25 = 0.0 - 2.5\n    \"{(2.5).round()}|{(3.5).round()}|{n25.round()}|{(2.4).round()}|{(2.6).round()}|{(2.7).floor()}|{(0.0 - 2.7).floor()}|{(2.7).ceil()}|{(0.0 - 2.7).ceil()}\" }\n"),
            "3.0|4.0|-3.0|2.0|3.0|2.0|-3.0|3.0|-2.0"
        );
        // to_int truncates toward zero and returns an Int; to_int of an out-of-range float
        // SATURATES, NaN -> 0, +inf -> i64::MAX, -inf -> i64::MIN — the native C `(long)double`
        // cast is UB for these, so this pins it to Rust's saturating `as i64`.
        assert_eq!(
            differential("fun probe() -> Str { let big = 1.0e20\n    let nan = 0.0 / 0.0\n    let inf = 1.0 / 0.0\n    \"{(3.9).to_int()}|{(0.0 - 3.9).to_int()}|{(0.9).to_int()}|{big.to_int()}|{nan.to_int()}|{inf.to_int()}|{(0.0 - inf).to_int()}\" }\n"),
            "3|-3|0|9223372036854775807|0|9223372036854775807|-9223372036854775808"
        );
        // Int -> Float is exact for small ints and rounds for huge ones (i64::MAX rounds up).
        assert_eq!(
            differential("fun probe() -> Str { let half = (7).to_float() / 2.0\n    \"{(5).to_float()}|{half}|{half.round()}|{(9223372036854775807).to_float()}\" }\n"),
            "5.0|3.5|4.0|9223372036854775808.0"
        );
    }

    #[test]
    fn diff_float_formatting_extremes_and_specials() {
        // The manual decimal formatter is byte-identical on interp/KVM for special
        // values, IEEE semantics, shortest-round-trip precision, and negative zero.
        assert_eq!(
            differential("fun probe() -> Str { let z = 0.0\n    \"{1.0 / z}|{-1.0 / z}|{z / z}|{(1.0/z) - (1.0/z)}\" }\n"),
            "inf|-inf|NaN|NaN"
        );
        assert_eq!(
            differential("fun probe() -> Str { let nan = 0.0 / 0.0\n    let inf = 1.0 / 0.0\n    \"{nan == nan}|{nan != nan}|{inf == inf}|{inf > 1.0}\" }\n"),
            "false|true|true|true"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{0.1 + 0.2}|{1.0 / 3.0}|{2.0 / 3.0}\" }\n"),
            "0.30000000000000004|0.3333333333333333|0.6666666666666666"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{0.0 - 0.0}|{0.0 * -1.0}|{-1.5e-10}\" }\n"),
            "0.0|-0.0|-0.00000000015"
        );
        // Every magnitude — including 1e308 and 1e-10 — round-trips through the
        // formatter and parse_float exactly (positional, no exponent).
        assert_eq!(
            differential("fun probe() -> Str { let v = [0.1 + 0.2, 1e20, 1e-10, 1e308, 3.14159265358979]\n    \"{v.map(fn x { \"{x}\".parse_float().unwrap_or(0.0) == x })}\" }\n"),
            "[true, true, true, true, true]"
        );
    }

    #[test]
    fn diff_map_higher_order_ordering() {
        // Maps are insertion-ordered; every HOF preserves that order, byte-identical on
        // interp/KVM (PR-it128, completing the collection-HOF trio with sets/lists).
        // merge: a shared key takes the SECOND map's value but keeps the FIRST map's
        // position; new keys are appended.
        assert_eq!(
            differential("fun probe() -> Str { let a = Map().insert(\"x\", 1).insert(\"y\", 2).insert(\"z\", 3)\n    let b = Map().insert(\"y\", 20).insert(\"w\", 40)\n    \"{a.merge(b)}\" }\n"),
            "Map{\"x\": 1, \"y\": 20, \"z\": 3, \"w\": 40}"
        );
        // map_values keeps key order; fold visits entries in insertion order (a
        // non-commutative string fold); filter keeps surviving entries in order.
        assert_eq!(
            differential("fun probe() -> Str { let m = Map().insert(\"c\", 3).insert(\"a\", 1).insert(\"b\", 2)\n    \"{m.map_values(fn v { v * 10 })}|{m.fold(\"\", fn(acc, k, v) { \"{acc}{k}={v};\" })}\" }\n"),
            "Map{\"c\": 30, \"a\": 10, \"b\": 20}|c=3;a=1;b=2;"
        );
        assert_eq!(
            differential("fun probe() -> Str { let m = Map().insert(\"a\", 1).insert(\"b\", 2).insert(\"c\", 3).insert(\"d\", 4)\n    \"{m.filter(fn(k, v) { v % 2 == 0 })}\" }\n"),
            "Map{\"b\": 2, \"d\": 4}"
        );
        // keys()/values() are in insertion order; a duplicate-key insert updates in place
        // (keeps position); get_or returns the value or the default.
        assert_eq!(
            differential("fun probe() -> Str { let m = Map().insert(\"z\", 26).insert(\"a\", 1).insert(\"m\", 13).insert(\"z\", 99)\n    \"{m.keys()}|{m.values()}|{m.get_or(\"z\", 0)}|{m.get_or(\"q\", 0 - 1)}\" }\n"),
            "[\"z\", \"a\", \"m\"]|[99, 1, 13]|99|-1"
        );
    }

    #[test]
    fn diff_set_algebra_preserves_insertion_order() {
        // Set algebra is insertion-ordered and byte-identical on interp/KVM: union keeps
        // a's order then b's new elements; intersect/difference keep a's order;
        // symmetric_difference is a's uniques then b's uniques (PR-it123).
        assert_eq!(
            differential("fun probe() -> Str { \"{Set([3, 1, 2]).union(Set([2, 5, 4]))}\" }\n"),
            "Set{3, 1, 2, 5, 4}"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{Set([3, 1, 2, 5]).intersect(Set([5, 2, 9]))}\" }\n"),
            "Set{2, 5}"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{Set([3, 1, 2, 5]).difference(Set([2, 5]))}\" }\n"),
            "Set{3, 1}"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{Set([1, 2, 3]).symmetric_difference(Set([3, 4, 5]))}\" }\n"),
            "Set{1, 2, 4, 5}"
        );
        // subset checks + self/empty-set edge cases.
        assert_eq!(
            differential("fun probe() -> Str { let a = Set([1, 2, 3])\n    let e = Set([])\n    \"{Set([1, 2]).is_subset(a)}|{a.is_subset(a)}|{a.union(a)}|{a.difference(a)}|{a.union(e)}|{e.union(a)}|{a.intersect(e)}\" }\n"),
            "true|true|Set{1, 2, 3}|Set{}|Set{1, 2, 3}|Set{1, 2, 3}|Set{}"
        );
    }

    #[test]
    fn diff_number_parsing_is_strict_and_consistent() {
        // parse_int is Rust-strict (not lenient C strtoll): a leading sign is fine, but
        // surrounding whitespace, trailing junk, and a decimal point all yield None; an
        // OVERFLOW past i64 yields None (never a saturate or panic) — PR-it131.
        assert_eq!(
            differential("fun probe() -> Str { \"{\"42\".parse_int()}|{\"-42\".parse_int()}|{\"+42\".parse_int()}|{\"  42  \".parse_int()}|{\"42abc\".parse_int()}|{\"\".parse_int()}|{\"3.5\".parse_int()}\" }\n"),
            "Some(42)|Some(-42)|Some(42)|None|None|None|None"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{\"9223372036854775807\".parse_int()}|{\"9223372036854775808\".parse_int()}|{\"999999999999999999999999999999\".parse_int()}\" }\n"),
            "Some(9223372036854775807)|None|None"
        );
        // parse_float accepts scientific notation and the specials inf/nan, overflows to inf,
        // and rejects empty/whitespace/double-dot — identically on all engines.
        assert_eq!(
            differential("fun probe() -> Str { \"{\"3.14\".parse_float()}|{\"1e10\".parse_float()}|{\"inf\".parse_float()}|{\"nan\".parse_float()}|{\"\".parse_float()}|{\"1.2.3\".parse_float()}\" }\n"),
            "Some(3.14)|Some(10000000000.0)|Some(inf)|Some(NaN)|None|None"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{\".5\".parse_float()}|{\"5.\".parse_float()}|{\"42\".parse_float()}|{\"1e400\".parse_float()}|{\"  1.5 \".parse_float()}\" }\n"),
            "Some(0.5)|Some(5.0)|Some(42.0)|Some(inf)|None"
        );
    }

    #[test]
    fn diff_nan_in_by_reductions_and_tensors() {
        // max_by/min_by with a Float key that can be NaN use k_cmp's strict comparison (fixed
        // in PR-it148/149), so a NaN key is inert — it wins only as the seed, matching the
        // interpreter's first-seeded fold across every NaN position (PR-it150).
        let mb = "type P = P(id: Int, key: Float)\n\
                  fun wmax(xs: List[P]) -> Int { match xs.max_by(fn(p: P) { p.key }) {\n        Some(p) => p.id\n        None => 0 - 1\n    } }\n\
                  fun wmin(xs: List[P]) -> Int { match xs.min_by(fn(p: P) { p.key }) {\n        Some(p) => p.id\n        None => 0 - 1\n    } }\n\
                  fun probe() -> Str {\n    let nan = 0.0 / 0.0\n    \
                  let mid = [P(id: 1, key: 1.0), P(id: 2, key: nan), P(id: 3, key: 2.0)]\n    \
                  let first = [P(id: 1, key: nan), P(id: 2, key: 3.0), P(id: 3, key: 1.0)]\n    \
                  let last = [P(id: 1, key: 3.0), P(id: 2, key: 1.0), P(id: 3, key: nan)]\n    \
                  \"{wmax(mid)},{wmin(mid)}|{wmax(first)},{wmin(first)}|{wmax(last)},{wmin(last)}\"\n}\n";
        assert_eq!(differential(mb), "3,1|1,1|1,2");
        // Tensor reductions propagate NaN (a NaN element poisons sum/mean/dot).
        assert_eq!(
            differential("fun probe() -> Str { let t = tensor([1.0, 0.0 / 0.0, 2.0])\n    \"{t.sum()}|{t.mean()}|{t.dot(tensor([1.0, 1.0, 1.0]))}\" }\n"),
            "NaN|NaN|NaN"
        );
    }

    #[test]
    fn diff_nan_in_collections() {
        // NaN in collections follows from `nan != nan` and its unordered comparisons — sort
        // is deterministic and identical across engines (the PR-it148 k_cmp fix propagated),
        // min/max SKIP NaN, and equality-based ops keep duplicate NaNs (PR-it149).
        let src = "fun probe() -> Str { let nan = 0.0 / 0.0\n    let xs = [3.0, nan, 1.0, 2.0]\n    \
                   let dup = [nan, nan, 1.0, 1.0]\n    \
                   \"{xs.sort()}|{xs.min()}|{xs.max()}|{dup.unique()}|{dup.contains(nan)}|{[1.0, 2.0].contains(2.0)}\" }\n";
        assert_eq!(differential(src), "[3.0, NaN, 1.0, 2.0]|Some(1.0)|Some(3.0)|[NaN, NaN, 1.0]|false|true");
        // Set and Map are equality-keyed, and nan != nan, so duplicate NaN elements/keys are
        // all kept and a NaN key can never be looked up.
        let sm = "fun probe() -> Str { let nan = 0.0 / 0.0\n    let s = Set([nan, nan, 1.0])\n    \
                  let m = Map().insert(nan, 1).insert(nan, 2)\n    \"{s.len()}|{m.len()}|{m.get_or(nan, 0 - 1)}\" }\n";
        assert_eq!(differential(sm), "3|2|-1");
    }

    #[test]
    fn diff_comparison_operator_edges() {
        // NaN is IEEE-UNORDERED: every comparison against it (incl. <= and >=, and nan <= nan
        // / nan >= nan) is false, and nan != nan is true. -0.0 == 0.0 but is not < 0.0. inf
        // orders above finite values (PR-it148).
        let f = "fun probe() -> Str { let nan = 0.0 / 0.0\n    let inf = 1.0 / 0.0\n    let nz = 0.0 - 0.0\n    \
                 \"{nan == nan}|{nan != nan}|{nan < 1.0}|{nan <= nan}|{nan >= nan}|{1.0 < nan}|{inf > 1.0e308}|{inf == inf}|{0.0 == nz}|{nz < 0.0}\" }\n";
        assert_eq!(differential(f), "false|true|false|false|false|false|true|true|true|false");
        // Ordinary float / int comparisons, and lexicographic Str ordering.
        let ord = "fun probe() -> Str { \"{1.5 < 2.5}|{2.5 <= 2.5}|{3.0 >= 2.0}|{\"apple\" < \"banana\"}|{\"Apple\" < \"apple\"}|{\"a\" < \"ab\"}|{\"\" < \"a\"}\" }\n";
        assert_eq!(differential(ord), "true|true|true|true|true|true|true");
    }

    #[test]
    fn diff_numeric_bitwise_shift_and_sized_arithmetic() {
        // Int bitwise/shift/number-theory methods, byte-identical on interp/KVM. The
        // key case: `shr` is ARITHMETIC (sign-extends) while `ushr` is LOGICAL — the
        // classic Rust-vs-C signed-shift divergence, handled identically here (PR-it124).
        let ints = "fun probe() -> Str { \"{(0 - 8).shr(1)}|{(0 - 8).ushr(1)}|{(0 - 1).ushr(60)}|{(0).bnot()}|{(5).bnot()}|{(0 - 1).band(5)}|{(0 - 255).to_hex()}|{(12).gcd(18)}|{(0 - 12).gcd(8)}|{(17).isqrt()}|{(0 - 5).sign()}\" }\n";
        assert_eq!(differential(ints), "-4|9223372036854775804|15|-1|-6|5|-ff|6|4|4|-1");
        // isqrt of a negative is a clean, identical panic.
        assert_eq!(differential("fun probe() -> Str { \"{(0 - 5).isqrt()}\" }\n"), "panic: `isqrt` of a negative Int");
        // sized-int saturating vs wrapping arithmetic clamps/wraps at the type bounds.
        let sized = "fun probe() -> Str { \"{(255u8).saturating_add(1u8)}|{(255u8).wrapping_add(1u8)}|{(127i8).saturating_add(1i8)}|{(127i8).wrapping_add(1i8)}\" }\n";
        assert_eq!(differential(sized), "255|0|127|-128");
    }

    #[test]
    fn diff_bigint_and_rational_edges() {
        // Arbitrary-precision BigInt: exact huge products, truncated-toward-zero div/mod
        // with negatives, and a clean div-by-zero panic — byte-identical on interp/KVM.
        assert_eq!(
            differential("fun probe() -> Str { \"{big(1000000000000) * big(1000000000000)}\" }\n"),
            "1000000000000000000000000"
        );
        assert_eq!(
            differential("fun probe() -> Str { \"{big(17) / big(5)}|{big(17) % big(5)}|{big(-17) / big(5)}|{big(-17) % big(5)}\" }\n"),
            "3|2|-3|-2"
        );
        assert_eq!(differential("fun probe() -> Str { \"{big(5) / big(0)}\" }\n"), "panic: division by zero");
        let fact = "fun fact(n: Int) -> BigInt {\n    var acc = big(1)\n    var i = 1\n    \
                    while i <= n { acc = acc * big(i)\n        i = i + 1 }\n    acc\n}\n\
                    fun probe() -> Str { \"{fact(30)}\" }\n";
        assert_eq!(differential(fact), "265252859812191058636308480000000");
        // Exact Rational: reduction (2/4->1/2, 6/3->2), sign normalized to the numerator,
        // arithmetic, division, conversions, and a zero-denominator panic.
        assert_eq!(
            differential("fun probe() -> Str { \"{rat(2, 4)}|{rat(1, 3) + rat(1, 6)}|{rat(1, 3) / rat(1, 2)}|{rat(6, 3)}|{rat(2, -4)}\" }\n"),
            "1/2|1/2|2/3|2|-1/2"
        );
        assert_eq!(differential("fun probe() -> Str { \"{rat(1, 0)}\" }\n"), "panic: division by zero");
        assert_eq!(
            differential("fun probe() -> Str { let r = rat(3, 4)\n    \"{r.to_float()}|{r.recip()}|{r.num()}|{r.den()}\" }\n"),
            "0.75|4/3|3|4"
        );
    }

    #[test]
    fn diff_tensor_elementwise_arithmetic() {
        // Elementwise +,-,*,/ over equal-length tensors, byte-identical on interp/KVM.
        let src = "fun probe() -> Str {\n    let a = tensor([6.0, 8.0])\n    let b = tensor([2.0, 4.0])\n    \
                   \"{(a + b).to_list()}|{(a - b).to_list()}|{(a * b).to_list()}|{(a / b).to_list()}\"\n}\n";
        assert_eq!(differential(src), "[8.0, 12.0]|[4.0, 4.0]|[12.0, 32.0]|[3.0, 2.0]");
        // subtraction that cancels stays +0.0 (no -0.0 hole), chained ops, empty+empty.
        assert_eq!(
            differential("fun probe() -> Str { \"{(tensor([1.0, 5.0]) - tensor([1.0, 5.0])).to_list()}\" }\n"),
            "[0.0, 0.0]"
        );
        assert_eq!(differential("fun probe() -> Str { \"{(zeros(0) + zeros(0)).to_list()}\" }\n"), "[]");
        // a length mismatch is a clean, identical panic on both engines.
        assert_eq!(
            differential("fun probe() -> Str { \"{(tensor([1.0, 2.0]) + tensor([1.0, 2.0, 3.0])).to_list()}\" }\n"),
            "panic: tensor length mismatch (2 vs 3)"
        );
    }

    #[test]
    fn diff_map_self_insert_in_place_preserves_aliasing() {
        // The `m = m.insert(k, v)` in-place fast path (PR-it91, O(n^2)->O(n) map
        // build) must only fire when the Map is uniquely owned. An aliased map must
        // be untouched — value semantics. Here `alias` shares m before the insert,
        // so it stays len 1 while m grows to 2, on both interp and KVM.
        let src = "fun probe() -> Str {\n    var m = Map().insert(\"a\", 1)\n    let alias = m\n    \
                   m = m.insert(\"b\", 2)\n    \"{alias.len()}|{m.len()}|{alias.get(\"b\")}\"\n}\n";
        assert_eq!(differential(src), "1|2|None");
        // and a self-insert loop with an overwrite of an existing key stays correct.
        let loop_src = "fun probe() -> Str {\n    var m = Map()\n    \
                        for i in 0..4 { m = m.insert(\"k{i}\", i) }\n    \
                        m = m.insert(\"k1\", 99)\n    \"{m.keys()}|{m.values()}\"\n}\n";
        assert_eq!(differential(loop_src), "[\"k0\", \"k1\", \"k2\", \"k3\"]|[0, 99, 2, 3]");
    }

    #[test]
    fn diff_set_self_insert_in_place_preserves_aliasing_and_dedup() {
        // `s = s.insert(v)` in-place (PR-it92, O(n^2)->O(n) set build) fires only on a
        // uniquely-owned Set. An aliased set is untouched, dedup still holds (inserting
        // a present element is a no-op), insertion order is preserved. interp == KVM.
        let src = "fun probe() -> Str {\n    var s: Set[Int] = Set().insert(1).insert(2)\n    \
                   let alias = s\n    s = s.insert(2)\n    s = s.insert(3)\n    \
                   \"{alias.to_list()}|{s.to_list()}\"\n}\n";
        assert_eq!(differential(src), "[1, 2]|[1, 2, 3]");
        // a build loop with duplicates dedups to the distinct values, in order.
        let loop_src = "fun probe() -> Str {\n    var s: Set[Int] = Set()\n    \
                        for i in 0..6 { s = s.insert(i % 3) }\n    \"{s.to_list()}\"\n}\n";
        assert_eq!(differential(loop_src), "[0, 1, 2]");
    }

    #[test]
    fn diff_map_set_method_semantics() {
        // Map/Set methods behave identically on interp and KVM, and iteration order
        // is INSERTION order (not sorted, not hash order) — keys/values follow the
        // insert sequence, an overwrite keeps the position + last value. Reads: keys
        // (insertion order, dedup on overwrite), values (last-write), get present/
        // missing, contains_key missing, len (overwrite doesn't grow), remove missing
        // (unchanged), union/intersect/difference.
        let src = "fun probe() -> Str {\n    \
                   let m = Map().insert(\"banana\", 1).insert(\"apple\", 2).insert(\"banana\", 9)\n    \
                   let a: Set[Int] = Set().insert(1).insert(2).insert(3)\n    \
                   let b: Set[Int] = Set().insert(2).insert(3).insert(4)\n    \
                   \"{m.keys()}|{m.values()}|{m.get(\"apple\")}|{m.get(\"z\")}|{m.contains_key(\"z\")}|\
                   {m.len()}|{m.remove(\"z\").len()}|{a.union(b).to_list()}|{a.intersect(b).to_list()}|\
                   {a.difference(b).to_list()}\"\n}\n";
        assert_eq!(
            differential(src),
            "[\"banana\", \"apple\"]|[9, 2]|Some(2)|None|false|2|2|[1, 2, 3, 4]|[2, 3]|[1]"
        );
    }

    #[test]
    fn diff_numeric_and_math_edge_cases() {
        // Numeric/math edges are identical on interp and KVM — including the full
        // 17-digit transcendental strings (Rust f64 vs libm agree) and IEEE
        // special values. Reads: parse_int garbage(None)/valid(Some), truncated mod
        // sign (-7%3=-1, 7%-3=1), float div (inf, NaN), sqrt of a negative (NaN),
        // sqrt(2), log(2), 1e20 formatting, to_hex, gcd, negative to_hex.
        let src = "fun probe() -> Str {\n    let neg = 0.0 - 1.0\n    \
                   \"{\"abc\".parse_int()}|{\"42\".parse_int()}|{-7 % 3}|{7 % -3}|{1.0 / 0.0}|\
                   {0.0 / 0.0}|{neg.sqrt()}|{(2.0).sqrt()}|{(2.0).log()}|{100000000000000000000.0}|\
                   {(255).to_hex()}|{(48).gcd(36)}|{(0 - 8).to_hex()}\"\n}\n";
        assert_eq!(
            differential(src),
            "None|Some(42)|-1|1|inf|NaN|NaN|1.4142135623730951|0.6931471805599453|100000000000000000000.0|ff|12|-8"
        );
        // Int divide/modulo by zero is a clean guarded panic, same message everywhere.
        assert_eq!(differential("fun probe() -> Str { \"{7 / 0}\" }\n"), "panic: division by zero");
        assert_eq!(differential("fun probe() -> Str { \"{7 % 0}\" }\n"), "panic: remainder by zero");
    }

    #[test]
    fn diff_stdlib_method_edge_cases() {
        // Boundary/empty/unicode/out-of-range inputs to stdlib methods behave
        // identically on interp and KVM. Reads: slice past end (clamps), reversed
        // slice (empty), take/drop past len, split with empty field, pad_left,
        // multibyte reverse, index_of not-found (None), zip_with unequal (truncates),
        // first() of empty (None), get() out-of-range (None).
        let src = "fun probe() -> Str {\n    let xs = [1, 2, 3]\n    let e: List[Int] = []\n    \
                   \"{\"hello\".slice(2, 100)}|{\"hello\".slice(3, 1)}|{xs.take(10)}|{xs.drop(10)}|\
                   {\"a,,b\".split(\",\").len()}|{\"hi\".pad_left(5, \" \")}|{\"héllo\".reverse()}|\
                   {\"x\".index_of(\"z\")}|{xs.zip_with([10, 20], fn(a, b) { a + b })}|{e.first()}|\
                   {[1, 2].get(5)}\"\n}\n";
        assert_eq!(differential(src), "llo||[1, 2, 3]|[]|3|   hi|olléh|None|[11, 22]|None|None");
        // chunk(0) is a clean guarded panic (not a native div-by-zero), same message.
        assert_eq!(
            differential("fun probe() -> Str { \"{[1, 2, 3].chunk(0)}\" }\n"),
            "panic: `chunk` needs a positive Int"
        );
    }

    #[test]
    fn diff_equality_and_comparison_semantics() {
        // Structural (deep, not identity) equality across every compound shape,
        // order-independent Map equality, IEEE float edges, and codepoint string
        // ordering — all identical on interp and KVM. The bool string reads:
        // list, nested-list, ctor, variant==, variant!=, nested-Option, Map(reordered),
        // NaN==NaN(false), NaN!=NaN(true), -0.0==0.0(true), "Z"<"a"(codepoint).
        let src = "type P = Pt(x: Int, y: Int)\ntype C = Red | Green | Blue\n\
                   fun probe() -> Str {\n    let ma = Map().insert(\"x\", 1).insert(\"y\", 2)\n    \
                   let mb = Map().insert(\"y\", 2).insert(\"x\", 1)\n    let nan = 0.0 / 0.0\n    \
                   \"{[1, 2] == [1, 2]}{[[1], [2]] == [[1], [2]]}{Pt(1, 2) == Pt(1, 2)}{Red == Red}\
                   {Red == Blue}{Some([1, 2]) == Some([1, 2])}{ma == mb}{nan == nan}{nan != nan}\
                   {-0.0 == 0.0}{\"Z\" < \"a\"}\"\n}\n";
        assert_eq!(differential(src), "truetruetruetruefalsetruetruefalsetruetruetrue");
    }

    #[test]
    fn diff_pattern_match_semantics() {
        // First-match-wins (the literal `1` arm before the guard before `_`), an
        // arm guard (`x if x > 10`) that falls through when false, and a nested
        // `Some(x)` binding — all identical on interp and KVM.
        let src = "fun classify(n: Int) -> Str {\n    match n {\n        1 => \"one\"\n        \
                   x if x > 10 => \"big\"\n        _ => \"other\"\n    }\n}\n\
                   fun f(o: Option[Int]) -> Str {\n    match o {\n        Some(x) => \"some {x}\"\n        \
                   None => \"none\"\n    }\n}\n\
                   fun probe() -> Str { \"{classify(1)},{classify(20)},{classify(5)}|{f(Some(9))},{f(None)}\" }\n";
        assert_eq!(differential(src), "one,big,other|some 9,none");
    }

    #[test]
    fn diff_eval_order_and_short_circuit() {
        // `&&`/`||` short-circuit: the RHS (which would panic on divide-by-zero) is
        // NOT evaluated when the LHS already decides the result — identically on
        // interp and KVM. If either engine evaluated it, this would be "panic: …".
        let sc = "fun bad() -> Bool { let z = 0\n    1 / z == 1 }\n\
                  fun probe() -> Str { let a = false && bad()\n    let b = true || bad()\n    \"{a},{b}\" }\n";
        assert_eq!(differential(sc), "false,true");
        // Loop-variable capture: each closure built in the loop captures its OWN
        // iteration value (value capture, PR-it76), not a shared last value.
        let lc = "fun probe() -> Str { var fns: List[fn() -> Int] = []\n    \
                  for i in 0..3 { fns = fns.push(fn() { i * 10 }) }\n    var out = \"\"\n    \
                  for f in fns { out = out + \"{f()};\" }\n    out }\n";
        assert_eq!(differential(lc), "0;10;20;");
    }

    #[test]
    fn diff_closure_value_capture() {
        // Closures capture free locals BY VALUE (a snapshot rebound per call), so
        // interp == KVM: mutating the outer var after the closure is made is NOT
        // seen (1, not 99), and a "counter" closure does not accumulate across calls
        // (each starts from the captured snapshot). This used to diverge — the
        // interpreter did live env-reference capture. make() also proves per-call
        // isolation and independence of two closure instances.
        let src = "fun make() -> fn() -> Int {\n    var n = 0\n    fn() { n = n + 1\n        n }\n}\n\
                   fun probe() -> Str {\n    var x = 1\n    let f = fn() { x }\n    x = 99\n    \
                   let c = make()\n    let d = make()\n    \"{f()}|{c()}{c()}{d()}{c()}\"\n}\n";
        assert_eq!(differential(src), "1|1111");
        // a closure over an unmutated outer var (the common map/fold idiom) is
        // unchanged and identical
        assert_eq!(
            differential("fun probe() -> Str {\n    let y = 10\n    \"{[1, 2, 3].map(fn x { x + y })}\"\n}\n"),
            "[11, 12, 13]"
        );
    }

    #[test]
    fn diff_csv_pathological_input() {
        // csv_parse handles hostile/edge input identically on both engines without
        // panicking: an unterminated quoted field takes the rest of the row, doubled
        // quotes unescape, a trailing comma yields a trailing empty field, empty
        // input is no rows.
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{csv_parse(\"a,\\\"unterminated,b\")}\"\n}\n"),
            "[[\"a\", \"unterminated,b\"]]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    \"{csv_parse(\"a,\\\"he said \\\"\\\"hi\\\"\\\"\\\",c\")}\"\n}\n"),
            "[[\"a\", \"he said \"hi\"\", \"c\"]]"
        );
        assert_eq!(differential("fun probe() -> Str {\n    \"{csv_parse(\"a,b,\")}\"\n}\n"), "[[\"a\", \"b\", \"\"]]");
        assert_eq!(differential("fun probe() -> Str {\n    \"{csv_parse(\"\")}\"\n}\n"), "[]");
    }

    #[test]
    fn diff_runtime_panic_messages_actionable() {
        // Certify that the common runtime panics carry actionable context and are
        // identical on both engines: overflow says WHICH op, pow names its
        // constraint. (Vague ones were fixed in it64 tensor-index / it65 expect;
        // the non-exhaustive-match fall-through is unreachable — the K0256/K0257
        // exhaustiveness checker rejects it at compile time.)
        assert_eq!(differential("fun probe() -> Int {\n    2.pow(0 - 1)\n}\n"), "panic: `pow` needs a non-negative exponent");
        assert_eq!(differential("fun probe() -> Int {\n    let m = (0 - 9223372036854775807) - 1\n    m.abs()\n}\n"), "panic: integer overflow in abs");
        assert_eq!(differential("fun probe() -> Int {\n    let m = (0 - 9223372036854775807) - 1\n    m / (0 - 1)\n}\n"), "panic: integer overflow in division");
    }

    #[test]
    fn diff_expect_stmt() {
        let src = "fun probe() -> Int {\n    expect 1 + 1 == 2\n    7\n}\n";
        assert_eq!(differential(src), "7");
        // a FAILED expect names the failing expression (rendered from source) so the
        // panic says WHAT failed — identical on both engines.
        assert_eq!(
            differential("fun probe() -> Int {\n    expect 1 == 2\n    7\n}\n"),
            "panic: expectation failed: 1 == 2"
        );
        assert_eq!(
            differential("fun probe() -> Int {\n    let x = 5\n    expect x > 10\n    7\n}\n"),
            "panic: expectation failed: x > 10"
        );
    }

    /// Components: drive the same instance on both engines via sends + exposes.
    #[test]
    fn diff_component_state_machine() {
        let src = r#"
type Entry = { key: Str, value: Str }

component Store {
    intent "Keyed store with a sent-in default."

    in preload: Str

    state entries: List[Entry] = []
    state loads: Int = 0

    on preload(key) {
        entries = entries.push(Entry(key: key, value: "preloaded:{key}"))
        loads += 1
    }

    expose fun put(key: Str, value: Str) -> Int {
        entries = entries.filter(fn e { e.key != key }).push(Entry(key: key, value: value))
        entries.len()
    }

    expose fun get(key: Str) -> Str {
        match entries.find(fn e { e.key == key }) {
            Some(e) => e.value
            None => "missing"
        }
    }

    expose fun stats() -> Str {
        "{entries.len()} entries, {loads} loads"
    }
}
"#;
        let compiled = crate::run::compile(src).expect("compiles");

        // interpreter
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        let inst = interp
            .instantiate("Store", &[], crate::diag::Span::default())
            .ok()
            .and_then(|v| match v {
                Value::Component(id) => Some(id),
                _ => None,
            })
            .expect("instantiates");
        interp.start_all().ok();
        let call_i = |interp: &mut Interp, id: usize, name: &str, args: Vec<Value>| -> String {
            let f = Value::Bound(id, std::rc::Rc::new(name.to_string()));
            match interp.call_value(f, args, crate::diag::Span::default()) {
                Ok(v) => v.to_string(),
                Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
                Err(_) => "flow".into(),
            }
        };
        interp.send(inst, "preload", Value::str("alpha")).unwrap_or(());
        let mut i_log = Vec::new();
        i_log.push(call_i(&mut interp, inst, "put", vec![Value::str("k"), Value::str("v1")]));
        i_log.push(call_i(&mut interp, inst, "put", vec![Value::str("k"), Value::str("v2")]));
        i_log.push(call_i(&mut interp, inst, "get", vec![Value::str("k")]));
        i_log.push(call_i(&mut interp, inst, "get", vec![Value::str("alpha")]));
        i_log.push(call_i(&mut interp, inst, "get", vec![Value::str("nope")]));
        i_log.push(call_i(&mut interp, inst, "stats", vec![]));

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut vm = Vm::new(&module);
        let id = vm.instantiate_named("Store", vec![]).expect("instantiates");
        vm.send(id, "preload", Value::str("alpha")).expect("send");
        let call_v = |vm: &mut Vm, id: usize, name: &str, args: Vec<Value>| -> String {
            match vm.call_expose(id, name, args) {
                Ok(v) => v.to_string(),
                Err(e) => format!("panic: {}", e.msg),
            }
        };
        let mut v_log = Vec::new();
        v_log.push(call_v(&mut vm, id, "put", vec![Value::str("k"), Value::str("v1")]));
        v_log.push(call_v(&mut vm, id, "put", vec![Value::str("k"), Value::str("v2")]));
        v_log.push(call_v(&mut vm, id, "get", vec![Value::str("k")]));
        v_log.push(call_v(&mut vm, id, "get", vec![Value::str("alpha")]));
        v_log.push(call_v(&mut vm, id, "get", vec![Value::str("nope")]));
        v_log.push(call_v(&mut vm, id, "stats", vec![]));

        assert_eq!(i_log, v_log, "interpreter and KVM disagree on component behavior");
        assert_eq!(v_log[2], "v2");
        assert_eq!(v_log[3], "preloaded:alpha");
        assert_eq!(v_log[5], "2 entries, 1 loads"); // alpha + k (v2 overwrote v1)
    }

    #[test]
    fn diff_component_isolation_and_panic() {
        // Two instances of the same component keep SEPARATE private state, and a
        // panic inside an expose is caught identically on both engines (a clean
        // "panic: …", never an ICE). Locks in actor state isolation + failure.
        let src = r#"
component Acc {
    intent "isolated accumulator"
    state sum: Int = 0
    expose fun add(n: Int) -> Int { sum += n; sum }
    expose fun risky(d: Int) -> Int { sum / d }
}
"#;
        let compiled = crate::run::compile(src).expect("compiles");

        // interpreter — two independent instances
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        let mk = |interp: &mut Interp| match interp
            .instantiate("Acc", &[], crate::diag::Span::default())
        {
            Ok(Value::Component(id)) => id,
            _ => panic!("instantiates"),
        };
        let (a, b) = (mk(&mut interp), mk(&mut interp));
        interp.start_all().ok();
        let call_i = |interp: &mut Interp, id: usize, name: &str, args: Vec<Value>| -> String {
            let f = Value::Bound(id, std::rc::Rc::new(name.to_string()));
            match interp.call_value(f, args, crate::diag::Span::default()) {
                Ok(v) => v.to_string(),
                Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
                Err(_) => "flow".into(),
            }
        };
        let mut i_log = Vec::new();
        i_log.push(call_i(&mut interp, a, "add", vec![Value::Int(10)]));
        i_log.push(call_i(&mut interp, b, "add", vec![Value::Int(100)]));
        i_log.push(call_i(&mut interp, a, "add", vec![Value::Int(1)])); // a isolated from b
        i_log.push(call_i(&mut interp, b, "add", vec![Value::Int(5)]));
        i_log.push(call_i(&mut interp, a, "risky", vec![Value::Int(0)])); // panic

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut vm = Vm::new(&module);
        let (va, vb) = (
            vm.instantiate_named("Acc", vec![]).unwrap(),
            vm.instantiate_named("Acc", vec![]).unwrap(),
        );
        let call_v = |vm: &mut Vm, id: usize, name: &str, args: Vec<Value>| -> String {
            match vm.call_expose(id, name, args) {
                Ok(v) => v.to_string(),
                Err(e) => format!("panic: {}", e.msg),
            }
        };
        let mut v_log = Vec::new();
        v_log.push(call_v(&mut vm, va, "add", vec![Value::Int(10)]));
        v_log.push(call_v(&mut vm, vb, "add", vec![Value::Int(100)]));
        v_log.push(call_v(&mut vm, va, "add", vec![Value::Int(1)]));
        v_log.push(call_v(&mut vm, vb, "add", vec![Value::Int(5)]));
        v_log.push(call_v(&mut vm, va, "risky", vec![Value::Int(0)]));

        assert_eq!(i_log, v_log, "interp and KVM disagree on component isolation/panic");
        assert_eq!(i_log[2], "11", "instance a is isolated from b (10 + 1, not + 100)");
        assert_eq!(i_log[3], "105");
        assert_eq!(i_log[4], "panic: division by zero");
    }

    #[test]
    fn diff_ai_fun_mock_provider() {
        // Deterministic mock responses; per-fun vars avoid cross-test races.
        std::env::set_var("KUPL_AI_MOCK_DIFF_SUMMARIZE", "  a short summary  ");
        std::env::set_var(
            "KUPL_AI_MOCK_DIFF_JUDGE",
            "{\"value\":{\"label\":\"positive\",\"score\":0.75}}",
        );
        std::env::set_var("KUPL_AI_MOCK_DIFF_BROKEN", "this is not json");
        let src = "type Verdict = { label: Str, score: Float }\n\
ai fun diff_summarize(text: Str) -> Str {\n    intent \"Summarize.\"\n}\n\
ai fun diff_judge(text: Str) -> Result[Verdict, Str] {\n    intent \"Judge.\"\n}\n\
ai fun diff_broken(text: Str) -> Result[Int, Str] {\n    intent \"Count.\"\n}\n\
fun probe() -> Str {\n\
    let s = diff_summarize(\"x\")\n\
    let judged = match diff_judge(\"x\") {\n        Ok(v) => \"{v.label}:{v.score}\"\n        Err(e) => \"err\"\n    }\n\
    let broken = match diff_broken(\"x\") {\n        Ok(n) => \"ok\"\n        Err(e) => \"captured\"\n    }\n\
    \"{s}|{judged}|{broken}\"\n\
}\n";
        assert_eq!(differential(src), "a short summary|positive:0.75|captured");
    }

    #[test]
    fn kx_execution_roundtrip_is_byte_identical() {
        // A .kx compiled module, after a full encode -> decode serialization round-trip, runs to
        // the EXACT same output as the in-memory interp/KVM result — across ADT match, HOF
        // map/fold, Map, and numeric/string builtins. This certifies the on-disk bytecode format
        // preserves execution semantics byte-for-byte (PR-it187).
        let src = "type Shape = Circle(r: Float) | Rect(w: Float, h: Float)\n\
fun area(s: Shape) -> Float {\n    match s {\n        Circle(r) => 3.0 * r * r\n        Rect(w, h) => w * h\n    }\n}\n\
fun probe() -> Str {\n    let shapes = [Circle(2.0), Rect(3.0, 4.0)]\n    \
let areas = shapes.map(fn s { area(s) })\n    let m = Map().insert(\"n\", shapes.len())\n    \
\"{areas}|{areas.fold(0.0, fn(a, x) { a + x })}|{m.keys()}|{(10).factorial()}|{(255).count_ones()}\"\n}\n";
        // In-memory interp == KVM baseline.
        let expected = differential(src);
        assert_eq!(expected, "[12.0, 12.0]|24.0|[\"n\"]|3628800|8");
        // Now compile, serialize to .kx bytes, deserialize, and run on the KVM.
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let bytes = crate::kx::encode(&module);
        let decoded = crate::kx::decode(&bytes).expect("decodes");
        let mut vm = Vm::new(&decoded);
        let v = vm.call_named("probe", vec![]).expect("runs");
        assert_eq!(v.to_string(), expected);
    }

    #[test]
    fn ai_fun_kx_roundtrip() {
        std::env::set_var("KUPL_AI_MOCK_KX_HAIKU", "one two three");
        let src = "ai fun kx_haiku(topic: Str) -> Str {\n    intent \"Haiku.\"\n}\n\
fun probe() -> Str {\n    kx_haiku(\"t\")\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let bytes = crate::kx::encode(&module);
        let decoded = crate::kx::decode(&bytes).expect("decodes");
        assert_eq!(module.ai_funs, decoded.ai_funs);
        let mut vm = Vm::new(&decoded);
        let v = vm.call_named("probe", vec![]).expect("runs");
        assert_eq!(v.to_string(), "one two three");
    }

    #[test]
    fn diff_ai_fun_tool_loop() {
        // Scripted mock: two tool calls, then a final answer built from them.
        std::env::set_var(
            "KUPL_AI_MOCK_DIFF_ASSIST",
            "[{\"tool\":\"diff_add\",\"input\":{\"a\":2,\"b\":3}},\
{\"tool\":\"diff_greet\",\"input\":{\"who\":\"Ada\"}},\
{\"final\":\"done\"}]",
        );
        let src = "fun diff_add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
fun diff_greet(who: Str) -> Str {\n    \"hi {who}\"\n}\n\
ai fun diff_assist(q: Str) -> Str tools [diff_add, diff_greet] {\n    intent \"Assist.\"\n}\n\
fun probe() -> Str {\n    diff_assist(\"x\")\n}\n";
        assert_eq!(differential(src), "done");
    }

    // helper: source that builds List[Int] = [0, 1, …, n-1] via a loop
    #[cfg(test)]
    const MK: &str = "fun mk(n: Int) -> List[Int] {\n    \
                      var xs: List[Int] = []\n    var i = 0\n    \
                      while i < n {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    xs\n}\n";

    #[test]
    fn diff_par_map_pure_it33() {
        // A pure named fn over a list crossing the 256 threshold takes the
        // real-thread path in the interpreter; the KVM computes it sequentially.
        // The differential assert proves the parallel result is byte-identical.
        let src = format!(
            "{MK}fun dbl(n: Int) -> Int {{\n    n * 2 + 1\n}}\n\
             fun probe() -> Int {{\n    mk(1000).par_map(dbl).sum()\n}}\n"
        );
        // sum of (2i+1) for i in 0..1000 = 2*(0+..+999) + 1000 = 999000 + 1000
        assert_eq!(differential(&src), "1000000");

        // heavier pure fn (a loop) over a big list — actually exercises workers
        let heavy = format!(
            "{MK}fun work(n: Int) -> Int {{\n    var acc = 0\n    \
             for i in 0..100 {{\n        acc = acc + (n % (i + 1))\n    }}\n    acc\n}}\n\
             fun probe() -> Int {{\n    mk(500).par_map(work).sum()\n}}\n"
        );
        // differential() asserts interp==KVM internally
        let _ = differential(&heavy);

        // ordering: probe RETURNS the mapped list, so its string encodes order —
        // the differential assert catches any mis-ordering of the parallel result
        let ordered = format!(
            "{MK}fun tag(n: Int) -> Int {{\n    n * 1000 + n\n}}\n\
             fun probe() -> List[Int] {{\n    mk(300).par_map(tag)\n}}\n"
        );
        let s = differential(&ordered);
        assert!(s.starts_with("[0, 1001, 2002,"), "ordered head: {s}");
        assert!(s.ends_with("298298, 299299]"), "ordered tail: {s}");
    }

    // Run `probe` on the KVM WITH the parallel image set (so par_map/par_filter
    // take the real-thread path on the VM). Asserting ABSOLUTE expected values
    // anchors correctness even though both engines now parallelize.
    #[cfg(test)]
    fn vm_parallel(src: &str) -> String {
        let compiled = crate::run::compile(src).expect("program must compile");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut vm = Vm::new(&module);
        vm.set_image(crate::parallel::ProgramImage::from_db(&db));
        match vm.call_named("probe", vec![]) {
            Ok(v) => v.to_string(),
            Err(e) => format!("panic: {}", e.msg),
        }
    }

    #[test]
    fn vm_parallel_par_map_absolute_it35() {
        // par_map(dbl) over [0..1000) on the KVM's real-thread path, summed
        let src = format!(
            "{MK}fun dbl(n: Int) -> Int {{\n    n * 2 + 1\n}}\n\
             fun probe() -> Int {{\n    mk(1000).par_map(dbl).sum()\n}}\n"
        );
        assert_eq!(vm_parallel(&src), "1000000");

        // par_filter(is_even) over [0..600) returns the exact even list, in order
        let evens = format!(
            "{MK}fun is_even(n: Int) -> Bool {{\n    n % 2 == 0\n}}\n\
             fun probe() -> List[Int] {{\n    mk(600).par_filter(is_even)\n}}\n"
        );
        let s = vm_parallel(&evens);
        assert!(s.starts_with("[0, 2, 4,") && s.ends_with("596, 598]"), "vm evens: {s}");

        // a panicking pure element reports the lowest-index panic on the VM too
        let boom = format!(
            "{MK}fun bad(n: Int) -> Int {{\n    100 / (n - 300)\n}}\n\
             fun probe() -> Int {{\n    mk(400).par_map(bad).sum()\n}}\n"
        );
        assert_eq!(vm_parallel(&boom), "panic: division by zero");
    }

    #[test]
    fn diff_par_filter_pure_it34() {
        // pure predicate over a list crossing 256 takes the real-thread path in
        // the interpreter; the KVM filters sequentially. probe RETURNS the
        // filtered list, so its string encodes both selection AND order.
        let evens = format!(
            "{MK}fun is_even(n: Int) -> Bool {{\n    n % 2 == 0\n}}\n\
             fun probe() -> List[Int] {{\n    mk(600).par_filter(is_even)\n}}\n"
        );
        let s = differential(&evens);
        assert!(s.starts_with("[0, 2, 4,"), "evens head: {s}");
        assert!(s.ends_with("596, 598]"), "evens tail: {s}");

        // sparse selection: keep multiples of 100 → order + selection correctness
        let sparse = format!(
            "{MK}fun keep(n: Int) -> Bool {{\n    n % 100 == 0\n}}\n\
             fun probe() -> List[Int] {{\n    mk(500).par_filter(keep)\n}}\n"
        );
        assert_eq!(differential(&sparse), "[0, 100, 200, 300, 400]");

        // count survivors (aggregate) — crosses threshold, pure predicate
        let count = format!(
            "{MK}fun big(n: Int) -> Bool {{\n    n >= 250\n}}\n\
             fun probe() -> Int {{\n    mk(300).par_filter(big).len()\n}}\n"
        );
        assert_eq!(differential(&count), "50"); // 250..=299
    }

    #[test]
    fn diff_par_filter_falls_back_it34() {
        // closure predicate cannot take the thread path; still identical
        let lambda = format!(
            "{MK}fun probe() -> Int {{\n    mk(400).par_filter(fn n {{ n % 3 == 0 }}).len()\n}}\n"
        );
        // multiples of 3 in 0..400: 0,3,…,399 → 134
        assert_eq!(differential(&lambda), "134");

        // below threshold stays sequential; identical
        let small = format!(
            "{MK}fun odd(n: Int) -> Bool {{\n    n % 2 == 1\n}}\n\
             fun probe() -> List[Int] {{\n    mk(6).par_filter(odd)\n}}\n"
        );
        assert_eq!(differential(&small), "[1, 3, 5]");
    }

    #[test]
    fn diff_par_map_impure_stays_sequential_it33() {
        // a closure (non-named) callback cannot take the thread path — but the
        // OUTPUT must still be identical interp vs KVM (sequential fallback).
        let lambda = format!(
            "{MK}fun probe() -> Int {{\n    mk(400).par_map(fn n {{ n + 1 }}).sum()\n}}\n"
        );
        // sum of (i+1) for i in 0..400 = (0+..+399) + 400 = 79800 + 400 = 80200
        assert_eq!(differential(&lambda), "80200");

        // below threshold stays sequential; still identical
        let small = format!(
            "{MK}fun dbl(n: Int) -> Int {{\n    n * 2\n}}\n\
             fun probe() -> Int {{\n    mk(10).par_map(dbl).sum()\n}}\n"
        );
        assert_eq!(differential(&small), "90");
    }

    #[test]
    fn diff_sized_methods_it29() {
        // wrapping
        assert_eq!(differential("fun probe() -> u8 {\n    (200u8).wrapping_add(100u8)\n}\n"), "44");
        assert_eq!(differential("fun probe() -> u8 {\n    (0u8).wrapping_sub(1u8)\n}\n"), "255");
        assert_eq!(differential("fun probe() -> i8 {\n    (127i8).wrapping_add(1i8)\n}\n"), "-128");
        // saturating
        assert_eq!(differential("fun probe() -> u8 {\n    (200u8).saturating_add(100u8)\n}\n"), "255");
        assert_eq!(differential("fun probe() -> u8 {\n    (0u8).saturating_sub(5u8)\n}\n"), "0");
        assert_eq!(differential("fun probe() -> i8 {\n    (100i8).saturating_mul(2i8)\n}\n"), "127");
        // bitwise
        assert_eq!(differential("fun probe() -> u8 {\n    (0xF0u8).band(0x0Fu8)\n}\n"), "0");
        assert_eq!(differential("fun probe() -> u8 {\n    (0xF0u8).bor(0x0Fu8)\n}\n"), "255");
        assert_eq!(differential("fun probe() -> u8 {\n    (5u8).bnot()\n}\n"), "250");
        assert_eq!(differential("fun probe() -> u8 {\n    (1u8).shl(4)\n}\n"), "16");
        assert_eq!(differential("fun probe() -> u8 {\n    (255u8).shr(4)\n}\n"), "15");
        assert_eq!(differential("fun probe() -> i8 {\n    (0i8 - 2i8).shr(1)\n}\n"), "-1");
        // conversion matrix
        assert_eq!(differential("fun probe() -> u16 {\n    (200u8).to_u16()\n}\n"), "200");
        assert_eq!(
            differential("fun probe() -> u8 {\n    (300u16).to_u8()\n}\n"),
            "panic: 300 out of range for `u8`"
        );
        assert_eq!(
            differential("fun probe() -> u8 {\n    (0i32 - 1i32).to_u8()\n}\n"),
            "panic: -1 out of range for `u8`"
        );
        // shift out of range panics
        assert_eq!(
            differential("fun probe() -> u8 {\n    (1u8).shl(8)\n}\n"),
            "panic: shift amount must be in 0..=7"
        );
    }

    #[test]
    fn diff_f32_it28() {
        assert_eq!(differential("fun probe() -> f32 {\n    1.5f32 + 2.0f32\n}\n"), "3.5");
        assert_eq!(differential("fun probe() -> f32 {\n    1.0f32\n}\n"), "1.0");
        assert_eq!(differential("fun probe() -> f32 {\n    2.0f32 * 3.0f32\n}\n"), "6.0");
        assert_eq!(differential("fun probe() -> f32 {\n    10.0f32 / 4.0f32\n}\n"), "2.5");
        assert_eq!(differential("fun probe() -> Bool {\n    1.0f32 < 2.0f32\n}\n"), "true");
        assert_eq!(differential("fun probe() -> Float {\n    (3.5f32).to_float()\n}\n"), "3.5");
        // integer-bodied f32 literal, and f32 rounding
        assert_eq!(differential("fun probe() -> f32 {\n    10f32\n}\n"), "10.0");
        assert_eq!(differential("fun probe() -> f32 {\n    (3.14).to_f32()\n}\n"), "3.14");
    }

    #[test]
    fn f32_float_mix_is_type_error_it28() {
        let (_, diags) = crate::check::check(&crate::parser::parse("fun f() {\n    let x = 1.0f32 + 2.0\n}\n").0);
        assert!(diags.iter().any(|d| d.code == "K0200"), "{diags:?}");
    }

    #[test]
    fn f32_native_compiles_it42() {
        // f32 now compiles to native (it42) — emit_c succeeds. (Runtime byte-
        // identity vs the interpreter is covered by the cc-guarded cgen tests.)
        let compiled = crate::run::compile("fun main() {\n    let x = 22.0f32 / 7.0f32\n    let _ = x\n}\n")
            .expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        assert!(crate::cgen::emit_c(&module).is_ok(), "native should compile f32 now");
    }

    #[test]
    fn diff_sized_ints_it27() {
        // arithmetic within a width, checked, byte-identical on both engines
        assert_eq!(differential("fun probe() -> u8 {\n    200u8 + 55u8\n}\n"), "255");
        assert_eq!(differential("fun probe() -> i16 {\n    1000i16\n}\n"), "1000");
        assert_eq!(differential("fun probe() -> i32 {\n    100i32 * 3i32\n}\n"), "300");
        assert_eq!(differential("fun probe() -> Bool {\n    10u8 < 20u8\n}\n"), "true");
        // hex literal with a width suffix
        assert_eq!(differential("fun probe() -> u8 {\n    0xFFu8\n}\n"), "255");
        // overflow panics with the shared Int message
        assert_eq!(
            differential("fun probe() -> u8 {\n    200u8 + 100u8\n}\n"),
            "panic: integer overflow in addition"
        );
        assert_eq!(
            differential("fun probe() -> i8 {\n    127i8 + 1i8\n}\n"),
            "panic: integer overflow in addition"
        );
        assert_eq!(
            differential("fun probe() -> i32 {\n    1000000i32 * 1000000i32\n}\n"),
            "panic: integer overflow in multiplication"
        );
        // conversions
        assert_eq!(differential("fun probe() -> Int {\n    (255u8).to_int()\n}\n"), "255");
        assert_eq!(differential("fun probe() -> u16 {\n    (65535).to_u16()\n}\n"), "65535");
        assert_eq!(
            differential("fun probe() -> u8 {\n    (300).to_u8()\n}\n"),
            "panic: 300 out of range for `u8`"
        );
    }

    #[test]
    fn value_enum_did_not_grow_it27() {
        // The baseline Value is 32 bytes (max variant Ctor = 3 pointers = 24,
        // plus an 8-byte discriminant — there is no niche, since Int(i64)/Range
        // use every bit). Sized ints box their (i128, IntW) payload, so adding
        // them does NOT grow the enum past that baseline.
        assert!(
            std::mem::size_of::<Value>() <= 32,
            "Value grew to {} bytes",
            std::mem::size_of::<Value>()
        );
    }

    #[test]
    fn mixed_width_is_type_error_it27() {
        let (_, diags) = crate::check::check(&crate::parser::parse("fun f() {\n    let x = 1i32 + 2i16\n}\n").0);
        assert!(diags.iter().any(|d| d.code == "K0200"), "{diags:?}");
    }

    #[test]
    fn sized_int_native_compiles_it40() {
        // sized ints now compile to native (it40) — emit_c succeeds. (Runtime
        // byte-identity vs the interpreter is covered by the cc-guarded tests in
        // cgen.rs; here we just confirm the backend no longer defers.)
        let compiled =
            crate::run::compile("fun main() {\n    let x = 200u8 + 55u8\n    let _ = x\n}\n")
                .expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        assert!(crate::cgen::emit_c(&module).is_ok(), "native should compile sized ints now");
    }

    #[test]
    fn diff_url_it26() {
        assert_eq!(differential("fun probe() -> Str {\n    url_encode(\"a b&c\")\n}\n"), "a%20b%26c");
        assert_eq!(differential("fun probe() -> Str {\n    url_encode(\"a-b_c.d~e\")\n}\n"), "a-b_c.d~e");
        // decode incl. + as space and %XX
        assert_eq!(
            differential("fun probe() -> Str {\n    match url_decode(\"a+b%26c\") {\n        Ok(s) => s\n        Err(e) => e\n    }\n}\n"),
            "a b&c"
        );
        // round-trip incl. unicode
        assert_eq!(
            differential("fun probe() -> Bool {\n    match url_decode(url_encode(\"π≈3.14 x/y\")) {\n        Ok(s) => s == \"π≈3.14 x/y\"\n        Err(_) => false\n    }\n}\n"),
            "true"
        );
        // malformed escape → Err
        assert_eq!(
            differential("fun probe() -> Bool {\n    match url_decode(\"%2\") {\n        Ok(_) => false\n        Err(_) => true\n    }\n}\n"),
            "true"
        );
        // query build + parse round-trip
        assert_eq!(
            differential("fun probe() -> Str {\n    query_build([[\"n\", \"A B\"], [\"r\", \"x+y\"]])\n}\n"),
            "n=A%20B&r=x%2By"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    let q = query_build([[\"n\", \"A B\"], [\"r\", \"x+y\"]])\n    query_parse(q) == [[\"n\", \"A B\"], [\"r\", \"x+y\"]]\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_csv_it25() {
        // parse into rows × fields
        assert_eq!(
            differential("fun probe() -> Int {\n    csv_parse(\"a,b,c\\n1,2,3\").len()\n}\n"),
            "2"
        );
        // quoted field with an embedded comma
        assert_eq!(
            differential("fun probe() -> Str {\n    match csv_parse(\"\\\"a,b\\\",c\").first() {\n        Some(row) => row.join(\"|\")\n        None => \"none\"\n    }\n}\n"),
            "a,b|c"
        );
        // stringify quotes a field with a comma
        assert_eq!(
            differential("fun probe() -> Str {\n    csv_stringify([[\"x,y\", \"z\"]])\n}\n"),
            "\"x,y\",z"
        );
        // round-trip stability, including embedded newline + doubled quote
        assert_eq!(
            differential("fun probe() -> Bool {\n    let rows = csv_parse(\"a,\\\"b\\nc\\\"\\n\\\"he \\\"\\\"q\\\"\\\"\\\",d\")\n    csv_parse(csv_stringify(rows)) == rows\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_numeric_formatting_it24() {
        // Int radix formatting (lowercase, sign on the magnitude)
        assert_eq!(differential("fun probe() -> Str {\n    (255).to_hex()\n}\n"), "ff");
        assert_eq!(differential("fun probe() -> Str {\n    (255).to_binary()\n}\n"), "11111111");
        assert_eq!(differential("fun probe() -> Str {\n    (64).to_octal()\n}\n"), "100");
        assert_eq!(differential("fun probe() -> Str {\n    (0 - 255).to_hex()\n}\n"), "-ff");
        assert_eq!(differential("fun probe() -> Str {\n    (0).to_hex()\n}\n"), "0");
        assert_eq!(differential("fun probe() -> Str {\n    (1000).to_radix(36)\n}\n"), "rs");
        // to_radix out-of-range panics identically
        assert_eq!(
            differential("fun probe() -> Str {\n    (5).to_radix(40)\n}\n"),
            "panic: `to_radix` base must be in 2..=36"
        );
        // isqrt
        assert_eq!(differential("fun probe() -> Int {\n    (144).isqrt()\n}\n"), "12");
        assert_eq!(differential("fun probe() -> Int {\n    (145).isqrt()\n}\n"), "12");
        assert_eq!(differential("fun probe() -> Int {\n    (0).isqrt()\n}\n"), "0");
        assert_eq!(
            differential("fun probe() -> Int {\n    (0 - 4).isqrt()\n}\n"),
            "panic: `isqrt` of a negative Int"
        );
        // Float.format at several precisions (round-half-to-even, both sides)
        assert_eq!(differential("fun probe() -> Str {\n    (3.14159).format(2)\n}\n"), "3.14");
        assert_eq!(differential("fun probe() -> Str {\n    (2.5).format(0)\n}\n"), "2");
        assert_eq!(differential("fun probe() -> Str {\n    (1.0).format(3)\n}\n"), "1.000");
        // transcendentals
        assert_eq!(differential("fun probe() -> Float {\n    (3.0).hypot(4.0)\n}\n"), "5.0");
        assert_eq!(differential("fun probe() -> Float {\n    (8.0).log2()\n}\n"), "3.0");
        assert_eq!(differential("fun probe() -> Float {\n    (27.0).cbrt()\n}\n"), "3.0");
    }

    #[test]
    fn diff_encoding_it23() {
        // known vectors, identical on both engines
        assert_eq!(differential("fun probe() -> Str {\n    base64_encode(\"hello\")\n}\n"), "aGVsbG8=");
        assert_eq!(differential("fun probe() -> Str {\n    hex_encode(\"AB\")\n}\n"), "4142");
        // round-trips
        assert_eq!(
            differential("fun probe() -> Bool {\n    match base64_decode(base64_encode(\"the quick brown fox\")) {\n        Ok(s) => s == \"the quick brown fox\"\n        Err(_) => false\n    }\n}\n"),
            "true"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    match hex_decode(hex_encode(\"KUPL\")) {\n        Ok(s) => s == \"KUPL\"\n        Err(_) => false\n    }\n}\n"),
            "true"
        );
        // FNV is stable and equal across engines
        assert_eq!(differential("fun probe() -> Int {\n    hash_fnv(\"foobar\")\n}\n"), (0x85944171f73967e8u64 as i64).to_string());
        assert_eq!(differential("fun probe() -> Bool {\n    hash_fnv(\"a\") == hash_fnv(\"a\")\n}\n"), "true");
        // invalid input → Err on both engines
        assert_eq!(
            differential("fun probe() -> Bool {\n    match hex_decode(\"zz\") {\n        Ok(_) => false\n        Err(_) => true\n    }\n}\n"),
            "true"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    match base64_decode(\"abc\") {\n        Ok(_) => false\n        Err(_) => true\n    }\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_time_it22() {
        // fixed epochs → deterministic UTC strings, identical on both engines
        assert_eq!(differential("fun probe() -> Str {\n    format_time(0)\n}\n"), "1970-01-01 00:00:00");
        assert_eq!(
            differential("fun probe() -> Str {\n    format_time(1000000000)\n}\n"),
            "2001-09-09 01:46:40"
        );
        // negative (pre-1970) epoch, floor-division correct
        assert_eq!(differential("fun probe() -> Str {\n    format_time(0 - 1)\n}\n"), "1969-12-31 23:59:59");
        // component extractors
        assert_eq!(differential("fun probe() -> Int {\n    year_of(1000000000)\n}\n"), "2001");
        assert_eq!(differential("fun probe() -> Int {\n    month_of(1000000000)\n}\n"), "9");
        assert_eq!(differential("fun probe() -> Int {\n    day_of(1000000000)\n}\n"), "9");
        assert_eq!(differential("fun probe() -> Int {\n    hour_of(1000000000)\n}\n"), "1");
        assert_eq!(differential("fun probe() -> Int {\n    weekday_of(0)\n}\n"), "4");
        // now() is nondeterministic, so only assert it's a plausible Int
        assert_eq!(differential("fun probe() -> Bool {\n    now() > 1700000000\n}\n"), "true");
    }

    #[test]
    fn diff_regex_it20() {
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"^\\\\d+$\", \"12345\")\n}\n"), "true");
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"^\\\\d+$\", \"12a45\")\n}\n"), "false");
        assert_eq!(
            differential("fun probe() -> List[Str] {\n    re_find_all(\"\\\\d+\", \"a1b22c333\")\n}\n"),
            "[\"1\", \"22\", \"333\"]"
        );
        assert_eq!(
            differential("fun probe() -> Str {\n    re_replace(\"\\\\s+\", \"a  b c\", \"_\")\n}\n"),
            "a_b_c"
        );
        // alternation + groups + quantifiers
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"^(cat|dog)s?$\", \"dogs\")\n}\n"), "true");
        assert_eq!(differential("fun probe() -> Bool {\n    re_match(\"^(ab)+$\", \"ababab\")\n}\n"), "true");
        // find returns the first match substring
        assert_eq!(
            differential("fun probe() -> Str {\n    match re_find(\"[a-z]+\", \"123abc456\") {\n        Some(m) => m\n        None => \"none\"\n    }\n}\n"),
            "abc"
        );
        // a malformed pattern panics identically on both engines
        assert_eq!(
            differential("fun probe() -> Bool {\n    re_match(\"(unclosed\", \"x\")\n}\n"),
            "panic: invalid regex: unclosed group `(`"
        );
    }

    #[test]
    fn diff_http_err_path_it19() {
        // deterministic, network-free: nothing listens on 127.0.0.1:9, so the
        // request fails and both engines observe an Err (message text may vary
        // by platform, so only the Ok/Err structure is asserted)
        let src = "fun probe() -> Bool {\n\
            match http_get(\"http://127.0.0.1:9/\") {\n\
                Ok(_) => false\n\
                Err(_) => true\n\
            }\n\
        }\n";
        assert_eq!(differential(src), "true");
    }

    #[test]
    fn diff_seeded_random_it18() {
        // a fixed seed yields an exact, reproducible sequence on both engines
        assert_eq!(
            differential("fun probe() -> List[Int] {\n    random_ints(42, 3)\n}\n"),
            "[6255019084209693600, -4016670646968046118, -3871288216479333770]"
        );
        // floats land in [0, 1) and match byte-for-byte across engines
        assert_eq!(
            differential("fun probe() -> Bool {\n    random_floats(42, 100).all(fn f { f >= 0.0 && f < 1.0 })\n}\n"),
            "true"
        );
        // shuffle is a permutation and deterministic for a given seed
        assert_eq!(
            differential("fun probe() -> List[Int] {\n    shuffle(7, [1, 2, 3, 4, 5, 6])\n}\n"),
            "[6, 2, 1, 3, 4, 5]"
        );
        // shuffle is generic over element type
        assert_eq!(
            differential("fun probe() -> List[Str] {\n    shuffle(7, [\"a\", \"b\", \"c\", \"d\"])\n}\n"),
            "[\"b\", \"a\", \"d\", \"c\"]"
        );
        // same seed → same output; different seeds differ
        assert_eq!(
            differential("fun probe() -> Bool {\n    random_ints(42, 8) == random_ints(42, 8)\n}\n"),
            "true"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    random_ints(1, 8) == random_ints(2, 8)\n}\n"),
            "false"
        );
        // count <= 0 → empty
        assert_eq!(differential("fun probe() -> Int {\n    random_ints(9, 0 - 3).len()\n}\n"), "0");
    }

    #[test]
    fn diff_bitwise_it17() {
        assert_eq!(differential("fun probe() -> Int {\n    (12).band(10)\n}\n"), "8");
        assert_eq!(differential("fun probe() -> Int {\n    (12).bor(10)\n}\n"), "14");
        assert_eq!(differential("fun probe() -> Int {\n    (12).bxor(10)\n}\n"), "6");
        assert_eq!(differential("fun probe() -> Int {\n    (0).bnot()\n}\n"), "-1");
        assert_eq!(differential("fun probe() -> Int {\n    (1).shl(8)\n}\n"), "256");
        assert_eq!(differential("fun probe() -> Int {\n    (256).shr(2)\n}\n"), "64");
        // arithmetic vs logical right shift differ on negatives
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 8).shr(1)\n}\n"), "-4");
        assert_eq!(
            differential("fun probe() -> Int {\n    (0 - 8).ushr(1)\n}\n"),
            "9223372036854775804"
        );
        // out-of-range shift panics identically on both engines
        assert_eq!(
            differential("fun probe() -> Int {\n    (1).shl(64)\n}\n"),
            "panic: shift amount must be in 0..=63"
        );
    }

    #[test]
    fn diff_int_literal_forms_it17() {
        assert_eq!(differential("fun probe() -> Int {\n    0xFF\n}\n"), "255");
        assert_eq!(differential("fun probe() -> Int {\n    0b1010\n}\n"), "10");
        assert_eq!(differential("fun probe() -> Int {\n    0xDEAD_BEEF\n}\n"), "3735928559");
        assert_eq!(differential("fun probe() -> Int {\n    1_000_000\n}\n"), "1000000");
        // full 64-bit hex pattern reinterpreted as i64
        assert_eq!(differential("fun probe() -> Int {\n    0xFFFFFFFFFFFFFFFF\n}\n"), "-1");
    }

    #[test]
    fn diff_env_var_it16() {
        // deterministic env read: a fixed set variable is Some on both engines,
        // an unset one is None. (args()/exit are covered by CLI-level checks,
        // since the in-process test harness has nondeterministic argv.)
        std::env::set_var("KUPL_DIFFTEST_IT16", "present");
        assert_eq!(
            differential("fun probe() -> Str {\n    match env_var(\"KUPL_DIFFTEST_IT16\") {\n        Some(v) => v\n        None => \"missing\"\n    }\n}\n"),
            "present"
        );
        assert_eq!(
            differential("fun probe() -> Bool {\n    match env_var(\"KUPL_DEFINITELY_UNSET_XYZ_IT16\") {\n        Some(_) => true\n        None => false\n    }\n}\n"),
            "false"
        );
    }

    #[test]
    fn diff_json_it15() {
        // parse → stringify round-trips, key order preserved, ints without `.0`;
        // interpreter and KVM must agree byte-for-byte (both use crate::json)
        assert_eq!(
            differential("fun probe() -> Str {\n    match json_parse(\"[1, 2, 3]\") {\n        Ok(j) => json_stringify(j)\n        Err(e) => e\n    }\n}\n"),
            "[1,2,3]"
        );
        // build programmatically → stringify (no literal braces in source)
        assert_eq!(
            differential("fun probe() -> Str {\n    json_stringify(JObj(Map().insert(\"a\", JNum(1.0)).insert(\"b\", JBool(true))))\n}\n"),
            "{\"a\":1,\"b\":true}"
        );
        // nested round-trip stability: stringify(parse(s)) == s
        assert_eq!(
            differential("fun probe() -> Bool {\n    let s = json_stringify(JArr([JNull, JStr(\"x\"), JNum(2.5)]))\n    match json_parse(s) {\n        Ok(j) => json_stringify(j) == s\n        Err(_) => false\n    }\n}\n"),
            "true"
        );
        // matching drives structural inspection
        assert_eq!(
            differential("fun probe() -> Int {\n    match json_parse(\"[10, 20, 30, 40]\") {\n        Ok(JArr(xs)) => xs.len()\n        Ok(_) => 0\n        Err(_) => 0 - 1\n    }\n}\n"),
            "4"
        );
    }

    #[test]
    fn diff_json_malformed_is_err_it15() {
        assert_eq!(
            differential("fun probe() -> Bool {\n    match json_parse(\"not json\") {\n        Ok(_) => false\n        Err(_) => true\n    }\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_file_io_roundtrip_it14() {
        // write → exists → read → append → delete → gone, all via a fixed temp
        // path; interpreter and KVM must agree byte-for-byte (both use fs_builtin)
        let src = "fun probe() -> Str {\n\
            let p = \"/tmp/kupl_difftest_it14.txt\"\n\
            let _ = write_file(p, \"alpha\\nbeta\")\n\
            let exists = file_exists(p)\n\
            let n = match read_file(p) {\n\
                Ok(c) => c.lines().len()\n\
                Err(_) => 0 - 1\n\
            }\n\
            let _ = append_file(p, \"\\ngamma\")\n\
            let n2 = match read_file(p) {\n\
                Ok(c) => c.lines().len()\n\
                Err(_) => 0 - 1\n\
            }\n\
            let _ = delete_file(p)\n\
            let gone = file_exists(p)\n\
            \"exists={exists} n={n} n2={n2} gone={gone}\"\n\
        }\n";
        assert_eq!(differential(src), "exists=true n=2 n2=3 gone=false");
    }

    #[test]
    fn diff_file_read_missing_is_err_it14() {
        // reading a missing file yields Err on both engines (message text may
        // vary by platform, so we only observe the Ok/Err structure)
        let src = "fun probe() -> Bool {\n\
            match read_file(\"/nonexistent/kupl/xyz\") {\n\
                Ok(_) => false\n\
                Err(_) => true\n\
            }\n\
        }\n";
        assert_eq!(differential(src), "true");
    }

    #[test]
    fn diff_parallel_iteration_it13() {
        // par_map / par_filter / par_each — deterministic, identical on both engines
        assert_eq!(
            differential("fun probe() -> List[Int] {\n    [1, 2, 3, 4].par_map(fn n { n * n })\n}\n"),
            "[1, 4, 9, 16]"
        );
        assert_eq!(
            differential("fun probe() -> List[Int] {\n    [1, 2, 3, 4, 5].par_filter(fn n { n % 2 == 1 })\n}\n"),
            "[1, 3, 5]"
        );
        // par_each returns Unit; results collected via a fold to observe order
        assert_eq!(
            differential("fun probe() -> Str {\n    let r = [\"a\", \"b\", \"c\"].par_map(fn s { s.to_upper() })\n    r.join(\"-\")\n}\n"),
            "A-B-C"
        );
        // par_map matches map exactly (same semantics, deterministic order)
        assert_eq!(
            differential("fun probe() -> Bool {\n    let xs = [5, 3, 8, 1]\n    xs.par_map(fn n { n + 1 }) == xs.map(fn n { n + 1 })\n}\n"),
            "true"
        );
    }

    #[test]
    fn diff_stdlib_batch_it12() {
        // new List/Str/Int/Float/Map/Set methods, identical on both engines
        assert_eq!(differential("fun probe() -> List[Int] {\n    [3, 1, 2, 3, 1].unique()\n}\n"), "[3, 1, 2]");
        assert_eq!(differential("fun probe() -> Int {\n    [2, 3, 4].product()\n}\n"), "24");
        assert_eq!(differential("fun probe() -> Option[Int] {\n    [4, 1, 3].min()\n}\n"), "Some(1)");
        assert_eq!(differential("fun probe() -> Option[Int] {\n    [4, 1, 3].max()\n}\n"), "Some(4)");
        assert_eq!(differential("fun probe() -> List[Int] {\n    [[1, 2], [3]].flatten()\n}\n"), "[1, 2, 3]");
        assert_eq!(differential("fun probe() -> Int {\n    [1, 2, 3, 4].count(fn n { n % 2 == 0 })\n}\n"), "2");
        assert_eq!(differential("fun probe() -> List[Int] {\n    [1, 2, 3].flat_map(fn n { [n, n] })\n}\n"), "[1, 1, 2, 2, 3, 3]");
        assert_eq!(differential("fun probe() -> Int {\n    [1, 2, 3, 4, 5].window(2).len()\n}\n"), "4");
        assert_eq!(differential("fun probe() -> Int {\n    [1, 2, 3, 4, 5].chunk(2).len()\n}\n"), "3");
        assert_eq!(differential("fun probe() -> Str {\n    \"ab\".pad_left(4, \"0\")\n}\n"), "00ab");
        assert_eq!(differential("fun probe() -> Str {\n    \"hello\".reverse()\n}\n"), "olleh");
        assert_eq!(differential("fun probe() -> Option[Int] {\n    \"hello\".index_of(\"ll\")\n}\n"), "Some(2)");
        assert_eq!(differential("fun probe() -> Str {\n    \"hello\".slice(1, 4)\n}\n"), "ell");
        assert_eq!(differential("fun probe() -> Int {\n    (12).gcd(18)\n}\n"), "6");
        assert_eq!(differential("fun probe() -> Int {\n    (2).pow(10)\n}\n"), "1024");
        assert_eq!(differential("fun probe() -> Int {\n    (7).clamp(0, 5)\n}\n"), "5");
        assert_eq!(differential("fun probe() -> Bool {\n    (10).is_even()\n}\n"), "true");
        assert_eq!(differential("fun probe() -> Int {\n    (0 - 3).sign()\n}\n"), "-1");
        assert_eq!(differential("fun probe() -> Float {\n    (100.0).clamp(0.0, 50.0)\n}\n"), "50.0");
        assert_eq!(differential("fun probe() -> Int {\n    Map().insert(\"a\", 1).get_or(\"z\", 99)\n}\n"), "99");
        assert_eq!(differential("fun probe() -> Bool {\n    Set([1, 2]).is_subset(Set([1, 2, 3]))\n}\n"), "true");
    }

    #[test]
    fn stdlib_batch_it12_overflow_panics() {
        // product overflows checked-int → panic, same as sum
        assert_eq!(
            differential("fun probe() -> Int {\n    [9223372036854775807, 2].product()\n}\n"),
            "panic: integer overflow in product"
        );
    }

    #[test]
    fn diff_par_fork_join() {
        // structured fork-join: independent branches collected into a list,
        // deterministic branch order, identical on both engines
        assert_eq!(
            differential("fun sq(n: Int) -> Int {\n    n * n\n}\nfun probe() -> Int {\n    par { sq(2)  sq(3)  sq(4) }.sum()\n}\n"),
            "29"
        );
        // par yields a list in branch order
        assert_eq!(
            differential("fun probe() -> List[Int] {\n    par { 1  1 + 1  1 + 2 }\n}\n"),
            "[1, 2, 3]"
        );
    }

    #[test]
    fn par_branches_must_agree_in_type() {
        let src = "fun probe() {\n    let _ = par { 1  \"two\" }\n}\n";
        let (_, diags) = crate::check::check(&crate::parser::parse(src).0);
        assert!(diags.iter().any(|d| d.code == "K0200"), "{diags:?}");
    }

    #[test]
    fn diff_par_over_ai_fun_fanout() {
        // the payoff use case: fan out independent ai fun calls in parallel
        std::env::set_var("KUPL_AI_MOCK_PAR_LABEL", "yes");
        let src = "ai fun par_label(x: Str) -> Str {\n    intent \"Label {x}\"\n}\n\
fun probe() -> Str {\n    par { par_label(\"a\")  par_label(\"b\") }.join(\",\")\n}\n";
        assert_eq!(differential(src), "yes,yes");
    }

    #[test]
    fn diff_timers_fire_identically_under_advance() {
        // A recurring and a one-shot timer; drive the virtual clock on both
        // engines and assert identical timer-driven emissions.
        let src = "component T {\n    intent \"timers\"\n    out tick: Int\n    out ready: Int\n    state n: Int = 0\n\
    on every 5s {\n        n += 1\n        emit tick(n)\n    }\n\
    on after 2s {\n        emit ready(1)\n    }\n\
    expose fun ticks() -> Int {\n        n\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");

        // interpreter
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut it = Interp::new(db);
        let iid = match it.instantiate("T", &[], crate::diag::Span::default()) {
            Ok(Value::Component(id)) => id,
            _ => panic!("inst"),
        };
        it.start_all().ok();
        assert!(it.advance(12_000).is_ok()); // fires every@5,10 and after@2
        let i_ticks = {
            let f = Value::Bound(iid, std::rc::Rc::new("ticks".to_string()));
            match it.call_value(f, vec![], crate::diag::Span::default()) {
                Ok(v) => v.to_string(),
                Err(_) => panic!("ticks call failed"),
            }
        };
        let i_ready = it.instances[iid].last_emit.get("ready").cloned().unwrap().to_string();
        let i_tick = it.instances[iid].last_emit.get("tick").cloned().unwrap().to_string();

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut vm = Vm::new(&module);
        let vid = vm.instantiate_named("T", vec![]).expect("inst");
        vm.advance(12_000).expect("advance");
        let v_ticks = vm.call_expose(vid, "ticks", vec![]).unwrap().to_string();

        assert_eq!(i_ticks, v_ticks, "interpreter and KVM disagree on timer count");
        assert_eq!(i_ticks, "2"); // every 5s fired at 5s and 10s within 12s
        assert_eq!(i_ready, "1");
        assert_eq!(i_tick, "2");
    }

    #[test]
    fn timers_kx_roundtrip() {
        let src = "component T {\n    intent \"t\"\n    out tick: Int\n    state n: Int = 0\n\
    on every 3s {\n        n += 1\n        emit tick(n)\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let decoded = crate::kx::decode(&crate::kx::encode(&module)).expect("decodes");
        assert_eq!(module.components[0].timers, decoded.components[0].timers);
        assert_eq!(decoded.components[0].timers[0].interval_ms, 3000);
    }

    #[test]
    fn diff_agent_component() {
        // A stateful component whose expose calls a tool-using ai fun with an
        // interpolated intent; history persists across turns. Both engines must
        // agree, turn for turn.
        std::env::set_var(
            "KUPL_AI_MOCK_AGENT_REPLY",
            "[{\"tool\":\"agent_add\",\"input\":{\"a\":4,\"b\":6}},{\"final\":\"10\"}]",
        );
        let src = "fun agent_add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
ai fun agent_reply(history: List[Str], msg: Str) -> Str tools [agent_add] {\n    intent \"History {history}, reply to {msg}\"\n}\n\
component Bot {\n    intent \"stateful bot\"\n    state history: List[Str] = []\n\
    expose fun ask(msg: Str) uses ai -> Str {\n        let a = agent_reply(history, msg)\n        history = history.push(\"u:{msg}\").push(\"b:{a}\")\n        a\n    }\n\
    expose fun turns() -> Int {\n        history.len()\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");

        // interpreter
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        let iid = match interp.instantiate("Bot", &[], crate::diag::Span::default()) {
            Ok(Value::Component(id)) => id,
            _ => panic!("instantiate failed"),
        };
        interp.start_all().ok();
        let call_i = |it: &mut Interp, id: usize, m: &str, a: Vec<Value>| -> String {
            let f = Value::Bound(id, std::rc::Rc::new(m.to_string()));
            match it.call_value(f, a, crate::diag::Span::default()) {
                Ok(v) => v.to_string(),
                Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
                Err(_) => "flow".into(),
            }
        };
        let mut i_log = Vec::new();
        i_log.push(call_i(&mut interp, iid, "ask", vec![Value::str("4+6?")]));
        i_log.push(call_i(&mut interp, iid, "ask", vec![Value::str("thanks")]));
        i_log.push(call_i(&mut interp, iid, "turns", vec![]));

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut vm = Vm::new(&module);
        let vid = vm.instantiate_named("Bot", vec![]).expect("instantiate");
        let call_v = |vm: &mut Vm, id: usize, m: &str, a: Vec<Value>| -> String {
            match vm.call_expose(id, m, a) {
                Ok(v) => v.to_string(),
                Err(e) => format!("panic: {}", e.msg),
            }
        };
        let mut v_log = Vec::new();
        v_log.push(call_v(&mut vm, vid, "ask", vec![Value::str("4+6?")]));
        v_log.push(call_v(&mut vm, vid, "ask", vec![Value::str("thanks")]));
        v_log.push(call_v(&mut vm, vid, "turns", vec![]));

        assert_eq!(i_log, v_log, "interpreter and KVM disagree on agent component");
        assert_eq!(v_log[0], "10");
        assert_eq!(v_log[2], "4"); // 2 asks x 2 history pushes
    }

    #[test]
    fn ai_fun_tools_kx_roundtrip() {
        std::env::set_var(
            "KUPL_AI_MOCK_KX_ASSIST",
            "[{\"tool\":\"kx_add\",\"input\":{\"a\":10,\"b\":5}},{\"final\":\"ok\"}]",
        );
        let src = "fun kx_add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
ai fun kx_assist(q: Str) -> Str tools [kx_add] {\n    intent \"Assist.\"\n}\n\
fun probe() -> Str {\n    kx_assist(\"x\")\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let decoded = crate::kx::decode(&crate::kx::encode(&module)).expect("decodes");
        assert_eq!(module.ai_funs, decoded.ai_funs);
        assert_eq!(decoded.ai_funs[0].tools.len(), 1);
        let mut vm = Vm::new(&decoded);
        assert_eq!(vm.call_named("probe", vec![]).unwrap().to_string(), "ok");
    }

    #[test]
    fn diff_contract_typed_prop_dispatch() {
        // A consumer with a contract-typed prop dispatches dynamically to
        // whichever fulfilling component is injected — same on both engines.
        let src = "contract Store {\n    intent \"kv\"\n    expose fun put(k: Str, v: Str) -> Int\n    expose fun get(k: Str) -> Option[Str]\n}\n\
component Mem fulfills Store {\n    intent \"mem\"\n    state m: Map[Str, Str] = Map()\n    expose fun put(k: Str, v: Str) -> Int {\n        m = m.insert(k, v)\n        m.len()\n    }\n    expose fun get(k: Str) -> Option[Str] {\n        m.get(k)\n    }\n}\n\
component Prefix fulfills Store {\n    intent \"prefix\"\n    state m: Map[Str, Str] = Map()\n    expose fun put(k: Str, v: Str) -> Int {\n        m = m.insert(k, \"P:{v}\")\n        m.len()\n    }\n    expose fun get(k: Str) -> Option[Str] {\n        m.get(k)\n    }\n}\n\
component Cache {\n    intent \"consumer\"\n    prop store: Store\n    expose fun remember(k: Str, v: Str) -> Int {\n        store.put(k, v)\n    }\n    expose fun recall(k: Str) -> Str {\n        match store.get(k) { Some(x) => x, None => \"<miss>\" }\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");

        // interpreter
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut it = Interp::new(db);
        let store = match it.instantiate("Prefix", &[], crate::diag::Span::default()) {
            Ok(v) => v,
            _ => panic!("store"),
        };
        let cache = match it.instantiate("Cache", &[(Some("store".into()), store)], crate::diag::Span::default()) {
            Ok(Value::Component(id)) => id,
            _ => panic!("cache"),
        };
        it.start_all().ok();
        let ci = |it: &mut Interp, id: usize, m: &str, a: Vec<Value>| -> String {
            let f = Value::Bound(id, std::rc::Rc::new(m.to_string()));
            match it.call_value(f, a, crate::diag::Span::default()) {
                Ok(v) => v.to_string(),
                Err(Flow::Panic { msg, .. }) => format!("panic: {msg}"),
                Err(_) => "flow".into(),
            }
        };
        let mut ilog = Vec::new();
        ilog.push(ci(&mut it, cache, "remember", vec![Value::str("a"), Value::str("x")]));
        ilog.push(ci(&mut it, cache, "recall", vec![Value::str("a")]));

        // KVM
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut vm = Vm::new(&module);
        let vstore = vm.instantiate_named("Prefix", vec![]).expect("store");
        let vcache =
            vm.instantiate_named("Cache", vec![Value::Component(vstore)]).expect("cache");
        let cv = |vm: &mut Vm, id: usize, m: &str, a: Vec<Value>| -> String {
            match vm.call_expose(id, m, a) {
                Ok(v) => v.to_string(),
                Err(e) => format!("panic: {}", e.msg),
            }
        };
        let mut vlog = Vec::new();
        vlog.push(cv(&mut vm, vcache, "remember", vec![Value::str("a"), Value::str("x")]));
        vlog.push(cv(&mut vm, vcache, "recall", vec![Value::str("a")]));

        assert_eq!(ilog, vlog, "interpreter and KVM disagree on contract dispatch");
        assert_eq!(vlog[1], "P:x"); // dispatched to Prefix's implementation
    }

    #[test]
    fn contract_prop_rejects_non_fulfilling() {
        let src = "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Option[Str]\n}\n\
component NotAStore {\n    intent \"x\"\n    expose fun hello() -> Str { \"hi\" }\n}\n\
component Cache {\n    intent \"c\"\n    prop store: Store\n    expose fun recall(k: Str) -> Option[Str] { store.get(k) }\n}\n\
fun main() {\n    let c = Cache(store: NotAStore())\n    let _ = c\n}\n";
        let (_, diags) = crate::check::check(&crate::parser::parse(src).0);
        assert!(diags.iter().any(|d| d.code == "K0200"), "{diags:?}");
    }

    #[test]
    fn forall_property_passes_and_fails_with_shrunk_counterexample() {
        // run a top-level law body on the interpreter and inspect the outcome
        let run_law = |src: &str| -> Result<(), String> {
            let compiled = crate::run::compile(src).expect("compiles");
            let law = compiled
                .program
                .items
                .iter()
                .find_map(|i| match i {
                    crate::ast::Item::Law(l) => Some(l.clone()),
                    _ => None,
                })
                .expect("has a law");
            let db = ProgramDb::build(&compiled.program, &compiled.checked);
            let mut it = Interp::new(db);
            let env = it.globals.child();
            match it.exec_block(&law.body, &env) {
                Ok(_) => Ok(()),
                Err(Flow::Panic { msg, .. }) => Err(msg),
                Err(_) => Err("flow".into()),
            }
        };

        // a true property holds across all generated cases
        run_law("law \"comm\" {\n    forall a: Int, b: Int { expect a + b == b + a }\n}\n")
            .expect("commutativity holds");

        // a false property fails and shrinks to the minimal counterexample n = 50
        let err = run_law("law \"small\" {\n    forall n: Int { expect n < 50 }\n}\n")
            .expect_err("must fail");
        assert!(err.contains("n = 50"), "expected shrunk counterexample, got: {err}");
    }

    #[test]
    fn forall_is_rejected_by_the_kvm_compiler() {
        // forall is interpreter-only (kupl test); compiling it to the KVM errors
        let src = "fun probe() -> Int {\n    forall n: Int { expect n >= 0 }\n    0\n}\n";
        let compiled = crate::run::compile(src).expect("type-checks");
        let err = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect_err("KVM must reject forall");
        assert!(err.iter().any(|d| d.code == "K0804"), "{err:?}");
    }

    #[test]
    fn ai_fun_unknown_tool_is_rejected() {
        let src = "ai fun bad(q: Str) -> Str tools [nope] {\n    intent \"x\"\n}\n";
        let (_, diags) = crate::check::check(&crate::parser::parse(src).0);
        assert!(diags.iter().any(|d| d.code == "K0272"), "{diags:?}");
    }

    #[test]
    fn ai_fun_native_compiles_it51() {
        // ai funs now compile to native via the deterministic mock path (it51);
        // emit_c succeeds. (Byte-identity vs the interpreter under KUPL_AI_MOCK
        // is covered by the cc-guarded native_ai_mock test in cgen.rs.)
        let src = "ai fun nat_x(t: Str) -> Str {\n    intent \"X.\"\n}\n\
fun main() {\n    print(nat_x(\"t\"))\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        assert!(crate::cgen::emit_c(&module).is_ok(), "native should compile ai funs now");
    }
}
