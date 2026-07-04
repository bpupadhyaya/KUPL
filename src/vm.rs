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
        }
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
        while let Some((id, port, value)) = self.queue.pop_front() {
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
                        Err(msg) => return Err(VmError { msg, span }),
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
                Op::Add(d, a, b) => bin!(d, a, b, B::Add),
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
                        | BUILTIN_DELETE_FILE | BUILTIN_FILE_EXISTS => {
                            let name = match which {
                                BUILTIN_READ_FILE => "read_file",
                                BUILTIN_WRITE_FILE => "write_file",
                                BUILTIN_APPEND_FILE => "append_file",
                                BUILTIN_DELETE_FILE => "delete_file",
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
                        BUILTIN_ENV_VAR | BUILTIN_ARGS | BUILTIN_EPRINT => {
                            let name = match which {
                                BUILTIN_ENV_VAR => "env_var",
                                BUILTIN_ARGS => "args",
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
                        | BUILTIN_SECOND_OF | BUILTIN_WEEKDAY_OF => {
                            let name = match which {
                                BUILTIN_FORMAT_TIME => "format_time",
                                BUILTIN_YEAR_OF => "year_of",
                                BUILTIN_MONTH_OF => "month_of",
                                BUILTIN_DAY_OF => "day_of",
                                BUILTIN_HOUR_OF => "hour_of",
                                BUILTIN_MINUTE_OF => "minute_of",
                                BUILTIN_SECOND_OF => "second_of",
                                _ => "weekday_of",
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
                    let r = reg!(recv);
                    let method = match &chunk.consts[name as usize] {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => return Err(VmError { msg: "bad method name".into(), span }),
                    };
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
                    let mut call = |f: Value, args: Vec<Value>| self.call_value_nested(f, args);
                    match shared_method(&r, &method, args, &mut call) {
                        Ok(v) => set!(dst, v),
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
    fn diff_expect_stmt() {
        let src = "fun probe() -> Int {\n    expect 1 + 1 == 2\n    7\n}\n";
        assert_eq!(differential(src), "7");
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
    fn ai_fun_native_backend_rejects() {
        let src = "ai fun nat_x(t: Str) -> Str {\n    intent \"X.\"\n}\n\
fun main() {\n    print(nat_x(\"t\"))\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let err = crate::cgen::emit_c(&module).expect_err("native must reject ai funs");
        assert!(err.contains("not supported by the native backend"), "{err}");
    }
}
