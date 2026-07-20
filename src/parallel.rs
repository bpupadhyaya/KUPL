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
///
/// Iterative (production-hardening PR-it807): plain recursion here is the SIXTH
/// instance of the Value stack-overflow bug family (Drop it799, equality
/// it800-801/805, Display it802-803, approx_byte_size it804, json_stringify
/// it806) -- a `Value` built ITERATIVELY to be deeply nested, then passed to
/// `xs.par_map(pure_fn)`/`par_filter` with `xs.len() >= THRESHOLD`, crashes the
/// native call stack converting just ONE deeply-nested element. Unlike the prior
/// five fixes (which reduce an existing tree to a scalar/bool/string), this
/// function BUILDS A NEW TREE (`Value` -> `PortableValue`), so the technique is
/// different again: an explicit post-order work-stack, where a container first
/// pushes a "build" marker (how many just-converted children to collect) then
/// its children (so they're converted first), and popping a "build" marker
/// assembles the container from the last N entries already on the results
/// stack -- the same idea a stack-based bytecode VM uses to evaluate an
/// expression tree without a native call per node.
pub fn to_portable(v: &Value) -> Option<PortableValue> {
    enum Frame<'a> {
        Visit(&'a Value),
        BuildList(usize),
        BuildCtor(String, String, usize),
        BuildMap(usize),
        BuildSet(usize),
    }
    let mut stack: Vec<Frame> = vec![Frame::Visit(v)];
    let mut results: Vec<PortableValue> = Vec::new();
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Visit(x) => match x {
                Value::Int(n) => results.push(PortableValue::Int(*n)),
                Value::SizedInt(b) => results.push(PortableValue::SizedInt(b.0, b.1)),
                Value::F32(f) => results.push(PortableValue::F32(*f)),
                Value::BigInt(b) => results.push(PortableValue::BigInt((**b).clone())),
                Value::Rational(r) => results.push(PortableValue::Rational((**r).clone())),
                Value::Float(f) => results.push(PortableValue::Float(*f)),
                Value::Bool(b) => results.push(PortableValue::Bool(*b)),
                Value::Str(s) => results.push(PortableValue::Str((**s).clone())),
                Value::Unit => results.push(PortableValue::Unit),
                Value::List(xs) => {
                    stack.push(Frame::BuildList(xs.len()));
                    for item in xs.iter().rev() {
                        stack.push(Frame::Visit(item));
                    }
                }
                Value::Ctor { ty, variant, fields } => {
                    stack.push(Frame::BuildCtor((**ty).clone(), (**variant).clone(), fields.len()));
                    for f in fields.iter().rev() {
                        stack.push(Frame::Visit(f));
                    }
                }
                Value::Tensor(d) => results.push(PortableValue::Tensor((**d).clone())),
                Value::Map(pairs) => {
                    stack.push(Frame::BuildMap(pairs.len()));
                    for (k, val) in pairs.iter().rev() {
                        stack.push(Frame::Visit(val));
                        stack.push(Frame::Visit(k));
                    }
                }
                Value::Set(xs) => {
                    stack.push(Frame::BuildSet(xs.len()));
                    for item in xs.iter().rev() {
                        stack.push(Frame::Visit(item));
                    }
                }
                Value::Range(a, b, inc) => results.push(PortableValue::Range(*a, *b, *inc)),
                Value::Closure(_)
                | Value::Fun(_)
                | Value::Component(_)
                | Value::Bound(..)
                | Value::VmClosure(..) => return None,
            },
            Frame::BuildList(n) => {
                let items = results.split_off(results.len() - n);
                results.push(PortableValue::List(items));
            }
            Frame::BuildCtor(ty, variant, n) => {
                let fields = results.split_off(results.len() - n);
                results.push(PortableValue::Ctor { ty, variant, fields });
            }
            Frame::BuildMap(n) => {
                let flat = results.split_off(results.len() - 2 * n);
                let mut pairs = Vec::with_capacity(n);
                let mut it = flat.into_iter();
                while let (Some(k), Some(val)) = (it.next(), it.next()) {
                    pairs.push((k, val));
                }
                results.push(PortableValue::Map(pairs));
            }
            Frame::BuildSet(n) => {
                let items = results.split_off(results.len() - n);
                results.push(PortableValue::Set(items));
            }
        }
    }
    results.pop()
}

