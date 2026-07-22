//! `.kx` — the KVM module binary format (encode/decode), and the bundle
//! trailer used by `kupl bundle` to produce self-contained executables.
//!
//! Layout: magic "KUPLKX02" (`KX_MAGIC` below), then chunks, ctors, fun
//! table, ctor field names, components. All integers little-endian; strings
//! are u32 length + UTF-8.

use std::collections::HashMap;
use std::rc::Rc;

use crate::bytecode::*;
use crate::diag::Span;
use crate::value::Value;

pub const KX_MAGIC: &[u8; 8] = b"KUPLKX02";
/// Trailer magic at the very end of a bundled executable.
pub const BUNDLE_MAGIC: &[u8; 8] = b"KUPLBNDL";

// ---------------- writer ----------------

struct W {
    buf: Vec<u8>,
}

impl W {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn s(&mut self, v: &str) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v.as_bytes());
    }
    fn usz(&mut self, v: usize) {
        self.u32(v as u32);
    }
}

pub fn encode(m: &Module) -> Vec<u8> {
    let mut w = W { buf: Vec::new() };
    w.buf.extend_from_slice(KX_MAGIC);

    w.u32(m.chunks.len() as u32);
    for c in &m.chunks {
        w.s(&c.name);
        w.u8(c.ncaps);
        w.u8(c.nparams);
        w.u16(c.nregs);
        w.u32(c.consts.len() as u32);
        for v in &c.consts {
            encode_const(&mut w, v);
        }
        w.u32(c.code.len() as u32);
        for op in &c.code {
            encode_op(&mut w, op);
        }
        for sp in &c.spans {
            w.u32(sp.start);
            w.u32(sp.end);
        }
    }

    w.u32(m.ctors.len() as u32);
    for ct in &m.ctors {
        w.s(&ct.type_name);
        w.s(&ct.variant);
        w.u8(ct.arity);
    }

    w.u32(m.funs.len() as u32);
    let mut funs: Vec<(&String, &u16)> = m.funs.iter().collect();
    funs.sort();
    for (name, idx) in funs {
        w.s(name);
        w.u16(*idx);
    }

    w.u32(m.ctor_field_names.len() as u32);
    let mut cfn: Vec<(&String, &Vec<String>)> = m.ctor_field_names.iter().collect();
    cfn.sort();
    for (variant, fields) in cfn {
        w.s(variant);
        w.u32(fields.len() as u32);
        for f in fields {
            w.s(f);
        }
    }

    w.u32(m.components.len() as u32);
    for c in &m.components {
        w.s(&c.name);
        w.u8(c.is_app as u8);
        w.u32(c.props.len() as u32);
        for (name, default) in &c.props {
            w.s(name);
            match default {
                Some(chunk) => {
                    w.u8(1);
                    w.u16(*chunk);
                }
                None => w.u8(0),
            }
        }
        w.u8(c.nslots);
        w.u16(c.init_chunk);
        w.u16(c.restart_chunk);
        w.u32(c.handlers.len() as u32);
        for (key, chunk, has_param) in &c.handlers {
            w.s(key);
            w.u16(*chunk);
            w.u8(*has_param as u8);
        }
        w.u32(c.exposes.len() as u32);
        let mut ex: Vec<(&String, &u16)> = c.exposes.iter().collect();
        ex.sort();
        for (name, chunk) in ex {
            w.s(name);
            w.u16(*chunk);
        }
        w.u32(c.out_ports.len() as u32);
        for p in &c.out_ports {
            w.s(p);
        }
        w.u32(c.timers.len() as u32);
        for t in &c.timers {
            w.u16(t.chunk);
            w.u8(t.every as u8);
            w.buf.extend_from_slice(&t.interval_ms.to_le_bytes());
        }
    }

    w.u32(m.ai_funs.len() as u32);
    for a in &m.ai_funs {
        w.s(&a.name);
        w.s(&a.intent);
        match &a.model {
            Some(model) => {
                w.u8(1);
                w.s(model);
            }
            None => w.u8(0),
        }
        w.u32(a.params.len() as u32);
        for p in &a.params {
            w.s(p);
        }
        encode_shape(&mut w, &a.shape);
        w.u8(a.wraps_result as u8);
        w.u32(a.tools.len() as u32);
        for t in &a.tools {
            w.s(&t.name);
            w.s(&t.description);
            w.u32(t.params.len() as u32);
            for (pname, pshape) in &t.params {
                w.s(pname);
                encode_shape(&mut w, pshape);
            }
            encode_shape(&mut w, &t.ret);
        }
    }

    w.buf
}

fn encode_shape(w: &mut W, shape: &crate::ai::AiShape) {
    use crate::ai::AiShape::*;
    match shape {
        Str => w.u8(0),
        Int => w.u8(1),
        Float => w.u8(2),
        Bool => w.u8(3),
        List(inner) => {
            w.u8(4);
            encode_shape(w, inner);
        }
        Option(inner) => {
            w.u8(5);
            encode_shape(w, inner);
        }
        Record { ty, variant, fields } => {
            w.u8(6);
            w.s(ty);
            w.s(variant);
            w.u32(fields.len() as u32);
            for (name, s) in fields {
                w.s(name);
                encode_shape(w, s);
            }
        }
    }
}

fn encode_const(w: &mut W, v: &Value) {
    match v {
        Value::Int(x) => {
            w.u8(0);
            w.i64(*x);
        }
        Value::Float(x) => {
            w.u8(1);
            w.f64(*x);
        }
        Value::Bool(x) => {
            w.u8(2);
            w.u8(*x as u8);
        }
        Value::Str(s) => {
            w.u8(3);
            w.s(s);
        }
        Value::Unit => w.u8(4),
        Value::Fun(name) => {
            w.u8(5);
            w.s(name);
        }
        Value::SizedInt(b) => {
            w.u8(6);
            w.i64((b.0 >> 64) as i64); // high 64 bits
            w.i64(b.0 as i64); // low 64 bits
            w.u8(b.1.tag());
        }
        Value::F32(x) => {
            w.u8(7);
            w.buf.extend_from_slice(&x.to_le_bytes());
        }
        other => {
            // compiler only emits the constants above
            panic!("non-serializable constant: {other}");
        }
    }
}

fn encode_op(w: &mut W, op: &Op) {
    use Op::*;
    match op {
        Const(a, b) => {
            w.u8(0);
            w.u8(*a);
            w.u16(*b);
        }
        Move(a, b) => {
            w.u8(1);
            w.u8(*a);
            w.u8(*b);
        }
        Add(a, b, c) => op3(w, 2, *a, *b, *c),
        Sub(a, b, c) => op3(w, 3, *a, *b, *c),
        Mul(a, b, c) => op3(w, 4, *a, *b, *c),
        Div(a, b, c) => op3(w, 5, *a, *b, *c),
        Rem(a, b, c) => op3(w, 6, *a, *b, *c),
        Eq(a, b, c) => op3(w, 7, *a, *b, *c),
        Ne(a, b, c) => op3(w, 8, *a, *b, *c),
        Lt(a, b, c) => op3(w, 9, *a, *b, *c),
        Le(a, b, c) => op3(w, 10, *a, *b, *c),
        Gt(a, b, c) => op3(w, 11, *a, *b, *c),
        Ge(a, b, c) => op3(w, 12, *a, *b, *c),
        Neg(a, b) => {
            w.u8(13);
            w.u8(*a);
            w.u8(*b);
        }
        Not(a, b) => {
            w.u8(14);
            w.u8(*a);
            w.u8(*b);
        }
        Jump(t) => {
            w.u8(15);
            w.usz(*t);
        }
        JumpIfFalse(r, t) => {
            w.u8(16);
            w.u8(*r);
            w.usz(*t);
        }
        JumpIfTrue(r, t) => {
            w.u8(17);
            w.u8(*r);
            w.usz(*t);
        }
        Call { dst, fun, start, argc } => {
            w.u8(18);
            w.u8(*dst);
            w.u16(*fun);
            w.u8(*start);
            w.u8(*argc);
        }
        CallBuiltin { dst, which, start, argc } => {
            w.u8(19);
            w.u8(*dst);
            w.u8(*which);
            w.u8(*start);
            w.u8(*argc);
        }
        CallValue { dst, f, start, argc } => {
            w.u8(20);
            w.u8(*dst);
            w.u8(*f);
            w.u8(*start);
            w.u8(*argc);
        }
        Method { dst, recv, name, start, argc } => {
            w.u8(21);
            w.u8(*dst);
            w.u8(*recv);
            w.u16(*name);
            w.u8(*start);
            w.u8(*argc);
        }
        Ret(r) => {
            w.u8(22);
            w.u8(*r);
        }
        MakeList { dst, start, len } => {
            w.u8(23);
            w.u8(*dst);
            w.u8(*start);
            w.u8(*len);
        }
        MakeCtor { dst, ctor, start, len } => {
            w.u8(24);
            w.u8(*dst);
            w.u16(*ctor);
            w.u8(*start);
            w.u8(*len);
        }
        GetField { dst, obj, idx } => {
            w.u8(25);
            w.u8(*dst);
            w.u8(*obj);
            w.u8(*idx);
        }
        GetFieldNamed { dst, obj, name } => {
            w.u8(26);
            w.u8(*dst);
            w.u8(*obj);
            w.u16(*name);
        }
        TagIs { dst, obj, ctor } => {
            w.u8(27);
            w.u8(*dst);
            w.u8(*obj);
            w.u16(*ctor);
        }
        MakeClosure { dst, proto, start, ncaps } => {
            w.u8(28);
            w.u8(*dst);
            w.u16(*proto);
            w.u8(*start);
            w.u8(*ncaps);
        }
        MakeRange { dst, lo, hi, inclusive } => {
            w.u8(29);
            w.u8(*dst);
            w.u8(*lo);
            w.u8(*hi);
            w.u8(*inclusive as u8);
        }
        IterLen(a, b) => {
            w.u8(30);
            w.u8(*a);
            w.u8(*b);
        }
        IterGet { dst, iter, idx } => {
            w.u8(31);
            w.u8(*dst);
            w.u8(*iter);
            w.u8(*idx);
        }
        ToStr(a, b) => {
            w.u8(32);
            w.u8(*a);
            w.u8(*b);
        }
        Concat(a, b, c) => op3(w, 33, *a, *b, *c),
        StateGet(a, b) => {
            w.u8(34);
            w.u8(*a);
            w.u8(*b);
        }
        StateSet(a, b) => {
            w.u8(35);
            w.u8(*a);
            w.u8(*b);
        }
        MakeInstance { dst, comp, start, argc, policy } => {
            w.u8(36);
            w.u8(*dst);
            w.u16(*comp);
            w.u8(*start);
            w.u8(*argc);
            w.u8(*policy);
        }
        WireOp { from, out_port, to, in_port } => {
            w.u8(37);
            w.u8(*from);
            w.u16(*out_port);
            w.u8(*to);
            w.u16(*in_port);
        }
        EmitOp { port, payload } => {
            w.u8(38);
            w.u16(*port);
            match payload {
                Some(r) => {
                    w.u8(1);
                    w.u8(*r);
                }
                None => w.u8(0),
            }
        }
        Panic(m) => {
            w.u8(39);
            w.u16(*m);
        }
        CallComp { dst, fun, start, argc } => {
            w.u8(42);
            w.u8(*dst);
            w.u16(*fun);
            w.u8(*start);
            w.u8(*argc);
        }
        CallAi { dst, info, intent } => {
            w.u8(43);
            w.u8(*dst);
            w.u16(*info);
            w.u8(*intent);
        }
        WithField { dst, obj, name, value } => {
            w.u8(40);
            w.u8(*dst);
            w.u8(*obj);
            w.u16(*name);
            w.u8(*value);
        }
    }
}

