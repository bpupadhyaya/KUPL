//! kser: the canonical binary wire encoding for `PortableValue`
//! (`docs/design/DISTRIBUTION.md`'s own "Wire format: kser" section),
//! shared groundwork for both `INTEROP.md`'s external-format story and
//! `weight distributed`'s own real network transport. Hand-rolled (this
//! project is zero-dependency throughout — no `serde`), the same
//! discipline `bigint.rs`/`decimal.rs`/`rational.rs`/`encoding.rs`'s own
//! SHA-256 already established.
//!
//! One tag byte per `PortableValue` variant, unsigned LEB128 varints for
//! every length prefix (a well-known, simple format — not invented here),
//! fixed-width little-endian for every fixed-size numeric field. Every
//! length-prefixed read is capped (`MAX_COLLECTION_LEN`, or `BigInt`'s own
//! pre-existing `MAX_BIGINT_LIMBS`) before any allocation happens, so a
//! malformed or adversarial byte stream can only ever fail cleanly — never
//! attempt an unbounded allocation from a single claimed length prefix.

use crate::bigint::BigInt;
use crate::decimal::Decimal;
use crate::parallel::PortableValue;
use crate::rational::Rational;
use crate::value::IntW;

/// A backstop against a malformed/adversarial length prefix claiming an
/// absurd element count -- mirrors `MAX_BIGINT_LIMBS`'s own "cap before
/// allocating" discipline. Deliberately generous (10 million elements is
/// already a very large single message) so no legitimate program's own
/// data is ever rejected.
pub const MAX_COLLECTION_LEN: u64 = 10_000_000;

const TAG_INT: u8 = 0;
const TAG_SIZED_INT: u8 = 1;
const TAG_F32: u8 = 2;
const TAG_BIGINT: u8 = 3;
const TAG_RATIONAL: u8 = 4;
const TAG_DECIMAL: u8 = 5;
const TAG_FLOAT: u8 = 6;
const TAG_BOOL: u8 = 7;
const TAG_STR: u8 = 8;
const TAG_CHAR: u8 = 9;
const TAG_UNIT: u8 = 10;
const TAG_LIST: u8 = 11;
const TAG_CTOR: u8 = 12;
const TAG_TENSOR: u8 = 13;
const TAG_MAP: u8 = 14;
const TAG_SET: u8 = 15;
const TAG_RANGE: u8 = 16;
const TAG_CAP_NET: u8 = 17;
const TAG_CAP_FS: u8 = 18;

const INT_W_I8: u8 = 0;
const INT_W_I16: u8 = 1;
const INT_W_I32: u8 = 2;
const INT_W_I64: u8 = 3;
const INT_W_U8: u8 = 4;
const INT_W_U16: u8 = 5;
const INT_W_U32: u8 = 6;
const INT_W_U64: u8 = 7;

fn int_w_tag(w: IntW) -> u8 {
    match w {
        IntW::I8 => INT_W_I8,
        IntW::I16 => INT_W_I16,
        IntW::I32 => INT_W_I32,
        IntW::I64 => INT_W_I64,
        IntW::U8 => INT_W_U8,
        IntW::U16 => INT_W_U16,
        IntW::U32 => INT_W_U32,
        IntW::U64 => INT_W_U64,
    }
}

