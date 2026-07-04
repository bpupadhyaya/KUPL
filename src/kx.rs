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
        CallAi { dst, info } => {
            w.u8(43);
            w.u8(*dst);
            w.u16(*info);
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
}

pub fn decode(buf: &[u8]) -> DecodeResult<Module> {
    let mut r = R { buf, pos: 0 };
    if r.take(8)? != KX_MAGIC {
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
        let mut consts = Vec::with_capacity(nconsts as usize);
        for _ in 0..nconsts {
            consts.push(decode_const(&mut r)?);
        }
        let ncode = r.u32()?;
        let mut code = Vec::with_capacity(ncode as usize);
        for _ in 0..ncode {
            code.push(decode_op(&mut r)?);
        }
        let mut spans = Vec::with_capacity(ncode as usize);
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
        let mut fields = Vec::with_capacity(n as usize);
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
        let mut props = Vec::with_capacity(nprops as usize);
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
        let mut handlers = Vec::with_capacity(nhandlers as usize);
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
        let mut out_ports = Vec::with_capacity(nports as usize);
        for _ in 0..nports {
            out_ports.push(r.s()?);
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
        });
    }

    let nai = r.u32()?;
    for _ in 0..nai {
        let name = r.s()?;
        let intent = r.s()?;
        let model = if r.u8()? != 0 { Some(r.s()?) } else { None };
        let nparams = r.u32()?;
        let mut params = Vec::with_capacity(nparams as usize);
        for _ in 0..nparams {
            params.push(r.s()?);
        }
        let shape = decode_shape(&mut r)?;
        let wraps_result = r.u8()? != 0;
        m.ai_funs.push(crate::ai::AiFunMeta { name, intent, model, params, shape, wraps_result });
    }

    Ok(m)
}

fn decode_shape(r: &mut R) -> DecodeResult<crate::ai::AiShape> {
    use crate::ai::AiShape::*;
    Ok(match r.u8()? {
        0 => Str,
        1 => Int,
        2 => Float,
        3 => Bool,
        4 => List(Box::new(decode_shape(r)?)),
        5 => Option(Box::new(decode_shape(r)?)),
        6 => {
            let ty = r.s()?;
            let variant = r.s()?;
            let n = r.u32()?;
            let mut fields = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let name = r.s()?;
                fields.push((name, decode_shape(r)?));
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
        43 => CallAi { dst: r.u8()?, info: r.u16()? },
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
    if exe.len() < 16 + kx_len {
        return None;
    }
    let kx = &exe[exe.len() - 16 - kx_len..exe.len() - 16];
    Some(decode(kx))
}

#[cfg(test)]
mod tests {
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
    }
}