/// Rebuild a `Value` from its portable form (thread-local; makes fresh `Rc`s).
///
/// Iterative (production-hardening PR-it807, the mirror side of `to_portable`'s
/// fix): same post-order work-stack technique, always succeeds so no early-exit
/// `None` case to handle.
pub fn from_portable(p: &PortableValue) -> Value {
    enum Frame<'a> {
        Visit(&'a PortableValue),
        BuildList(usize),
        BuildCtor(String, String, usize),
        BuildMap(usize),
        BuildSet(usize),
    }
    let mut stack: Vec<Frame> = vec![Frame::Visit(p)];
    let mut results: Vec<Value> = Vec::new();
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Visit(x) => match x {
                PortableValue::Int(n) => results.push(Value::Int(*n)),
                PortableValue::SizedInt(v, w) => results.push(Value::SizedInt(Box::new((*v, *w)))),
                PortableValue::F32(f) => results.push(Value::F32(*f)),
                PortableValue::BigInt(b) => results.push(Value::BigInt(Rc::new(b.clone()))),
                PortableValue::Rational(r) => results.push(Value::Rational(Rc::new(r.clone()))),
                PortableValue::Float(f) => results.push(Value::Float(*f)),
                PortableValue::Bool(b) => results.push(Value::Bool(*b)),
                PortableValue::Str(s) => results.push(Value::Str(Rc::new(s.clone()))),
                PortableValue::Unit => results.push(Value::Unit),
                PortableValue::List(xs) => {
                    stack.push(Frame::BuildList(xs.len()));
                    for item in xs.iter().rev() {
                        stack.push(Frame::Visit(item));
                    }
                }
                PortableValue::Ctor { ty, variant, fields } => {
                    stack.push(Frame::BuildCtor(ty.clone(), variant.clone(), fields.len()));
                    for f in fields.iter().rev() {
                        stack.push(Frame::Visit(f));
                    }
                }
                PortableValue::Tensor(d) => results.push(Value::Tensor(Rc::new(d.clone()))),
                PortableValue::Map(pairs) => {
                    stack.push(Frame::BuildMap(pairs.len()));
                    for (k, val) in pairs.iter().rev() {
                        stack.push(Frame::Visit(val));
                        stack.push(Frame::Visit(k));
                    }
                }
                PortableValue::Set(xs) => {
                    stack.push(Frame::BuildSet(xs.len()));
                    for item in xs.iter().rev() {
                        stack.push(Frame::Visit(item));
                    }
                }
                PortableValue::Range(a, b, inc) => results.push(Value::Range(*a, *b, *inc)),
            },
            Frame::BuildList(n) => {
                let items = results.split_off(results.len() - n);
                results.push(Value::List(Rc::new(items)));
            }
            Frame::BuildCtor(ty, variant, n) => {
                let fields = results.split_off(results.len() - n);
                results.push(Value::Ctor { ty: Rc::new(ty), variant: Rc::new(variant), fields: Rc::new(fields) });
            }
            Frame::BuildMap(n) => {
                let flat = results.split_off(results.len() - 2 * n);
                let mut pairs = Vec::with_capacity(n);
                let mut it = flat.into_iter();
                while let (Some(k), Some(val)) = (it.next(), it.next()) {
                    pairs.push((k, val));
                }
                results.push(Value::Map(Rc::new(pairs)));
            }
            Frame::BuildSet(n) => {
                let items = results.split_off(results.len() - n);
                results.push(Value::Set(Rc::new(items)));
            }
        }
    }
    results.pop().expect("from_portable always produces exactly one Value")
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
fn gate(recv: &Value, args: &[Value], image: &Arc<ProgramImage>) -> Option<(Vec<PortableValue>, String)> {
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
    // A REAL bug found+fixed (production-hardening PR-it922, an Explore
    // survey finding, independently re-verified live before implementing):
    // this function already falls back to the sequential path when an
    // INPUT element isn't portable (the loop above), but had NO equivalent
    // check for the callback's own OUTPUT -- `eval_one` (below) treats a
    // non-portable RESULT as a hard error instead of a fallback, which
    // `try_par_map`/`try_par_filter` then propagate as an uncaught panic.
    // `check.rs` leaves a `par_map` callback's return type as a fully
    // unconstrained fresh type variable, so `List[fn(Int) -> Int]` (a list
    // of CLOSURES, non-portable across a thread boundary) is perfectly
    // valid, well-typed KUPL -- and `effects.rs` classifies a function that
    // merely BUILDS and returns a closure as pure, the same purity class
    // `pure_funs` already requires here. Live-confirmed BEFORE this fix: a
    // 300-element `xs.par_map(make_adder)` (`make_adder`'s own return type
    // is `fn(Int) -> Int`) PANICKED with "parallel callback returned a
    // non-portable value" -- while the IDENTICAL program with a 200-element
    // list (below `THRESHOLD`, so the sequential path runs instead),
    // `xs.map(make_adder)` (never gated at all), and `kupl native` (which
    // always runs `par_map` sequentially, no real threading in cgen.rs) all
    // completed successfully -- a genuine, purely SIZE/ENGINE-dependent
    // crash on valid input, driven entirely by an internal performance-
    // tuning constant with no business being semantically visible. Fixed
    // by test-calling the callback ONCE, sequentially, on the FIRST
    // portable input element here, mirroring the input-side fallback this
    // function already does -- safe because `pure_funs` already guarantees
    // no observable side effects from calling it an extra time, and because
    // KUPL's static typing guarantees ONE fixed return type for the whole
    // list, so the first element's own portability is representative of
    // every other element's. Fall back to `None` (sequential) ONLY when the
    // trial call SUCCEEDS but produces a non-portable value -- that is the
    // one condition this fix exists to catch. A trial call that PANICS for
    // any OTHER reason (a genuine runtime error the callback would raise on
    // ANY engine/path) must NOT be treated the same way and must NOT
    // prevent the real parallel dispatch below -- REAL, live-confirmed
    // BEFORE this exact refinement, via this file's OWN pre-existing
    // `diff_par_map_pure_fn_deep_recursion_does_not_crash_the_process` test
    // (vm.rs): that test's callback deliberately recurses close to
    // `MAX_CALL_DEPTH` specifically to prove a REAL worker thread's 2GiB
    // stack (`par_eval`, PR-it729) safely absorbs it before the depth guard
    // cleanly fires -- an earlier draft of this fix treated ANY trial-call
    // panic (including a legitimate, EXPECTED depth-guard panic) as "not
    // portable", falling back to sequential evaluation on the CALLING
    // thread's ordinary (non-2GiB) stack, which then genuinely crashed the
    // WHOLE PROCESS with an uncatchable native stack overflow -- worse than
    // the very bug this fix exists to close. The trial call itself also
    // MUST run on a thread with `par_eval`'s own 2GiB stack size, not the
    // caller's own thread, for the identical reason.
    if let Some(first) = portable.first() {
        let first = first.clone();
        let image_clone = Arc::clone(image);
        let fname_clone = fname.clone();
        let trial = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(move || {
                let mut interp = crate::interp::Interp::new_bare(image_clone.worker_db());
                let f = Value::Fun(Rc::new(fname_clone));
                // `Some(true)`/`Some(false)`: the call genuinely SUCCEEDED,
                // reporting whether the result is portable. `None`: the
                // call itself panicked/errored -- INCONCLUSIVE about
                // portability, must not be conflated with `Some(false)`.
                match interp.call_value(f, vec![from_portable(&first)], Span::default()) {
                    Ok(v) => Some(to_portable(&v).is_some()),
                    Err(_) => None,
                }
            })
            .and_then(|h| h.join().map_err(|_| std::io::Error::other("trial thread panicked")));
        // Only a CONFIRMED non-portable result blocks the parallel path;
        // a spawn/join failure or an inconclusive (panicked) trial falls
        // through to the real dispatch below, unaffected.
        if let Ok(Some(false)) = trial {
            return None;
        }
    }
    Some((portable, fname))
}