fn int_w_from_tag(tag: u8) -> Result<IntW, String> {
    match tag {
        INT_W_I8 => Ok(IntW::I8),
        INT_W_I16 => Ok(IntW::I16),
        INT_W_I32 => Ok(IntW::I32),
        INT_W_I64 => Ok(IntW::I64),
        INT_W_U8 => Ok(IntW::U8),
        INT_W_U16 => Ok(IntW::U16),
        INT_W_U32 => Ok(IntW::U32),
        INT_W_U64 => Ok(IntW::U64),
        other => Err(format!("kser: unknown IntW tag {other}")),
    }
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = buf.get(*pos) else {
            return Err("kser: truncated varint".to_string());
        };
        *pos += 1;
        if shift >= 64 {
            return Err("kser: varint too long".to_string());
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Reads a length prefix, capped at `max` -- the one place every
/// length-prefixed collection in this module goes through, so the cap can
/// never be bypassed by a future variant forgetting to check it.
fn read_len(buf: &[u8], pos: &mut usize, max: u64) -> Result<usize, String> {
    let len = read_varint(buf, pos)?;
    if len > max {
        return Err(format!("kser: claimed length {len} exceeds the cap of {max}"));
    }
    Ok(len as usize)
}

fn write_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize, max: u64) -> Result<&'a [u8], String> {
    let len = read_len(buf, pos, max)?;
    let Some(slice) = buf.get(*pos..*pos + len) else {
        return Err("kser: truncated byte string".to_string());
    };
    *pos += len;
    Ok(slice)
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_bytes(buf, s.as_bytes());
}

fn read_str(buf: &[u8], pos: &mut usize) -> Result<String, String> {
    let bytes = read_bytes(buf, pos, MAX_COLLECTION_LEN)?;
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("kser: invalid UTF-8 string: {e}"))
}

/// `BigInt` has no public accessor for its own `neg`/`limbs` fields (both
/// private, by design -- every OTHER module reaches it only through its
/// public arithmetic/parsing API), so this encodes through the ONE
/// existing lossless, canonical round-trip it already guarantees:
/// `to_string`/`from_str`. Simpler and provably correct (reuses
/// `BigInt::from_str`'s own `MAX_BIGINT_LIMBS` enforcement for free)
/// rather than a second, parallel limb-level encoding that could drift
/// out of sync with `BigInt`'s own internal representation.
fn write_bigint(buf: &mut Vec<u8>, n: &BigInt) {
    write_str(buf, &n.to_string());
}

fn read_bigint(buf: &[u8], pos: &mut usize) -> Result<BigInt, String> {
    let s = read_str(buf, pos)?;
    BigInt::from_str(&s).ok_or_else(|| format!("kser: invalid BigInt digits: {s:?}"))
}

