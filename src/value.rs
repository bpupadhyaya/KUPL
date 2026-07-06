//! Runtime values and environments.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::ast::Block;

/// A fixed-width integer type. Values are stored in an `i128`, which exactly
/// represents every `i8..=u64` value (`u64::MAX < i128::MAX`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntW {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntW {
    pub fn min(self) -> i128 {
        match self {
            IntW::I8 => i8::MIN as i128,
            IntW::I16 => i16::MIN as i128,
            IntW::I32 => i32::MIN as i128,
            IntW::I64 => i64::MIN as i128,
            IntW::U8 | IntW::U16 | IntW::U32 | IntW::U64 => 0,
        }
    }
    pub fn max(self) -> i128 {
        match self {
            IntW::I8 => i8::MAX as i128,
            IntW::I16 => i16::MAX as i128,
            IntW::I32 => i32::MAX as i128,
            IntW::I64 => i64::MAX as i128,
            IntW::U8 => u8::MAX as i128,
            IntW::U16 => u16::MAX as i128,
            IntW::U32 => u32::MAX as i128,
            IntW::U64 => u64::MAX as i128,
        }
    }
    pub fn check_range(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }
    /// Width in bits.
    pub fn bits(self) -> u32 {
        match self {
            IntW::I8 | IntW::U8 => 8,
            IntW::I16 | IntW::U16 => 16,
            IntW::I32 | IntW::U32 => 32,
            IntW::I64 | IntW::U64 => 64,
        }
    }
    pub fn is_signed(self) -> bool {
        matches!(self, IntW::I8 | IntW::I16 | IntW::I32 | IntW::I64)
    }
    /// Reduce an arbitrary i128 into this width by modular wraparound.
    pub fn wrap(self, v: i128) -> i128 {
        let m = 1i128 << self.bits(); // 2^b
        let r = v.rem_euclid(m); // 0..2^b
        if self.is_signed() && r > self.max() {
            r - m
        } else {
            r
        }
    }
    /// Clamp an arbitrary i128 into this width's range.
    pub fn saturate(self, v: i128) -> i128 {
        v.clamp(self.min(), self.max())
    }
    pub fn name(self) -> &'static str {
        match self {
            IntW::I8 => "i8",
            IntW::I16 => "i16",
            IntW::I32 => "i32",
            IntW::I64 => "i64",
            IntW::U8 => "u8",
            IntW::U16 => "u16",
            IntW::U32 => "u32",
            IntW::U64 => "u64",
        }
    }
    /// A stable byte tag for serialization (.kx modules).
    pub fn tag(self) -> u8 {
        match self {
            IntW::I8 => 0,
            IntW::I16 => 1,
            IntW::I32 => 2,
            IntW::I64 => 3,
            IntW::U8 => 4,
            IntW::U16 => 5,
            IntW::U32 => 6,
            IntW::U64 => 7,
        }
    }
    pub fn from_tag(t: u8) -> Option<IntW> {
        Some(match t {
            0 => IntW::I8,
            1 => IntW::I16,
            2 => IntW::I32,
            3 => IntW::I64,
            4 => IntW::U8,
            5 => IntW::U16,
            6 => IntW::U32,
            7 => IntW::U64,
            _ => return None,
        })
    }
    /// Parse a width suffix / type name.
    pub fn from_name(s: &str) -> Option<IntW> {
        Some(match s {
            "i8" => IntW::I8,
            "i16" => IntW::I16,
            "i32" => IntW::I32,
            "i64" => IntW::I64,
            "u8" => IntW::U8,
            "u16" => IntW::U16,
            "u32" => IntW::U32,
            "u64" => IntW::U64,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod intw_tests {
    use super::IntW;

    #[test]
    fn wrap_wraps_modularly() {
        assert_eq!(IntW::U8.wrap(256), 0);
        assert_eq!(IntW::U8.wrap(255), 255);
        assert_eq!(IntW::U8.wrap(-1), 255);
        assert_eq!(IntW::U8.wrap(300), 44);
        assert_eq!(IntW::I8.wrap(128), -128);
        assert_eq!(IntW::I8.wrap(-129), 127);
        assert_eq!(IntW::I8.wrap(127), 127);
    }

    #[test]
    fn saturate_clamps() {
        assert_eq!(IntW::U8.saturate(300), 255);
        assert_eq!(IntW::U8.saturate(-5), 0);
        assert_eq!(IntW::I8.saturate(200), 127);
        assert_eq!(IntW::I8.saturate(-200), -128);
        assert_eq!(IntW::I8.saturate(42), 42);
    }
}

#[derive(Clone)]
pub enum Value {
    // (Debug is implemented manually below via Display)
    Int(i64),
    /// A fixed-width integer (`255u8`, `1000i16`, …). The `i128` value + width
    /// are boxed so `Value` stays 24 bytes (a bare `i128` is 16-byte-aligned and
    /// would grow the whole enum to 32 — sized ints are rare, so they pay the
    /// indirection instead of every value paying the size).
    SizedInt(Box<(i128, IntW)>),
    /// A single-precision float (`1.5f32`).
    F32(f32),
    /// An arbitrary-precision integer (`big(…)`).
    BigInt(Rc<crate::bigint::BigInt>),
    Rational(Rc<crate::rational::Rational>),
    Float(f64),
    Bool(bool),
    Str(Rc<String>),
    Unit,
    List(Rc<Vec<Value>>),
    /// ADT value: `Ctor { ty: "Shape", variant: "Circle", fields: [1.5] }`.
    Ctor {
        ty: Rc<String>,
        variant: Rc<String>,
        fields: Rc<Vec<Value>>,
    },
    Closure(Rc<Closure>),
    /// Reference to a named top-level function.
    Fun(Rc<String>),
    /// Reference to a component instance in the runtime.
    Component(usize),
    /// An expose function bound to a live instance (used by laws/tests).
    Bound(usize, Rc<String>),
    /// A KVM closure: prototype index + captured values (captured by value).
    VmClosure(u16, Rc<Vec<Value>>),
    /// A rank-1 tensor of f64 (shapes/dtypes arrive with KIR; ops are native loops).
    Tensor(Rc<Vec<f64>>),
    /// Insertion-ordered immutable map (association pairs; updates keep position).
    Map(Rc<Vec<(Value, Value)>>),
    /// Insertion-ordered immutable set.
    Set(Rc<Vec<Value>>),
    Range(i64, i64, bool),
}

pub struct Closure {
    pub params: Vec<String>,
    pub body: Rc<Block>,
    /// Free locals captured BY VALUE at creation (a snapshot), rebound fresh on
    /// every call — matching the KVM/native `MakeClosure` semantics. (A live env
    /// clone would give reference capture, which diverges across engines.)
    pub captures: Vec<(Box<str>, Value)>,
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(s.into()))
    }
    pub fn some(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Option".into()),
            variant: Rc::new("Some".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn none() -> Value {
        Value::Ctor {
            ty: Rc::new("Option".into()),
            variant: Rc::new("None".into()),
            fields: Rc::new(vec![]),
        }
    }
    pub fn ok(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Result".into()),
            variant: Rc::new("Ok".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn err(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Result".into()),
            variant: Rc::new("Err".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn type_name(&self) -> String {
        match self {
            Value::Int(_) => "Int".into(),
            Value::SizedInt(b) => b.1.name().into(),
            Value::F32(_) => "f32".into(),
            Value::BigInt(_) => "BigInt".into(),
            Value::Rational(_) => "Rational".into(),
            Value::Float(_) => "Float".into(),
            Value::Bool(_) => "Bool".into(),
            Value::Str(_) => "Str".into(),
            Value::Unit => "Unit".into(),
            Value::List(_) => "List".into(),
            Value::Ctor { ty, .. } => ty.as_str().into(),
            Value::Closure(_) => "fn".into(),
            Value::Fun(_) => "fn".into(),
            Value::Component(_) => "component".into(),
            Value::Bound(..) => "fn".into(),
            Value::VmClosure(..) => "fn".into(),
            Value::Tensor(_) => "Tensor".into(),
            Value::Map(_) => "Map".into(),
            Value::Set(_) => "Set".into(),
            Value::Range(..) => "Range".into(),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            // sized ints are equal iff both value AND width match
            (Value::SizedInt(a), Value::SizedInt(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (Value::Rational(a), Value::Rational(b)) => a == b,
            (Value::F32(a), Value::F32(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            (Value::List(a), Value::List(b)) => a == b,
            (
                Value::Ctor { ty: t1, variant: v1, fields: f1 },
                Value::Ctor { ty: t2, variant: v2, fields: f2 },
            ) => t1 == t2 && v1 == v2 && f1 == f2,
            (Value::Component(a), Value::Component(b)) => a == b,
            (Value::Range(a, b, i), Value::Range(c, d, j)) => a == c && b == d && i == j,
            (Value::Tensor(a), Value::Tensor(b)) => a == b,
            // Map/Set equality is order-insensitive (Python dict/set semantics)
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(k, v)| {
                        b.iter().any(|(k2, v2)| k == k2 && v == v2)
                    })
            }
            (Value::Set(a), Value::Set(b)) => {
                a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| x == y))
            }
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{v}"),
            Value::SizedInt(b) => write!(f, "{}", b.0),
            Value::BigInt(b) => write!(f, "{b}"),
            Value::Rational(r) => write!(f, "{r}"),
            Value::F32(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            Value::Float(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            Value::Bool(v) => write!(f, "{v}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Unit => write!(f, "()"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", DebugStr(item))?;
                }
                write!(f, "]")
            }
            Value::Ctor { variant, fields, .. } => {
                write!(f, "{variant}")?;
                if !fields.is_empty() {
                    write!(f, "(")?;
                    for (i, field) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", DebugStr(field))?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::Closure(_) => write!(f, "<fn>"),
            Value::Fun(name) => write!(f, "<fn {name}>"),
            Value::Component(id) => write!(f, "<component #{id}>"),
            Value::Bound(id, name) => write!(f, "<fn {name} of #{id}>"),
            Value::VmClosure(proto, _) => write!(f, "<fn @{proto}>"),
            Value::Map(pairs) => {
                write!(f, "Map{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", DebugStr(k), DebugStr(v))?;
                }
                write!(f, "}}")
            }
            Value::Set(items) => {
                write!(f, "Set{{")?;
                for (i, x) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", DebugStr(x))?;
                }
                write!(f, "}}")
            }
            Value::Tensor(data) => {
                write!(f, "Tensor([")?;
                for (i, x) in data.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", Value::Float(*x))?;
                }
                write!(f, "])")
            }
            Value::Range(a, b, incl) => write!(f, "{a}..{}{b}", if *incl { "=" } else { "" }),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// Like Display, but strings are quoted (used inside containers).
struct DebugStr<'a>(&'a Value);

impl fmt::Display for DebugStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Str(s) => write!(f, "\"{s}\""),
            other => write!(f, "{other}"),
        }
    }
}

/// Lexically scoped, shared, mutable environment (closures capture it).
///
/// A scope holds its bindings in a small `Vec` scanned linearly rather than a
/// `HashMap`: real scopes hold only a handful of variables (function params +
/// a few locals), for which a linear scan over contiguous memory beats hashing
/// a `String` key — and it allocates far less per call (no hash table per scope).
/// Binding order is not observable (the env is only get/set/define, never
/// iterated for output), so this cannot affect byte-identity.
#[derive(Clone)]
pub struct Env(Rc<RefCell<EnvInner>>);

struct EnvInner {
    vars: Vec<(Box<str>, Value)>,
    parent: Option<Env>,
}

impl Env {
    pub fn new() -> Env {
        Env(Rc::new(RefCell::new(EnvInner { vars: Vec::new(), parent: None })))
    }
    pub fn child(&self) -> Env {
        Env(Rc::new(RefCell::new(EnvInner {
            vars: Vec::new(),
            parent: Some(self.clone()),
        })))
    }
    pub fn define(&self, name: &str, value: Value) {
        let mut inner = self.0.borrow_mut();
        // Re-`let` in the same scope overwrites (HashMap-insert semantics).
        if let Some(slot) = inner.vars.iter_mut().find(|(k, _)| &**k == name) {
            slot.1 = value;
        } else {
            inner.vars.push((name.into(), value));
        }
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        let inner = self.0.borrow();
        // Scan newest-first so a shadowing binding wins (matches lexical scope).
        for (k, v) in inner.vars.iter().rev() {
            if &**k == name {
                return Some(v.clone());
            }
        }
        match &inner.parent {
            Some(p) => p.get(name),
            None => None,
        }
    }
    /// Fast path for `x = x + <str>`: if `name` is bound to a UNIQUELY-owned `Str`,
    /// append `suffix` to it in place and return true. Returns false if the binding
    /// is missing, isn't a `Str`, or is shared (Rc strong_count > 1) — the caller
    /// then falls back to an allocating concat, so value semantics are preserved (a
    /// string aliased by another binding is never mutated). Turns an O(n^2) build
    /// loop (`while … { s = s + "x" }`) into O(n). Two NUL-free UTF-8 strings
    /// concatenate to a NUL-free UTF-8 string, so K0008 still holds.
    pub fn append_str_in_place(&self, name: &str, suffix: &str) -> bool {
        let mut inner = self.0.borrow_mut();
        if let Some(slot) = inner.vars.iter_mut().rev().find(|(k, _)| &**k == name) {
            if let Value::Str(rc) = &mut slot.1 {
                if let Some(s) = Rc::get_mut(rc) {
                    s.push_str(suffix);
                    return true;
                }
            }
            return false;
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.append_str_in_place(name, suffix)
            }
            None => false,
        }
    }

    /// Fast path for `xs = xs.push(item)`: if `name` is bound to a UNIQUELY-owned
    /// `List`, push `item` onto it in place and return `None`. Otherwise (missing,
    /// not a List, or shared) hand the item back as `Some(item)` so the caller can
    /// fall back to the allocating push — value semantics preserved (an aliased list
    /// is never mutated). Turns an O(n^2) build loop into O(n).
    pub fn push_list_in_place(&self, name: &str, item: Value) -> Option<Value> {
        let mut inner = self.0.borrow_mut();
        if let Some(slot) = inner.vars.iter_mut().rev().find(|(k, _)| &**k == name) {
            if let Value::List(rc) = &mut slot.1 {
                if let Some(v) = Rc::get_mut(rc) {
                    v.push(item);
                    return None;
                }
            }
            return Some(item);
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.push_list_in_place(name, item)
            }
            None => Some(item),
        }
    }

    /// `m = m.insert(k, v)` in place when `m` is a uniquely-owned Map — updates or
    /// appends the pair without cloning the whole assoc-list, turning an O(n^2)
    /// map-building loop into O(n) allocations. Returns None on success, or
    /// Some((k, v)) to fall back (shared map / not found / other shape). Behaves
    /// exactly like `.insert` (same overwrite semantics, same insertion order).
    pub fn insert_map_in_place(&self, name: &str, key: Value, val: Value) -> Option<(Value, Value)> {
        let mut inner = self.0.borrow_mut();
        if let Some(slot) = inner.vars.iter_mut().rev().find(|(k, _)| &**k == name) {
            if let Value::Map(rc) = &mut slot.1 {
                if let Some(pairs) = Rc::get_mut(rc) {
                    match pairs.iter_mut().find(|(pk, _)| *pk == key) {
                        Some(pair) => pair.1 = val,
                        None => pairs.push((key, val)),
                    }
                    return None;
                }
            }
            return Some((key, val));
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.insert_map_in_place(name, key, val)
            }
            None => Some((key, val)),
        }
    }

    /// `s = s.insert(v)` in place when `s` is a uniquely-owned Set — the Set-build
    /// analogue of `insert_map_in_place` (a Set is an insertion-ordered `Vec` with
    /// dedup). None on success, Some(v) to fall back.
    pub fn insert_set_in_place(&self, name: &str, v: Value) -> Option<Value> {
        let mut inner = self.0.borrow_mut();
        if let Some(slot) = inner.vars.iter_mut().rev().find(|(k, _)| &**k == name) {
            if let Value::Set(rc) = &mut slot.1 {
                if let Some(items) = Rc::get_mut(rc) {
                    if !items.iter().any(|x| *x == v) {
                        items.push(v);
                    }
                    return None;
                }
            }
            return Some(v);
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.insert_set_in_place(name, v)
            }
            None => Some(v),
        }
    }

    /// Assign to an existing binding (walks up the chain). Returns false if unbound.
    pub fn set(&self, name: &str, value: Value) -> bool {
        let mut inner = self.0.borrow_mut();
        if let Some(slot) = inner.vars.iter_mut().rev().find(|(k, _)| &**k == name) {
            slot.1 = value;
            return true;
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.set(name, value)
            }
            None => false,
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}
