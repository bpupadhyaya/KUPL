//! Real-thread parallelism for the narrowest provably-safe case: `par_map` with
//! a PURE named callback over a large list. Everything else falls back to the
//! sequential `shared_method` path unchanged.
//!
//! Why this is safe. A pure top-level function (empty inferred effect set) is
//! referentially transparent: it cannot do I/O, cannot observe the clock or
//! randomness, and KUPL has no global mutable state. So evaluating it on N
//! elements in any order, on any thread, yields the same N results. We place
//! each result in its input-index slot, so the output list is identical to the
//! sequential `map` — byte-for-byte. The differential harness (interp vs the
//! sequential KVM) proves this on every run.
//!
//! Values are `Rc`-based and not `Send`, so nothing of type `Value` crosses a
//! thread boundary. `PortableValue` is a deep-cloned, `Send + Sync` mirror of
//! the plain-data variants; workers receive `PortableValue` arguments, rebuild
//! `Value`s thread-locally, evaluate, and return `PortableValue` results.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::FunDecl;
use crate::diag::Span;
use crate::value::{IntW, Value};

/// Below this length, the thread setup isn't worth it — stay sequential.
const THRESHOLD: usize = 256;

/// A `Send + Sync` mirror of the plain-data `Value` variants (no `Rc`, no
/// closures/instances). This is the only thing that crosses a thread boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum PortableValue {
    Int(i64),
    SizedInt(i128, IntW),
    F32(f32),
    BigInt(crate::bigint::BigInt),
    Rational(crate::rational::Rational),
    Float(f64),
    Bool(bool),
    Str(String),
    Unit,
    List(Vec<PortableValue>),
    Ctor { ty: String, variant: String, fields: Vec<PortableValue> },
    Tensor(Vec<f64>),
    Map(Vec<(PortableValue, PortableValue)>),
    Set(Vec<PortableValue>),
    Range(i64, i64, bool),
}

/// Convert a `Value` to its portable form, or `None` if it holds anything
/// non-portable (a closure, function reference, live component, or VM closure).
pub fn to_portable(v: &Value) -> Option<PortableValue> {
    Some(match v {
        Value::Int(n) => PortableValue::Int(*n),
        Value::SizedInt(b) => PortableValue::SizedInt(b.0, b.1),
        Value::F32(f) => PortableValue::F32(*f),
        Value::BigInt(b) => PortableValue::BigInt((**b).clone()),
        Value::Rational(r) => PortableValue::Rational((**r).clone()),
        Value::Float(f) => PortableValue::Float(*f),
        Value::Bool(b) => PortableValue::Bool(*b),
        Value::Str(s) => PortableValue::Str((**s).clone()),
        Value::Unit => PortableValue::Unit,
        Value::List(xs) => {
            PortableValue::List(xs.iter().map(to_portable).collect::<Option<_>>()?)
        }
        Value::Ctor { ty, variant, fields } => PortableValue::Ctor {
            ty: (**ty).clone(),
            variant: (**variant).clone(),
            fields: fields.iter().map(to_portable).collect::<Option<_>>()?,
        },
        Value::Tensor(d) => PortableValue::Tensor((**d).clone()),
        Value::Map(pairs) => PortableValue::Map(
            pairs
                .iter()
                .map(|(k, v)| Some((to_portable(k)?, to_portable(v)?)))
                .collect::<Option<_>>()?,
        ),
        Value::Set(xs) => {
            PortableValue::Set(xs.iter().map(to_portable).collect::<Option<_>>()?)
        }
        Value::Range(a, b, inc) => PortableValue::Range(*a, *b, *inc),
        Value::Closure(_)
        | Value::Fun(_)
        | Value::Component(_)
        | Value::Bound(..)
        | Value::VmClosure(..) => return None,
    })
}