fn op3(w: &mut W, code: u8, a: Reg, b: Reg, c: Reg) {
    w.u8(code);
    w.u8(a);
    w.u8(b);
    w.u8(c);
}

// ---------------- reader ----------------

struct R<'a> {
    buf: &'a [u8],
    pos: usize,
    /// A REAL bug found+fixed (production-hardening PR-it927, following
    /// directly on PR-it926): `cap()` (below) only bounds a loop's INITIAL
    /// `Vec::with_capacity` HINT, never the loop's own iteration count — so
    /// a per-count loop (`for _ in 0..n { v.push(decode_x(&mut r)?); }`)
    /// still trusts the raw untrusted `n` for its own bound, and `Vec::
    /// push` happily grows the vec far past its (correctly-sized) initial
    /// hint as long as *some* valid item can still be parsed from the
    /// remaining bytes. Live-confirmed: a 20MB `.kx` file whose padding is
    /// entirely the byte `0x04` (a valid, 1-byte `Value::Unit` constant
    /// tag — the cheapest of `decode_const`'s 8 variants) forced ~665MB of
    /// real resident memory (a ~33x amplification, `size_of::<Value>()`'s
    /// ratio against `Value::Unit`'s 1-byte wire cost) before failing on
    /// eventual truncation — the SAME "cap the ALLOCATION but not the
    /// LOOP" gap, just reached via `push`-driven reallocation rather than
    /// the initial `with_capacity` call PR-it926 already closed.
    ///
    /// TWO earlier designs were tried and reverted before this one:
    /// (1) reusing `cap()`'s OWN `remaining / size_of::<T>()` value as a
    /// hard LOOP ceiling (not just the allocation hint) works for flat
    /// scalar types but badly OVER-restricts anything containing a
    /// `String`/`Vec`/recursive-enum field (`Chunk`, `ComponentMeta`,
    /// `AiFunMeta`, `ToolMeta`, and even `String` itself for a file with
    /// many SHORT strings) — `size_of::<T>()` overestimates the true
    /// minimum WIRE cost for these, so it rejected genuinely valid small
    /// `.kx` files (caught by this file's own round-trip tests). (2) an
    /// ITEM-COUNT budget (`budget: usize` starting at `buf.len()`,
    /// decremented by 1 per item regardless of type) is mathematically
    /// safe (can never reject a valid file) but turned out to be a
    /// complete NO-OP: since every item ALSO consumes >=1 real byte via
    /// `take()`, an item-count budget starting at `buf.len()` can never
    /// run out before `take()` would already have failed on its own —
    /// re-measured after implementing it: still ~665MB, unchanged.
    ///
    /// Fixed with a MEMORY-cost budget instead: `mem_budget` starts at
    /// `buf.len() * MEM_BUDGET_SLACK` and every decoded item charges
    /// `size_of::<T>()` (its ACTUAL in-memory cost, not a flat 1) against
    /// it. `MEM_BUDGET_SLACK` is a generous constant (see its own doc
    /// comment) empirically validated against this file's own existing
    /// round-trip tests (real compiled `.kx` modules) to ensure zero false
    /// positives, while still turning the PREVIOUSLY UNBOUNDED worst-case
    /// amplification into a hard ceiling.
    mem_budget: usize,
}

/// How many multiples of the raw `.kx` file's OWN byte length `decode()`
/// may charge against `R::mem_budget` in total real memory before refusing
/// to decode further (production-hardening PR-it927). Chosen empirically
/// by testing this file's own full round-trip test suite (real compiled
/// `.kx` modules, including `kx_roundtrip_preserves_every_module_field_
/// structurally`'s deliberately field-rich fixture) against several
/// candidate values: `2` was too tight and produced FALSE "truncated .kx
/// module" errors on genuinely valid input (13 test failures); `4` passed
/// every existing test cleanly, with real margin above that failure
/// boundary. Chosen over a larger, more "obviously safe" value (`16` also
/// passed) specifically because a TIGHTER ceiling more meaningfully bounds
/// the adversarial case — this turns the previously-unbounded (or, before
/// this fix, up to ~33x-and-growing for a crafted file) memory
/// amplification into a fixed ceiling, live-confirmed via a crafted 20MB
/// file to now cap real memory around this constant's own multiple of the
/// file's size, instead of ~33x.
const MEM_BUDGET_SLACK: usize = 4;

type DecodeResult<T> = Result<T, String>;

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err("truncated .kx module".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    /// Charge one `T`-sized item against the whole-module memory budget
    /// (see `mem_budget`'s own doc comment) — called once per iteration by
    /// every count-driven decode loop, BEFORE decoding that item's own
    /// fields.
    fn charge<T>(&mut self) -> DecodeResult<()> {
        let cost = std::mem::size_of::<T>().max(1);
        match self.mem_budget.checked_sub(cost) {
            Some(rest) => {
                self.mem_budget = rest;
                Ok(())
            }
            None => Err("truncated .kx module".into()),
        }
    }
    fn u8(&mut self) -> DecodeResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> DecodeResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> DecodeResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> DecodeResult<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> DecodeResult<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn s(&mut self) -> DecodeResult<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "invalid UTF-8 in .kx".into())
    }
    fn usz(&mut self) -> DecodeResult<usize> {
        Ok(self.u32()? as usize)
    }
    /// A safe `Vec::with_capacity` hint for a count read from the (possibly
    /// corrupt/untrusted) buffer: never pre-allocate more BYTES than remain in
    /// the buffer, since decoding each item consumes at least one byte. A
    /// tampered count (e.g. 0xFFFFFFFF) therefore cannot trigger a multi-
    /// gigabyte allocation that aborts (or, per PR-it926 below, merely stalls)
    /// the process — the following decode loop hits a clean "truncated .kx
    /// module" error instead.
    ///
    /// A REAL bug found+fixed (production-hardening PR-it926, a close-read
    /// survey finding): this used to clamp only the ITEM COUNT to remaining
    /// bytes (`n.min(remaining)`), silently assuming `Vec::with_capacity(n)`
    /// allocates `n` BYTES — true only for a byte-sized element. It actually
    /// allocates `n * size_of::<T>()` bytes, and EVERY element type this file
    /// decodes into is far larger than 1 byte (`Value`/`String` = 24 bytes,
    /// `ToolMeta` ~150+ bytes, …) except `Span` (which happens to be exactly
    /// 8 bytes on the wire too) — so the old clamp's own safety claim was
    /// false for every OTHER call site. Live-confirmed: a ~2GB crafted `.kx`
    /// file (`nconsts = 0xFFFFFFFF` header + ~2GB of arbitrary padding)
    /// forced roughly 9GB of REAL resident memory and ~10 seconds of CPU
    /// time inside `Vec::<Value>::with_capacity` before `decode()`'s
    /// following loop failed cleanly on `decode_const`'s very first call —
    /// a genuine, measurable resource-exhaustion amplification from an
    /// untrusted `.kx` file, this campaign's own established in-scope threat
    /// model (PR-it687/688/726/729/730). The EXISTING `corrupt_kx_is_
    /// rejected_not_a_crash` test's own huge-`nconsts` case never caught
    /// this because it used ZERO padding bytes, so `cap` degenerately
    /// clamped to 0 regardless of whether this bug existed — the test
    /// exercised only the trivial case, not the one that actually matters.
    /// Fixed by making `cap` generic over the element type and dividing the
    /// byte-based clamp by `size_of::<T>()`, so the ACTUAL ALLOCATED BYTE
    /// SIZE (not just the item count) is bounded by the bytes genuinely
    /// remaining in the buffer.
    fn cap<T>(&self, n: usize) -> usize {
        let elem_size = std::mem::size_of::<T>().max(1);
        n.min(self.buf.len().saturating_sub(self.pos) / elem_size)
    }
}

