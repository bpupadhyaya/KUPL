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
pub const BUILTIN_RE_MATCH: u8 = 25;
pub const BUILTIN_RE_FIND: u8 = 26;
pub const BUILTIN_RE_FIND_ALL: u8 = 27;
pub const BUILTIN_RE_REPLACE: u8 = 28;
pub const BUILTIN_FORMAT_TIME: u8 = 29;
pub const BUILTIN_YEAR_OF: u8 = 30;
pub const BUILTIN_MONTH_OF: u8 = 31;
pub const BUILTIN_DAY_OF: u8 = 32;
pub const BUILTIN_HOUR_OF: u8 = 33;
pub const BUILTIN_MINUTE_OF: u8 = 34;
pub const BUILTIN_SECOND_OF: u8 = 35;
pub const BUILTIN_WEEKDAY_OF: u8 = 36;
pub const BUILTIN_NOW: u8 = 37;
pub const BUILTIN_BASE64_ENCODE: u8 = 38;
pub const BUILTIN_BASE64_DECODE: u8 = 39;
pub const BUILTIN_HEX_ENCODE: u8 = 40;
pub const BUILTIN_HEX_DECODE: u8 = 41;
pub const BUILTIN_HASH_FNV: u8 = 42;
pub const BUILTIN_CSV_PARSE: u8 = 43;
pub const BUILTIN_CSV_STRINGIFY: u8 = 44;
pub const BUILTIN_URL_ENCODE: u8 = 45;
pub const BUILTIN_URL_DECODE: u8 = 46;
pub const BUILTIN_QUERY_PARSE: u8 = 47;
pub const BUILTIN_QUERY_BUILD: u8 = 48;
pub const BUILTIN_DATE_MAKE: u8 = 49;
pub const BUILTIN_PARSE_ISO: u8 = 50;
pub const BUILTIN_DATE_ISO: u8 = 51;
pub const BUILTIN_YEARDAY_OF: u8 = 52;
pub const BUILTIN_READ_LINE: u8 = 53;
pub const BUILTIN_READ_ALL: u8 = 54;
pub const BUILTIN_EXEC: u8 = 55;
pub const BUILTIN_PATH_JOIN: u8 = 56;
pub const BUILTIN_PATH_BASE: u8 = 57;
pub const BUILTIN_PATH_DIR: u8 = 58;
pub const BUILTIN_PATH_EXT: u8 = 59;
pub const BUILTIN_LIST_DIR: u8 = 60;
pub const BUILTIN_MAKE_DIR: u8 = 61;
pub const BUILTIN_REMOVE_DIR: u8 = 62;
pub const BUILTIN_BIG: u8 = 63;
pub const BUILTIN_HTTP_SERVE: u8 = 64;
pub const BUILTIN_RAT: u8 = 65;

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

/// Registers `op` could hand an existing value's pointer through to another
/// live reference (aliasing it), as opposed to registers `op` merely
/// overwrites or reads a scalar from. Used by [`method_recv_escapes`] to
/// decide whether a self-rebind `Op::Method` (`xs = xs.push(item)`-shaped
/// code, compiled with `dst == recv`) is safe for a backend to mutate in
/// place instead of copying.
///
/// Deliberately narrow: only the op kinds that can actually smuggle a
/// List/Map/Set/record pointer to somewhere else are listed. An `Op::Method`
/// call that merely reads `recv` as its receiver (e.g. `.len()`) is NOT
/// treated as an escape here — most builtin methods read rather than store
/// their receiver. This should be re-examined per-method before any backend
/// relies on this analysis for a method whose semantics could stash the
/// receiver elsewhere.
fn aliasing_regs(op: &Op) -> Vec<Reg> {
    match op {
        Op::Move(_dst, src) => vec![*src],
        Op::Call { start, argc, .. }
        | Op::CallComp { start, argc, .. }
        | Op::CallBuiltin { start, argc, .. }
        | Op::CallValue { start, argc, .. } => (*start..start.saturating_add(*argc)).collect(),
        Op::MakeList { start, len, .. } | Op::MakeCtor { start, len, .. } => {
            (*start..start.saturating_add(*len)).collect()
        }
        Op::MakeClosure { start, ncaps, .. } => (*start..start.saturating_add(*ncaps)).collect(),
        Op::WithField { value, .. } => vec![*value],
        Op::StateSet(_slot, src) => vec![*src],
        Op::EmitOp { payload: Some(r), .. } => vec![*r],
        _ => vec![],
    }
}