/// Rebuild a `Value` from its portable form (thread-local; makes fresh `Rc`s).
pub fn from_portable(p: &PortableValue) -> Value {
    match p {
        PortableValue::Int(n) => Value::Int(*n),
        PortableValue::SizedInt(v, w) => Value::SizedInt(Box::new((*v, *w))),
        PortableValue::F32(f) => Value::F32(*f),
        PortableValue::BigInt(b) => Value::BigInt(std::rc::Rc::new(b.clone())),
        PortableValue::Rational(r) => Value::Rational(std::rc::Rc::new(r.clone())),
        PortableValue::Float(f) => Value::Float(*f),
        PortableValue::Bool(b) => Value::Bool(*b),
        PortableValue::Str(s) => Value::Str(Rc::new(s.clone())),
        PortableValue::Unit => Value::Unit,
        PortableValue::List(xs) => Value::List(Rc::new(xs.iter().map(from_portable).collect())),
        PortableValue::Ctor { ty, variant, fields } => Value::Ctor {
            ty: Rc::new(ty.clone()),
            variant: Rc::new(variant.clone()),
            fields: Rc::new(fields.iter().map(from_portable).collect()),
        },
        PortableValue::Tensor(d) => Value::Tensor(Rc::new(d.clone())),
        PortableValue::Map(pairs) => Value::Map(Rc::new(
            pairs.iter().map(|(k, v)| (from_portable(k), from_portable(v))).collect(),
        )),
        PortableValue::Set(xs) => Value::Set(Rc::new(xs.iter().map(from_portable).collect())),
        PortableValue::Range(a, b, inc) => Value::Range(*a, *b, *inc),
    }
}

/// Everything a PURE function needs to evaluate on a worker thread, in a
/// `Send + Sync` form (AST `FunDecl`s hold only owned data; `Rc` is replaced by
/// `Arc`). Built once alongside `ProgramDb`.
pub struct ProgramImage {
    pub funs: Arc<HashMap<String, Arc<FunDecl>>>,
    pub ctors: Arc<HashMap<String, (String, Vec<String>)>>,
    pub type_variants: Arc<crate::prop::TypeDb>,
    pub pure_funs: Arc<HashSet<String>>,
}

impl ProgramImage {
    pub fn from_db(db: &crate::interp::ProgramDb) -> Arc<ProgramImage> {
        let funs = db.funs.iter().map(|(k, v)| (k.clone(), Arc::new((**v).clone()))).collect();
        Arc::new(ProgramImage {
            funs: Arc::new(funs),
            ctors: Arc::new(db.ctors.clone()),
            type_variants: Arc::new(db.type_variants.clone()),
            pure_funs: Arc::new(db.pure_funs.clone()),
        })
    }

    /// Rebuild a minimal `ProgramDb` for a worker thread — enough to evaluate a
    /// pure function (its own funs, constructors, and type variants). Components,
    /// contracts, and ai-funs are irrelevant to pure code.
    fn worker_db(&self) -> crate::interp::ProgramDb {
        crate::interp::ProgramDb {
            funs: self.funs.iter().map(|(k, v)| (k.clone(), Rc::new((**v).clone()))).collect(),
            components: HashMap::new(),
            contracts: HashMap::new(),
            ctors: (*self.ctors).clone(),
            ai_funs: HashMap::new(),
            type_variants: (*self.type_variants).clone(),
            pure_funs: HashSet::new(), // workers stay sequential (no nested threads)
        }
    }
}

