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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Default, PartialEq)]
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
        // A REAL gap found+fixed (production-hardening PR-it811): `Op::Method`
        // was entirely absent from this match (falling through to the `_`
        // catch-all), so NO method call was ever treated as a potential alias
        // site -- despite this function's own doc comment explicitly warning
        // "this should be re-examined per-method before any backend relies on
        // this analysis for a method whose semantics could stash the receiver
        // elsewhere." At least one builtin method DOES exactly that: native's
        // `Str.replace_first` short-circuits to `return recv;` (unchanged,
        // same pointer) when the pattern isn't found (cgen.rs) -- so
        // `let backup = s.replace_first("nomatch", "y")` makes `backup` a
        // genuine alias of `s` in native (never in interp/vm, which always
        // reallocate via `replacen` regardless of match). A later self-append
        // `s = s + "!"` -- gated on `add_lhs_escapes` seeing no alias of `s`
        // -- then mutated the SHARED buffer in place via
        // `k_str_append_inplace`, silently corrupting `backup` too. CONFIRMED
        // LIVE before this fix: `var s = "a"; s = s + "b"; s = s + "c"; let
        // backup = s.replace_first("nomatch", "y"); s = s + "d";` printed
        // `backup=abc` on interp/vm (correct) but `backup=abcd` (corrupted)
        // on `kupl native` -- a genuine, silent WRONG-ANSWER divergence, not
        // a crash. Both `recv` and the argument registers are treated as
        // escaping, matching `Call`/`CallBuiltin`'s existing conservative
        // treatment of their own argument ranges -- a method could just as
        // plausibly return one of its ARGUMENTS unchanged as its receiver.
        Op::Method { recv, start, argc, .. } => {
            let mut regs = vec![*recv];
            regs.extend(*start..start.saturating_add(*argc));
            regs
        }
        Op::MakeList { start, len, .. } | Op::MakeCtor { start, len, .. } => {
            (*start..start.saturating_add(*len)).collect()
        }
        // A REAL static-analysis gap found+fixed (production-hardening
        // PR-it968, the EIGHTH instance of the escape-analysis-completeness
        // family first opened at it615): `Op::MakeInstance` (component
        // construction) had no arm here at all, falling into the `_ =>
        // vec![]` catch-all, unlike its sibling `MakeList`/`MakeCtor` arm
        // just above, which already treats their own `start..start+len`
        // staging window as aliasing. Currently masked in practice ONLY by
        // `compile.rs::instance_expr`'s own convention of unconditionally
        // re-staging every prop value through a fresh `Op::Move` before
        // emitting `MakeInstance` -- the Move's OWN arm already flags the
        // ORIGINAL register, so no currently-compiler-generated
        // `MakeInstance` is unsafe -- but that is a fragile CONVENTION, not
        // a structural guarantee (unlike `reg_traces_to_a_parameter`/`go`'s
        // own separate, K0233-based argument at PR-it847 for why THAT
        // function doesn't need a `MakeInstance` arm: an external
        // `w.prop_name`/`GetFieldNamed` read is rejected at type-check
        // time, but a component's OWN exposed methods CAN read a prop's
        // value back out internally, e.g. `expose fun get() -> T { prop }`
        // -- a different bytecode mechanism than `GetFieldNamed`, so that
        // argument does not transfer to this function).
        Op::MakeInstance { start, argc, .. } => (*start..start.saturating_add(*argc)).collect(),
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

/// True if `reg` is a genuine chunk-local register — not a capture or
/// parameter, which the CALLER of this chunk could hold an independent
/// reference to (captures occupy `[0, ncaps)`, params `[ncaps,
/// ncaps+nparams)`, per compile.rs's FnCompiler register allocation order).
///
/// This alone is NOT sufficient to prove a self-rebind register is safe to
/// mutate in place — see [`reg_traces_to_a_parameter`], which additionally
/// accounts for a register whose CURRENT VALUE was copied (via `Move`) from
/// a parameter/capture even though the register's own NUMBER is chunk-local.
pub fn is_chunk_local_reg(chunk: &Chunk, reg: Reg) -> bool {
    (reg as u16) >= chunk.ncaps as u16 + chunk.nparams as u16
}