/// The smallest `[lo, hi]` instruction-index range enclosing `idx` that some
/// backward jump (`Jump`/`JumpIfFalse`/`JumpIfTrue` whose target is `<=` its
/// own index) loops over, or `None` if `idx` isn't inside such a range.
/// Nested/overlapping enclosing loops are merged into one conservative range.
fn enclosing_loop_range(chunk: &Chunk, idx: usize) -> Option<(usize, usize)> {
    let mut result: Option<(usize, usize)> = None;
    for (i, op) in chunk.code.iter().enumerate() {
        let target = match op {
            Op::Jump(t) => Some(*t),
            Op::JumpIfFalse(_, t) => Some(*t),
            Op::JumpIfTrue(_, t) => Some(*t),
            _ => None,
        };
        let Some(t) = target else { continue };
        if t <= i && t <= idx && idx <= i {
            result = Some(match result {
                None => (t, i),
                Some((lo, hi)) => (lo.min(t), hi.max(i)),
            });
        }
    }
    result
}

/// True if the value in register `recv` at the self-rebind `Op::Method` site
/// `chunk.code[method_idx]` (`dst == recv`, i.e. `xs = xs.push(item)`-shaped
/// code) could be aliased by another live reference, making an in-place
/// mutation there unsafe.
///
/// Conservative by construction: this only returns `false` (safe) when it
/// can prove no alias-creating op touches `recv` in the relevant window;
/// anything this analysis doesn't recognize, or is ambiguous about, is
/// decided in favor of `true` (unsafe) — worst case a missed fast path,
/// never a wrongly-taken one.
///
/// Two windows, chosen to stay sound across both straight-line code and
/// loops:
/// - If `method_idx` is inside a loop (per [`enclosing_loop_range`]), the
///   WHOLE loop body is scanned — including ops textually AFTER
///   `method_idx` within that body. This matters because an alias created
///   late in one iteration (e.g. `xs = xs.push(i); ys = xs`) is live BEFORE
///   the `Method` op on the next iteration; a scan bounded only by
///   `method_idx`'s textual position would miss it.
/// - Otherwise, the whole prefix `[0, method_idx)` is scanned (not just
///   "since `recv`'s last write"), since a branch can make the nearest
///   textual write to `recv` reachable only on some paths, leaving an
///   earlier alias on another path still live at `method_idx`.
///
/// Out of scope: this is a single-chunk (intraprocedural) analysis. A
/// register holding a function PARAMETER can always be aliased by the
/// caller before the call, which this analysis cannot see — callers of this
/// function must treat parameter registers as always-escaped separately.
pub fn method_recv_escapes(chunk: &Chunk, method_idx: usize) -> bool {
    let recv = match chunk.code.get(method_idx) {
        Some(Op::Method { recv, .. }) => *recv,
        _ => return true,
    };
    reg_escapes(chunk, method_idx, recv)
}

/// True if the value in register `a` at `Op::Add(d, a, b)` where `d == a`
/// (the `s = s + expr` string self-append shape compile.rs compiles to a
/// dst==src Add, mirroring its dst==recv Op::Method compilation for
/// `xs = xs.push(item)`) could be aliased elsewhere in this chunk. Same
/// soundness argument and same out-of-scope caveat as
/// [`method_recv_escapes`] — see its doc comment; this is that same
/// analysis for a different self-rebind op shape, sharing the underlying
/// [`reg_escapes`] window logic instead of duplicating it.
pub fn add_lhs_escapes(chunk: &Chunk, add_idx: usize) -> bool {
    let a = match chunk.code.get(add_idx) {
        Some(Op::Add(_, a, _)) => *a,
        _ => return true,
    };
    reg_escapes(chunk, add_idx, a)
}

