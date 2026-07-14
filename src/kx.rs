//! `.kx` — the KVM module binary format (encode/decode), and the bundle
//! trailer used by `kupl bundle` to produce self-contained executables.
//!
//! Layout: magic "KUPLKX01", then chunks, ctors, fun table, ctor field names,
//! components. All integers little-endian; strings are u32 length + UTF-8.

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
}

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
    /// corrupt/untrusted) buffer: never pre-allocate for more items than there are
    /// bytes left, since decoding each item consumes at least one byte. A tampered
    /// count (e.g. 0xFFFFFFFF) therefore cannot trigger a multi-gigabyte allocation
    /// that aborts the process — the following decode loop hits a clean
    /// "truncated .kx module" error instead.
    fn cap(&self, n: usize) -> usize {
        n.min(self.buf.len().saturating_sub(self.pos))
    }
}

pub fn decode(buf: &[u8]) -> DecodeResult<Module> {
    let mut r = R { buf, pos: 0 };
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
        let name = r.s()?;
        let ncaps = r.u8()?;
        let nparams = r.u8()?;
        let nregs = r.u16()?;
        let nconsts = r.u32()?;
        let mut consts = Vec::with_capacity(r.cap(nconsts as usize));
        for _ in 0..nconsts {
            consts.push(decode_const(&mut r)?);
        }
        let ncode = r.u32()?;
        let mut code = Vec::with_capacity(r.cap(ncode as usize));
        for _ in 0..ncode {
            code.push(decode_op(&mut r)?);
        }
        let mut spans = Vec::with_capacity(r.cap(ncode as usize));
        for _ in 0..ncode {
            let start = r.u32()?;
            let end = r.u32()?;
            spans.push(Span::new(start, end));
        }
        m.chunks.push(Chunk { name, ncaps, nparams, nregs, consts, code, spans });
    }

    let nctors = r.u32()?;
    for _ in 0..nctors {
        let type_name = r.s()?;
        let variant = r.s()?;
        let arity = r.u8()?;
        m.ctors.push(CtorMeta { type_name, variant, arity });
    }

    let nfuns = r.u32()?;
    for _ in 0..nfuns {
        let name = r.s()?;
        let idx = r.u16()?;
        m.funs.insert(name, idx);
    }

    let ncfn = r.u32()?;
    for _ in 0..ncfn {
        let variant = r.s()?;
        let n = r.u32()?;
        let mut fields = Vec::with_capacity(r.cap(n as usize));
        for _ in 0..n {
            fields.push(r.s()?);
        }
        m.ctor_field_names.insert(variant, fields);
    }

    let ncomps = r.u32()?;
    for i in 0..ncomps {
        let name = r.s()?;
        let is_app = r.u8()? != 0;
        let nprops = r.u32()?;
        let mut props = Vec::with_capacity(r.cap(nprops as usize));
        for _ in 0..nprops {
            let pname = r.s()?;
            let has_default = r.u8()? != 0;
            let default = if has_default { Some(r.u16()?) } else { None };
            props.push((pname, default));
        }
        let nslots = r.u8()?;
        let init_chunk = r.u16()?;
        let restart_chunk = r.u16()?;
        let nhandlers = r.u32()?;
        let mut handlers = Vec::with_capacity(r.cap(nhandlers as usize));
        for _ in 0..nhandlers {
            let key = r.s()?;
            let chunk = r.u16()?;
            let has_param = r.u8()? != 0;
            handlers.push((key, chunk, has_param));
        }
        let nexposes = r.u32()?;
        let mut exposes = HashMap::new();
        for _ in 0..nexposes {
            let ename = r.s()?;
            let chunk = r.u16()?;
            exposes.insert(ename, chunk);
        }
        let nports = r.u32()?;
        let mut out_ports = Vec::with_capacity(r.cap(nports as usize));
        for _ in 0..nports {
            out_ports.push(r.s()?);
        }
        let ntimers = r.u32()?;
        let mut timers = Vec::with_capacity(r.cap(ntimers as usize));
        for _ in 0..ntimers {
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
        let name = r.s()?;
        let intent = r.s()?;
        let model = if r.u8()? != 0 { Some(r.s()?) } else { None };
        let nparams = r.u32()?;
        let mut params = Vec::with_capacity(r.cap(nparams as usize));
        for _ in 0..nparams {
            params.push(r.s()?);
        }
        let shape = decode_shape(&mut r, 0)?;
        let wraps_result = r.u8()? != 0;
        let ntools = r.u32()?;
        let mut tools = Vec::with_capacity(r.cap(ntools as usize));
        for _ in 0..ntools {
            let tname = r.s()?;
            let description = r.s()?;
            let nparams = r.u32()?;
            let mut tparams = Vec::with_capacity(r.cap(nparams as usize));
            for _ in 0..nparams {
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
            let mut fields = Vec::with_capacity(r.cap(n as usize));
            for _ in 0..n {
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
        let mut r = super::R { buf: &too_deep, pos: 0 };
        let err = super::decode_shape(&mut r, 0).expect_err("must be a clean error, not a panic");
        assert!(err.contains("corrupt .kx module"), "{err}");

        // well within the cap, followed by a real leaf tag (`0` = Str): decodes fine.
        let mut ok_buf = vec![4u8; 10];
        ok_buf.push(0);
        let mut r = super::R { buf: &ok_buf, pos: 0 };
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
