//! KVM bytecode: register-based, one `Chunk` per function.
//!
//! v0.4 uses a structured `Op` enum for clarity; the packed 32-bit encoding
//! described in TOOLCHAIN.md §8 is a later, mechanical change once the op set
//! stabilizes. Registers are frame-local (max 256/frame). Jump targets are
//! absolute instruction indices, patched at compile time.

use std::fmt::Write as _;

use crate::diag::Span;
use crate::value::Value;

pub type Reg = u8;

#[derive(Debug, Clone)]
pub enum Op {
    /// dst <- consts[idx]
    Const(Reg, u16),
    Move(Reg, Reg),

    Add(Reg, Reg, Reg),
    Sub(Reg, Reg, Reg),
    Mul(Reg, Reg, Reg),
    Div(Reg, Reg, Reg),
    Rem(Reg, Reg, Reg),
    Eq(Reg, Reg, Reg),
    Ne(Reg, Reg, Reg),
    Lt(Reg, Reg, Reg),
    Le(Reg, Reg, Reg),
    Gt(Reg, Reg, Reg),
    Ge(Reg, Reg, Reg),
    Neg(Reg, Reg),
    Not(Reg, Reg),

    Jump(usize),
    JumpIfFalse(Reg, usize),
    JumpIfTrue(Reg, usize),

    /// dst <- chunks[fun](regs[start .. start+argc])
    Call { dst: Reg, fun: u16, start: Reg, argc: u8 },
    /// like Call, but the callee runs with the CURRENT instance (component funs)
    CallComp { dst: Reg, fun: u16, start: Reg, argc: u8 },
    /// dst <- builtin(regs[start .. start+argc]); 0=print 1=to_str 2=panic
    CallBuiltin { dst: Reg, which: u8, start: Reg, argc: u8 },
    /// dst <- (regs[f])(regs[start .. start+argc]) — closures, fn refs
    CallValue { dst: Reg, f: Reg, start: Reg, argc: u8 },
    /// dst <- regs[recv].name(regs[start .. start+argc]) — builtin methods
    Method { dst: Reg, recv: Reg, name: u16, start: Reg, argc: u8 },
    Ret(Reg),

    MakeList { dst: Reg, start: Reg, len: u8 },
    /// dst <- ctors[ctor](regs[start .. start+len])
    MakeCtor { dst: Reg, ctor: u16, start: Reg, len: u8 },
    /// dst <- regs[obj].fields[idx]
    GetField { dst: Reg, obj: Reg, idx: u8 },
    /// dst <- regs[obj].field named consts[name] (records: resolved at runtime)
    GetFieldNamed { dst: Reg, obj: Reg, name: u16 },
    /// dst <- copy of regs[obj] with field consts[name] replaced by regs[value]
    WithField { dst: Reg, obj: Reg, name: u16, value: Reg },
    /// dst <- Bool: is regs[obj] an instance of ctors[ctor]?
    TagIs { dst: Reg, obj: Reg, ctor: u16 },
    /// dst <- closure over chunks[proto], capturing regs[start .. start+ncaps]
    MakeClosure { dst: Reg, proto: u16, start: Reg, ncaps: u8 },
    MakeRange { dst: Reg, lo: Reg, hi: Reg, inclusive: bool },

    /// Iteration support: length of a List or Range; element at index.
    IterLen(Reg, Reg),
    IterGet { dst: Reg, iter: Reg, idx: Reg },

    ToStr(Reg, Reg),
    Concat(Reg, Reg, Reg),

    // ---- component ops (execute with a current-instance context) ----
    /// dst <- current instance slot (props then state)
    StateGet(Reg, u8),
    /// current instance slot <- regs[src]
    StateSet(u8, Reg),
    /// dst <- new instance of components[comp]; props from regs[start..start+argc].
    /// policy: 0 = escalate on panic, 1 = restart on failure (set by the parent's
    /// `supervise` clause, resolved at compile time).
    MakeInstance { dst: Reg, comp: u16, start: Reg, argc: u8, policy: u8 },
    /// wire regs[from].out consts[out_port] -> regs[to].in consts[in_port]
    WireOp { from: Reg, out_port: u16, to: Reg, in_port: u16 },
    /// emit on the current instance's out port consts[port]
    EmitOp { port: u16, payload: Option<Reg> },

    /// Unconditional panic with message consts[idx].
    Panic(u16),

    /// dst <- ai_funs[info](frame params) with the resolved intent in
    /// regs[intent]. The body of a compiled `ai fun` chunk first builds the
    /// interpolated intent string, then this op reads the parameter registers,
    /// performs the provider call, and converts the response per the shape.
    CallAi { dst: Reg, info: u16, intent: Reg },
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub name: String,
    /// Number of leading registers holding captures (lambdas only).
    pub ncaps: u8,
    /// Number of parameter registers (after captures).
    pub nparams: u8,
    /// Total registers this frame needs.
    pub nregs: u16,
    pub consts: Vec<Value>,
    pub code: Vec<Op>,
    /// Source span per instruction (for panics).
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone)]
pub struct CtorMeta {
    pub type_name: String,
    pub variant: String,
    pub arity: u8,
}