/// Shared window-scan behind [`method_recv_escapes`] and [`add_lhs_escapes`]:
/// true if `reg`'s value at `op_idx` could be aliased by another op in this
/// chunk. See `method_recv_escapes`'s doc comment for the two-window
/// soundness argument (loop body vs whole prefix) this implements.
fn reg_escapes(chunk: &Chunk, op_idx: usize, reg: Reg) -> bool {
    let (start, end) = match enclosing_loop_range(chunk, op_idx) {
        Some((lo, hi)) => (lo, hi + 1),
        None => (0, op_idx),
    };
    chunk
        .code
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .any(|(i, op)| i != op_idx && aliasing_regs(op).contains(&reg))
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    fn chunk(code: Vec<Op>) -> Chunk {
        let spans = vec![Span::default(); code.len()];
        Chunk {
            name: "test".into(),
            ncaps: 0,
            nparams: 0,
            nregs: 8,
            consts: vec![],
            code,
            spans,
        }
    }

    const PUSH: u16 = 0;

    fn self_push(reg: Reg) -> Op {
        Op::Method { dst: reg, recv: reg, name: PUSH, start: reg, argc: 1 }
    }

    #[test]
    fn straight_line_self_push_with_no_prior_use_is_safe() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            self_push(0),
        ]);
        assert!(!method_recv_escapes(&c, 1));
    }

    #[test]
    fn straight_line_move_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::Move(1, 0), // ys = xs
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn straight_line_escape_on_a_branch_not_reached_by_the_nearest_write_is_still_flagged() {
        // xs = []; ys = xs; if cond { xs = other }; xs = xs.push(1)
        // the nearest textual write to xs before the push is inside the
        // conditional; the true escape (ys = xs) predates it and must still
        // be caught by scanning the whole prefix, not just since that write.
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 }, // 0: xs = []
            Op::Move(1, 0),                            // 1: ys = xs   <- escape
            Op::JumpIfFalse(2, 4),                     // 2: if !cond -> 4
            Op::Move(0, 3),                            // 3: xs = other
            self_push(0),                              // 4: xs = xs.push(1)
        ]);
        assert!(method_recv_escapes(&c, 4));
    }

    #[test]
    fn call_argument_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::Call { dst: 1, fun: 0, start: 0, argc: 1 }, // xs passed as an arg
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn closure_capture_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::MakeClosure { dst: 1, proto: 0, start: 0, ncaps: 1 },
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn embedding_in_a_new_list_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::MakeList { dst: 1, start: 0, len: 1 }, // [xs] embeds it
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn with_field_value_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::WithField { dst: 1, obj: 2, name: 0, value: 0 },
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn state_set_before_push_escapes() {
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::StateSet(0, 0),
            self_push(0),
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn emit_payload_before_push_escapes_but_a_portless_emit_does_not() {
        let escapes = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::EmitOp { port: 0, payload: Some(0) },
            self_push(0),
        ]);
        assert!(method_recv_escapes(&escapes, 2));

        let safe = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::EmitOp { port: 0, payload: None },
            self_push(0),
        ]);
        assert!(!method_recv_escapes(&safe, 2));
    }

    #[test]
    fn simple_loop_with_no_escape_is_safe() {
        // xs = []; while i < n { xs = xs.push(i); i = i + 1 }
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 }, // 0
            Op::Lt(2, 1, 3),                           // 1: loop head, i < n
            Op::JumpIfFalse(2, 5),                     // 2
            self_push(0),                              // 3: xs = xs.push(i)
            Op::Add(1, 1, 4),                           // 4: i = i + 1 (falls through to jump)
            Op::Jump(1),                                // 5: back-edge to the loop head
        ]);
        // method op is at index 3; the back-edge Jump(1) is at index 5.
        assert!(!method_recv_escapes(&c, 3));
    }

    #[test]
    fn loop_body_alias_after_the_push_still_escapes_the_next_iteration() {
        // while i < n { xs = xs.push(i); ys = xs; i = i + 1 }
        // the alias (ys = xs) happens AFTER the push textually, but is live
        // BEFORE the push on the next iteration — must still be flagged.
        let c = chunk(vec![
            Op::Lt(2, 1, 3),        // 0: loop head
            Op::JumpIfFalse(2, 6),  // 1
            self_push(0),           // 2: xs = xs.push(i)
            Op::Move(5, 0),         // 3: ys = xs   <- escape, textually AFTER the push
            Op::Add(1, 1, 4),       // 4: i = i + 1
            Op::Jump(0),            // 5: back-edge to loop head
        ]);
        assert!(method_recv_escapes(&c, 2));
    }

    #[test]
    fn non_method_op_at_the_index_is_conservatively_treated_as_escaping() {
        let c = chunk(vec![Op::Move(0, 1)]);
        assert!(method_recv_escapes(&c, 0));
    }

    fn self_add(reg: Reg, b: Reg) -> Op {
        Op::Add(reg, reg, b)
    }

    #[test]
    fn straight_line_self_add_with_no_prior_use_is_safe() {
        let c = chunk(vec![Op::Const(0, 0), Op::Const(1, 1), self_add(0, 1)]);
        assert!(!add_lhs_escapes(&c, 2));
    }

    #[test]
    fn straight_line_move_before_self_add_escapes() {
        // s = ""; t = s; s = s + x
        let c = chunk(vec![
            Op::Const(0, 0),
            Op::Move(2, 0), // t = s
            Op::Const(1, 1),
            self_add(0, 1),
        ]);
        assert!(add_lhs_escapes(&c, 3));
    }

    #[test]
    fn loop_body_alias_after_the_add_still_escapes_the_next_iteration() {
        // while i < n { s = s + x; t = s; i = i + 1 }
        let c = chunk(vec![
            Op::Lt(2, 1, 3),   // 0: loop head
            Op::JumpIfFalse(2, 6), // 1
            self_add(0, 4),    // 2: s = s + x
            Op::Move(5, 0),    // 3: t = s   <- escape, textually AFTER the add
            Op::Add(1, 1, 4),  // 4: i = i + 1
            Op::Jump(0),       // 5: back-edge to loop head
        ]);
        assert!(add_lhs_escapes(&c, 2));
    }

    #[test]
    fn simple_loop_self_add_with_no_escape_is_safe() {
        let c = chunk(vec![
            Op::Const(0, 0),      // 0: s = ""
            Op::Lt(2, 1, 3),      // 1: loop head
            Op::JumpIfFalse(2, 5), // 2
            self_add(0, 4),        // 3: s = s + x
            Op::Add(1, 1, 4),      // 4: i = i + 1
            Op::Jump(1),            // 5: back-edge
        ]);
        assert!(!add_lhs_escapes(&c, 3));
    }

    #[test]
    fn call_argument_before_self_add_escapes() {
        let c = chunk(vec![
            Op::Const(0, 0),
            Op::Call { dst: 1, fun: 0, start: 0, argc: 1 }, // s passed as an arg
            Op::Const(2, 1),
            self_add(0, 2),
        ]);
        assert!(add_lhs_escapes(&c, 3));
    }

    #[test]
    fn non_add_op_at_the_index_is_conservatively_treated_as_escaping() {
        let c = chunk(vec![Op::Move(0, 1)]);
        assert!(add_lhs_escapes(&c, 0));
    }
}