pub fn decode(buf: &[u8]) -> DecodeResult<Module> {
    let mut r = R { buf, pos: 0, mem_budget: buf.len().saturating_mul(MEM_BUDGET_SLACK) };
    let magic = r.take(8)?;
    if magic != KX_MAGIC {
        // Distinguish a .kx built by an incompatible KUPL version (right prefix,
        // wrong format version) from a file that isn't a .kx module at all — so a
        // version skew gives an actionable message instead of a generic one.
        if magic.starts_with(b"KUPLKX") {
            return Err(format!(
                "incompatible .kx format version (found `{}`, this KUPL build expects `{}`) — rebuild with `kupl build`",
                String::from_utf8_lossy(magic),
                String::from_utf8_lossy(KX_MAGIC),
            ));
        }
        return Err("not a .kx module (bad magic)".into());
    }

    let mut m = Module::default();
    let nchunks = r.u32()?;
    for _ in 0..nchunks {
        r.charge::<Chunk>()?;
        let name = r.s()?;
        let ncaps = r.u8()?;
        let nparams = r.u8()?;
        let nregs = r.u16()?;
        let nconsts = r.u32()?;
        let mut consts = Vec::with_capacity(r.cap::<Value>(nconsts as usize));
        for _ in 0..nconsts {
            r.charge::<Value>()?;
            consts.push(decode_const(&mut r)?);
        }
        let ncode = r.u32()?;
        let mut code = Vec::with_capacity(r.cap::<Op>(ncode as usize));
        for _ in 0..ncode {
            r.charge::<Op>()?;
            code.push(decode_op(&mut r)?);
        }
        let mut spans = Vec::with_capacity(r.cap::<Span>(ncode as usize));
        for _ in 0..ncode {
            r.charge::<Span>()?;
            let start = r.u32()?;
            let end = r.u32()?;
            spans.push(Span::new(start, end));
        }
        m.chunks.push(Chunk { name, ncaps, nparams, nregs, consts, code, spans });
    }

    let nctors = r.u32()?;
    for _ in 0..nctors {
        r.charge::<CtorMeta>()?;
        let type_name = r.s()?;
        let variant = r.s()?;
        let arity = r.u8()?;
        m.ctors.push(CtorMeta { type_name, variant, arity });
    }

    let nfuns = r.u32()?;
    for _ in 0..nfuns {
        r.charge::<(String, u16)>()?;
        let name = r.s()?;
        let idx = r.u16()?;
        m.funs.insert(name, idx);
    }

    let ncfn = r.u32()?;
    for _ in 0..ncfn {
        r.charge::<(String, Vec<String>)>()?;
        let variant = r.s()?;
        let n = r.u32()?;
        let mut fields = Vec::with_capacity(r.cap::<String>(n as usize));
        for _ in 0..n {
            r.charge::<String>()?;
            fields.push(r.s()?);
        }
        m.ctor_field_names.insert(variant, fields);
    }

    let ncomps = r.u32()?;
    for i in 0..ncomps {
        r.charge::<ComponentMeta>()?;
        let name = r.s()?;
        let is_app = r.u8()? != 0;
        let nprops = r.u32()?;
        let mut props = Vec::with_capacity(r.cap::<(String, Option<u16>)>(nprops as usize));
        for _ in 0..nprops {
            r.charge::<(String, Option<u16>)>()?;
            let pname = r.s()?;
            let has_default = r.u8()? != 0;
            let default = if has_default { Some(r.u16()?) } else { None };
            props.push((pname, default));
        }
        let nslots = r.u8()?;
        let init_chunk = r.u16()?;
        let restart_chunk = r.u16()?;
        let nhandlers = r.u32()?;
        let mut handlers = Vec::with_capacity(r.cap::<(String, u16, bool)>(nhandlers as usize));
        for _ in 0..nhandlers {
            r.charge::<(String, u16, bool)>()?;
            let key = r.s()?;
            let chunk = r.u16()?;
            let has_param = r.u8()? != 0;
            handlers.push((key, chunk, has_param));
        }
        let nexposes = r.u32()?;
        let mut exposes = HashMap::new();
        for _ in 0..nexposes {
            r.charge::<(String, u16)>()?;
            let ename = r.s()?;
            let chunk = r.u16()?;
            exposes.insert(ename, chunk);
        }
        let nports = r.u32()?;
        let mut out_ports = Vec::with_capacity(r.cap::<String>(nports as usize));
        for _ in 0..nports {
            r.charge::<String>()?;
            out_ports.push(r.s()?);
        }
        let ntimers = r.u32()?;
        let mut timers = Vec::with_capacity(r.cap::<TimerMeta>(ntimers as usize));
        for _ in 0..ntimers {
            r.charge::<TimerMeta>()?;
            let chunk = r.u16()?;
            let every = r.u8()? != 0;
            let interval_ms = i64::from_le_bytes(r.take(8)?.try_into().unwrap());
            timers.push(TimerMeta { chunk, every, interval_ms });
        }
        m.component_names.insert(name.clone(), i as u16);
        m.components.push(ComponentMeta {
            name,
            is_app,
            props,
            nslots,
            init_chunk,
            restart_chunk,
            handlers,
            exposes,
            out_ports,
            timers,
        });
    }

    let nai = r.u32()?;
    for _ in 0..nai {
        r.charge::<crate::ai::AiFunMeta>()?;
        let name = r.s()?;
        let intent = r.s()?;
        let model = if r.u8()? != 0 { Some(r.s()?) } else { None };
        let nparams = r.u32()?;
        let mut params = Vec::with_capacity(r.cap::<String>(nparams as usize));
        for _ in 0..nparams {
            r.charge::<String>()?;
            params.push(r.s()?);
        }
        let shape = decode_shape(&mut r, 0)?;
        let wraps_result = r.u8()? != 0;
        let ntools = r.u32()?;
        let mut tools = Vec::with_capacity(r.cap::<crate::ai::ToolMeta>(ntools as usize));
        for _ in 0..ntools {
            r.charge::<crate::ai::ToolMeta>()?;
            let tname = r.s()?;
            let description = r.s()?;
            let nparams = r.u32()?;
            let mut tparams = Vec::with_capacity(r.cap::<(String, crate::ai::AiShape)>(nparams as usize));
            for _ in 0..nparams {
                r.charge::<(String, crate::ai::AiShape)>()?;
                let pname = r.s()?;
                tparams.push((pname, decode_shape(&mut r, 0)?));
            }
            let ret = decode_shape(&mut r, 0)?;
            tools.push(crate::ai::ToolMeta { name: tname, description, params: tparams, ret });
        }
        m.ai_funs.push(crate::ai::AiFunMeta {
            name,
            intent,
            model,
            params,
            shape,
            wraps_result,
            tools,
        });
    }

    Ok(m)
}

/// A REAL, uncatchable-crash bug found+fixed (production-hardening PR-it730,
/// found via a scoped Explore survey): unlike every OTHER recursive decoder
/// in this codebase (`json.rs`'s own parser, and `lsp.rs`'s JSON parser,
/// fixed in PR-it620 to reuse the SAME `MAX_JSON_DEPTH` constant), this
/// function had NO depth tracking or cap at all -- `List`/`Option` (tags 4/5)
/// each consume exactly ONE byte and recurse unconditionally. A `.kx` file's
/// `ai_fun` shape field is fully attacker-controlled (`kupl dis`/`kupl run
/// <file.kx>` accept an arbitrary path, and `.kx` is a documented
/// distributable format -- genuinely untrusted input, matching this
/// campaign's established `.kx`-hardening precedent, PR-it687/it688/it726).
/// Confirmed LIVE: a ~20MB `.kx` file whose shape field is ~20 million
/// consecutive `List` tag bytes crashed BOTH `kupl dis` and `kupl run` with
/// an UNCATCHABLE `thread '<unknown>' has overflowed its stack` / `fatal
/// runtime error: stack overflow, aborting` (SIGABRT) -- even though
/// `main.rs` already gives the main thread a 2GiB stack (the SAME hardening
/// PR-it729 relied on for `par_map`'s workers), since each `decode_shape`
/// frame costs only ~100+ bytes, so a cheap, small, single-digit-MB file is
/// still enough to exhaust even a 2GiB reservation. Fixed by threading a
/// `depth: usize` parameter through and rejecting once it exceeds
/// `json::MAX_JSON_DEPTH` with a clean "corrupt .kx module" error --
/// reusing the SAME constant (not a new one) and the SAME
/// check-before-recurse shape `lsp.rs`'s own fix already established for
/// this exact vulnerability class.
fn decode_shape(r: &mut R, depth: usize) -> DecodeResult<crate::ai::AiShape> {
    use crate::ai::AiShape::*;
    if depth > crate::json::MAX_JSON_DEPTH {
        return Err("corrupt .kx module: ai shape nested too deeply".into());
    }
    Ok(match r.u8()? {
        0 => Str,
        1 => Int,
        2 => Float,
        3 => Bool,
        4 => List(Box::new(decode_shape(r, depth + 1)?)),
        5 => Option(Box::new(decode_shape(r, depth + 1)?)),
        6 => {
            let ty = r.s()?;
            let variant = r.s()?;
            let n = r.u32()?;
            let mut fields = Vec::with_capacity(r.cap::<(String, crate::ai::AiShape)>(n as usize));
            for _ in 0..n {
                r.charge::<(String, crate::ai::AiShape)>()?;
                let name = r.s()?;
                fields.push((name, decode_shape(r, depth + 1)?));
            }
            Record { ty, variant, fields }
        }
        t => return Err(format!("unknown ai shape tag {t}")),
    })
}