/// Evaluate `fname` on every element across worker threads, returning the
/// per-element results in INPUT-INDEX ORDER (or a PREFIX of them, up through
/// and including the first error — see below). Each worker owns a disjoint
/// index range, builds one thread-local `Interp`, and returns its results in
/// order; `PortableValue` is the only thing that crosses a thread boundary.
///
/// UNSCOPED threads + a channel, not `std::thread::scope` (production-
/// hardening PR-it821): a REAL, live-confirmed HANG bug found+fixed. `scope()`
/// is a Rust std API GUARANTEE that it cannot return until EVERY spawned
/// thread has been joined — so if ANY worker's chunk contains a genuinely
/// non-terminating element (an infinite loop; `eval_one` only catches KUPL-
/// level PANICS, `Err(Flow::Panic{..})`, not non-termination), the WHOLE call
/// hangs forever, even when a DIFFERENT worker already found a definitive
/// error. Confirmed live (it815): a 400-element list, index 0 panics
/// (division by zero), index 300 never terminates — `kupl run`/`--vm` hung
/// indefinitely (required `kill -9`); `kupl native` (no real threading here)
/// completed instantly with a clean panic. Sequential `interp.rs` evaluates
/// index by index and stops at the FIRST error, so it does NOT hang on this
/// exact input (it never reaches index 300) — the threaded path must match
/// that, not just avoid hanging in general.
///
/// Each worker sends `(worker_index, Vec<Result<..>>)` back over an
/// `mpsc::channel` as soon as its OWN chunk finishes (whether every element
/// succeeded or one panicked — `.map().collect()` doesn't short-circuit
/// internally, so a worker always finishes its full chunk quickly unless one
/// of ITS OWN elements doesn't terminate). After every message, check whether
/// the CONTIGUOUS PREFIX of workers received so far (worker 0, then 1, then
/// 2, …, with no gap) contains an error anywhere: if so, return immediately
/// with just that prefix — this is enough for the caller (`try_par_map`/
/// `try_par_filter`, which already short-circuits at the first `Err` in
/// index order) to produce the byte-identical answer, and it means we never
/// need to wait on a worker whose chunk starts AFTER the earliest error,
/// hung or not. A worker whose chunk starts BEFORE an unresolved gap can
/// never be skipped this way — matching interp: if the EARLIEST failing (or
/// non-terminating) element is genuinely upstream of everything else, both
/// engines block on it identically, which is correct, not a regression.
///
/// Any worker we return without waiting for is simply ABANDONED: its
/// `JoinHandle` (implicitly, since it's never bound to a variable) is
/// dropped, which DETACHES the OS thread rather than killing it — it keeps
/// running in the background, holding its own `Arc<ProgramImage>` clone and
/// owned input slice, until it naturally finishes or the whole process
/// exits. Rust has no safe thread-cancellation API; this is the best
/// achievable outcome without one, and was the exact tradeoff already
/// reasoned through (but not yet implemented) at PR-it815.
fn par_eval(
    portable: &[PortableValue],
    fname: &str,
    image: &Arc<ProgramImage>,
) -> Vec<Result<PortableValue, String>> {
    let n = portable.len();
    let workers =
        std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1).clamp(1, n.max(1));
    let chunk = n.div_ceil(workers);

    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<Result<PortableValue, String>>)>();
    let mut num_spawned = 0usize;
    for w in 0..workers {
        let start = w * chunk;
        if start >= n {
            break;
        }
        let end = ((w + 1) * chunk).min(n);
        // Owned, not borrowed: an unscoped thread must be `'static`.
        let owned_slice: Vec<PortableValue> = portable[start..end].to_vec();
        let image = Arc::clone(image);
        let fname = fname.to_string();
        let tx = tx.clone();
        // Same 2GiB stack sizing as before (PR-it729) -- unrelated to this
        // fix, still required for the SAME reason (a pure function
        // recursing near MAX_CALL_DEPTH must hit the clean guard panic, not
        // a real native stack overflow).
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(move || {
                let mut interp = crate::interp::Interp::new_bare(image.worker_db());
                // A REAL, LIVE-CONFIRMED HANG bug found+fixed (production-
                // hardening PR-it844, the TWENTY-FIFTH broad Explore survey):
                // PR-it821's own fix (see this function's doc comment above)
                // only closed the gap ACROSS workers -- a worker whose chunk
                // starts after the earliest error is safely abandoned once
                // that error is known. But WITHIN a single worker, the OLD
                // `owned_slice.iter().map(|p| eval_one(..)).collect::<Vec<_>>()`
                // never short-circuits: plain `Iterator::map().collect()`
                // always evaluates every element regardless of an earlier
                // `Err`, so a worker whose OWN chunk contains BOTH an early
                // panicking index AND a later genuinely non-terminating index
                // hung forever inside its own `collect()` call, never reaching
                // `tx.send` at all -- exactly the same "sequential interp
                // stops at the first error, the threaded path must match
                // that" violation PR-it821 fixed one level up, just not
                // caught at THIS granularity. Live-confirmed: a 10,000-
                // element list, index 0 divides by zero, index 50 never
                // terminates (`chunk = 10000.div_ceil(18) = 556` on this
                // machine's 18 cores, so worker 0 alone spans indices
                // [0, 556) -- comfortably containing both) hung `kupl run`
                // and `kupl run --vm` indefinitely (required `kill -9`);
                // `kupl native` (no real threading here) completed instantly
                // with a clean panic, confirming a genuine engine divergence
                // on top of the hang itself. (A first repro attempt using an
                // explicit `panic(...)` call instead of `1 / x` did NOT
                // reproduce the hang -- traced to an UNRELATED, pre-existing,
                // NOT-a-bug quirk: `effects.rs::collect_expr` doesn't
                // recognize a bare `panic(...)` call as a "resolved" builtin
                // for purity purposes, since `builtin_effects("panic")`
                // returns `None`, so a function containing an explicit
                // `panic()` call is conservatively marked `unresolved` and
                // excluded from `pure_funs()` entirely -- it never enters the
                // real-thread fast path at all, falling back to the ALREADY-
                // correct sequential `map`/`filter`. This is a safe,
                // over-conservative purity classification consistent with
                // this module's own established "over-attribution is sound"
                // philosophy, not a bug -- but it means a runtime error that
                // is NOT an explicit `panic()` call, like a division by zero
                // or an out-of-bounds index, is required to reach this
                // specific hang.) FIXED by evaluating this worker's OWN
                // slice one element at a time and breaking out IMMEDIATELY
                // after the first `Err`, before ever evaluating the NEXT
                // element -- mirroring interp.rs's own stop-at-first-error
                // semantics one level deeper than PR-it821 reached. A
                // resulting `results` vec shorter than this worker's chunk
                // (because it stopped early) is fully compatible with every
                // consumer: the receiving loop below just extends `out` with
                // whatever is present, and `try_par_map`/`try_par_filter`
                // (this function's own callers) already stop at the first
                // `Err` they see regardless of how many elements follow it.
                let mut results: Vec<Result<PortableValue, String>> =
                    Vec::with_capacity(owned_slice.len());
                for p in &owned_slice {
                    let r = eval_one(&mut interp, &fname, p);
                    let is_err = r.is_err();
                    results.push(r);
                    if is_err {
                        break;
                    }
                }
                // Ignore a send failure: it only means the receiver already
                // returned early (an earlier worker's prefix had an error)
                // and dropped `rx` -- this worker's own result is simply
                // discarded, exactly as intended.
                let _ = tx.send((w, results));
            })
            .expect("spawn par_map/par_filter worker thread");
        num_spawned += 1;
    }
    drop(tx); // our own extra sender; each worker holds its own clone

    let mut slots: Vec<Option<Vec<Result<PortableValue, String>>>> = vec![None; num_spawned];
    let mut received = 0usize;
    while received < num_spawned {
        match rx.recv() {
            Ok((w, results)) => {
                slots[w] = Some(results);
                received += 1;
            }
            // All senders dropped without every worker reporting -- only
            // possible if a worker suffered a REAL Rust panic (not a KUPL
            // one; those are already caught inside `eval_one`), which drops
            // its `tx` clone during unwinding. Break out; the `.expect(..)`
            // below turns this into a loud, unmissable crash, matching the
            // ORIGINAL code's own `.expect("par worker thread panicked")`
            // intent for this exceedingly rare, non-user-triggered case.
            Err(_) => break,
        }
        let prefix_len = slots.iter().take_while(|s| s.is_some()).count();
        let has_error =
            slots[..prefix_len].iter().any(|s| s.as_ref().unwrap().iter().any(|r| r.is_err()));
        if has_error {
            let mut out = Vec::with_capacity(n);
            for slot in &slots[..prefix_len] {
                out.extend(slot.as_ref().unwrap().iter().cloned());
            }
            return out;
        }
    }
    let mut out = Vec::with_capacity(n);
    for slot in slots {
        out.extend(slot.expect("par worker thread panicked (no result received)"));
    }
    out
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
        // A REAL coverage gap found+closed (production-hardening PR-it652): this
        // test's own NAME claims "every plain variant", but `Value::BigInt`/
        // `Value::Rational` -- the two `Rc`-wrapped, heap-allocated numeric
        // types this module's `PortableValue` mirror exists specifically to
        // move safely across a thread boundary -- were never actually
        // included. Verified live first (not assumed): a real `par_map` over
        // 300+ `BigInt` elements produces byte-identical output to `--vm`,
        // so this closes a coverage gap, not a functional bug.
        let big = crate::bigint::BigInt::from_i64(123_456_789_012_345);
        let rat = crate::rational::Rational::from_ints(22, 7).expect("22/7 is a valid rational");
        let samples = vec![
            Value::Int(-7),
            Value::SizedInt(Box::new((200, IntW::U8))),
            Value::F32(1.5),
            Value::BigInt(Rc::new(big)),
            Value::Rational(Rc::new(rat)),
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

    /// Runs a REAL `kupl run`/`kupl run --vm` subprocess and returns its output,
    /// or `None` if it doesn't finish within `timeout` -- a genuine hang, not
    /// just a slow run. Mirrors `main.rs::tests::wait_with_timeout` exactly
    /// (same reasoning: `wait_with_output`, not a hand-rolled `try_wait`
    /// polling loop, so the child's stdout/stderr get drained concurrently on
    /// a background thread and can never pipe-deadlock against this test,
    /// racing that against the timeout via a channel). Duplicated rather than
    /// shared across crates/modules since `main.rs`'s copy is private to its
    /// own `#[cfg(test)]` module.
    fn wait_with_timeout(
        child: std::process::Child,
        timeout: std::time::Duration,
    ) -> Option<std::process::Output> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        rx.recv_timeout(timeout).ok().and_then(Result::ok)
    }

    /// A REAL, live-confirmed HANG bug found+fixed (production-hardening
    /// PR-it821, first surfaced and deliberately deferred at PR-it815): a
    /// `par_map`/`par_filter` call over a `THRESHOLD`-or-larger list whose
    /// EARLIEST panicking element sits in one worker's chunk while a LATER,
    /// genuinely non-terminating element sits in a DIFFERENT worker's chunk
    /// used to hang the whole program forever -- `std::thread::scope`
    /// guarantees it cannot return until every spawned thread joins, so the
    /// hung worker blocked the entire call even though a definitive error was
    /// already available. Sequential `interp.rs` does NOT hang on this input
    /// (it evaluates index by index and stops at the FIRST error, never
    /// reaching the later non-terminating element), so the threaded path
    /// must match that -- not merely "avoid hanging in general," but resolve
    /// exactly when interp itself would. Spawns a REAL `kupl run`/`kupl run
    /// --vm` subprocess (not an in-process call) since the hang is a genuine
    /// OS-thread block that an in-process test could itself get stuck on;
    /// bounded to 15s (this repro resolves in ~1-2s when fixed; 15s is a
    /// wide, CI-safe margin with no risk of confusing "slow" with "hung",
    /// unlike a tight wall-clock LATENCY assertion -- the two outcomes here
    /// are "resolves in a couple seconds" vs "never resolves at all," not a
    /// close call sensitive to CPU contention).
    #[test]
    fn par_map_does_not_hang_when_an_earlier_panic_makes_a_later_infinite_loop_unreachable() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun bad(x: Int) -> Int {\n    \
                   if x == 0 {\n        1 / x\n    } else if x == 300 {\n        \
                   var y = 0\n        while true {\n            y = y + 1\n        }\n        y\n    \
                   } else {\n        x\n    }\n}\n\
                   fun main() uses io {\n    \
                   var xs = []\n    var i = 0\n    while i < 400 {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let ys = xs.par_map(bad)\n    print(\"{ys.len()}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hang.kupl");
        std::fs::write(&path, src).unwrap();
        for extra_args in [vec![], vec!["--vm".to_string()]] {
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("run");
            for a in &extra_args {
                cmd.arg(a);
            }
            cmd.arg(&path);
            let child = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("kupl run spawns");
            let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
            let out = out.unwrap_or_else(|| {
                panic!("kupl run {extra_args:?} hung instead of returning the earlier panic")
            });
            let combined =
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(
                combined.contains("division by zero"),
                "expected the earlier (index 0) panic, got: {combined:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, live-confirmed HANG bug found+fixed (production-hardening
    /// PR-it844, the TWENTY-FIFTH broad Explore survey): PR-it821's fix above
    /// only closed the gap ACROSS workers (a panicking element in one
    /// worker's chunk, a non-terminating element in a DIFFERENT worker's
    /// chunk). This is the finer-grained sibling: an early panic and a later
    /// non-terminating element landing in the SAME worker's OWN chunk still
    /// hung, since `owned_slice.iter().map(..).collect::<Vec<_>>()` never
    /// short-circuits WITHIN one worker's own iteration -- fixed by breaking
    /// out of that iteration immediately after the first `Err`. Uses a
    /// 10,000-element list specifically so `chunk = n.div_ceil(workers)`
    /// comfortably exceeds the gap between the panicking index (0) and the
    /// non-terminating index (50) on any realistic core count, guaranteeing
    /// BOTH land in worker 0's own chunk (unlike PR-it821's own 400-element/
    /// index-300 repro, sized specifically to land in a DIFFERENT worker and
    /// exercise the cross-worker case instead). Deliberately uses `1 / x`,
    /// not an explicit `panic(...)` call, to trigger the runtime error: a
    /// bare `panic(...)` call is conservatively (and separately, correctly)
    /// excluded from `pure_funs()` by `effects.rs::collect_expr` (it doesn't
    /// recognize `panic` as a "resolved" builtin call for purity purposes),
    /// so a function containing one never enters the real-thread fast path
    /// at all and this hang wouldn't reproduce -- confirmed by hand during
    /// investigation, not itself a bug given this module's own established
    /// "over-attribution [of impurity] is sound" philosophy.
    #[test]
    fn par_map_does_not_hang_when_an_earlier_panic_and_a_later_infinite_loop_share_one_workers_own_chunk()
    {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun bad(x: Int) -> Int {\n    \
                   if x == 0 {\n        1 / x\n    } else if x == 50 {\n        \
                   var y = 0\n        while true {\n            y = y + 1\n        }\n        y\n    \
                   } else {\n        x\n    }\n}\n\
                   fun main() uses io {\n    \
                   var xs: List[Int] = []\n    var i = 0\n    while i < 10000 {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let ys = xs.par_map(bad)\n    print(\"{ys.len()}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-intraworker-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hang.kupl");
        std::fs::write(&path, src).unwrap();
        for extra_args in [vec![], vec!["--vm".to_string()]] {
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("run");
            for a in &extra_args {
                cmd.arg(a);
            }
            cmd.arg(&path);
            let child = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("kupl run spawns");
            let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
            let out = out.unwrap_or_else(|| {
                panic!("kupl run {extra_args:?} hung instead of returning the earlier panic")
            });
            let combined =
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(
                combined.contains("division by zero"),
                "expected the earlier (index 0) panic, got: {combined:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `par_filter` analogue of the hang-fix test above, same root cause
    /// and same fix -- `try_par_filter` shares the exact same `par_eval`.
    #[test]
    fn par_filter_does_not_hang_when_an_earlier_panic_makes_a_later_infinite_loop_unreachable() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun badpred(x: Int) -> Bool {\n    \
                   if x == 0 {\n        1 / x == 0\n    } else if x == 300 {\n        \
                   var y = 0\n        while true {\n            y = y + 1\n        }\n        y == 0\n    \
                   } else {\n        x % 2 == 0\n    }\n}\n\
                   fun main() uses io {\n    \
                   var xs = []\n    var i = 0\n    while i < 400 {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let ys = xs.par_filter(badpred)\n    print(\"{ys.len()}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-filter-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("filterhang.kupl");
        std::fs::write(&path, src).unwrap();
        let child = std::process::Command::new(&bin)
            .arg("run")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl run spawns");
        let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
        let out = out.unwrap_or_else(|| panic!("kupl run par_filter hung instead of returning the earlier panic"));
        let combined =
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(combined.contains("division by zero"), "expected the earlier (index 0) panic, got: {combined:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A coverage-closing test (production-hardening PR-it845, no NEW bug
    /// found): the `par_filter` intra-worker analogue of the `par_map` test
    /// above. Both `try_par_map` and `try_par_filter` call the identical
    /// shared `par_eval`, so this was expected to already pass given it844's
    /// fix -- confirmed live rather than assumed, per this campaign's own
    /// discipline of verifying shared-code-path assumptions explicitly. Adds
    /// the missing half of the intra-worker pairing: it844 only added a
    /// dedicated intra-worker test for `par_map` (mirroring how PR-it821
    /// itself dual-tested BOTH `par_map` and `par_filter` for the
    /// cross-worker case) -- this closes that asymmetry.
    #[test]
    fn par_filter_does_not_hang_when_an_earlier_panic_and_a_later_infinite_loop_share_one_workers_own_chunk()
    {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun badpred(x: Int) -> Bool {\n    \
                   if x == 0 {\n        1 / x == 0\n    } else if x == 50 {\n        \
                   var y = 0\n        while true {\n            y = y + 1\n        }\n        y == 0\n    \
                   } else {\n        x % 2 == 0\n    }\n}\n\
                   fun main() uses io {\n    \
                   var xs: List[Int] = []\n    var i = 0\n    while i < 10000 {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let ys = xs.par_filter(badpred)\n    print(\"{ys.len()}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-filter-intraworker-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("filterhang.kupl");
        std::fs::write(&path, src).unwrap();
        for extra_args in [vec![], vec!["--vm".to_string()]] {
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("run");
            for a in &extra_args {
                cmd.arg(a);
            }
            cmd.arg(&path);
            let child = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("kupl run spawns");
            let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
            let out = out.unwrap_or_else(|| {
                panic!("kupl run {extra_args:?} par_filter hung instead of returning the earlier panic")
            });
            let combined =
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(
                combined.contains("division by zero"),
                "expected the earlier (index 0) panic, got: {combined:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `par_map`/`par_filter`'s happy path (no errors anywhere) is unaffected
    /// by the hang fix above -- still produces the SAME result as sequential
    /// `map`/`filter`, exercised via the real threaded fast path (both
    /// callbacks are named top-level pure functions over a list well past
    /// `THRESHOLD`).
    #[test]
    fn par_map_and_par_filter_happy_path_matches_sequential_after_the_hang_fix() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun sq(x: Int) -> Int { x * x }\n\
                   fun div7(x: Int) -> Bool { x % 7 == 0 }\n\
                   fun main() uses io {\n    \
                   var xs = []\n    var i = 0\n    while i < 500 {\n        xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let ys = xs.par_map(sq)\n    let zs = xs.par_filter(div7)\n    \
                   let seq_ys = xs.map(sq)\n    let seq_zs = xs.filter(div7)\n    \
                   print(\"{ys == seq_ys}|{zs == seq_zs}|{ys.len()}|{zs.len()}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-happy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("happy.kupl");
        std::fs::write(&path, src).unwrap();
        let child = std::process::Command::new(&bin)
            .arg("run")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl run spawns");
        let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
        let out = out.unwrap_or_else(|| panic!("kupl run par_map/par_filter happy path hung"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.trim(), "true|true|500|72", "stdout={stdout:?} stderr={:?}", String::from_utf8_lossy(&out.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it922, an Explore
    /// survey finding): see `gate`'s own doc comment (immediately above its
    /// output-portability trial call) for the full writeup. This test locks
    /// in that a `par_map` callback whose return type is NON-portable (a
    /// closure, here) no longer panics on a large (real-thread-path) list --
    /// matching the SAME program's already-correct behavior below
    /// `THRESHOLD` (the sequential path, never gated at all).
    #[test]
    fn par_map_with_a_non_portable_callback_output_falls_back_to_sequential_instead_of_panicking() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let src = "fun make_adder(x: Int) -> fn(Int) -> Int {\n    fn(y: Int) { x + y }\n}\n\n\
                   fun main() uses io {\n    \
                   var xs: List[Int] = []\n    var i = 0\n    while i < 300 {\n        \
                   xs = xs.push(i)\n        i = i + 1\n    }\n    \
                   let fns = xs.par_map(make_adder)\n    \
                   let f10 = fns.get(10).unwrap_or(fn(y: Int) { -1 })\n    \
                   print(\"{fns.len()} {f10(1)}\")\n}\n";
        let dir = std::env::temp_dir().join(format!("kupl-parallel-nonportable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nonportable.kupl");
        std::fs::write(&path, src).unwrap();
        let child = std::process::Command::new(&bin)
            .arg("run")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl run spawns");
        let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
        let out = out.unwrap_or_else(|| panic!("kupl run par_map non-portable-output test hung"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            "300 11",
            "must succeed with the correct closures (element 10 adds 10, so (…)(1) == 11), not panic: \
             stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