fn write_value(buf: &mut Vec<u8>, v: &PortableValue) {
    match v {
        PortableValue::Int(n) => {
            buf.push(TAG_INT);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        PortableValue::SizedInt(n, w) => {
            buf.push(TAG_SIZED_INT);
            buf.extend_from_slice(&n.to_le_bytes());
            buf.push(int_w_tag(*w));
        }
        PortableValue::F32(f) => {
            buf.push(TAG_F32);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        PortableValue::BigInt(n) => {
            buf.push(TAG_BIGINT);
            write_bigint(buf, n);
        }
        PortableValue::Rational(r) => {
            buf.push(TAG_RATIONAL);
            write_bigint(buf, &r.num);
            write_bigint(buf, &r.den);
        }
        PortableValue::Decimal(d) => {
            buf.push(TAG_DECIMAL);
            write_bigint(buf, &d.sig);
            write_varint(buf, d.scale as u64);
        }
        PortableValue::Float(f) => {
            buf.push(TAG_FLOAT);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        PortableValue::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(if *b { 1 } else { 0 });
        }
        PortableValue::Str(s) => {
            buf.push(TAG_STR);
            write_str(buf, s);
        }
        PortableValue::Char(c) => {
            buf.push(TAG_CHAR);
            buf.extend_from_slice(&(*c as u32).to_le_bytes());
        }
        PortableValue::Unit => {
            buf.push(TAG_UNIT);
        }
        PortableValue::List(items) => {
            buf.push(TAG_LIST);
            write_varint(buf, items.len() as u64);
            for item in items {
                write_value(buf, item);
            }
        }
        PortableValue::Ctor { ty, variant, fields } => {
            buf.push(TAG_CTOR);
            write_str(buf, ty);
            write_str(buf, variant);
            write_varint(buf, fields.len() as u64);
            for f in fields {
                write_value(buf, f);
            }
        }
        PortableValue::Tensor(xs) => {
            buf.push(TAG_TENSOR);
            write_varint(buf, xs.len() as u64);
            for x in xs {
                buf.extend_from_slice(&x.to_bits().to_le_bytes());
            }
        }
        PortableValue::Map(entries) => {
            buf.push(TAG_MAP);
            write_varint(buf, entries.len() as u64);
            for (k, val) in entries {
                write_value(buf, k);
                write_value(buf, val);
            }
        }
        PortableValue::Set(items) => {
            buf.push(TAG_SET);
            write_varint(buf, items.len() as u64);
            for item in items {
                write_value(buf, item);
            }
        }
        PortableValue::Range(lo, hi, inclusive) => {
            buf.push(TAG_RANGE);
            buf.extend_from_slice(&lo.to_le_bytes());
            buf.extend_from_slice(&hi.to_le_bytes());
            buf.push(if *inclusive { 1 } else { 0 });
        }
        PortableValue::CapNet(scope) => {
            buf.push(TAG_CAP_NET);
            write_option_str(buf, scope);
        }
        PortableValue::CapFs(scope) => {
            buf.push(TAG_CAP_FS);
            write_option_str(buf, scope);
        }
    }
}

fn write_option_str(buf: &mut Vec<u8>, s: &Option<String>) {
    match s {
        Some(s) => {
            buf.push(1);
            write_str(buf, s);
        }
        None => buf.push(0),
    }
}

fn read_option_str(buf: &[u8], pos: &mut usize) -> Result<Option<String>, String> {
    let Some(&tag) = buf.get(*pos) else {
        return Err("kser: truncated Option<Str> tag".to_string());
    };
    *pos += 1;
    match tag {
        0 => Ok(None),
        1 => Ok(Some(read_str(buf, pos)?)),
        other => Err(format!("kser: invalid Option<Str> tag {other}")),
    }
}

fn read_fixed<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let Some(slice) = buf.get(*pos..*pos + n) else {
        return Err("kser: truncated fixed-width field".to_string());
    };
    *pos += n;
    Ok(slice)
}

fn read_value(buf: &[u8], pos: &mut usize) -> Result<PortableValue, String> {
    let Some(&tag) = buf.get(*pos) else {
        return Err("kser: truncated value tag".to_string());
    };
    *pos += 1;
    match tag {
        TAG_INT => {
            let bytes = read_fixed(buf, pos, 8)?;
            Ok(PortableValue::Int(i64::from_le_bytes(bytes.try_into().unwrap())))
        }
        TAG_SIZED_INT => {
            let bytes = read_fixed(buf, pos, 16)?;
            let n = i128::from_le_bytes(bytes.try_into().unwrap());
            let Some(&w_tag) = buf.get(*pos) else {
                return Err("kser: truncated IntW tag".to_string());
            };
            *pos += 1;
            Ok(PortableValue::SizedInt(n, int_w_from_tag(w_tag)?))
        }
        TAG_F32 => {
            let bytes = read_fixed(buf, pos, 4)?;
            Ok(PortableValue::F32(f32::from_bits(u32::from_le_bytes(bytes.try_into().unwrap()))))
        }
        TAG_BIGINT => Ok(PortableValue::BigInt(read_bigint(buf, pos)?)),
        TAG_RATIONAL => {
            let num = read_bigint(buf, pos)?;
            let den = read_bigint(buf, pos)?;
            Ok(PortableValue::Rational(Rational { num, den }))
        }
        TAG_DECIMAL => {
            let sig = read_bigint(buf, pos)?;
            let scale = read_varint(buf, pos)?;
            if scale > u32::MAX as u64 {
                return Err(format!("kser: Decimal scale {scale} exceeds u32::MAX"));
            }
            Ok(PortableValue::Decimal(Decimal { sig, scale: scale as u32 }))
        }
        TAG_FLOAT => {
            let bytes = read_fixed(buf, pos, 8)?;
            Ok(PortableValue::Float(f64::from_bits(u64::from_le_bytes(bytes.try_into().unwrap()))))
        }
        TAG_BOOL => {
            let bytes = read_fixed(buf, pos, 1)?;
            Ok(PortableValue::Bool(bytes[0] != 0))
        }
        TAG_STR => Ok(PortableValue::Str(read_str(buf, pos)?)),
        TAG_CHAR => {
            let bytes = read_fixed(buf, pos, 4)?;
            let cp = u32::from_le_bytes(bytes.try_into().unwrap());
            char::from_u32(cp).map(PortableValue::Char).ok_or_else(|| format!("kser: invalid char codepoint {cp}"))
        }
        TAG_UNIT => Ok(PortableValue::Unit),
        TAG_LIST => {
            let len = read_len(buf, pos, MAX_COLLECTION_LEN)?;
            let mut items = Vec::with_capacity(len.min(1024));
            for _ in 0..len {
                items.push(read_value(buf, pos)?);
            }
            Ok(PortableValue::List(items))
        }
        TAG_CTOR => {
            let ty = read_str(buf, pos)?;
            let variant = read_str(buf, pos)?;
            let len = read_len(buf, pos, MAX_COLLECTION_LEN)?;
            let mut fields = Vec::with_capacity(len.min(1024));
            for _ in 0..len {
                fields.push(read_value(buf, pos)?);
            }
            Ok(PortableValue::Ctor { ty, variant, fields })
        }
        TAG_TENSOR => {
            let len = read_len(buf, pos, MAX_COLLECTION_LEN)?;
            let mut xs = Vec::with_capacity(len.min(1024));
            for _ in 0..len {
                let bytes = read_fixed(buf, pos, 8)?;
                xs.push(f64::from_bits(u64::from_le_bytes(bytes.try_into().unwrap())));
            }
            Ok(PortableValue::Tensor(xs))
        }
        TAG_MAP => {
            let len = read_len(buf, pos, MAX_COLLECTION_LEN)?;
            let mut entries = Vec::with_capacity(len.min(1024));
            for _ in 0..len {
                let k = read_value(buf, pos)?;
                let val = read_value(buf, pos)?;
                entries.push((k, val));
            }
            Ok(PortableValue::Map(entries))
        }
        TAG_SET => {
            let len = read_len(buf, pos, MAX_COLLECTION_LEN)?;
            let mut items = Vec::with_capacity(len.min(1024));
            for _ in 0..len {
                items.push(read_value(buf, pos)?);
            }
            Ok(PortableValue::Set(items))
        }
        TAG_RANGE => {
            let lo_bytes = read_fixed(buf, pos, 8)?;
            let lo = i64::from_le_bytes(lo_bytes.try_into().unwrap());
            let hi_bytes = read_fixed(buf, pos, 8)?;
            let hi = i64::from_le_bytes(hi_bytes.try_into().unwrap());
            let incl_bytes = read_fixed(buf, pos, 1)?;
            Ok(PortableValue::Range(lo, hi, incl_bytes[0] != 0))
        }
        TAG_CAP_NET => Ok(PortableValue::CapNet(read_option_str(buf, pos)?)),
        TAG_CAP_FS => Ok(PortableValue::CapFs(read_option_str(buf, pos)?)),
        other => Err(format!("kser: unknown value tag {other}")),
    }
}

/// Encode a `PortableValue` to its canonical `kser` byte representation.
pub fn to_bytes(v: &PortableValue) -> Vec<u8> {
    let mut buf = Vec::new();
    write_value(&mut buf, v);
    buf
}

/// Decode a `PortableValue` from a `kser` byte slice. `Err` for any
/// truncated, malformed, or length-prefix-exceeds-the-cap input -- never a
/// panic, matching this module's own "fail clean on adversarial input"
/// discipline (the same posture `MAILBOX_CAP`/`MAX_ACTOR_INSTANCES`/
/// `MAX_BIGINT_LIMBS` already take elsewhere in this codebase).
pub fn from_bytes(bytes: &[u8]) -> Result<PortableValue, String> {
    let mut pos = 0;
    let v = read_value(bytes, &mut pos)?;
    if pos != bytes.len() {
        return Err(format!("kser: {} trailing byte(s) after a complete value", bytes.len() - pos));
    }
    Ok(v)
}

/// The maximum size, in bytes, of a single encoded frame's PAYLOAD (the
/// `kser` bytes, not counting the 4-byte length prefix itself) — the
/// message-boundary equivalent of `MAX_COLLECTION_LEN`: a backstop against
/// an adversarial or corrupt length prefix causing an unbounded read-side
/// allocation, not a normal-operation limit. 64 MiB is deliberately
/// generous (`MAX_COLLECTION_LEN`'s own 10-million-element cap alone could
/// already approach this for a `List`/`Map`/`Set` of anything but the
/// smallest elements) so no legitimate single message is ever rejected.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Write one length-prefixed frame: a 4-byte little-endian `u32` byte
/// count, then the payload itself. This is the ONLY message-boundary
/// mechanism this module defines — a raw `TcpStream` (or any other
/// `std::io::Write`) is just a byte stream with no message boundaries of
/// its own, so every caller that wants to send one discrete value per
/// call needs exactly this. Generic over `Write`/`Read` (not hardcoded to
/// `TcpStream`) so it round-trips against an in-memory buffer in tests,
/// with zero behavioral difference from the real socket path.
pub fn write_frame<W: std::io::Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME_LEN as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("kser: frame payload of {} bytes exceeds MAX_FRAME_LEN ({MAX_FRAME_LEN})", payload.len()),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    Ok(())
}