/// True if the value in register `reg`, as observed at `op_idx`, could have
/// originated — directly, or through a chain of `Move`s and/or field reads —
/// from a capture or parameter register.
///
/// This closes a real gap `is_chunk_local_reg` alone misses: the common
/// `fun f(xs: List[Int]) { var ys = xs; ys = ys.push(item) }` shape compiles
/// `var ys = xs` to `Move(ys_reg, xs_reg)` — `ys_reg` gets a FRESH,
/// chunk-local register number (so `is_chunk_local_reg` alone says "safe"),
/// but its value is a Move'd ALIAS of the parameter `xs`, exactly as unsafe
/// to mutate in place as `xs` itself: the CALLER's own reference to the list
/// it passed as `xs` would observe the mutation too. `Op::GetField`/
/// `Op::GetFieldNamed` (production-hardening PR-it819) are the SAME kind of
/// hazard one layer down: `fun f(b: Box) { var xs = b.items; xs =
/// xs.push(item) }` reads a field OUT of a parameter — `k_field`/
/// `k_field_named` (cgen.rs) copy the field's `KValue` with NO clone/
/// refcount bump, so `xs` and `b`'s stored field become the literal same
/// heap object, just as aliased as a direct `Move` from the parameter.
///
/// Recursive: follows every `Move`/`GetField`/`GetFieldNamed` writing into
/// `reg` within the same loop-body-or-whole-prefix window [`reg_escapes`]
/// scans (same soundness argument — see `method_recv_escapes`'s doc
/// comment), and treats ANY path that reaches a parameter/capture as
/// tainting the whole thing — conservative, since a register conditionally
/// assigned from either a safe or unsafe source on different branches must
/// be treated as unsafe on the union of both. Depth-bounded by `nregs` as a
/// cycle guard (a well-formed chunk's Move/GetField chain can't legitimately
/// need more hops than it has registers); hitting the bound is
/// conservatively treated as unsafe.
pub fn reg_traces_to_a_parameter(chunk: &Chunk, op_idx: usize, reg: Reg) -> bool {
    fn go(chunk: &Chunk, op_idx: usize, reg: Reg, depth: u16) -> bool {
        if !is_chunk_local_reg(chunk, reg) {
            return true;
        }
        if depth as usize > chunk.nregs as usize {
            return true;
        }
        // A REAL, live-confirmed silent value-corruption bug found+fixed
        // (production-hardening PR-it984): when `op_idx` is inside a loop,
        // this used to narrow the scan to JUST the loop body (`(lo, hi + 1)`,
        // mirroring `reg_escapes`'s own loop-body window) -- sound for
        // `reg_escapes`'s question ("was ANOTHER alias created that could
        // touch `reg`"), since a fresh alias formed anywhere else is
        // irrelevant to whether THIS loop's mutation is safe. But this
        // function asks a DIFFERENT question -- "does `reg`'s CURRENT value
        // trace back to a parameter/capture" -- and the very `Move`/
        // `GetField`/`IterGet` edge that establishes that aliasing is, for
        // the single most common shape (`var t = s; for ... { t = t + x }`),
        // textually BEFORE the loop, in the prefix the loop-only window
        // never sees. Live-confirmed: `fun f(s: Str) -> Str { var t = s; for
        // i in 0..3 { t = t + "x" } t }` called as `f(a)` where `a` is a live
        // caller variable used again afterward -- interp/KVM correctly kept
        // `a` unmutated, but native's fast path (wrongly proven "safe" by
        // this narrowed window, which never saw `var t = s`) mutated `t`'s
        // buffer in place on the very FIRST loop iteration, silently
        // corrupting `a` too (`a` observed a stray leaked "x" appended by
        // the callee). Fixed by always including the full prefix `[0, ..)`,
        // in addition to (not instead of) the loop's own body -- still
        // bounded (never scans past the loop's own end), still resolves the
        // ORIGINAL loop-wraparound case `enclosing_loop_range` was chosen
        // for (a defining write textually AFTER `op_idx` within the same
        // loop body, live on the NEXT iteration), while now also correctly
        // seeing a pre-loop `Move`/`GetField`/`IterGet` that seeds the
        // register's value entering the loop's first iteration.
        let (start, end) = match enclosing_loop_range(chunk, op_idx) {
            Some((_, hi)) => (0, hi + 1),
            None => (0, op_idx),
        };
        for (i, op) in chunk.code.iter().enumerate().take(end).skip(start) {
            if i == op_idx {
                continue;
            }
            if let Op::Move(dst, src) = op {
                if *dst == reg && go(chunk, op_idx, *src, depth + 1) {
                    return true;
                }
            }
            // production-hardening PR-it819: a REAL, live-confirmed silent
            // value-corruption bug -- this trace used to follow ONLY
            // `Op::Move` edges, so `var xs = b.items` (a `GetField`/
            // `GetFieldNamed` reading a field OUT of a parameter/capture,
            // `b`) dead-ended at the field-read's own `dst` register
            // instead of continuing into `obj` (`b`). `k_field`/
            // `k_field_named` (cgen.rs) copy the field's `KValue` out with
            // NO clone/refcount bump (this runtime has no refcounting), so
            // `xs` and `b`'s stored field become the SAME heap object --
            // exactly as aliased as if `xs` had been `Move`'d directly from
            // a parameter. `xs = xs.push(item)` then wrongly took the
            // in-place fast path (this proof reported "not traced to a
            // parameter"), silently mutating the CALLER's own struct field.
            // Treat a field read as the same kind of taint-propagating edge
            // as a `Move`, recursing into the field's `obj` register.
            if let Op::GetField { dst, obj, .. } | Op::GetFieldNamed { dst, obj, .. } = op {
                if *dst == reg && go(chunk, op_idx, *obj, depth + 1) {
                    return true;
                }
            }
            // production-hardening PR-it820: a REAL, live-confirmed silent
            // value-corruption bug, ONE HOP past PR-it819's GetField fix --
            // `for x in xs { var y = x; y = y.push(item) }` compiles `x`'s
            // per-iteration binding to `Op::IterGet { dst, iter, idx }`.
            // `k_iter_get`'s `K_LIST` case (cgen.rs) returns
            // `v.as.list->items[idx]` by shallow copy, no clone/refcount
            // bump -- `y`'s value becomes the literal same heap object as
            // one of `xs`'s own elements, so `y.push(item)` taken as an
            // in-place mutation corrupts `xs` itself. UNLIKE `GetField`
            // (where recursing into `obj` and checking whether `obj` is a
            // parameter is sufficient, since a genuinely chunk-local,
            // non-escaping record's field really is safe to mutate through)
            // this canNOT simply recurse into `iter` and check whether
            // `iter` traces to a parameter: `xs` here is a perfectly
            // ordinary chunk-local list, not a parameter, and the
            // loop-body-scoped window `enclosing_loop_range` deliberately
            // narrows to (see `method_recv_escapes`'s own doc comment) does
            // NOT extend to `xs`'s uses AFTER the loop exits (e.g. `print
            // (xs)` following the loop) -- confirmed empirically: a
            // recurse-into-`iter` variant of this fix, tested BEFORE this
            // one was written, still let the corruption through, because
            // `iter`'s own register traces cleanly to a `MakeList` outside
            // the loop body window and never appears live within it. Since
            // this analysis has no cheap, sound way to prove `iter`'s
            // container is unused for the REST of the chunk (before AND
            // after the loop, potentially across nested loops), the
            // correct, conservative choice is to treat ANY register whose
            // value came from `Op::IterGet` as unconditionally unsafe --
            // exactly like reaching an actual parameter/capture -- rather
            // than attempting (and failing) to prove it safe.
            if let Op::IterGet { dst, .. } = op {
                if *dst == reg {
                    return true;
                }
            }
            // production-hardening PR-it822: a REAL, live-confirmed silent
            // value-corruption bug -- the FOURTH untracked edge found in
            // this exact function (Move/PR-it615, GetField/PR-it819,
            // IterGet/PR-it820). `var xs = items` (snapshotting a component
            // STATE field into a local) compiles to `Op::StateGet(dst,
            // slot)` followed by a `Move` into the local's own register.
            // `k_state_get` (cgen.rs) is `return k_insts[cur].slots[slot];`
            // -- a shallow copy, no clone/refcount bump -- so `xs` becomes
            // the literal same heap object the instance's OWN state slot
            // holds. `xs = xs.push(item)` then wrongly took the in-place
            // fast path, silently mutating the component's persisted state
            // with no `items = ...` assignment anywhere in the source.
            // UNLIKE `GetField`/`IterGet`, `Op::StateGet` has no source
            // REGISTER to recurse into at all (it reads directly from the
            // instance's state array by a compile-time slot index, not
            // another chunk register) -- so there is no recursive case to
            // write here, only an unconditional one. This is in fact an
            // EVEN STRONGER case for "always unsafe" than `IterGet`: a
            // state slot's value can be read again by ANY future handler
            // invocation on this same instance, for the ENTIRE remaining
            // lifetime of the component -- far beyond anything even a
            // whole-chunk (let alone loop-body) window could ever see.
            if let Op::StateGet(dst, _) = op {
                if *dst == reg {
                    return true;
                }
            }
            // production-hardening PR-it823: a FIFTH untracked edge, found
            // via a systematic completeness sweep of this function against
            // every `Op` variant after FOUR independent instances of this
            // same gap shape (Move/it615, GetField/it819, IterGet/it820,
            // StateGet/it822) -- `Op::Call`/`Op::CallComp`/`Op::CallValue`
            // (a user-defined function/closure call) and `Op::Method` (a
            // builtin OR component method call) can each return a value
            // that ALIASES one of the CALLEE's own live values, invisible
            // to this single-chunk analysis: a callee can return one of its
            // OWN parameters/captures unchanged (an identity/pass-through
            // function, live-confirmed via `fun identity(a) { a }`), or a
            // builtin method can return its receiver/argument unchanged on
            // some path (already the exact PR-it811 finding for
            // `Str.replace_first`'s no-match case -- that fix only taught
            // `aliasing_regs` that a `Method` op READS its recv/args in a
            // way that could hand them out elsewhere; it never taught THIS
            // function that a `Method`'s own `dst` could likewise BE one of
            // those same values). Unlike `GetField` (where recursing into
            // `obj` and checking whether `obj` traces to a parameter is
            // enough, since a genuinely local record's field really is
            // safe), this analysis is explicitly INTRAPROCEDURAL by design
            // (see this function's own doc comment) and has no way to see
            // what the CALLEE's own body does with ITS parameters/captures/
            // state -- even confirming every argument register here traces
            // to something safe would NOT prove the callee's return value
            // is safe, since the callee could derive it from something this
            // window can never observe. Any register written by one of
            // these ops must be treated as unconditionally unsafe, the
            // same conservative posture as `IterGet`/`StateGet`.
            // CONFIRMED LIVE for both: `fun identity(a){a}`, called as
            // `identity(ys)` then self-pushed, corrupted the caller's `ys`
            // on native; `s.replace_first("nomatch","y")` (no match, so
            // native's C mirror returns `s` unchanged) then self-appended
            // similarly corrupted the caller's string once it had spare
            // capacity to grow in place.
            if let Op::Call { dst, .. }
            | Op::CallComp { dst, .. }
            | Op::CallValue { dst, .. }
            | Op::Method { dst, .. } = op
            {
                if *dst == reg {
                    return true;
                }
            }
            // production-hardening PR-it824: a SIXTH untracked edge, found
            // finishing the systematic completeness sweep's remaining tail
            // (Op::WithField and Op::CallAi -- CallAi confirmed safe, see
            // this arm's sibling comment below). `dst <- copy of regs[obj]
            // with field consts[name] replaced by regs[value]` -- `b with
            // x: 99` -- compiles to `Op::WithField`. `k_with_field`
            // (cgen.rs) allocates a FRESH fields array and memcpy's `obj`'s
            // OWN field array into it before overwriting just the ONE
            // updated field -- so every OTHER (unchanged) field's `KValue`
            // is a SHALLOW copy of `obj`'s corresponding slot, no clone/
            // refcount bump. `(b with x: 99).items` (a field OTHER than
            // the one just updated) therefore aliases `b`'s own `items`
            // field exactly as directly as a plain `b.items` `GetField`
            // would. UNLIKE `IterGet`/`StateGet`/`Call`/`Method` (which
            // have no sound way to prove their result safe and so are
            // treated unconditionally unsafe), `WithField` DOES have a
            // genuine source register to recurse into here -- `obj` -- and
            // the SAME reasoning as `GetField` applies: if `obj` itself
            // doesn't trace to a parameter, a field carried through from a
            // truly local, non-escaping `obj` really is safe to mutate.
            if let Op::WithField { dst, obj, .. } = op {
                if *dst == reg && go(chunk, op_idx, *obj, depth + 1) {
                    return true;
                }
            }
            // A SEVENTH untracked edge (production-hardening PR-it847, the
            // TWENTY-SEVENTH broad Explore survey): `Op::MakeCtor` builds a
            // record's `KValue` fields via a plain, unchecked `memcpy`
            // (cgen.rs's `k_ctor`) from the `start..start+len` staging
            // window `order_ctor_args` fills -- if one of those staging
            // registers itself traces to a parameter (e.g. `Box(items:
            // some_param)`), the constructed record's field is just as
            // aliased as the parameter itself, and a LATER `GetFieldNamed`
            // extracting that field (already a tracked edge since PR-it819)
            // inherits that same aliasing -- but this function had NO arm
            // for `MakeCtor` at all, so `go()` fell through the whole loop
            // and returned `false` ("safe") for a register reached only via
            // `GetFieldNamed(obj: <MakeCtor's dst>)`. CONFIRMED via a
            // hand-built-chunk unit test (mirroring this module's own
            // established testing convention, since compile.rs's actual
            // register allocator turned out to reliably mask this: in every
            // KUPL-source-level repro attempted, the ctor's own transient
            // result register gets freed and immediately reused by the
            // following field-read's own transient register, which creates
            // a self-referential Move/GetFieldNamed cycle this function's
            // existing depth-bound cycle guard resolves conservatively
            // "safe" as a side effect -- a real, structural interaction of
            // THIS compiler's specific register-reuse strategy, not a fluke,
            // but not something a static analysis fix should ever rely on
            // holding forever). Confirmed the analogous `MakeInstance`
            // (component construction) case is NOT a live gap: a
            // component's props are not externally-visible record fields at
            // all -- `w.prop_name` is rejected at type-check time (K0233,
            // "only records and components have fields" -- components
            // expose behavior via methods, not `GetFieldNamed`), so no
            // `GetFieldNamed { obj: <MakeInstance result> }` can ever be
            // emitted; `MakeInstance` was left untouched. Fixed by
            // recursing into EVERY register in the constructor's own
            // `start..start+len` staging window -- mirroring
            // `aliasing_regs`'s existing (forward-direction) `MakeCtor`
            // handling exactly, and conservative in the same spirit as
            // `WithField`: if ANY field could be aliased, the whole
            // constructed value is treated as unsafe, since this analysis
            // doesn't track which specific field a later `GetFieldNamed`
            // will extract.
            if let Op::MakeCtor { dst, start, len, .. } = op {
                if *dst == reg {
                    for r in *start..start.saturating_add(*len) {
                        if go(chunk, op_idx, r, depth + 1) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
    go(chunk, op_idx, reg, 0)
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
    // A REAL, live-confirmed silent value-corruption bug found+fixed
    // (production-hardening PR-it985), the SIXTH instance of the escape-
    // analysis-completeness family opened at it615, and the SECOND caused by
    // a WINDOW-SCOPE bug (after PR-it984's `reg_traces_to_a_parameter` fix,
    // same shape, sibling function): when `op_idx` is inside a loop, this
    // used to scan ONLY the loop body (`(lo, hi + 1)`) -- correct for
    // catching an alias created (or re-created) WITHIN the loop itself,
    // live on the NEXT iteration, but blind to an alias created in the
    // PREFIX, before the loop, which is exactly as live at the loop's FIRST
    // iteration as one created inside it. Live-confirmed: `var t = "abcde"; t
    // = t + "f"; t = t + "g"; var getter = fn { t }; for i in 0..n { t = t +
    // "x" } "{getter()}|{t}"` -- interp/KVM correctly returned the closure's
    // VALUE-SEMANTICS SNAPSHOT taken at `MakeClosure` time ("abcdefg",
    // unaffected by the loop), but native's fast path (wrongly proven "safe"
    // by this narrowed window, which never saw the pre-loop `MakeClosure`
    // capturing `t`) mutated `t`'s buffer in place, corrupting the closure's
    // captured value too ("abcdefgxx", matching `t`'s post-loop value
    // instead of the snapshot). The straight-line (no loop) sibling shape,
    // and a loop preceded by a plain `Move`-based alias (`var backup = t`),
    // were already correctly handled before this fix -- this gap is
    // SPECIFIC to the loop-narrowed window, same root cause as PR-it984,
    // just reached through a different alias-creating op (`MakeClosure`
    // rather than `Move`/`GetField`/`IterGet`). Fixed identically to
    // PR-it984: widen the loop-case window to include the full prefix `[0,
    // ..)` in addition to (not instead of) the loop body, so a pre-loop
    // alias-creating op is never invisible again, while still catching the
    // ORIGINAL loop-wraparound case (an alias created late in one iteration,
    // live before the op on the NEXT iteration) this narrowing was for.
    let (start, end) = match enclosing_loop_range(chunk, op_idx) {
        Some((_, hi)) => (0, hi + 1),
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
        chunk_with_params(0, code)
    }

    fn chunk_with_params(nparams: u8, code: Vec<Op>) -> Chunk {
        let spans = vec![Span::default(); code.len()];
        Chunk {
            name: "test".into(),
            ncaps: 0,
            nparams,
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
    fn method_call_reading_the_receiver_before_push_escapes() {
        // production-hardening PR-it811: a method call that merely READS
        // `recv` as its receiver (e.g. `let backup = xs.replace_first(...)`)
        // must be treated as a potential alias site -- `Op::Method` was
        // entirely absent from `aliasing_regs` before this fix, so no method
        // call was ever recognized as escaping. Some builtin methods DO
        // return their receiver unchanged (native's `Str.replace_first` on
        // no match, `Set.insert` on a duplicate), making `backup` and `xs`
        // share the same underlying pointer in native's C backend -- a
        // subsequent in-place self-rebind mutation of `xs` would then
        // silently corrupt `backup` too, confirmed live before this fix.
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::Method { dst: 1, recv: 0, name: PUSH, start: 2, argc: 0 }, // xs read as a receiver
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
    fn embedding_in_a_new_component_instance_before_push_escapes() {
        // production-hardening PR-it968: a hand-built chunk bypassing
        // compile.rs::instance_expr's own Move-staging convention (which
        // masks this in every currently-compiler-generated MakeInstance --
        // see aliasing_regs's own doc comment on its new MakeInstance arm)
        // -- `xs` passed DIRECTLY into MakeInstance's prop window, with no
        // intervening Move, then self-pushed. Must be flagged as escaping,
        // mirroring the sibling MakeList/MakeCtor test just above.
        let c = chunk(vec![
            Op::MakeList { dst: 0, start: 0, len: 0 },
            Op::MakeInstance { dst: 1, comp: 0, start: 0, argc: 1, policy: 0 }, // Widget(items: xs) embeds it
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
    fn method_call_reading_the_receiver_before_self_add_escapes() {
        // production-hardening PR-it811, the Add-side counterpart of
        // `method_call_reading_the_receiver_before_push_escapes` -- see that
        // test's comment for the concrete `Str.replace_first` corruption
        // this closes.
        let c = chunk(vec![
            Op::Const(0, 0),
            Op::Method { dst: 1, recv: 0, name: PUSH, start: 2, argc: 0 }, // s read as a receiver
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

    #[test]
    fn a_parameter_register_itself_traces_to_a_parameter() {
        // register 0 is the sole parameter (nparams=1): using it directly
        // (no Move at all) must already be unsafe.
        let c = chunk_with_params(1, vec![self_push(0)]);
        assert!(reg_traces_to_a_parameter(&c, 0, 0));
    }

    #[test]
    fn a_chunk_local_alias_of_a_parameter_traces_to_it_through_one_move() {
        // fun f(xs: List[Int]) { var ys = xs; ys = ys.push(item) }
        // ys (register 1) is chunk-local by NUMBER (nparams=1, so only
        // register 0 is a parameter), but its value is a Move'd alias of
        // the parameter xs (register 0) -- this is the real bug PR-it614's
        // stress test found: is_chunk_local_reg alone said "safe" here,
        // wrongly, because it never looked at ys's value's PROVENANCE.
        let c = chunk_with_params(1, vec![Op::Move(1, 0), self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_two_hop_move_chain_from_a_parameter_is_still_caught() {
        // var tmp = xs; var ys = tmp; ys = ys.push(item) -- tmp and ys are
        // BOTH chunk-local by register number, but the value still
        // originates from the parameter two hops back.
        let c = chunk_with_params(1, vec![Op::Move(1, 0), Op::Move(2, 1), self_push(2)]);
        assert!(reg_traces_to_a_parameter(&c, 2, 2));
    }

    #[test]
    fn a_fresh_chunk_local_value_moved_into_its_bound_register_does_not_trace_to_a_parameter() {
        // var xs: List[Int] = [] -- compiles to MakeList into a temp
        // register, then Move into xs's own bound register. This is the
        // ORIGINAL, most common self-rebind shape (it609) and must NOT be
        // disqualified by this check: register 1 (the temp) was never
        // itself Move'd from anywhere, so the chain terminates safely.
        let c = chunk_with_params(0, vec![Op::MakeList { dst: 1, start: 1, len: 0 }, Op::Move(0, 1), self_push(0)]);
        assert!(!reg_traces_to_a_parameter(&c, 2, 0));
    }

    #[test]
    fn a_capture_register_traces_to_a_parameter_the_same_way_a_param_does() {
        // ncaps=1: register 0 is a closure capture, not chunk-local either.
        let mut c = chunk(vec![Op::Move(1, 0), self_push(1)]);
        c.ncaps = 1;
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_field_read_out_of_a_parameter_traces_to_it_just_like_a_move(
    ) {
        // production-hardening PR-it819: `fun f(b: Box) { var xs = b.items;
        // xs = xs.push(item) }` -- register 0 is the sole parameter `b`,
        // GetFieldNamed reads `b.items` into register 1, and register 1 is
        // never itself `Move`'d from anywhere. Before this fix, the trace
        // only followed `Op::Move` edges, so it dead-ended here and wrongly
        // reported "safe" -- even though `xs`'s VALUE is the literal same
        // heap object as `b`'s stored field (k_field/k_field_named copy the
        // KValue with no clone/refcount bump), exactly as aliased as a
        // direct Move from the parameter would be.
        let c = chunk_with_params(1, vec![Op::GetFieldNamed { dst: 1, obj: 0, name: 0 }, self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_field_read_via_the_indexed_getfield_variant_also_traces_to_a_parameter() {
        // Same as above but for the tuple/positional `Op::GetField` variant
        // (named-field access desugars to one or the other depending on
        // context) -- both must be covered, not just GetFieldNamed.
        let c = chunk_with_params(1, vec![Op::GetField { dst: 1, obj: 0, idx: 0 }, self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_field_read_out_of_a_fresh_chunk_local_record_does_not_trace_to_a_parameter() {
        // fun f() { let b = Box(items: []); var xs = b.items; xs =
        // xs.push(item) } -- `b` itself is chunk-local (built via MakeCtor,
        // never a parameter or capture), so extracting a field from it and
        // mutating that field's value in place is genuinely safe: no other
        // live reference to it exists. This must NOT be disqualified by the
        // new GetField/GetFieldNamed edge, mirroring the existing
        // `a_fresh_chunk_local_value_moved_into_its_bound_register_does_not_trace_to_a_parameter`
        // check for the Move case.
        let c = chunk_with_params(
            0,
            vec![
                Op::MakeCtor { dst: 0, ctor: 0, start: 1, len: 0 },
                Op::GetFieldNamed { dst: 1, obj: 0, name: 0 },
                self_push(1),
            ],
        );
        assert!(!reg_traces_to_a_parameter(&c, 2, 1));
    }

    /// A REAL, SEVENTH untracked edge found+fixed (production-hardening
    /// PR-it847, the TWENTY-SEVENTH broad Explore survey): `fun f(p: List[
    /// Int]) { let b = Box(items: p); var xs = b.items; xs = xs.push(item)
    /// }` -- `b` is chunk-local (built via `MakeCtor`), so the PRIOR test
    /// above (`a_field_read_out_of_a_fresh_chunk_local_record_does_not_
    /// trace_to_a_parameter`) would wrongly conclude this is ALSO safe --
    /// but here `b`'s `items` field was constructed FROM the parameter `p`
    /// itself (`MakeCtor`'s own `start..start+len` staging window includes
    /// register 0, which traces directly to the parameter), so extracting
    /// `items` back out and mutating it in place aliases the CALLER's own
    /// `p`. Deliberately uses STRICTLY INCREASING, never-reused register
    /// numbers (0, 1, 2, 3) so this test is NOT accidentally rescued by the
    /// depth-bound cycle guard the way every KUPL-source-level compilation
    /// attempt was during this bug's OWN investigation (see this file's
    /// `reg_traces_to_a_parameter`/`go` doc comment on its new `MakeCtor`
    /// arm for the full story) -- this test exercises the underlying
    /// analysis gap directly, independent of compile.rs's specific
    /// register-allocation behavior.
    #[test]
    fn a_field_read_out_of_a_ctor_built_from_a_parameter_traces_to_it() {
        let c = chunk_with_params(
            1,
            vec![
                Op::Move(1, 0),
                Op::MakeCtor { dst: 2, ctor: 0, start: 1, len: 1 },
                Op::GetFieldNamed { dst: 3, obj: 2, name: 0 },
                self_push(3),
            ],
        );
        assert!(reg_traces_to_a_parameter(&c, 3, 3));
    }

    #[test]
    fn a_move_then_field_read_chain_from_a_parameter_is_still_caught() {
        // var tmp = b; var xs = tmp.items; xs = xs.push(item) -- a Move hop
        // followed by a field-read hop, both from the same parameter.
        let c = chunk_with_params(
            1,
            vec![Op::Move(1, 0), Op::GetFieldNamed { dst: 2, obj: 1, name: 0 }, self_push(2)],
        );
        assert!(reg_traces_to_a_parameter(&c, 2, 2));
    }

    #[test]
    fn a_for_loop_iteration_variable_always_traces_to_a_parameter() {
        // production-hardening PR-it820: `for x in xs { var y = x; y =
        // y.push(item) }` -- xs is a PLAIN CHUNK-LOCAL list (nparams=0), not
        // a parameter, so this is UNLIKE every other case in this module:
        // there is no register to recurse into and prove safe/unsafe, since
        // k_iter_get (cgen.rs) hands back a SHALLOW alias of one of xs's own
        // elements with no way for this window-bounded, loop-body-scoped
        // analysis to know whether xs is read again after the loop. Any
        // register written by Op::IterGet must be treated as
        // unconditionally unsafe, exactly like reaching a real parameter.
        let c = chunk_with_params(0, vec![Op::IterGet { dst: 0, iter: 5, idx: 6 }, self_push(0)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 0));
    }

    #[test]
    fn a_move_from_an_iter_get_result_is_still_caught() {
        // var y = x; y = y.push(item) -- one Move hop away from the
        // IterGet-written register, same as the field-read chain test
        // above but for the iteration-variable case.
        let c = chunk_with_params(0, vec![Op::IterGet { dst: 0, iter: 5, idx: 6 }, Op::Move(1, 0), self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 2, 1));
    }

    #[test]
    fn a_component_state_read_always_traces_to_a_parameter() {
        // production-hardening PR-it822: `var xs = items; xs = xs.push(item)`
        // -- items is a component STATE field, not a parameter or capture,
        // so nparams=0 here too, same shape as the IterGet case above.
        // Unlike GetField/IterGet, Op::StateGet has no source REGISTER at
        // all (it reads directly from the instance's state array by a
        // compile-time slot index) -- so there's nothing to recurse into;
        // any register it writes must be unconditionally unsafe, since a
        // state slot's value can be read again by ANY future handler
        // invocation on this same instance, for the component's entire
        // remaining lifetime.
        let c = chunk_with_params(0, vec![Op::StateGet(0, 0), self_push(0)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 0));
    }

    #[test]
    fn a_move_from_a_state_get_result_is_still_caught() {
        // var xs = items; xs = xs.push(item) -- via one Move hop, same as
        // the field-read and iteration-variable chain tests above.
        let c = chunk_with_params(0, vec![Op::StateGet(0, 0), Op::Move(1, 0), self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 2, 1));
    }

    #[test]
    fn a_call_result_that_could_be_a_pass_through_parameter_always_traces_to_a_parameter() {
        // production-hardening PR-it823: `fun identity(a) { a }` called as
        // `xs = identity(ys); xs = xs.push(item)` where `ys` traces to a
        // parameter -- this analysis is intraprocedural, so it cannot see
        // that `identity`'s body just returns its own argument unchanged;
        // any `Op::Call` result must be treated as unconditionally unsafe.
        let c = chunk_with_params(1, vec![Op::Call { dst: 1, fun: 0, start: 0, argc: 1 }, self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_call_value_closure_result_always_traces_to_a_parameter() {
        // Same as above but for a closure invoked via Op::CallValue --
        // structurally identical concern (an identity closure).
        let c = chunk_with_params(1, vec![Op::CallValue { dst: 1, f: 0, start: 0, argc: 1 }, self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_method_call_result_always_traces_to_a_parameter() {
        // production-hardening PR-it823: a builtin OR component method can
        // return its receiver/argument unchanged on some path (the exact
        // PR-it811 finding for `Str.replace_first`'s no-match case) --
        // `xs = ys.someMethod(); xs = xs.push(item)` must be unconditionally
        // unsafe for the SAME reason `Call`'s result is.
        let c = chunk_with_params(
            1,
            vec![Op::Method { dst: 1, recv: 0, name: 0, start: 0, argc: 0 }, self_push(1)],
        );
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_move_from_a_call_result_is_still_caught() {
        // var xs = identity(ys); var y2 = xs; y2 = y2.push(item) -- one Move
        // hop away from the Call-written register.
        let c = chunk_with_params(
            1,
            vec![Op::Call { dst: 1, fun: 0, start: 0, argc: 1 }, Op::Move(2, 1), self_push(2)],
        );
        assert!(reg_traces_to_a_parameter(&c, 2, 2));
    }

    #[test]
    fn a_with_field_of_a_parameter_traces_to_it_via_its_obj_register() {
        // production-hardening PR-it824: `(b with x: 99).items` -- reading
        // a DIFFERENT field than the one just updated -- compiles to
        // WithField then GetFieldNamed. k_with_field (cgen.rs) shallow-
        // copies every OTHER field from `obj`, so if `obj` (here, the
        // parameter `b`) traces to a parameter, WithField's `dst` aliases
        // `obj`'s untouched fields exactly as directly as GetField would.
        // Unlike IterGet/StateGet/Call/Method, WithField DOES have a real
        // source register (`obj`) to recurse into, mirroring GetField.
        let c = chunk_with_params(1, vec![Op::WithField { dst: 1, obj: 0, name: 0, value: 0 }, self_push(1)]);
        assert!(reg_traces_to_a_parameter(&c, 1, 1));
    }

    #[test]
    fn a_with_field_of_a_fresh_chunk_local_ctor_does_not_trace_to_a_parameter() {
        // fun f() { let b = Box(items: []); var xs = (b with x: 1).items;
        // xs = xs.push(item) } -- b itself is chunk-local (MakeCtor, never
        // a parameter), so a field carried through unchanged really is
        // safe to mutate. Must NOT be disqualified, mirroring the existing
        // GetField/IterGet "fresh chunk-local" preservation checks.
        let c = chunk_with_params(
            0,
            vec![
                Op::MakeCtor { dst: 0, ctor: 0, start: 1, len: 0 },
                Op::WithField { dst: 1, obj: 0, name: 0, value: 0 },
                self_push(1),
            ],
        );
        assert!(!reg_traces_to_a_parameter(&c, 2, 1));
    }

    #[test]
    fn a_get_field_through_a_with_field_of_a_parameter_is_still_caught() {
        // (b with x: 99).items -- a GetFieldNamed hop reading a WithField's
        // dst, which itself traces back to the parameter `b`.
        let c = chunk_with_params(
            1,
            vec![
                Op::WithField { dst: 1, obj: 0, name: 0, value: 0 },
                Op::GetFieldNamed { dst: 2, obj: 1, name: 1 },
                self_push(2),
            ],
        );
        assert!(reg_traces_to_a_parameter(&c, 2, 2));
    }
}