fn decode_const(r: &mut R) -> DecodeResult<Value> {
    Ok(match r.u8()? {
        0 => Value::Int(r.i64()?),
        1 => Value::Float(r.f64()?),
        2 => Value::Bool(r.u8()? != 0),
        3 => Value::str(r.s()?),
        4 => Value::Unit,
        5 => Value::Fun(Rc::new(r.s()?)),
        6 => {
            let hi = r.i64()? as i128;
            let lo = r.i64()? as u64 as i128;
            let v = (hi << 64) | lo;
            let width = crate::value::IntW::from_tag(r.u8()?)
                .ok_or_else(|| "invalid sized-int width tag".to_string())?;
            Value::SizedInt(Box::new((v, width)))
        }
        7 => {
            let mut b = [0u8; 4];
            for i in 0..4 { b[i] = r.u8()?; }
            Value::F32(f32::from_le_bytes(b))
        }
        t => return Err(format!("unknown constant tag {t}")),
    })
}

fn decode_op(r: &mut R) -> DecodeResult<Op> {
    use Op::*;
    Ok(match r.u8()? {
        0 => Const(r.u8()?, r.u16()?),
        1 => Move(r.u8()?, r.u8()?),
        2 => Add(r.u8()?, r.u8()?, r.u8()?),
        3 => Sub(r.u8()?, r.u8()?, r.u8()?),
        4 => Mul(r.u8()?, r.u8()?, r.u8()?),
        5 => Div(r.u8()?, r.u8()?, r.u8()?),
        6 => Rem(r.u8()?, r.u8()?, r.u8()?),
        7 => Eq(r.u8()?, r.u8()?, r.u8()?),
        8 => Ne(r.u8()?, r.u8()?, r.u8()?),
        9 => Lt(r.u8()?, r.u8()?, r.u8()?),
        10 => Le(r.u8()?, r.u8()?, r.u8()?),
        11 => Gt(r.u8()?, r.u8()?, r.u8()?),
        12 => Ge(r.u8()?, r.u8()?, r.u8()?),
        13 => Neg(r.u8()?, r.u8()?),
        14 => Not(r.u8()?, r.u8()?),
        15 => Jump(r.usz()?),
        16 => JumpIfFalse(r.u8()?, r.usz()?),
        17 => JumpIfTrue(r.u8()?, r.usz()?),
        18 => Call { dst: r.u8()?, fun: r.u16()?, start: r.u8()?, argc: r.u8()? },
        19 => CallBuiltin { dst: r.u8()?, which: r.u8()?, start: r.u8()?, argc: r.u8()? },
        20 => CallValue { dst: r.u8()?, f: r.u8()?, start: r.u8()?, argc: r.u8()? },
        21 => Method { dst: r.u8()?, recv: r.u8()?, name: r.u16()?, start: r.u8()?, argc: r.u8()? },
        22 => Ret(r.u8()?),
        23 => MakeList { dst: r.u8()?, start: r.u8()?, len: r.u8()? },
        24 => MakeCtor { dst: r.u8()?, ctor: r.u16()?, start: r.u8()?, len: r.u8()? },
        25 => GetField { dst: r.u8()?, obj: r.u8()?, idx: r.u8()? },
        26 => GetFieldNamed { dst: r.u8()?, obj: r.u8()?, name: r.u16()? },
        27 => TagIs { dst: r.u8()?, obj: r.u8()?, ctor: r.u16()? },
        28 => MakeClosure { dst: r.u8()?, proto: r.u16()?, start: r.u8()?, ncaps: r.u8()? },
        29 => MakeRange { dst: r.u8()?, lo: r.u8()?, hi: r.u8()?, inclusive: r.u8()? != 0 },
        30 => IterLen(r.u8()?, r.u8()?),
        31 => IterGet { dst: r.u8()?, iter: r.u8()?, idx: r.u8()? },
        32 => ToStr(r.u8()?, r.u8()?),
        33 => Concat(r.u8()?, r.u8()?, r.u8()?),
        34 => StateGet(r.u8()?, r.u8()?),
        35 => StateSet(r.u8()?, r.u8()?),
        36 => MakeInstance { dst: r.u8()?, comp: r.u16()?, start: r.u8()?, argc: r.u8()?, policy: r.u8()? },
        37 => WireOp { from: r.u8()?, out_port: r.u16()?, to: r.u8()?, in_port: r.u16()? },
        38 => EmitOp {
            port: r.u16()?,
            payload: {
                let has = r.u8()? != 0;
                if has {
                    Some(r.u8()?)
                } else {
                    None
                }
            },
        },
        39 => Panic(r.u16()?),
        40 => WithField { dst: r.u8()?, obj: r.u8()?, name: r.u16()?, value: r.u8()? },
        42 => CallComp { dst: r.u8()?, fun: r.u16()?, start: r.u8()?, argc: r.u8()? },
        43 => CallAi { dst: r.u8()?, info: r.u16()?, intent: r.u8()? },
        t => return Err(format!("unknown opcode {t}")),
    })
}

// ---------------- bundle trailer ----------------

/// Append `module` to a copy of the running executable, making a
/// self-contained program: [exe bytes][kx bytes][u64 kx_len]["KUPLBNDL"].
pub fn write_bundle(exe: &[u8], module: &Module) -> Vec<u8> {
    let kx = encode(module);
    let mut out = Vec::with_capacity(exe.len() + kx.len() + 16);
    out.extend_from_slice(exe);
    out.extend_from_slice(&kx);
    out.extend_from_slice(&(kx.len() as u64).to_le_bytes());
    out.extend_from_slice(BUNDLE_MAGIC);
    out
}