/// Read one length-prefixed frame written by `write_frame`. The length
/// prefix is checked against `MAX_FRAME_LEN` BEFORE the payload buffer is
/// allocated, so a corrupt or adversarial 4-byte prefix can only ever
/// fail cleanly (`Err`) — never attempt an unbounded allocation. A clean
/// EOF exactly at a frame boundary (nothing left to read at all) also
/// returns `Err(UnexpectedEof)`, matching `read_exact`'s own convention —
/// callers that need to distinguish "peer closed cleanly between frames"
/// from "peer closed mid-frame" should check `ErrorKind::UnexpectedEof`
/// themselves (this module doesn't need that distinction internally).
pub fn read_frame<R: std::io::Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("kser: frame length prefix {len} exceeds MAX_FRAME_LEN ({MAX_FRAME_LEN})"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

/// `write_frame` + `to_bytes` composed — the common case, sending one
/// `PortableValue` as one frame.
pub fn write_value_frame<W: std::io::Write>(w: &mut W, v: &PortableValue) -> std::io::Result<()> {
    write_frame(w, &to_bytes(v))
}

/// `read_frame` + `from_bytes` composed. A `kser` decode failure (a
/// well-formed frame whose payload isn't a valid encoded value) is
/// reported as `std::io::ErrorKind::InvalidData`, so callers can handle
/// every failure mode of this function through one `std::io::Result`.
pub fn read_value_frame<R: std::io::Read>(r: &mut R) -> std::io::Result<PortableValue> {
    let payload = read_frame(r)?;
    from_bytes(&payload).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bigint::BigInt;
    use crate::decimal::Decimal;
    use crate::rational::Rational;

    fn roundtrip(v: PortableValue) {
        let bytes = to_bytes(&v);
        let back = from_bytes(&bytes).unwrap_or_else(|e| panic!("decode failed for {v:?}: {e}"));
        assert_eq!(back, v, "kser round-trip mismatch");
    }

    #[test]
    fn scalars_roundtrip() {
        roundtrip(PortableValue::Int(0));
        roundtrip(PortableValue::Int(-1));
        roundtrip(PortableValue::Int(i64::MIN));
        roundtrip(PortableValue::Int(i64::MAX));
        roundtrip(PortableValue::SizedInt(-128, IntW::I8));
        roundtrip(PortableValue::SizedInt(255, IntW::U8));
        roundtrip(PortableValue::F32(3.5));
        roundtrip(PortableValue::F32(f32::NEG_INFINITY));
        roundtrip(PortableValue::Float(0.1));
        roundtrip(PortableValue::Float(f64::INFINITY));
        roundtrip(PortableValue::Bool(true));
        roundtrip(PortableValue::Bool(false));
        roundtrip(PortableValue::Str("hello, \u{1F600} world".to_string()));
        roundtrip(PortableValue::Str(String::new()));
        roundtrip(PortableValue::Char('x'));
        roundtrip(PortableValue::Char('\u{1F600}'));
        roundtrip(PortableValue::Unit);
        roundtrip(PortableValue::Range(-5, 10, true));
        roundtrip(PortableValue::Range(0, 0, false));
        roundtrip(PortableValue::CapNet(None));
        roundtrip(PortableValue::CapNet(Some("example.com".to_string())));
        roundtrip(PortableValue::CapFs(Some("/tmp".to_string())));
    }

    /// `NaN != NaN` under IEEE 754 (and therefore under `PartialEq`), so
    /// `roundtrip`'s plain `assert_eq!` can never be used for it -- checked
    /// via exact bit-pattern preservation instead, which is what actually
    /// matters for a wire format (an f64/f32 NaN's payload bits are
    /// meaningful in some use cases, e.g. NaN-boxing; `to_bits`/
    /// `from_bits` round-trips them exactly, unlike a naive re-derivation
    /// through arithmetic).
    #[test]
    fn nan_bit_patterns_survive_the_wire_exactly() {
        let f32_bytes = to_bytes(&PortableValue::F32(f32::NAN));
        match from_bytes(&f32_bytes).unwrap() {
            PortableValue::F32(f) => assert_eq!(f.to_bits(), f32::NAN.to_bits()),
            other => panic!("expected F32, got {other:?}"),
        }
        let f64_bytes = to_bytes(&PortableValue::Float(f64::NAN));
        match from_bytes(&f64_bytes).unwrap() {
            PortableValue::Float(f) => assert_eq!(f.to_bits(), f64::NAN.to_bits()),
            other => panic!("expected Float, got {other:?}"),
        }
        let tensor_bytes = to_bytes(&PortableValue::Tensor(vec![1.0, f64::NAN]));
        match from_bytes(&tensor_bytes).unwrap() {
            PortableValue::Tensor(xs) => {
                assert_eq!(xs[0], 1.0);
                assert_eq!(xs[1].to_bits(), f64::NAN.to_bits());
            }
            other => panic!("expected Tensor, got {other:?}"),
        }
    }

    #[test]
    fn bigint_rational_decimal_roundtrip() {
        roundtrip(PortableValue::BigInt(BigInt::zero()));
        roundtrip(PortableValue::BigInt(BigInt::from_i64(-123456789012345)));
        let huge = BigInt::from_str(&"9".repeat(400)).unwrap();
        roundtrip(PortableValue::BigInt(huge));
        roundtrip(PortableValue::Rational(Rational { num: BigInt::from_i64(3), den: BigInt::from_i64(4) }));
        roundtrip(PortableValue::Decimal(Decimal { sig: BigInt::from_i64(31415), scale: 4 }));
    }

    #[test]
    fn nested_collections_roundtrip() {
        roundtrip(PortableValue::List(vec![PortableValue::Int(1), PortableValue::Str("a".to_string())]));
        roundtrip(PortableValue::List(vec![]));
        roundtrip(PortableValue::Tensor(vec![1.0, -2.5, 7.25]));
        roundtrip(PortableValue::Map(vec![(PortableValue::Str("k".to_string()), PortableValue::Int(1))]));
        roundtrip(PortableValue::Set(vec![PortableValue::Int(1), PortableValue::Int(2)]));
        roundtrip(PortableValue::Ctor {
            ty: "Result".to_string(),
            variant: "Ok".to_string(),
            fields: vec![PortableValue::List(vec![PortableValue::Ctor {
                ty: "Option".to_string(),
                variant: "Some".to_string(),
                fields: vec![PortableValue::Int(42)],
            }])],
        });
    }

    /// A length prefix claiming far more than the actual remaining bytes
    /// could ever hold must fail cleanly (`Err`), never attempt to
    /// allocate/read that claimed amount.
    #[test]
    fn a_length_prefix_past_the_cap_is_rejected_cleanly() {
        let mut buf = vec![TAG_LIST];
        write_varint(&mut buf, MAX_COLLECTION_LEN + 1);
        assert!(from_bytes(&buf).is_err());
    }

    /// A length prefix within the cap but past the ACTUAL remaining bytes
    /// (a truncated/corrupt stream, not necessarily adversarial) must also
    /// fail cleanly, not panic on an out-of-bounds slice.
    #[test]
    fn a_truncated_stream_is_rejected_cleanly_not_a_panic() {
        let mut buf = vec![TAG_LIST];
        write_varint(&mut buf, 5);
        buf.extend_from_slice(&to_bytes(&PortableValue::Int(1)));
        assert!(from_bytes(&buf).is_err());
    }

    #[test]
    fn an_unknown_tag_is_rejected_cleanly() {
        let buf = vec![255u8];
        assert!(from_bytes(&buf).is_err());
    }

    #[test]
    fn trailing_bytes_after_a_complete_value_are_rejected() {
        let mut buf = to_bytes(&PortableValue::Int(1));
        buf.push(0);
        assert!(from_bytes(&buf).is_err());
    }

    #[test]
    fn a_value_survives_a_frame_round_trip_over_an_in_memory_buffer() {
        let mut buf = Vec::new();
        write_value_frame(&mut buf, &PortableValue::Str("hello over the wire".to_string())).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back = read_value_frame(&mut cursor).unwrap();
        assert_eq!(back, PortableValue::Str("hello over the wire".to_string()));
    }

    /// Multiple frames written back-to-back must each be read back
    /// independently, in order -- proving the length prefix genuinely
    /// delimits messages on a byte stream with no boundaries of its own,
    /// not just that a single isolated frame round-trips.
    #[test]
    fn multiple_frames_in_sequence_are_read_back_in_order() {
        let mut buf = Vec::new();
        write_value_frame(&mut buf, &PortableValue::Int(1)).unwrap();
        write_value_frame(&mut buf, &PortableValue::Int(2)).unwrap();
        write_value_frame(&mut buf, &PortableValue::Str("three".to_string())).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_value_frame(&mut cursor).unwrap(), PortableValue::Int(1));
        assert_eq!(read_value_frame(&mut cursor).unwrap(), PortableValue::Int(2));
        assert_eq!(read_value_frame(&mut cursor).unwrap(), PortableValue::Str("three".to_string()));
    }

    /// A length prefix claiming more than `MAX_FRAME_LEN` is rejected
    /// cleanly BEFORE any payload allocation, exactly like `kser`'s own
    /// `MAX_COLLECTION_LEN` guard one level down.
    #[test]
    fn a_frame_length_past_the_cap_is_rejected_cleanly() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// `write_frame` itself refuses to write a payload past the cap,
    /// rather than silently writing a length prefix its own `read_frame`
    /// would then reject -- symmetry: nothing this module ever WRITES can
    /// fail its own READ-side check.
    #[test]
    fn write_frame_itself_refuses_a_payload_past_the_cap() {
        let oversized = vec![0u8; MAX_FRAME_LEN as usize + 1];
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, &oversized).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// An empty stream (nothing written at all) must fail cleanly, not
    /// panic or hang -- `read_exact`'s own `UnexpectedEof` propagates
    /// through unchanged.
    #[test]
    fn reading_a_frame_from_an_empty_stream_fails_cleanly() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// A stream that ends partway through a frame's declared payload
    /// (length prefix says N bytes, fewer than N are actually present)
    /// must also fail cleanly, not read short/garbage data.
    #[test]
    fn a_frame_truncated_mid_payload_fails_cleanly() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&[1, 2, 3]);
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