/// The gated real-thread fast path for `xs.par_map(pure_fn)`. Returns `None`
/// (→ sequential fallback) unless every precondition holds: the method is
/// `par_map`, the receiver is a list of at least `THRESHOLD` fully-portable
/// elements, and the callback is a named function known to be pure.
pub fn try_par_map(
    recv: &Value,
    name: &str,
    args: &[Value],
    image: &Arc<ProgramImage>,
) -> Option<Result<Value, String>> {
    if name != "par_map" {
        return None;
    }
    let (portable, fname) = gate(recv, args, image)?;
    // map keeps EVERY result, from_portable back into `Value`s, in index order.
    let mut out = Vec::with_capacity(portable.len());
    for r in par_eval(&portable, &fname, image) {
        match r {
            Ok(pv) => out.push(from_portable(&pv)),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(Value::List(Rc::new(out))))
}

/// Real-thread `xs.par_filter(pure_pred)`: evaluate the predicate on every
/// element in parallel, then keep the ORIGINAL elements whose predicate is
/// `true`, in input-index order — byte-identical to the sequential `filter`
/// (which keeps `x` only when `call(pred, x)` matches `Value::Bool(true)`).
pub fn try_par_filter(
    recv: &Value,
    name: &str,
    args: &[Value],
    image: &Arc<ProgramImage>,
) -> Option<Result<Value, String>> {
    if name != "par_filter" {
        return None;
    }
    let (portable, fname) = gate(recv, args, image)?;
    let mut out = Vec::new();
    for (i, r) in par_eval(&portable, &fname, image).into_iter().enumerate() {
        match r {
            Ok(PortableValue::Bool(true)) => out.push(from_portable(&portable[i])),
            // false OR any non-Bool: excluded, exactly as the sequential
            // `if let Value::Bool(true) = …` does (it never errors on non-Bool).
            Ok(_) => {}
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(Value::List(Rc::new(out))))
}

// Note: there is deliberately no `try_par_each`. A pure function has no effects,
// so `list.par_each(pure_fn)` does nothing observable — parallelizing a no-op is
// pointless. A callback with effects isn't pure and wouldn't qualify anyway.

/// Shared preconditions for a parallel list method: the receiver is a list of at
/// least `THRESHOLD` fully-portable elements and the callback is a named pure
/// function. Returns the portable elements + the function name, or `None` to
/// fall back to the sequential path.
fn gate(recv: &Value, args: &[Value], image: &ProgramImage) -> Option<(Vec<PortableValue>, String)> {
    let Value::List(items) = recv else { return None };
    if items.len() < THRESHOLD {
        return None;
    }
    let fname: String = match args.first() {
        Some(Value::Fun(n)) if image.pure_funs.contains(n.as_str()) => (**n).clone(),
        _ => return None,
    };
    let mut portable: Vec<PortableValue> = Vec::with_capacity(items.len());
    for it in items.iter() {
        portable.push(to_portable(it)?); // any non-portable element → sequential
    }
    Some((portable, fname))
}

/// Evaluate `fname` on every element across worker threads, returning the
/// per-element results in INPUT-INDEX ORDER. Each worker owns a disjoint index
/// range, builds one thread-local `Interp`, and returns its results in order;
/// `PortableValue` is the only thing that crosses a thread boundary.
fn par_eval(
    portable: &[PortableValue],
    fname: &str,
    image: &Arc<ProgramImage>,
) -> Vec<Result<PortableValue, String>> {
    let n = portable.len();
    let workers =
        std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1).clamp(1, n.max(1));
    let chunk = n.div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for w in 0..workers {
            let start = w * chunk;
            if start >= n {
                break;
            }
            let end = ((w + 1) * chunk).min(n);
            let slice = &portable[start..end];
            let image = image;
            handles.push(scope.spawn(move || {
                let mut interp = crate::interp::Interp::new_bare(image.worker_db());
                slice.iter().map(|p| eval_one(&mut interp, fname, p)).collect::<Vec<_>>()
            }));
        }
        let mut all = Vec::with_capacity(n);
        for h in handles {
            all.extend(h.join().expect("par worker thread panicked"));
        }
        all
    })
}

/// Evaluate the pure function on one element (thread-local `Value`s only).
fn eval_one(
    interp: &mut crate::interp::Interp,
    fname: &str,
    arg: &PortableValue,
) -> Result<PortableValue, String> {
    let f = Value::Fun(Rc::new(fname.to_string()));
    match interp.call_value(f, vec![from_portable(arg)], Span::default()) {
        Ok(v) => to_portable(&v)
            .ok_or_else(|| "parallel callback returned a non-portable value".to_string()),
        Err(crate::interp::Flow::Panic { msg, .. }) => Err(msg),
        Err(_) => Err("invalid control flow in parallel callback".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn portable_is_send_sync() {
        assert_send_sync::<PortableValue>();
        assert_send_sync::<Arc<ProgramImage>>();
    }

    #[test]
    fn round_trip_every_plain_variant() {
        let samples = vec![
            Value::Int(-7),
            Value::SizedInt(Box::new((200, IntW::U8))),
            Value::F32(1.5),
            Value::Float(3.25),
            Value::Bool(true),
            Value::Str(Rc::new("héllo".to_string())),
            Value::Unit,
            Value::List(Rc::new(vec![Value::Int(1), Value::Bool(false)])),
            Value::Ctor {
                ty: Rc::new("Shape".to_string()),
                variant: Rc::new("Circle".to_string()),
                fields: Rc::new(vec![Value::Float(2.0)]),
            },
            Value::Tensor(Rc::new(vec![1.0, 2.0, 3.0])),
            Value::Map(Rc::new(vec![(Value::Str(Rc::new("k".into())), Value::Int(9))])),
            Value::Set(Rc::new(vec![Value::Int(1), Value::Int(2)])),
            Value::Range(0, 10, true),
        ];
        for v in &samples {
            let p = to_portable(v).expect("plain-data value is portable");
            let back = from_portable(&p);
            assert_eq!(v.to_string(), back.to_string(), "round-trip changed {v}");
        }
    }

    #[test]
    fn non_portable_values_rejected() {
        assert!(to_portable(&Value::Fun(Rc::new("f".to_string()))).is_none());
        assert!(to_portable(&Value::Component(0)).is_none());
        // a list containing a non-portable element is itself non-portable
        let mixed = Value::List(Rc::new(vec![Value::Int(1), Value::Fun(Rc::new("f".into()))]));
        assert!(to_portable(&mixed).is_none());
    }
}
