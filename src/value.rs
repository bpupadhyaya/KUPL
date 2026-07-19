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
    /// The narrowest built-in width whose range holds `v`, for suggesting a fix
    /// when a literal overflows its declared width. Prefers the same signedness
    /// family, but falls back to the signed widths when `v` is negative (no
    /// unsigned width can hold a negative). Returns `None` when `v` is larger
    /// than every fixed width (only reachable above the u64 / below the i64 range).
    pub fn widen_to_fit(self, v: i128) -> Option<IntW> {
        let signed = [IntW::I8, IntW::I16, IntW::I32, IntW::I64];
        let unsigned = [IntW::U8, IntW::U16, IntW::U32, IntW::U64];
        let order: &[IntW] = if self.is_signed() || v < 0 {
            &signed
        } else {
            &unsigned
        };
        order.iter().copied().find(|w| w.check_range(v))
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
    /// `a.wrapping_mul(b)` for a fixed width, reduced with `wrap` (PR-it671).
    /// `a * b` for two `U64`/`I64`-range operands can itself overflow `i128`
    /// (`u64::MAX * u64::MAX` is roughly `2^128`, past `i128::MAX`'s `2^127`),
    /// which panics under `cargo test`'s default overflow-checks -- confirmed
    /// as a live crash (`kupl: internal compiler error`). `i128::wrapping_mul`
    /// never panics, and since `2^128` is a multiple of `2^64`, its low 64
    /// bits agree with the mathematically-true product's, so `wrap`'s
    /// subsequent `rem_euclid` on the wrapped-mod-2^128 value still yields the
    /// mathematically-correct wrapped result at this width.
    pub fn wrapping_mul(self, a: i128, b: i128) -> i128 {
        self.wrap(a.wrapping_mul(b))
    }
    /// `a.saturating_mul(b)` for a fixed width (PR-it671). Unlike `wrap`'s
    /// case, `saturate` needs the TRUE product's magnitude/sign to clamp
    /// correctly -- naively reducing an overflowed product mod `2^128` first
    /// (as `wrapping_mul` above does) can flip its sign, so clamping THAT
    /// value gives the WRONG answer (`u64::MAX.saturating_mul(u64::MAX)`
    /// would wrongly saturate to `0`, not `u64::MAX` -- confirmed by direct
    /// computation before this fix). `checked_mul` detects the overflow
    /// directly; when it overflows, the two width-bounded operands' signs
    /// alone determine which extreme the true product saturates to.
    pub fn saturating_mul(self, a: i128, b: i128) -> i128 {
        match a.checked_mul(b) {
            Some(v) => self.saturate(v),
            None => if (a >= 0) == (b >= 0) { self.max() } else { self.min() },
        }
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

    /// A REAL, severe bug (PR-it671): the plain `a * b` (both `i128`) this
    /// used to route through can itself overflow `i128` for `U64`/`I64`-range
    /// operands (`u64::MAX * u64::MAX` is ~2^128, past `i128::MAX`'s ~2^127),
    /// which panicked (`cargo test`'s default overflow-checks) instead of
    /// wrapping to the correct answer -- a genuine `kupl: internal compiler
    /// error` crash on valid, in-range KUPL source, confirmed live before this
    /// fix. `wrapping_mul` must still give the mathematically-correct
    /// modular-wraparound answer despite the i128-level overflow along the way.
    #[test]
    fn wrapping_mul_does_not_panic_and_stays_correct_when_the_raw_i128_product_overflows() {
        let max = u64::MAX as i128;
        // u64::MAX ≡ -1 (mod 2^64), so (-1)*(-1) = 1 is the correct wraparound.
        assert_eq!(IntW::U64.wrapping_mul(max, max), 1);
        // i64::MIN * -1 doesn't overflow i128 at all, but exercises the same
        // call path with a signed width for good measure.
        let i64_min = i64::MIN as i128;
        assert_eq!(IntW::I64.wrapping_mul(i64_min, -1), i64_min);
    }

    /// A REAL correctness bug, not just a crash (PR-it671): naively reducing
    /// an i128-overflowed product mod 2^128 BEFORE clamping (i.e. reusing
    /// `wrapping_mul`'s result for `saturate`) flips its sign for a large
    /// enough overflow, so a positive-times-positive product that should
    /// saturate UP to the width's max instead wrongly clamps DOWN to the
    /// width's min (confirmed by direct computation before this fix: it gave
    /// 0, not `u64::MAX`). `saturating_mul` must detect the true overflow
    /// (not the wrapped-then-clamped one) to pick the correct extreme.
    #[test]
    fn saturating_mul_clamps_toward_the_mathematically_correct_extreme_not_the_wrapped_ones_sign() {
        let max = u64::MAX as i128;
        assert_eq!(IntW::U64.saturating_mul(max, max), max, "positive overflow must clamp UP to max, not down to 0");
        let i64_min = i64::MIN as i128;
        // (i64::MIN) * 2: true product is very negative -> must clamp to i64::MIN.
        assert_eq!(IntW::I64.saturating_mul(i64_min, 2), i64_min);
        // i64::MAX * i64::MAX: true product is huge and positive -> clamp to i64::MAX.
        let i64_max = i64::MAX as i128;
        assert_eq!(IntW::I64.saturating_mul(i64_max, i64_max), i64_max);
        // (i64::MIN) * (-2): true product is huge and positive (both negative) -> clamp to i64::MAX.
        assert_eq!(IntW::I64.saturating_mul(i64_min, -2), i64_max);
        // no overflow at all: behaves exactly like plain saturate(a*b).
        assert_eq!(IntW::U8.saturating_mul(100, 3), 255);
        assert_eq!(IntW::U8.saturating_mul(10, 3), 30);
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
    /// A KVM closure: prototype index + captured values (captured by value) +
    /// the component instance that was "current" when the closure was made
    /// (None outside any component). Component-local function calls made from
    /// WITHIN the closure body resolve against THIS instance, not whatever
    /// instance happens to be ambiently "current" at the closure's CALL site
    /// -- a closure is bound to its creator, not to its caller.
    VmClosure(u16, Rc<Vec<Value>>, Option<usize>),
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
    /// The component instance that was "current" when this closure was made
    /// (None outside any component). A call to a component-local function
    /// FROM WITHIN the closure body resolves against THIS instance, not
    /// whatever instance is ambiently "current" at the closure's CALL site —
    /// a closure is bound to its creator, not to its caller.
    pub origin_instance: Option<usize>,
}

impl Value {
    /// Cheap approximate byte-size of a value's own data (leaf scalars count
    /// as a fixed 8 bytes; containers sum their children). Used to bound
    /// unbounded PAYLOAD growth in a `wire` cycle (production-hardening
    /// PR-it760) the same way `MAX_COMPONENT_MESSAGES` already bounds message
    /// COUNT: an ordinary self-wire handler like `emit grown(s + s)` doubles
    /// its payload every hop with no error, reaching 512MB in just 30
    /// messages -- 0.003% of the message-count cap -- confirmed live to climb
    /// unbounded toward the OS OOM killer rather than ever hitting a clean
    /// panic. `BigInt`/`Rational` are deliberately left at the flat 8-byte
    /// leaf cost: both already independently cap their own limb count
    /// (`bigint::MAX_BIGINT_LIMBS`), so neither can grow large enough on its
    /// own to matter here. This is a pure length/count computation (byte
    /// length, list length, ...), so it is naturally identical across
    /// interp.rs, vm.rs (reuses this), and cgen.rs's C mirror
    /// (`k_value_approx_size`) for any equivalent value -- all three engines
    /// cross the cap at the exact same message.
    // Iterative (production-hardening PR-it804): the List/Set/Ctor/Map arms
    // used to recurse via `.iter().map(Value::approx_byte_size).sum()`, so a
    // sufficiently deep structure (the SAME `Wrap(next: Chain)`-style repro
    // used to find+fix the other three members of this bug family --
    // Drop at it799, equality at it800-801, Display at it802-803) crashed
    // this function's OWN size CHECK, which exists specifically to bound
    // unbounded payload growth in a `wire` cycle -- ironic, since a deep-
    // enough payload stack-overflowed the very guard meant to reject it,
    // rather than ever reaching the `MAX_COMPONENT_MESSAGE_BYTES` panic.
    // Confirmed live crashing ALL THREE engines (interp/vm via Rust; native
    // via cgen.rs's `k_value_approx_size`, an identically-shaped recursive
    // C function -- unlike PR-it799's Drop bug, which spared native because
    // its arena allocator never frees, a size COMPUTATION has no such
    // protection, matching the equality/Display bugs' shape instead). This
    // is the SIMPLEST bug-family member to fix: unlike Drop (conditional
    // `Rc::get_mut`-gated teardown) or Display (must preserve exact output
    // order) or equality (must short-circuit on the first mismatch), a
    // byte-size SUM has no ordering or early-exit constraint at all -- a
    // flat work-list of not-yet-counted `&Value` references, popped in a
    // loop and accumulated into a running total, is a direct, unconditional
    // port with no design questions left to resolve.
    pub fn approx_byte_size(&self) -> u64 {
        let mut stack: Vec<&Value> = vec![self];
        let mut total: u64 = 0;
        while let Some(v) = stack.pop() {
            match v {
                Value::Str(s) => total += s.len() as u64,
                Value::List(xs) | Value::Set(xs) => stack.extend(xs.iter()),
                Value::Ctor { fields, .. } => stack.extend(fields.iter()),
                Value::Map(entries) => {
                    for (k, val) in entries.iter() {
                        stack.push(k);
                        stack.push(val);
                    }
                }
                Value::Tensor(xs) => total += xs.len() as u64 * 8,
                // A REAL bypass of this function's OWN size cap (production-
                // hardening PR-it877, found via this campaign's "re-audit a
                // function with prior fix history" technique): a closure's
                // captured environment is a real, first-class part of its
                // payload (captured BY VALUE at creation, per `Closure`'s own
                // doc comment) -- e.g. `let f = fn n { n + big.len() }` where
                // `big` is a captured Str -- but both `Closure`/`VmClosure`
                // fell through to the flat 8-byte leaf-scalar case above,
                // exactly like every OTHER container arm here (List/Set/Ctor/
                // Map) once did before being added one by one. A function is a
                // first-class `Value` that can flow through `emit`/the message
                // queue like any other (confirmed live: a component can
                // declare a `fn(...)-> ...`-typed port and `emit` a closure
                // through it), so a growing payload smuggled inside a
                // closure's captures silently bypassed the exact cap this
                // function exists to enforce. Confirmed live before this fix:
                // wiring a component that emits an 11MB `Str` directly through
                // a `Str`-typed port correctly panics with K0900 ("component
                // message payload too large"); the IDENTICAL 11MB `Str`
                // captured inside a closure and emitted through a
                // `fn(Int)->Int`-typed port instead ran to completion with NO
                // panic, on BOTH interp and vm (this function is shared).
                Value::Closure(c) => stack.extend(c.captures.iter().map(|(_, v)| v)),
                Value::VmClosure(_, captures, _) => stack.extend(captures.iter()),
                _ => total += 8,
            }
        }
        total
    }

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
            // demangled for display -- see the Display impl's Ctor arm below.
            Value::Ctor { ty, .. } => crate::resolve::demangle_for_display(ty).into(),
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

/// Iterative teardown for deeply-nested containers (production-hardening
/// PR-it799) -- avoids the stack overflow Rust's DEFAULT recursive `Drop`
/// would otherwise cause. `List`/`Ctor`/`Map`/`Set` all wrap an
/// `Rc<Vec<Value>>` (or `Rc<Vec<(Value, Value)>>`); an ordinary KUPL program
/// that builds a long chain ITERATIVELY (e.g. `type Chain = Wrap(next:
/// Chain) | End` grown via a `while` loop to millions of links -- no KUPL-
/// level function-call recursion at all, so `MAX_CALL_DEPTH` never applies)
/// then drops it (reassignment, scope exit, ...) used to recurse one Rust
/// stack frame per link: dropping the outer `Vec<Value>` drops its one
/// `Value` element, which is itself a container, so dropping THAT `Vec`
/// recurses again, and so on -- `fatal runtime error: stack overflow,
/// aborting` (SIGABRT), not a catchable panic. Confirmed live: interp.rs and
/// vm.rs both crash on a 50,000,000-deep chain; `kupl native` does NOT (its
/// "v0 memory model" is a pure arena allocator that never frees anything at
/// all, so it was never exposed to this class of bug in the first place).
///
/// The fix mirrors the standard technique for avoiding recursive-drop stack
/// overflow in a linked structure: instead of letting a container's normal
/// drop glue recurse into its children, pull the children out into a flat,
/// heap-allocated work list and finish tearing them down in a `while` LOOP
/// (bounded stack depth, unbounded iteration count) instead of Rust's own
/// call stack. `Rc::get_mut` is the key primitive -- it succeeds ONLY when
/// this is the LAST owner (`strong_count == 1`, `weak_count == 0`; nothing
/// in this codebase ever takes a `Weak<Vec<Value>>`/`Weak<Vec<(Value,
/// Value)>>` of these, confirmed via grep), which is exactly the condition
/// under which the container is ABOUT to be freed for real -- if another
/// clone of the same `Rc` is still alive elsewhere, `get_mut` correctly
/// fails and this function does nothing, leaving the ordinary `Rc` drop
/// glue to just decrement the refcount (an O(1) operation, not a deep
/// free) -- refcounts are respected exactly like the survey's fix
/// suggestion required, never blindly walked and freed regardless of
/// sharing. `Closure`/`VmClosure` (which can also capture arbitrary
/// `Value`s) were considered but deliberately NOT given their own
/// `collect_children` arm: they don't need one, since whatever `Value` they
/// capture is torn down through ITS OWN `Drop` impl when the closure's
/// captures `Vec` is freed -- which is this same, now-safe, iterative
/// `Value::drop` regardless of who holds the reference. A contrived chain
/// of closures each capturing the next closure value is a structurally
/// different (and far more unusual) recursion path than the survey's
/// ADT-chain repro and was not attempted/verified here; the identical
/// `Rc::get_mut`-and-drain technique would extend to it if ever found to
/// matter in practice.
fn collect_children(v: &mut Value, stack: &mut Vec<Value>) {
    match v {
        Value::List(rc) | Value::Set(rc) => {
            if let Some(vec) = Rc::get_mut(rc) {
                stack.extend(vec.drain(..));
            }
        }
        Value::Ctor { fields, .. } => {
            if let Some(vec) = Rc::get_mut(fields) {
                stack.extend(vec.drain(..));
            }
        }
        Value::Map(rc) => {
            if let Some(vec) = Rc::get_mut(rc) {
                for (k, val) in vec.drain(..) {
                    stack.push(k);
                    stack.push(val);
                }
            }
        }
        _ => {}
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        let mut stack: Vec<Value> = Vec::new();
        collect_children(self, &mut stack);
        while let Some(mut child) = stack.pop() {
            collect_children(&mut child, &mut stack);
            // `child`'s own container fields (if any) were just emptied above,
            // so its drop here -- and the recursive call back into THIS same
            // `Drop::drop` that it triggers -- does zero further work: O(1),
            // not a second recursion into the structure.
        }
    }
}

impl PartialEq for Value {
    // Iterative comparison (production-hardening PR-it800): the List/Ctor
    // arms used to recurse through `Vec<Value>`'s own derived `PartialEq`
    // (`a == b` on an `Rc<Vec<Value>>`), so comparing TWO INDEPENDENTLY-
    // built EQUAL deep structures (e.g. two 20 000 000-deep `Wrap(next:
    // Chain)` chains, each grown by its OWN iterative `while` loop -- no
    // KUPL-level call recursion, `MAX_CALL_DEPTH` never applies) via `==`
    // stack-overflowed -- confirmed live crashing ALL THREE engines (interp
    // and vm.rs abort with `fatal runtime error: stack overflow, aborting`;
    // `kupl native` ALSO crashes here, unlike the sibling `Drop` bug fixed
    // at PR-it799 -- native's arena allocator sidesteps deep FREES, but
    // `cgen.rs`'s `k_eq` is its own genuinely recursive C function
    // (`k_eq(a.as.ctor->fields[i], b.as.ctor->fields[i])`), so this is a
    // SEVERE bug shared by all three engines identically, not a cross-
    // engine divergence the way the Drop bug was -- native's OWN fix is
    // deliberately scoped OUT of this iteration (a C-side iterative
    // rewrite of `k_eq`/`k_key_eq` is a separate, larger undertaking; see
    // dotfiles memory PR-it800 for the followup plan). A flat work-list of
    // PENDING PAIRS to compare, processed in a `while` loop instead of via
    // recursive calls, avoids growing the native stack with structure
    // depth. Any mismatch found ANYWHERE returns `false` immediately --
    // preserving the ORIGINAL code's `&&`/`.all()` short-circuit semantics
    // exactly, not just its final answer.
    fn eq(&self, other: &Self) -> bool {
        let mut stack: Vec<(&Value, &Value)> = vec![(self, other)];
        while let Some((a, b)) = stack.pop() {
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => {
                    if x != y {
                        return false;
                    }
                }
                // sized ints are equal iff both value AND width match
                (Value::SizedInt(x), Value::SizedInt(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::BigInt(x), Value::BigInt(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Rational(x), Value::Rational(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::F32(x), Value::F32(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Float(x), Value::Float(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Bool(x), Value::Bool(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Str(x), Value::Str(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Unit, Value::Unit) => {}
                (Value::List(x), Value::List(y)) => {
                    if x.len() != y.len() {
                        return false;
                    }
                    stack.extend(x.iter().zip(y.iter()));
                }
                (
                    Value::Ctor { ty: t1, variant: v1, fields: f1 },
                    Value::Ctor { ty: t2, variant: v2, fields: f2 },
                ) => {
                    if t1 != t2 || v1 != v2 || f1.len() != f2.len() {
                        return false;
                    }
                    stack.extend(f1.iter().zip(f2.iter()));
                }
                (Value::Component(x), Value::Component(y)) => {
                    if x != y {
                        return false;
                    }
                }
                (Value::Range(a1, b1, i1), Value::Range(a2, b2, i2)) => {
                    if a1 != a2 || b1 != b2 || i1 != i2 {
                        return false;
                    }
                }
                (Value::Tensor(x), Value::Tensor(y)) => {
                    if x != y {
                        return false;
                    }
                }
                // Map/Set equality is order-insensitive (Python dict/set semantics).
                // Keys/elements compare via `value_key_eq` (PR-it691), not plain
                // `==`, so a `NaN` key/element compares consistently with what
                // `.get`/`.contains_key`/`.contains` would actually find --
                // otherwise `Map` `A` and `Map` `B` could each independently
                // report `.contains_key(nan)` as true while `A == B` disagreed
                // with that, an inconsistency worse than either alone.
                // `value_key_eq` is ITSELF made iterative below (the same bug,
                // reached via a DIFFERENT recursive call chain) -- this arm
                // doesn't need to push onto `stack` since the recursion into
                // nested keys/values happens entirely inside `value_key_eq`.
                (Value::Map(x), Value::Map(y)) => {
                    let equal = x.len() == y.len()
                        && x.iter().all(|(k, v)| {
                            y.iter().any(|(k2, v2)| value_key_eq(k, k2) && value_key_eq(v, v2))
                        });
                    if !equal {
                        return false;
                    }
                }
                (Value::Set(x), Value::Set(y)) => {
                    let equal =
                        x.len() == y.len() && x.iter().all(|x| y.iter().any(|y| value_key_eq(x, y)));
                    if !equal {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

/// Key/element identity for `Map`/`Set` (insert/get/contains_key/remove/
/// contains/merge/union/intersect/difference, and Map/Set's own `==`) —
/// DISTINCT from `PartialEq`/`==`, which correctly follows IEEE-754 (`NaN !=
/// NaN`, the mathematically standard and expected behavior for the `==`
/// OPERATOR and for value-sequence helpers like `List.contains`). A REAL bug
/// found+fixed (production-hardening PR-it691): every Map/Set method used
/// plain `==`/`PartialEq` for key/element identity, so `0.0 / 0.0` (an
/// ordinary, reachable NaN — KUPL float division has no zero-guard) broke
/// `docs/reference/STDLIB.md`'s own documented Map contract ("updates in
/// place positionally"): confirmed live, identically on interp AND the KVM,
/// that `m.insert(nan, 1)` then `m.get(nan)` returned `None` (not `Some(1)`),
/// and a SECOND `m.insert(nan, 2)` grew `m.len()` to 2 instead of updating
/// the existing entry — Set's `insert`/`contains` showed the identical
/// duplication. Most languages special-case NaN for CONTAINER-key identity
/// specifically (JS `Map`/`Set` use SameValueZero; Python's `dict`/`set`
/// short-circuit on identity before `==`) precisely to avoid this trap,
/// while leaving ordinary `==` IEEE-754-correct — this function is that
/// special case for KUPL. Recurses through every composite variant (not just
/// a top-level `Float`/`F32`) so a NaN buried inside a List/Ctor/Map/Set/
/// Range/Tensor used AS a Map key or Set element is ALSO handled correctly,
/// not just a bare NaN key.
///
/// Made iterative (production-hardening PR-it800), the SAME bug class and
/// fix technique as `Value`'s own `PartialEq::eq` just above -- a
/// SEPARATE, independently recursive call chain (List/Ctor here recurse
/// via `value_key_eq` calling itself directly, not through `PartialEq`),
/// reachable via ANY Map/Set operation (`insert`/`get`/`contains_key`/
/// `merge`/`union`/...), not just `==` -- so arguably an even more
/// EASILY-triggered path in practice than the plain `==` operator.
///
/// A REAL, live-confirmed GAP in that it800 fix (production-hardening
/// PR-it805, found by a fresh Explore-agent survey and independently
/// re-verified via live reproduction before fixing): the `Map`/`Set` arms
/// STILL called `value_key_eq` directly for each key/value candidate
/// comparison -- a genuine Rust function call, not a push onto the flat
/// `stack` above -- so a `Map`/`Set` NESTED INSIDE ITSELF (e.g. `type
/// Chain = Wrap(m: Map[Str, Chain]) | End`, grown 2,000,000 deep via an
/// ordinary iterative `while` loop, then compared with `==`) still
/// recursed one native stack frame per NESTING LEVEL and stack-overflowed
/// -- confirmed live crashing interp.rs and vm.rs (SIGABRT) and `kupl
/// native`'s `k_key_eq` mirror (SIGSEGV), the exact same bug class the
/// it799-804 campaign was fixing, just left open for this one shape.
///
/// Unlike `List`/`Ctor` (a pure CONJUNCTION: every positional child must
/// match, so "push all children onto the flat stack" is correct and
/// sufficient -- see `PartialEq::eq` and this function's own arms above),
/// `Map`/`Set` key identity is a DISJUNCTIVE SEARCH per entry ("does x's
/// i-th entry match ANY of y's entries") with retry-on-mismatch, which a
/// simple flat "push and continue" stack cannot express (there's no way
/// to say "if this specific candidate comparison fails, try the NEXT
/// candidate" using a plain LIFO of independent obligations -- a naive
/// stack can only express "if ANY of these fail, the WHOLE thing fails,"
/// which is conjunction, not the OR-with-retry Map/Set actually needs).
///
/// Fixed with a genuinely different, still fully iterative technique: an
/// explicit two-phase node graph, mirroring how a compiler lowers a
/// boolean expression to a flat instruction list instead of a recursive
/// AST walk. PHASE 1 (build) expands the WHOLE comparison into a flat
/// `Vec<Node>` via its OWN work QUEUE (a loop, never Rust recursion) --
/// `List`/`Ctor` become `And` nodes over their children (identical
/// semantics to the flat-stack version above); `Map`/`Set` become an
/// `And` (every x-entry must match) of `Or` nodes (each x-entry's search
/// over y-candidates), where each OR branch is itself an `And` of the
/// key-comparison and value-comparison sub-nodes -- so nested Map/Set
/// candidates expand into MORE nodes on the SAME flat list, never a
/// fresh call frame. EVERY combinator node (not just the top-level ones --
/// this matters, see below) is created via a SINGLE uniform `child!`
/// deferred-expansion helper: reserve a placeholder index NOW, queue the
/// REAL expansion for later. This guarantees every node's `kids` indices
/// are STRICTLY GREATER than the node's own index (a child's slot is
/// always reserved AFTER its parent's), which is exactly the invariant
/// PHASE 2 (evaluate) needs: a single `for i in (0..nodes.len()).rev()`
/// pass resolves the WHOLE graph bottom-up, since by the time node `i` is
/// reached, everything it references (strictly higher indices) is already
/// a resolved leaf boolean. (A first draft of this fix built the Map/Set
/// "candidate AND" and "search OR" nodes INLINE -- immediately, referencing
/// already-existing lower-index children, rather than via `child!` -- which
/// silently produced WRONG answers, not just a crash: `Map{"x": Map{"y":
/// 1}} == Map{"x": Map{"y": 1}}` incorrectly evaluated to `false`, because
/// the inner Map's own search/candidate nodes got created with HIGHER
/// indices than nodes that referenced them, breaking the single-pass
/// reverse-order evaluation invariant. Caught by the SAME correctness
/// battery used for the sibling PR-it799-804 fixes before this landed --
/// worth recording as a concrete example of why "iterative work-list, but
/// with an inconsistent index-ordering invariant" is a plausible-looking
/// but WRONG shortcut, not just a style choice.) Total heap use is bounded
/// by the VALUE's total node/entry count (like this campaign's other
/// iterative rewrites), never by nesting depth -- unbounded input depth
/// can grow the heap, never the native stack. CONSIDERED, deliberately
/// scoped out: PHASE 1 always fully expands the comparison graph before
/// evaluating, so unlike the OLD recursive version (which could bail out
/// the instant it found the FIRST mismatch anywhere in a huge structure),
/// this version does NOT short-circuit on an early mismatch -- correctness-
/// preserving but measurably slower for LARGE, early-differing values. A
/// hybrid that also short-circuits Phase 1 was considered, but the added
/// complexity (partial expansion + resumable search state) isn't justified
/// by this campaign's priority (production-safety over micro-optimizing an
/// already-rare pathological-comparison case); ordinary Map/Set sizes in
/// real KUPL code make this a non-issue in practice.
pub fn value_key_eq(a: &Value, b: &Value) -> bool {
    enum Node {
        Leaf(bool),
        And(Vec<usize>),
        Or(Vec<usize>),
    }
    /// A unit of comparison work still to be expanded into `Node`s.
    /// `Cmp` is a direct value-pair comparison (the general case);
    /// `MapSearch`/`SetSearch` and `MapPair` exist so a Map/Set's per-entry
    /// SEARCH (an OR over `y`'s candidates, each an AND of a key- and a
    /// value-comparison) is ALSO expanded via the SAME deferred `child!`
    /// mechanism as everything else, preserving the child-index-greater-
    /// than-parent-index invariant PHASE 2 depends on.
    enum Task<'a> {
        Cmp(&'a Value, &'a Value),
        MapSearch(&'a Value, &'a Value, &'a [(Value, Value)]),
        MapPair(&'a Value, &'a Value, &'a Value, &'a Value),
        SetSearch(&'a Value, &'a [Value]),
    }

    let mut nodes: Vec<Node> = vec![Node::Leaf(false)];
    let mut work: Vec<(usize, Task)> = vec![(0, Task::Cmp(a, b))];
    while let Some((idx, task)) = work.pop() {
        // Reserve a placeholder slot for a child task, queue its expansion
        // for later, and return its (now-fixed) index. Every call happens
        // AFTER `idx`'s own slot was already reserved, so every child index
        // this produces is strictly greater than every node that uses it.
        macro_rules! child {
            ($t:expr) => {{
                let child_idx = nodes.len();
                nodes.push(Node::Leaf(false));
                work.push((child_idx, $t));
                child_idx
            }};
        }
        let node = match task {
            Task::Cmp(x, y) => match (x, y) {
                (Value::F32(p), Value::F32(q)) => Node::Leaf(p == q || (p.is_nan() && q.is_nan())),
                (Value::Float(p), Value::Float(q)) => Node::Leaf(p == q || (p.is_nan() && q.is_nan())),
                (Value::List(xs), Value::List(ys)) => {
                    if xs.len() != ys.len() {
                        Node::Leaf(false)
                    } else {
                        Node::And(xs.iter().zip(ys.iter()).map(|(xi, yi)| child!(Task::Cmp(xi, yi))).collect())
                    }
                }
                (
                    Value::Ctor { ty: t1, variant: v1, fields: f1 },
                    Value::Ctor { ty: t2, variant: v2, fields: f2 },
                ) => {
                    if t1 != t2 || v1 != v2 || f1.len() != f2.len() {
                        Node::Leaf(false)
                    } else {
                        Node::And(f1.iter().zip(f2.iter()).map(|(xi, yi)| child!(Task::Cmp(xi, yi))).collect())
                    }
                }
                (Value::Range(a1, b1, i1), Value::Range(a2, b2, i2)) => {
                    Node::Leaf(a1 == a2 && b1 == b2 && i1 == i2)
                }
                (Value::Tensor(xs), Value::Tensor(ys)) => Node::Leaf(
                    xs.len() == ys.len()
                        && xs.iter().zip(ys.iter()).all(|(p, q)| p == q || (p.is_nan() && q.is_nan())),
                ),
                (Value::Map(xs), Value::Map(ys)) => {
                    if xs.len() != ys.len() {
                        Node::Leaf(false)
                    } else {
                        Node::And(
                            xs.iter().map(|(xk, xv)| child!(Task::MapSearch(xk, xv, ys))).collect(),
                        )
                    }
                }
                (Value::Set(xs), Value::Set(ys)) => {
                    if xs.len() != ys.len() {
                        Node::Leaf(false)
                    } else {
                        Node::And(xs.iter().map(|xi| child!(Task::SetSearch(xi, ys))).collect())
                    }
                }
                _ => Node::Leaf(x == y),
            },
            Task::MapSearch(xk, xv, ys) => {
                Node::Or(ys.iter().map(|(yk, yv)| child!(Task::MapPair(xk, xv, yk, yv))).collect())
            }
            Task::MapPair(xk, xv, yk, yv) => {
                Node::And(vec![child!(Task::Cmp(xk, yk)), child!(Task::Cmp(xv, yv))])
            }
            Task::SetSearch(xi, ys) => Node::Or(ys.iter().map(|yi| child!(Task::Cmp(xi, yi))).collect()),
        };
        nodes[idx] = node;
    }
    for i in (0..nodes.len()).rev() {
        let resolved = match &nodes[i] {
            Node::Leaf(b) => *b,
            Node::And(kids) => kids.iter().all(|&k| matches!(nodes[k], Node::Leaf(true))),
            Node::Or(kids) => kids.iter().any(|&k| matches!(nodes[k], Node::Leaf(true))),
        };
        nodes[i] = Node::Leaf(resolved);
    }
    matches!(nodes[0], Node::Leaf(true))
}

/// Work items for `Value`'s iterative Display formatter (production-
/// hardening PR-it802) -- see the `impl fmt::Display for Value` doc comment
/// below for why this exists. `Val`/`QuotedVal` are values still needing
/// formatting; `Str`/`Owned` are literal text fragments already queued for
/// output (`Owned` for text computed at push time, like a demangled ctor
/// name, that can't be a `&'static str`).
enum DisplayItem<'a> {
    Str(&'static str),
    Owned(String),
    Val(&'a Value),
    /// Like `Val`, but a `Str` renders quoted -- matches the OLD `DebugStr`
    /// helper's behavior for elements nested inside a container.
    QuotedVal(&'a Value),
}

impl fmt::Display for Value {
    // Iterative (production-hardening PR-it802): the List/Ctor/Map/Set arms
    // used to recurse through plain `write!(f, "{}", DebugStr(item))` calls
    // for each nested element, so `print()`-ing (or any `{value}`
    // interpolation, or `.to_string()`) of a deeply-nested structure built
    // ITERATIVELY (the SAME `type Chain = Wrap(next: Chain) | End`-style
    // repro used to find+fix PR-it799's Drop bug and PR-it800/it801's
    // equality bug) stack-overflowed -- confirmed live crashing BOTH
    // interp.rs and vm.rs (this `impl` is shared code, one fix point for
    // both, exactly like the Drop/PartialEq fixes before it) with `fatal
    // runtime error: stack overflow, aborting`. `kupl native`'s C mirror
    // (`cgen.rs`'s `k_display`) has the IDENTICAL bug shape and is
    // DELIBERATELY NOT fixed in this same change -- an ordered, iterative
    // tree-serializer is a meaningfully different (and larger) rewrite than
    // Drop's unordered teardown or equality's short-circuiting comparison,
    // so the native C fix is scoped to a dedicated followup iteration,
    // mirroring exactly how PR-it800/it801 split the equality fix across
    // two iterations (Rust, then C).
    //
    // Unlike Drop (order doesn't matter, PR-it799) or equality (short-
    // circuits on the first mismatch, PR-it800), formatting must emit
    // output in the EXACT correct left-to-right nested order -- this needs
    // an explicit stack of PENDING WORK ITEMS, not just pending values:
    // each container pushes its own closing bracket, its children
    // (interleaved with separators), and its opening bracket, in REVERSE
    // of their desired output order (since a stack pops last-pushed-first,
    // pushing in reverse makes the FIRST thing to output the LAST thing
    // pushed, so it pops first). A scalar `Val`/`QuotedVal` writes directly
    // with no further pushes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut stack: Vec<DisplayItem> = vec![DisplayItem::Val(self)];
        while let Some(item) = stack.pop() {
            let v = match item {
                DisplayItem::Str(s) => {
                    write!(f, "{s}")?;
                    continue;
                }
                DisplayItem::Owned(s) => {
                    write!(f, "{s}")?;
                    continue;
                }
                DisplayItem::QuotedVal(Value::Str(s)) => {
                    write!(f, "\"{s}\"")?;
                    continue;
                }
                DisplayItem::QuotedVal(v) | DisplayItem::Val(v) => v,
            };
            match v {
                Value::Int(x) => write!(f, "{x}")?,
                Value::SizedInt(b) => write!(f, "{}", b.0)?,
                Value::BigInt(b) => write!(f, "{b}")?,
                Value::Rational(r) => write!(f, "{r}")?,
                Value::F32(x) => {
                    if x.fract() == 0.0 && x.is_finite() {
                        write!(f, "{x:.1}")?;
                    } else {
                        write!(f, "{x}")?;
                    }
                }
                Value::Float(x) => {
                    if x.fract() == 0.0 && x.is_finite() {
                        write!(f, "{x:.1}")?;
                    } else {
                        write!(f, "{x}")?;
                    }
                }
                Value::Bool(x) => write!(f, "{x}")?,
                Value::Str(s) => write!(f, "{s}")?,
                Value::Unit => write!(f, "()")?,
                Value::List(items) => {
                    stack.push(DisplayItem::Str("]"));
                    for (i, item) in items.iter().enumerate().rev() {
                        stack.push(DisplayItem::QuotedVal(item));
                        if i > 0 {
                            stack.push(DisplayItem::Str(", "));
                        }
                    }
                    stack.push(DisplayItem::Str("["));
                }
                Value::Ctor { variant, fields, .. } => {
                    if !fields.is_empty() {
                        stack.push(DisplayItem::Str(")"));
                        for (i, field) in fields.iter().enumerate().rev() {
                            stack.push(DisplayItem::QuotedVal(field));
                            if i > 0 {
                                stack.push(DisplayItem::Str(", "));
                            }
                        }
                        stack.push(DisplayItem::Str("("));
                    }
                    // A REAL bug found+fixed (production-hardening PR-it628): a
                    // cross-package constructor's mangled name (`pkg$Name`, see
                    // resolve.rs) used to leak verbatim into `print()` output --
                    // demangled here for display; `variant` itself stays the
                    // full mangled name for equality/matching (see `PartialEq`
                    // below and interp.rs's pattern matching), only this
                    // rendering is affected.
                    stack.push(DisplayItem::Owned(crate::resolve::demangle_for_display(variant).to_string()));
                }
                Value::Closure(_) => write!(f, "<fn>")?,
                Value::Fun(name) => write!(f, "<fn {name}>")?,
                Value::Component(id) => write!(f, "<component #{id}>")?,
                Value::Bound(id, name) => write!(f, "<fn {name} of #{id}>")?,
                Value::VmClosure(proto, _, _) => write!(f, "<fn @{proto}>")?,
                Value::Map(pairs) => {
                    stack.push(DisplayItem::Str("}"));
                    for (i, (k, val)) in pairs.iter().enumerate().rev() {
                        stack.push(DisplayItem::QuotedVal(val));
                        stack.push(DisplayItem::Str(": "));
                        stack.push(DisplayItem::QuotedVal(k));
                        if i > 0 {
                            stack.push(DisplayItem::Str(", "));
                        }
                    }
                    stack.push(DisplayItem::Str("Map{"));
                }
                Value::Set(items) => {
                    stack.push(DisplayItem::Str("}"));
                    for (i, item) in items.iter().enumerate().rev() {
                        stack.push(DisplayItem::QuotedVal(item));
                        if i > 0 {
                            stack.push(DisplayItem::Str(", "));
                        }
                    }
                    stack.push(DisplayItem::Str("Set{"));
                }
                // `Tensor` holds a flat `Rc<Vec<f64>>` -- f64 is a leaf, not a
                // nested `Value`, so this can never recurse to unbounded depth
                // and is safe to format directly without going through the
                // work-list.
                Value::Tensor(data) => {
                    write!(f, "Tensor([")?;
                    for (i, x) in data.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", Value::Float(*x))?;
                    }
                    write!(f, "])")?;
                }
                Value::Range(a, b, incl) => write!(f, "{a}..{}{b}", if *incl { "=" } else { "" })?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
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
                    // `value_key_eq`, not plain `==` (PR-it692, a direct follow-up gap
                    // in PR-it691's NaN-key-identity fix): this fast path is a
                    // behavior-preserving shortcut for the general `.insert` method
                    // (which IS value_key_eq-based), so it must apply the SAME key
                    // identity or `m = m.insert(nan, 1); m = m.insert(nan, 2)` would
                    // silently diverge from `m = m.insert(nan,1).insert(nan,2)`.
                    match pairs.iter_mut().find(|(pk, _)| value_key_eq(pk, &key)) {
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
                    // value_key_eq, not plain `==` -- see insert_map_in_place above (PR-it692).
                    if !items.iter().any(|x| value_key_eq(x, &v)) {
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

    /// Collect the distinct component-instance ids referenced by any
    /// `Value::Bound(id, _)` binding reachable from this scope (this scope's
    /// own bindings AND every ancestor scope, since a `forall`'s own local
    /// scope is a CHILD of whatever scope bound the contract's exposed
    /// functions). Used by `interp.rs::forall_case` (production-hardening
    /// PR-it903 -- see that function's own doc comment) to reset a
    /// contract-law's tested component instance back to fresh state before
    /// every property-test case, so each case is judged on its own generated
    /// value rather than on how much state happened to accumulate from
    /// EARLIER cases sharing the same live instance.
    pub fn bound_instance_ids(&self, out: &mut std::collections::HashSet<usize>) {
        let inner = self.0.borrow();
        for (_, v) in &inner.vars {
            if let Value::Bound(id, _) = v {
                out.insert(*id);
            }
        }
        if let Some(p) = &inner.parent {
            p.bound_instance_ids(out);
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}