/// A compiled timer handler: its chunk, whether it recurs, and its interval
/// (virtual milliseconds).
#[derive(Debug, Clone, PartialEq)]
pub struct TimerMeta {
    pub chunk: u16,
    pub every: bool,
    pub interval_ms: i64,
}

/// A compiled component: slot layout + chunk indices for its behavior.
#[derive(Debug, Clone)]
pub struct ComponentMeta {
    pub name: String,
    pub is_app: bool,
    /// prop name + optional default-value chunk (no params, no instance)
    pub props: Vec<(String, Option<u16>)>,
    /// total instance slots: props, then state, then children
    pub nslots: u8,
    /// runs with the instance current: state inits, children, wires
    pub init_chunk: u16,
    /// state inits only — used by supervision restarts
    pub restart_chunk: u16,
    /// port name -> (chunk, has_param); "@start"/"@stop" for lifecycle
    pub handlers: Vec<(String, u16, bool)>,
    pub exposes: std::collections::HashMap<String, u16>,
    pub out_ports: Vec<String>,
    /// `on every`/`on after` timer handlers, in declaration order.
    pub timers: Vec<TimerMeta>,
}

/// A compiled program: all function chunks + the constructor table.
#[derive(Debug, Clone, Default)]
pub struct Module {
    pub chunks: Vec<Chunk>,
    pub ctors: Vec<CtorMeta>,
    /// top-level function name -> chunk index
    pub funs: std::collections::HashMap<String, u16>,
    /// variant name -> ordered field names (for record field access by name)
    pub ctor_field_names: std::collections::HashMap<String, Vec<String>>,
    pub components: Vec<ComponentMeta>,
    pub component_names: std::collections::HashMap<String, u16>,
    /// `ai fun` runtime signatures, indexed by `Op::CallAi`.
    pub ai_funs: Vec<crate::ai::AiFunMeta>,
}

pub const BUILTIN_PRINT: u8 = 0;
pub const BUILTIN_TO_STR: u8 = 1;
pub const BUILTIN_PANIC: u8 = 2;
pub const BUILTIN_TENSOR: u8 = 3;
pub const BUILTIN_ZEROS: u8 = 4;
pub const BUILTIN_ARANGE: u8 = 5;
pub const BUILTIN_MAP_NEW: u8 = 6;
pub const BUILTIN_SET_NEW: u8 = 7;
pub const BUILTIN_SET_FROM: u8 = 8;
pub const BUILTIN_READ_FILE: u8 = 9;
pub const BUILTIN_WRITE_FILE: u8 = 10;
pub const BUILTIN_APPEND_FILE: u8 = 11;
pub const BUILTIN_DELETE_FILE: u8 = 12;
pub const BUILTIN_FILE_EXISTS: u8 = 13;
pub const BUILTIN_JSON_PARSE: u8 = 14;
pub const BUILTIN_JSON_STRINGIFY: u8 = 15;
pub const BUILTIN_ENV_VAR: u8 = 16;
pub const BUILTIN_ARGS: u8 = 17;
pub const BUILTIN_EPRINT: u8 = 18;
pub const BUILTIN_EXIT: u8 = 19;
pub const BUILTIN_RANDOM_INTS: u8 = 20;
pub const BUILTIN_RANDOM_FLOATS: u8 = 21;
pub const BUILTIN_SHUFFLE: u8 = 22;
pub const BUILTIN_HTTP_GET: u8 = 23;
pub const BUILTIN_HTTP_POST: u8 = 24;

impl Module {
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (i, chunk) in self.chunks.iter().enumerate() {
            let _ = writeln!(
                out,
                "chunk #{i} {} (caps {}, params {}, regs {})",
                chunk.name, chunk.ncaps, chunk.nparams, chunk.nregs
            );
            for (j, c) in chunk.consts.iter().enumerate() {
                let _ = writeln!(out, "  const[{j}] = {c}");
            }
            for (pc, op) in chunk.code.iter().enumerate() {
                let _ = writeln!(out, "  {pc:4}  {op:?}");
            }
            let _ = writeln!(out);
        }
        if !self.ctors.is_empty() {
            let _ = writeln!(out, "ctors:");
            for (i, ct) in self.ctors.iter().enumerate() {
                let _ = writeln!(out, "  [{i}] {}::{}({})", ct.type_name, ct.variant, ct.arity);
            }
        }
        out
    }
}