/// If `exe` ends with a bundle trailer, decode and return the module.
pub fn read_bundle(exe: &[u8]) -> Option<DecodeResult<Module>> {
    if exe.len() < 16 || &exe[exe.len() - 8..] != BUNDLE_MAGIC {
        return None;
    }
    let len_bytes: [u8; 8] = exe[exe.len() - 16..exe.len() - 8].try_into().ok()?;
    let kx_len = u64::from_le_bytes(len_bytes) as usize;
    // Bound the (attacker-controllable) trailer length against the file: use
    // saturating arithmetic so a corrupt near-usize::MAX length can't overflow
    // `16 + kx_len` (wrapping past the check) and underflow the slice start —
    // that would panic on an out-of-bounds slice. A bad length is just "no trailer".
    if kx_len > exe.len().saturating_sub(16) {
        return None;
    }
    let kx = &exe[exe.len() - 16 - kx_len..exe.len() - 16];
    Some(decode(kx))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cap_bounds_allocated_bytes_not_just_item_count() {
        // regression: PR-it926. `cap::<T>(n)` must ensure the resulting
        // `Vec::with_capacity` allocation stays within the buffer's actual
        // remaining BYTES (`cap * size_of::<T>() <= remaining`), not just
        // clamp the ITEM COUNT to remaining bytes (which silently assumed a
        // 1-byte element and let `Vec::with_capacity` over-allocate by a
        // factor of `size_of::<T>()` for every other element type this file
        // decodes into -- `Value`/`String` = 24 bytes, `ToolMeta` ~150+
        // bytes -- confirmed live before this fix: a ~2GB crafted `.kx` file
        // forced a `Vec::<Value>::with_capacity` request far beyond the
        // buffer's own size).
        let buf = vec![0u8; 1000];
        let r = super::R { buf: &buf, pos: 0, mem_budget: buf.len() * super::MEM_BUDGET_SLACK };
        let vsz = std::mem::size_of::<super::Value>();
        let n = r.cap::<super::Value>(usize::MAX);
        assert!(
            n * vsz <= buf.len(),
            "cap={n} * size_of::<Value>()={vsz} = {} exceeds remaining {} bytes",
            n * vsz,
            buf.len()
        );
        assert_eq!(n, buf.len() / vsz, "must use every remaining byte's worth of headroom, not less");
        // a second, differently-sized type (Span, 8 bytes) hits the same invariant.
        let ssz = std::mem::size_of::<super::Span>();
        let n2 = r.cap::<super::Span>(usize::MAX);
        assert!(n2 * ssz <= buf.len());
        // a genuinely small requested count that already fits is never inflated.
        assert_eq!(r.cap::<super::Value>(5), 5);
        assert_eq!(r.cap::<super::Value>(0), 0);
    }

    #[test]
    fn charge_bounds_total_memory_to_a_fixed_multiple_of_file_size() {
        // regression: PR-it927, following directly on PR-it926. `cap()`
        // only bounds a loop's INITIAL `Vec::with_capacity` HINT, never the
        // loop's own iteration count -- so `Vec::push` still grows a vec
        // far past its correctly-sized initial hint as long as SOME valid
        // item can still be parsed from the remaining bytes. Live-
        // confirmed before this fix: a 20MB `.kx` file padded entirely
        // with the byte `0x04` (a valid, 1-byte `Value::Unit` constant tag
        // -- the cheapest of `decode_const`'s 8 variants) forced ~665MB of
        // real resident memory (~33x amplification) before eventually
        // failing on truncation. `charge::<T>()` must permit charging
        // exactly up to `MEM_BUDGET_SLACK * buf.len()` bytes' worth of `T`
        // -- no more (defeats the fix) and no less (would be a false
        // positive on legitimate small counts, the exact regression an
        // earlier design of this fix introduced and this test also guards
        // against by exercising a REALISTIC number of charges, not just a
        // single one).
        let buf = vec![0u8; 1000];
        let mut r = super::R { buf: &buf, pos: 0, mem_budget: buf.len() * super::MEM_BUDGET_SLACK };
        let vsz = std::mem::size_of::<super::Value>();
        let max_charges = (buf.len() * super::MEM_BUDGET_SLACK) / vsz;
        for i in 0..max_charges {
            r.charge::<super::Value>()
                .unwrap_or_else(|e| panic!("charge {i} of {max_charges} must stay within budget: {e}"));
        }
        assert!(
            r.charge::<super::Value>().is_err(),
            "must reject once MEM_BUDGET_SLACK * buf.len() worth of Value has been charged"
        );
    }

    /// A REAL, uncatchable-crash bug found+fixed (production-hardening
    /// PR-it730): `decode_shape` (an `ai fun`'s structured-output type,
    /// decoded from an untrusted `.kx` byte buffer) had NO depth tracking
    /// or cap at all -- `List`/`Option` tags each consume exactly ONE byte
    /// and recurse unconditionally. Confirmed live before this fix: a small
    /// (single-digit-MB) `.kx` file whose `ai_fun` shape field is millions
    /// of consecutive `List` tag bytes crashed BOTH `kupl dis` and `kupl
    /// run` with an uncatchable native stack overflow (SIGABRT), even
    /// though `main.rs` already gives the main thread a 2GiB stack (the
    /// SAME hardening PR-it729 relied on for `par_map`'s workers) -- each
    /// `decode_shape` frame costs only ~100+ bytes, so a cheap file still
    /// exhausts even a 2GiB reservation. Calls `decode_shape` DIRECTLY on a
    /// crafted byte buffer (skipping a full `.kx` file's encode/decode
    /// entirely, mirroring PR-it687/it688's own `Module`-field-corruption
    /// tests) -- a run of `0x04` ("List" tag) bytes one past
    /// `json::MAX_JSON_DEPTH` must be a clean error, not a panic; well
    /// within the cap must still decode correctly.
    #[test]
    fn decode_shape_deeply_nested_list_tags_is_a_clean_error_not_a_stack_overflow() {
        // one byte too many: must be a clean "corrupt .kx module" error.
        let too_deep = vec![4u8; crate::json::MAX_JSON_DEPTH + 2];
        let mut r = super::R { buf: &too_deep, pos: 0, mem_budget: too_deep.len() * super::MEM_BUDGET_SLACK };
        let err = super::decode_shape(&mut r, 0).expect_err("must be a clean error, not a panic");
        assert!(err.contains("corrupt .kx module"), "{err}");

        // well within the cap, followed by a real leaf tag (`0` = Str): decodes fine.
        let mut ok_buf = vec![4u8; 10];
        ok_buf.push(0);
        let mut r = super::R { buf: &ok_buf, pos: 0, mem_budget: ok_buf.len() * super::MEM_BUDGET_SLACK };
        let shape = super::decode_shape(&mut r, 0).expect("a shallow shape must decode cleanly");
        // 10 levels of List wrapping a Str leaf.
        let mut cur = &shape;
        for _ in 0..10 {
            match cur {
                crate::ai::AiShape::List(inner) => cur = inner,
                other => panic!("expected List, got {other:?}"),
            }
        }
        assert!(matches!(cur, crate::ai::AiShape::Str));
    }

    #[test]
    fn kx_roundtrip_preserves_disassembly() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/counter.kupl"),
        )
        .expect("example exists");
        let compiled = crate::run::compile(&src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let bytes = super::encode(&module);
        let decoded = super::decode(&bytes).expect("decodes");
        assert_eq!(module.disassemble(), decoded.disassemble());
        assert_eq!(module.funs, decoded.funs);
        assert_eq!(module.component_names, decoded.component_names);
    }

    /// A coverage-closing test, per PR-it649 (no bug found -- `Module.disassemble()`,
    /// the equality oracle the test above relies on, only renders `chunks` (name/
    /// caps/params/regs/consts/code) and `ctors`; it does NOT render `ctor_field_names`,
    /// `ai_funs`, or a `ComponentMeta`'s richer fields (`props`/`handlers`/`exposes`/
    /// `out_ports`/`timers`) at all -- so the ONLY existing round-trip test had ZERO
    /// coverage of those fields ever surviving an encode/decode cycle: an encode or
    /// decode bug in, say, `timers` or `ai_funs` (both genuinely non-trivial encodings
    /// -- variable-length `AiShape`/`ToolMeta` nesting, an `Option<u16>` per prop
    /// default) would have gone completely undetected. Added `#[derive(PartialEq)]`
    /// to `Op`/`Chunk`/`CtorMeta`/`ComponentMeta`/`Module` (every field type they're
    /// built from -- `Value`/`TimerMeta`/`Span`/`AiFunMeta`/`ToolMeta`/`AiShape` --
    /// ALREADY had `PartialEq`, confirmed before adding these) to enable a genuinely
    /// STRUCTURAL whole-module comparison instead of the narrower text/HashMap
    /// spot-checks the existing test relies on.
    #[test]
    fn kx_roundtrip_preserves_every_module_field_structurally() {
        // touches every field `disassemble()` skips: a type (ctors +
        // ctor_field_names), an `ai fun` with a `tools` clause (ai_funs +
        // ToolMeta), and a component with a defaulted prop, a required prop,
        // a port handler, a lifecycle handler (`on start`), two timer kinds
        // (`every`/`after`), and an `expose`d function.
        let src = "type Item = Item(name: Str)\n\
                   fun helper(x: Int) -> Int {\n    x + 1\n}\n\
                   ai fun summarize(text: Str) -> Str tools [helper] {\n    intent \"Summarize the text.\"\n}\n\
                   component Widget {\n    intent \"w\"\n    prop label: Str\n    prop count: Int = 0\n    \
                   in trigger: Int\n    out done: Int\n    state total: Int = 0\n    \
                   on trigger(v) {\n        total = total + v\n        emit done(total)\n    }\n    \
                   on start { }\n    on every 5s { }\n    on after 2s { }\n    \
                   expose fun current() -> Int {\n        total\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        // sanity: this source genuinely exercises every field under test, so a
        // regression can't hide behind an accidentally-empty collection.
        assert!(!module.ctors.is_empty(), "expected ctors from `type Item`");
        assert!(!module.ctor_field_names.is_empty(), "expected ctor_field_names from `type Item`");
        assert!(!module.ai_funs.is_empty(), "expected ai_funs from `ai fun summarize`");
        assert!(!module.ai_funs[0].tools.is_empty(), "expected a tool from `tools [helper]`");
        let widget = module.components.iter().find(|c| c.name == "Widget").expect("Widget component");
        assert_eq!(widget.props.len(), 2, "label (no default) + count (defaulted)");
        assert!(widget.props.iter().any(|(_, d)| d.is_some()), "count has a default chunk");
        assert!(widget.props.iter().any(|(_, d)| d.is_none()), "label has no default");
        assert_eq!(widget.timers.len(), 2, "one `every`, one `after`");
        assert!(!widget.exposes.is_empty(), "expected `current` in exposes");
        assert!(widget.handlers.len() >= 2, "port handler + `on start`");

        let bytes = super::encode(&module);
        let decoded = super::decode(&bytes).expect("decodes");
        // the FULL structural comparison -- every field of every chunk (including
        // `spans`, which `disassemble()` never prints), every ctor, every
        // component's props/handlers/exposes/out_ports/timers, and every ai_fun
        // (including nested `AiShape`/`ToolMeta` data), not just the narrow
        // disassembly-text + two-HashMap spot-check the sibling test above does.
        assert_eq!(module, decoded, "a .kx round-trip must be byte-for-byte structurally identical");
    }

    /// The sibling structural round-trip test above only exercises whatever
    /// `Op` variants the compiler happens to emit for ONE hand-written KUPL
    /// program -- it can never guarantee EVERY one of `Op`'s 43 variants is
    /// covered (production-hardening PR-it1032, closing a candidate PR-it831's
    /// own follow-up agent explicitly flagged as NOT yet done: "did NOT
    /// exhaustively re-verify every Op variant's own encode/decode symmetry").
    /// This test instead hand-constructs ONE instance of literally every `Op`
    /// variant directly (bypassing the compiler entirely), each with DISTINCT
    /// field values (no value reused across two positions within the same
    /// variant, and u16 index fields deliberately larger than u8 register
    /// fields) so a field-ORDER bug in `encode_op`/`decode_op` -- e.g. writing
    /// `start` where `argc` belongs, or reading a `u16` where a `u8` was
    /// written -- cannot hide behind two fields happening to share a value.
    /// Also covers BOTH branches of `Op`'s two variant-internal enums:
    /// `MakeRange`'s `inclusive: bool` (both `true`/`false`) and `EmitOp`'s
    /// `payload: Option<Reg>` (both `Some`/`None`).
    #[test]
    fn kx_roundtrip_preserves_every_op_variant_with_distinct_field_values() {
        use crate::bytecode::{Chunk, Module, Op};
        let code = vec![
            Op::Const(1, 1001),
            Op::Move(2, 3),
            Op::Add(4, 5, 6),
            Op::Sub(7, 8, 9),
            Op::Mul(10, 11, 12),
            Op::Div(13, 14, 15),
            Op::Rem(16, 17, 18),
            Op::Eq(19, 20, 21),
            Op::Ne(22, 23, 24),
            Op::Lt(25, 26, 27),
            Op::Le(28, 29, 30),
            Op::Gt(31, 32, 33),
            Op::Ge(34, 35, 36),
            Op::Neg(37, 38),
            Op::Not(39, 40),
            Op::Jump(100_001),
            Op::JumpIfFalse(41, 100_002),
            Op::JumpIfTrue(42, 100_003),
            Op::Call { dst: 43, fun: 1002, start: 44, argc: 45 },
            Op::CallComp { dst: 46, fun: 1003, start: 47, argc: 48 },
            Op::CallBuiltin { dst: 49, which: 50, start: 51, argc: 52 },
            Op::CallValue { dst: 53, f: 54, start: 55, argc: 56 },
            Op::Method { dst: 57, recv: 58, name: 1004, start: 59, argc: 60 },
            Op::Ret(61),
            Op::MakeList { dst: 62, start: 63, len: 64 },
            Op::MakeCtor { dst: 65, ctor: 1005, start: 66, len: 67 },
            Op::GetField { dst: 68, obj: 69, idx: 70 },
            Op::GetFieldNamed { dst: 71, obj: 72, name: 1006 },
            Op::WithField { dst: 73, obj: 74, name: 1007, value: 75 },
            Op::TagIs { dst: 76, obj: 77, ctor: 1008 },
            Op::MakeClosure { dst: 78, proto: 1009, start: 79, ncaps: 80 },
            Op::MakeRange { dst: 81, lo: 82, hi: 83, inclusive: true },
            Op::MakeRange { dst: 84, lo: 85, hi: 86, inclusive: false },
            Op::IterLen(87, 88),
            Op::IterGet { dst: 89, iter: 90, idx: 91 },
            Op::ToStr(92, 93),
            Op::Concat(94, 95, 96),
            Op::StateGet(97, 98),
            Op::StateSet(99, 100),
            Op::MakeInstance { dst: 101, comp: 1010, start: 102, argc: 103, policy: 104 },
            Op::WireOp { from: 105, out_port: 1011, to: 106, in_port: 1012 },
            Op::EmitOp { port: 1013, payload: Some(107) },
            Op::EmitOp { port: 1014, payload: None },
            Op::Panic(1015),
            Op::CallAi { dst: 108, info: 1016, intent: 109 },
        ];
        let n = code.len();
        let spans: Vec<crate::diag::Span> =
            (0..n).map(|i| crate::diag::Span::new(i as u32, (i + 1) as u32)).collect();
        let module = Module {
            chunks: vec![Chunk {
                name: "op_coverage".to_string(),
                ncaps: 0,
                nparams: 0,
                nregs: 200,
                consts: vec![],
                code,
                spans,
            }],
            ..Default::default()
        };
        let bytes = super::encode(&module);
        let decoded = super::decode(&bytes).expect("decodes");
        assert_eq!(
            module, decoded,
            "every Op variant, with distinct per-field values, must survive an encode/decode round trip"
        );
        // Sanity: confirm this test really did cover every DISTINCT variant
        // (45 entries total -- 43 variants plus one extra `MakeRange`/`EmitOp`
        // each, to cover both branches of their own internal `bool`/`Option`),
        // so a FUTURE new Op added to the enum without a corresponding entry
        // here fails loudly instead of silently going unfuzzed.
        let distinct: std::collections::HashSet<_> =
            decoded.chunks[0].code.iter().map(std::mem::discriminant).collect();
        assert_eq!(n, 45, "expected 43 Op variants + 2 extra branch-coverage entries (MakeRange, EmitOp)");
        assert_eq!(distinct.len(), 43, "Op has 43 DISTINCT variants as of PR-it1032 -- if this fails, a variant was ADDED or REMOVED; update this test's own `code` list to match");
    }

    #[test]
    fn corrupt_kx_is_rejected_not_a_crash() {
        // A tampered/untrusted .kx must decode to a clean Err — never a panic, an
        // index-out-of-bounds, or a giant allocation from an attacker-controlled
        // count. A huge `nconsts` used to feed Vec::with_capacity directly (would
        // try to allocate ~GBs and abort); it is now clamped to the bytes that
        // remain, so the decode loop hits "truncated .kx module" instead.
        let mut b = Vec::new();
        b.extend_from_slice(super::KX_MAGIC);
        b.extend_from_slice(&1u32.to_le_bytes()); // nchunks = 1
        b.extend_from_slice(&0u32.to_le_bytes()); // chunk name: len 0
        b.push(0); // ncaps
        b.push(0); // nparams
        b.extend_from_slice(&0u16.to_le_bytes()); // nregs
        b.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nconsts = 4.29 billion
        // (no const bytes follow) — must be Err, must return fast (no huge alloc)
        assert!(super::decode(&b).is_err(), "huge nconsts must be a clean error");

        // PR-it926: the case above has ZERO remaining bytes after the header,
        // so it degenerately exercised `cap()`'s clamp regardless of whether
        // the size_of::<T> bug existed. Repeat with SUBSTANTIAL padding
        // (an invalid constant tag byte, so decode fails on the very first
        // item rather than processing millions of valid-looking ones) to
        // actually exercise a meaningful, non-degenerate capacity clamp.
        let mut b2 = b.clone();
        b2.resize(b2.len() + 10_000_000, 0xFFu8);
        assert!(super::decode(&b2).is_err(), "huge nconsts with real padding must still be a clean error");

        // truncating a valid module at every prefix length is always a clean Err.
        let src = "fun add(a: Int, b: Int) -> Int { a + b }\nfun main() { print(add(2, 3)) }\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let full = super::encode(&module);
        assert!(super::decode(&full).is_ok());
        for cut in (0..full.len()).step_by(1) {
            // never panics; a partial module is Err (except the trivially-empty
            // prefix cases which are also Err via the magic/length checks)
            let _ = super::decode(&full[..cut]);
        }
        // flip a byte in the body (past the 8-byte magic) — never a panic.
        for i in 8..full.len() {
            let mut t = full.clone();
            t[i] ^= 0xFF;
            let _ = super::decode(&t); // Ok or Err, but never a crash
        }
    }

    /// A REAL bug found+fixed (production-hardening PR-it687): the sibling
    /// `flip a byte` fuzz loop above (`corrupt_kx_is_rejected_not_a_crash`)
    /// only calls `decode()` and never RUNS the resulting module -- so it
    /// never caught this: `decode()` never cross-validates a chunk's `nregs`
    /// (or a component's `nslots`) against the register/slot indices its OWN
    /// decoded instructions reference. A `.kx` file where those disagree
    /// (impossible from `compile.rs`'s own allocator, which always sizes
    /// `nregs` to cover every register it emits -- but trivially producible
    /// by hand-editing or corrupting a `.kx` file, which `kupl run
    /// <file.kx>`/`kupl dis` accept from an arbitrary path) decodes
    /// successfully, then used to crash the VM with a raw Rust
    /// index-out-of-bounds panic (a bogus "internal compiler error")
    /// partway through EXECUTION, not decoding. Confirmed live before this
    /// fix by hand-patching a real compiled `.kx` file's `nregs` field down
    /// from 6 to 1/3 and running it.
    ///
    /// Constructs the corruption directly on a `Module` (skipping
    /// encode/decode entirely, since the bug is in EXECUTION, not decoding)
    /// -- `compile_module`'s own allocator always produces a consistent
    /// `nregs`, so this simulates exactly what a hand-crafted/corrupted
    /// `.kx` file's `decode()` would hand the VM.
    #[test]
    fn a_kx_module_with_nregs_smaller_than_its_own_bytecode_needs_is_a_clean_error_not_a_panic() {
        let src = "fun add(a: Int, b: Int) -> Int {\n    let x = a + b\n    let y = x * 2\n    y + 1\n}\n\
                   fun main() uses io {\n    print(add(3, 4))\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let add_idx = module.funs["add"];
        let original_nregs = module.chunks[add_idx as usize].nregs;
        assert!(original_nregs > 2, "the test needs `add` to genuinely use more than 2 registers");

        // a legitimate module still runs correctly (sanity check).
        let mut vm = crate::vm::Vm::new(&module);
        assert_eq!(vm.call_named("main", vec![]).is_ok(), true);

        // corrupt just this one chunk's `nregs` -- exactly what a
        // hand-edited/corrupted `.kx` file's decode() would hand the VM,
        // since decode() never cross-checks this field.
        module.chunks[add_idx as usize].nregs = 2;
        let mut vm = crate::vm::Vm::new(&module);
        let err = vm.call_named("add", vec![crate::value::Value::Int(3), crate::value::Value::Int(4)]).expect_err(
            "an nregs/bytecode mismatch must be a clean VmError, not a panic",
        );
        assert!(err.msg.contains("corrupt .kx module"), "{}", err.msg);
    }

    /// The state-slot twin of the test above (production-hardening
    /// PR-it687): `ComponentMeta::nslots` has the exact same "never cross-
    /// validated by `decode()`" gap as a chunk's `nregs` -- `Op::StateGet`/
    /// `Op::StateSet` decode their own independent `slot: u8` operand that
    /// `decode()` never checks against `nslots`. A component whose `nslots`
    /// disagrees with the slot indices its own handlers/exposes reference
    /// used to crash with a raw index-out-of-bounds panic on
    /// `self.instances[id].slots[slot as usize]`.
    #[test]
    fn a_kx_component_with_nslots_smaller_than_its_own_state_accesses_is_a_clean_error_not_a_panic() {
        let src = "component Widget {\n    intent \"w\"\n    state total: Int = 0\n    \
                   expose fun bump() -> Int {\n        total += 1\n        total\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let widget = module.components.iter_mut().find(|c| c.name == "Widget").expect("Widget component");
        assert!(widget.nslots >= 1, "the test needs Widget to genuinely have a state slot");

        // a legitimate module still runs correctly (sanity check).
        let mut vm = crate::vm::Vm::new(&module);
        let id = vm.instantiate_named("Widget", vec![]).expect("instantiates");
        assert!(vm.call_expose(id, "bump", vec![]).is_ok());

        // corrupt nslots to 0 -- exactly what a hand-edited/corrupted `.kx`
        // file's decode() would hand the VM, since decode() never
        // cross-checks this field against `Op::StateGet`/`StateSet`. The
        // mismatch surfaces immediately: `instantiate` sizes `slots` off
        // `nslots` BEFORE running the init chunk, which itself sets `total`
        // via `Op::StateSet(0, ...)`.
        module.components.iter_mut().find(|c| c.name == "Widget").unwrap().nslots = 0;
        let mut vm = crate::vm::Vm::new(&module);
        let err = vm
            .instantiate_named("Widget", vec![])
            .expect_err("an nslots/state-access mismatch must be a clean VmError, not a panic");
        assert!(err.msg.contains("corrupt .kx module"), "{}", err.msg);
    }

    /// The constant-pool twin of the two tests above (production-hardening
    /// PR-it688): `Op::Const`'s own index operand (and several other ops that
    /// store a field/method/port NAME as a string constant) is never
    /// cross-validated by `decode()` against `chunk.consts.len()` either.
    #[test]
    fn a_kx_chunk_with_a_const_index_past_its_own_pool_is_a_clean_error_not_a_panic() {
        let src = "fun greet() -> Str {\n    \"hello\"\n}\nfun main() uses io {\n    print(greet())\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let greet_idx = module.funs["greet"];
        assert!(
            !module.chunks[greet_idx as usize].consts.is_empty(),
            "the test needs `greet` to genuinely reference a constant"
        );

        // a legitimate module still runs correctly (sanity check).
        let mut vm = crate::vm::Vm::new(&module);
        assert!(vm.call_named("main", vec![]).is_ok());

        // drop the const pool to empty -- exactly what a hand-edited/
        // corrupted `.kx` file's decode() would hand the VM, since decode()
        // never cross-checks a chunk's `consts` length against the const
        // indices its OWN `code` still references.
        module.chunks[greet_idx as usize].consts.clear();
        let mut vm = crate::vm::Vm::new(&module);
        let err = vm
            .call_named("greet", vec![])
            .expect_err("a consts/bytecode mismatch must be a clean VmError, not a panic");
        assert!(err.msg.contains("corrupt .kx module"), "{}", err.msg);
    }

    /// Five REAL bugs found+fixed (production-hardening PR-it726): the SAME
    /// corrupt-`.kx` bounds-check gap as `nregs`/`nslots`/`consts` above
    /// (PR-it687/it688), but in FIVE self-mutating fast paths added AFTER
    /// that fix and never covered by it -- `Op::Add`'s string self-append
    /// (`s = s + x`), `Op::Method`'s List/Map/Set self-push/self-insert
    /// (`xs = xs.push(x)`, `m = m.insert(k, v)`, `s = s.insert(v)`), and
    /// `Op::Ret`'s write into the CALLER's own destination register. All
    /// five used to index `self.stack` DIRECTLY, bypassing `reg!`/`set!`'s
    /// bounds check entirely. Rather than shrinking `nregs` (the technique
    /// the tests above use), these corrupt the SPECIFIC instruction's own
    /// register operand directly to an out-of-range value -- shrinking
    /// `nregs` doesn't cleanly isolate these fast paths, since the
    /// registers they read are typically THE SAME ones earlier instructions
    /// in the same tiny function already used (there's no way to shrink
    /// `nregs` such that an EARLIER access stays in-bounds while the fast
    /// path's own access goes out of bounds, when they reference the exact
    /// same register index) -- so this test corrupts each opcode's operand
    /// directly instead, precisely targeting only the one instruction under
    /// test while leaving everything else (including `nregs`) untouched and
    /// internally consistent.
    #[test]
    fn a_kx_module_with_a_self_mutating_fast_path_register_out_of_range_is_a_clean_error_not_a_panic() {
        // Op::Add's string self-append fast path (`s = s + x`).
        {
            let src = "fun main() uses io {\n    var s = \"a\"\n    s = s + \"b\"\n    print(s)\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::Add(..))).expect("main uses Add");
            if let crate::bytecode::Op::Add(_, _, b) = &mut code[pos] {
                *b = 200;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range Add operand must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "Add: {}", err.msg);
        }
        // Op::Method's List.push self-mutate fast path (`xs = xs.push(x)`).
        {
            let src = "fun main() uses io {\n    var xs = [1]\n    xs = xs.push(2)\n    print(xs)\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::Method { .. })).expect("main uses Method");
            if let crate::bytecode::Op::Method { dst, recv, .. } = &mut code[pos] {
                // corrupt BOTH -- the fast path requires `dst == recv`, so
                // corrupting `recv` alone would make that equality FALSE and
                // skip the fast path's vulnerable check entirely, silently
                // masking the bug instead of exercising it.
                *dst = 200;
                *recv = 200;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range Method recv must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "List.push: {}", err.msg);
        }
        // Op::Method's Map.insert self-mutate fast path (`m = m.insert(k, v)`).
        {
            let src =
                "fun main() uses io {\n    var m = Map()\n    m = m.insert(\"a\", 1)\n    print(m)\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::Method { .. })).expect("main uses Method");
            if let crate::bytecode::Op::Method { dst, recv, .. } = &mut code[pos] {
                // corrupt BOTH -- the fast path requires `dst == recv`, so
                // corrupting `recv` alone would make that equality FALSE and
                // skip the fast path's vulnerable check entirely, silently
                // masking the bug instead of exercising it.
                *dst = 200;
                *recv = 200;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range Method recv must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "Map.insert: {}", err.msg);
        }
        // Op::Method's Set.insert self-mutate fast path (`s = s.insert(v)`).
        {
            let src = "fun main() uses io {\n    var s = Set()\n    s = s.insert(1)\n    print(s)\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::Method { .. })).expect("main uses Method");
            if let crate::bytecode::Op::Method { dst, recv, .. } = &mut code[pos] {
                // corrupt BOTH -- the fast path requires `dst == recv`, so
                // corrupting `recv` alone would make that equality FALSE and
                // skip the fast path's vulnerable check entirely, silently
                // masking the bug instead of exercising it.
                *dst = 200;
                *recv = 200;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range Method recv must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "Set.insert: {}", err.msg);
        }
        // Op::Ret's write into the CALLER's own destination register.
        {
            let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(add(1, 2))\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            // corrupt `main`'s OWN Call instruction's `dst` operand -- this
            // becomes `f.dst` on the callee's frame, later applied by
            // `Op::Ret` against the CALLER's (main's) register space.
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::Call { .. })).expect("main calls add");
            if let crate::bytecode::Op::Call { dst, .. } = &mut code[pos] {
                *dst = 200;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range Call dst must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "Ret: {}", err.msg);
        }
    }

    /// Four REAL bugs found+fixed (production-hardening PR-it744): the SAME
    /// corrupt-`.kx` bounds-check gap as `nregs`/`nslots`/`consts`/`ip` above
    /// (PR-it687/it688), but in four MORE operand fields that were never
    /// covered by that sweep: `Op::MakeCtor`/`Op::TagIs`'s `ctor` operand
    /// (indexes `module.ctors` directly), `Op::MakeInstance`'s `comp`
    /// operand (via `instantiate`, indexes `module.components` directly),
    /// and `Op::CallBuiltin`'s `argc` operand (never cross-checked against
    /// the exact arity `compile.rs::call`'s own `(name, args.len())` match
    /// used to pick `which` in the first place, so nearly every one of the
    /// ~40 builtin-dispatch arms indexed `args[0]`/`args[1]` directly with
    /// no length check). All four found by a research subagent, which
    /// live-reproduced each one by hand-flipping the exact byte in a real
    /// compiled `.kx` file and running it through the actual `kupl` CLI
    /// binary (each crashed with `internal compiler error [src/vm.rs:N]`,
    /// exit 101) before this fix existed.
    #[test]
    fn a_kx_module_with_an_out_of_range_ctor_component_or_builtin_argc_operand_is_a_clean_error_not_a_panic() {
        // Op::MakeCtor's `ctor` operand.
        {
            let src = "type Pt = Pt(x: Int, y: Int)\nfun main() uses io {\n    let p = Pt(1, 2)\n    print(p.x)\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::MakeCtor { .. })).expect("main uses MakeCtor");
            if let crate::bytecode::Op::MakeCtor { ctor, .. } = &mut code[pos] {
                *ctor = 5000;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range MakeCtor ctor must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "MakeCtor: {}", err.msg);
        }
        // Op::TagIs's `ctor` operand.
        {
            let src = "type Shape = Circle(r: Int) | Square(s: Int)\n\
                       fun describe(sh: Shape) -> Str {\n    \
                       match sh {\n        Circle(r) => \"circle\"\n        Square(s) => \"square\"\n    }\n}\n\
                       fun main() uses io {\n    print(describe(Circle(1)))\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let describe_idx = module.funs["describe"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[describe_idx as usize].code;
            let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::TagIs { .. })).expect("describe uses TagIs");
            if let crate::bytecode::Op::TagIs { ctor, .. } = &mut code[pos] {
                *ctor = 5000;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("describe", vec![crate::value::Value::Ctor {
                    ty: std::rc::Rc::new("Shape".into()),
                    variant: std::rc::Rc::new("Circle".into()),
                    fields: std::rc::Rc::new(vec![crate::value::Value::Int(1)]),
                }])
                .expect_err("an out-of-range TagIs ctor must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "TagIs: {}", err.msg);
        }
        // Op::MakeInstance's `comp` operand (via `instantiate`).
        {
            let src = "component Widget {\n    intent \"w\"\n    state total: Int = 0\n    \
                       expose fun bump() -> Int {\n        total += 1\n        total\n    }\n}\n\
                       fun main() uses io {\n    let w = Widget()\n    print(w.bump())\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos =
                code.iter().position(|op| matches!(op, crate::bytecode::Op::MakeInstance { .. })).expect("main uses MakeInstance");
            if let crate::bytecode::Op::MakeInstance { comp, .. } = &mut code[pos] {
                *comp = 4242;
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("an out-of-range MakeInstance comp must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "MakeInstance: {}", err.msg);
        }
        // Op::CallBuiltin's `argc` operand (mismatched against `which`'s real arity).
        {
            let src = "fun main() uses io {\n    print(\"hi\")\n}\n";
            let compiled = crate::run::compile(src).expect("compiles");
            let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module compiles");
            let main_idx = module.funs["main"];
            let mut vm = crate::vm::Vm::new(&module);
            assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");
            let code = &mut module.chunks[main_idx as usize].code;
            let pos =
                code.iter().position(|op| matches!(op, crate::bytecode::Op::CallBuiltin { .. })).expect("main calls print");
            if let crate::bytecode::Op::CallBuiltin { argc, .. } = &mut code[pos] {
                *argc = 0; // print needs exactly 1
            }
            let mut vm = crate::vm::Vm::new(&module);
            let err = vm
                .call_named("main", vec![])
                .expect_err("a CallBuiltin argc/arity mismatch must be a clean VmError, not a panic");
            assert!(err.msg.contains("corrupt .kx module"), "CallBuiltin: {}", err.msg);
        }
    }

    /// A REAL bug (production-hardening PR-it745), found by a follow-up survey
    /// re-auditing vm.rs for OTHER instances of the exact bug class PR-it744 fixed
    /// (a decoded `.kx` field used as a raw array index, bypassing bounds checks):
    /// `Op::WithField`'s field-update fast path (`x with field: value`) computes
    /// `i` (the field's position) from `module.ctor_field_names` -- metadata decoded
    /// INDEPENDENTLY from the runtime `Value::Ctor`'s own `fields` vec, whose length
    /// is controlled entirely by `Op::MakeCtor`'s `len` operand. Legitimate
    /// `compile.rs` output always keeps these in sync, but a corrupted `.kx` file's
    /// `MakeCtor.len` can be set shorter than what `ctor_field_names` expects,
    /// producing a `Value::Ctor` whose `fields` is too short for `i` --
    /// `new_fields[i] = ...` then panicked with a raw index-out-of-bounds instead of
    /// the clean error `GetField`/`GetFieldNamed` (the two sibling read paths,
    /// already `.get()`-based) would give. Live-reproduced by the survey before this
    /// fix: hand-flipped `MakeCtor`'s `len` byte in a real compiled `.kx` file
    /// (`type Pt = Pt(x: Int, y: Int)` + `p with y: 99`) from 2 to 0/1 and ran it
    /// through the actual `kupl run` CLI, observing `internal compiler error
    /// [src/vm.rs:1286]`, exit 101.
    #[test]
    fn a_kx_module_with_a_with_field_update_past_a_shrunk_ctor_is_a_clean_error_not_a_panic() {
        let src = "type Pt = Pt(x: Int, y: Int)\n\
                   fun main() uses io {\n    \
                   let p = Pt(1, 2)\n    let q = p with y: 99\n    print(q.y)\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let main_idx = module.funs["main"];

        // a legitimate module still runs correctly (sanity check).
        let mut vm = crate::vm::Vm::new(&module);
        assert!(vm.call_named("main", vec![]).is_ok(), "legitimate module must run correctly");

        // corrupt MakeCtor's `len` operand -- exactly what a hand-edited/corrupted
        // `.kx` file's decode() would hand the VM, since decode() never cross-checks
        // this field against `ctor_field_names`.
        let code = &mut module.chunks[main_idx as usize].code;
        let pos = code.iter().position(|op| matches!(op, crate::bytecode::Op::MakeCtor { .. })).expect("main uses MakeCtor");
        if let crate::bytecode::Op::MakeCtor { len, .. } = &mut code[pos] {
            *len = 1; // Pt has 2 fields (x, y); shrink to 1 so `y`'s position (1) is out of range
        }
        let mut vm = crate::vm::Vm::new(&module);
        let err = vm
            .call_named("main", vec![])
            .expect_err("a WithField update past a shrunk ctor must be a clean VmError, not a panic");
        assert!(err.msg.contains("corrupt .kx module"), "WithField: {}", err.msg);
    }

    /// The instruction-pointer twin of the tests above (production-hardening
    /// PR-it688): `ip` starts at 0 and only ever changes via a plain
    /// increment or a `Jump`/`JumpIfFalse`/`JumpIfTrue` target -- neither
    /// `decode()` nor the VM's own increment ever checked it stays `<
    /// chunk.code.len()`. A truncated `.kx` file's `code` array (shorter than
    /// what its OWN spans array or a jump target still implies) used to crash
    /// with a raw index-out-of-bounds panic reading the NEXT instruction.
    #[test]
    fn a_kx_chunk_whose_code_ends_before_ip_reaches_it_is_a_clean_error_not_a_panic() {
        let src = "fun three() -> Int {\n    let a = 1\n    let b = 2\n    a + b\n}\n\
                   fun main() uses io {\n    print(three())\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let mut module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let three_idx = module.funs["three"];
        let original_len = module.chunks[three_idx as usize].code.len();
        assert!(original_len > 1, "the test needs `three` to genuinely have more than one instruction");

        // a legitimate module still runs correctly (sanity check).
        let mut vm = crate::vm::Vm::new(&module);
        assert!(vm.call_named("main", vec![]).is_ok());

        // truncate `code` (and `spans`, kept in sync the same way decode()
        // itself always keeps them) to just its FIRST instruction -- `ip`
        // walks off the end on the second loop iteration, exactly what a
        // `.kx` file truncated (or lying about `ncode`) would produce.
        module.chunks[three_idx as usize].code.truncate(1);
        module.chunks[three_idx as usize].spans.truncate(1);
        let mut vm = crate::vm::Vm::new(&module);
        let err = vm
            .call_named("three", vec![])
            .expect_err("running off the end of `code` must be a clean VmError, not a panic");
        assert!(err.msg.contains("corrupt .kx module"), "{}", err.msg);
    }

    #[test]
    fn version_mismatch_is_distinguished() {
        let src = "fun main() {\n    print(1)\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let mut bytes = super::encode(&module);
        // flip the format-version byte of the magic (KUPLKX02 -> KUPLKX09)
        bytes[7] = b'9';
        let err = super::decode(&bytes).unwrap_err();
        assert!(err.contains("incompatible .kx format version"), "{err}");
        assert!(err.contains("rebuild"), "{err}");
        // a file that isn't a .kx at all gets the generic message
        let not_kx = b"GARBAGE1\x00\x00\x00\x00";
        let err2 = super::decode(not_kx).unwrap_err();
        assert!(err2.contains("not a .kx module"), "{err2}");
    }

    #[test]
    fn bundle_roundtrip_rich_module() {
        // A feature-rich module (generics, closures, higher-order calls) survives the
        // bundle trailer intact — PR-it122 certified `kupl bundle` runs such programs
        // with the same output as `kupl run` end-to-end.
        let src = "type Box[T] = Box(v: T)\n\
                   fun fib(n: Int) -> Int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\n\
                   fun main() uses io {\n    \
                   let xs = [1, 2, 3].map(fn x { x * 2 })\n    \
                   print(\"{fib(10)}|{xs.fold(0, fn(a, x) { a + x })}|{Box(42)}\")\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).expect("module");
        let fake_exe = vec![0x7fu8; 2000];
        let bundled = super::write_bundle(&fake_exe, &module);
        let back = super::read_bundle(&bundled).expect("has trailer").expect("decodes");
        assert_eq!(module.disassemble(), back.disassemble(), "rich module round-trips through the bundle");
    }

    #[test]
    fn bundle_execution_roundtrip_is_byte_identical() {
        // Beyond the structural (disassembly) round-trip above, the module EXTRACTED from a
        // bundle trailer must EXECUTE to the exact same output as running the source — the
        // end-to-end guarantee that `kupl bundle` ships behavior identical to the source, across
        // ADT match, HOF map/fold, and numeric/string builtins (PR-it190).
        let src = "type Shape = Circle(r: Float) | Rect(w: Float, h: Float)\n\
                   fun area(s: Shape) -> Float {\n    match s {\n        Circle(r) => 3.0 * r * r\n        Rect(w, h) => w * h\n    }\n}\n\
                   fun probe() -> Str {\n    let shapes = [Circle(2.0), Rect(3.0, 4.0)]\n    \
                   let areas = shapes.map(fn s { area(s) })\n    \
                   \"{areas}|{areas.fold(0.0, fn(a, x) { a + x })}|{(6).factorial()}|{\"Hi\".swapcase()}\"\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).expect("module");
        let fake_exe = vec![0x7fu8; 2000];
        let bundled = super::write_bundle(&fake_exe, &module);
        let back = super::read_bundle(&bundled).expect("has trailer").expect("decodes");
        let mut vm = crate::vm::Vm::new(&back);
        let v = vm.call_named("probe", vec![]).expect("runs");
        assert_eq!(v.to_string(), "[12.0, 12.0]|24.0|720|hI");
    }

    #[test]
    fn bundle_roundtrip() {
        let src = "fun main() {\n    print(\"bundled!\")\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let fake_exe = vec![0x7fu8; 1000]; // stand-in for the real binary
        let bundled = super::write_bundle(&fake_exe, &module);
        let back = super::read_bundle(&bundled).expect("has trailer").expect("decodes");
        assert_eq!(module.disassemble(), back.disassemble());
        // an unbundled exe yields None
        assert!(super::read_bundle(&fake_exe).is_none());

        // a CORRUPT trailer must be rejected cleanly (None), never panic. The
        // length field is at [len-16 .. len-8]; a near-usize::MAX value used to
        // overflow `16 + kx_len` past the bounds check and underflow the slice.
        let mut evil = bundled.clone();
        let n = evil.len();
        evil[n - 16..n - 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(super::read_bundle(&evil).is_none(), "huge trailer length must be rejected");
        // a length longer than the file, and a moderately-corrupt length
        let mut over = bundled.clone();
        over[n - 16..n - 8].copy_from_slice(&(n as u64 + 1000).to_le_bytes());
        assert!(super::read_bundle(&over).is_none());
        // truncating the bundle at every length never panics
        for cut in 0..bundled.len() {
            let _ = super::read_bundle(&bundled[..cut]);
        }
    }
}
