//! Property-based testing support: a deterministic value generator and a
//! shrinker, driving `forall` in laws and tests.
//!
//! Everything is seeded from a fixed constant, so a `forall` that passes (or
//! fails) does so identically on every machine and every run — reproducible
//! and CI-friendly. On failure the runner shrinks the counterexample toward a
//! minimal falsifying case.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{TyExpr, TyExprKind};
use crate::value::Value;

/// Type name -> its variants, each `(variant name, fields as (name, type))`.
/// Lets the generator build records and pick enum variants.
pub type TypeDb = HashMap<String, Vec<(String, Vec<(String, TyExpr)>)>>;

/// Number of cases a `forall` runs before it is considered to hold.
pub const CASES: usize = 100;
/// Fixed seed — determinism is the whole point.
pub const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// A small deterministic PRNG (xorshift64*). No external dependencies.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 1 } else { seed })
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// Generate one value of the given type. `depth` bounds recursion on nested
/// collections/records so generation always terminates.
pub fn generate(ty: &TyExpr, rng: &mut Rng, types: &TypeDb, depth: usize) -> Result<Value, String> {
    match &ty.kind {
        TyExprKind::Name(n) => match n.as_str() {
            "Int" => Ok(Value::Int(gen_int(rng))),
            "Bool" => Ok(Value::Bool(rng.next() & 1 == 0)),
            "Float" => Ok(Value::Float(gen_float(rng))),
            "Str" => Ok(Value::str(gen_str(rng))),
            other => gen_named(other, rng, types, depth),
        },
        TyExprKind::Generic(n, args) => match (n.as_str(), args.len()) {
            ("List", 1) => {
                let len = if depth >= 4 { 0 } else { rng.below(6) as usize };
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(generate(&args[0], rng, types, depth + 1)?);
                }
                Ok(Value::List(Rc::new(items)))
            }
            ("Option", 1) => {
                if rng.next() & 1 == 0 || depth >= 4 {
                    Ok(Value::none())
                } else {
                    Ok(Value::some(generate(&args[0], rng, types, depth + 1)?))
                }
            }
            _ => Err(format!("no generator for type `{}`", tyname(ty))),
        },
        TyExprKind::Fun(..) => Err("cannot generate a function value in `forall`".into()),
    }
}

fn gen_int(rng: &mut Rng) -> i64 {
    // Bias toward small, boundary-ish values. The magnitude is capped at 1e6 so
    // ordinary arithmetic in a property (`a + b`, `a * b`) stays well inside
    // i64 — KUPL integers are checked, so an overflow would panic and mask the
    // property under test. Test boundary behavior with explicit concrete laws.
    match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => -1,
        3 => (rng.below(21) as i64) - 10, // -10..=10
        4 | 5 => (rng.below(201) as i64) - 100, // -100..=100
        _ => (rng.below(2_000_001) as i64) - 1_000_000, // -1e6..=1e6
    }
}

fn gen_float(rng: &mut Rng) -> f64 {
    match rng.below(6) {
        0 => 0.0,
        1 => 1.0,
        2 => -1.0,
        _ => {
            let n = (rng.below(20001) as f64) - 10000.0;
            n / 100.0
        }
    }
}

fn gen_str(rng: &mut Rng) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz ";
    let len = rng.below(8) as usize;
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize] as char)
        .collect()
}

fn gen_named(name: &str, rng: &mut Rng, types: &TypeDb, depth: usize) -> Result<Value, String> {
    let Some(variants) = types.get(name) else {
        return Err(format!("no generator for type `{name}` (unknown or unsupported)"));
    };
    if variants.is_empty() {
        return Err(format!("no generator for type `{name}`"));
    }
    // A REAL bug found+fixed (production-hardening PR-it636): `generate`'s own
    // doc comment claims "`depth` bounds recursion on nested collections/
    // records so generation always terminates" -- but this function never
    // checked `depth` at all before this fix, unlike its List/Option siblings
    // (both cap at `depth >= 4`). A self-referential record/enum (e.g. `type
    // Tree = Leaf | Node(l: Tree, r: Tree)`) had NO structural termination
    // guarantee -- only the PROBABILITY that a base-case variant eventually
    // gets picked, which the RNG's fixed seed either satisfies quickly or
    // doesn't. Beyond the SAME depth threshold List/Option already use,
    // strongly prefer a NULLARY variant (a natural base case, no fields to
    // recurse into) if the type has one -- restoring the doc comment's own
    // termination guarantee for named types too. If every variant of a type
    // is recursive (no nullary base case at all -- a type with no way to
    // construct a finite value), this can't force termination structurally
    // any more than it could before; that's a property of the TYPE itself,
    // not a gap this function can close.
    let nullary: Vec<usize> =
        variants.iter().enumerate().filter(|(_, (_, f))| f.is_empty()).map(|(i, _)| i).collect();
    let idx = if depth >= 4 && !nullary.is_empty() {
        nullary[rng.below(nullary.len() as u64) as usize]
    } else {
        rng.below(variants.len() as u64) as usize
    };
    let (variant, fields) = &variants[idx];
    let mut vals = Vec::with_capacity(fields.len());
    for (_, fty) in fields {
        vals.push(generate(fty, rng, types, depth + 1)?);
    }
    Ok(Value::Ctor {
        ty: Rc::new(name.to_string()),
        variant: Rc::new(variant.clone()),
        fields: Rc::new(vals),
    })
}

pub fn tyname(ty: &TyExpr) -> String {
    match &ty.kind {
        TyExprKind::Name(n) => n.clone(),
        TyExprKind::Generic(n, args) => {
            let inner: Vec<String> = args.iter().map(tyname).collect();
            format!("{n}[{}]", inner.join(", "))
        }
        TyExprKind::Fun(..) => "fn".into(),
    }
}

/// Whether `generate` can actually produce a value for `ty`, WITHOUT running
/// it — mirrors `generate`'s own match arms exactly, so this predicate and
/// `generate`'s actual behavior can never silently drift apart (production-
/// hardening PR-it693). `known_type` should report whether a NAMED type has a
/// registered, non-empty variant list (what `gen_named` itself requires) —
/// for the interp/vm's own `TypeDb`, that's
/// `|n| types.get(n).is_some_and(|v| !v.is_empty())`. Used by `check.rs`'s
/// `Stmt::Forall` handling to catch an unsupported `forall` binder type (e.g.
/// `Map[K, V]`, `Set[T]`, `Tensor`, a function type) at CHECK time instead of
/// letting it silently pass `kupl check` and then unconditionally fail every
/// single `kupl test` run — even for a tautologically true body — with no way
/// to have caught it ahead of time.
pub fn is_generatable(ty: &TyExpr, known_type: &impl Fn(&str) -> bool) -> bool {
    match &ty.kind {
        TyExprKind::Name(n) => matches!(n.as_str(), "Int" | "Bool" | "Float" | "Str") || known_type(n),
        TyExprKind::Generic(n, args) => match (n.as_str(), args.len()) {
            ("List", 1) | ("Option", 1) => is_generatable(&args[0], known_type),
            _ => false,
        },
        TyExprKind::Fun(..) => false,
    }
}

/// Candidate "smaller" values for shrinking. Greedy: the runner keeps the first
/// candidate that still fails and repeats, so ordering matters (smallest first).
pub fn shrink(v: &Value) -> Vec<Value> {
    match v {
        Value::Int(0) => Vec::new(),
        Value::Int(n) => {
            let mut out = vec![Value::Int(0)];
            // `n.abs()` panics for `n == i64::MIN` (its magnitude doesn't fit
            // in i64) -- `unsigned_abs()` never panics for any i64 value.
            // Unreachable TODAY (this function's only caller only ever shrinks
            // a value `generate` produced, and `gen_int` caps magnitude at
            // 1e6 -- see its own doc comment), but `shrink` is a `pub fn`
            // with no such restriction documented on ITS OWN signature, and
            // shrink candidates should never be able to re-introduce a panic
            // a well-behaved property test couldn't otherwise trigger
            // (production-hardening PR-it636, found auditing this module).
            if n.unsigned_abs() > 1 {
                out.push(Value::Int(n / 2));
            }
            let toward = if *n > 0 { n - 1 } else { n + 1 };
            if toward != 0 {
                out.push(Value::Int(toward));
            }
            out
        }
        Value::Bool(true) => vec![Value::Bool(false)],
        Value::Bool(false) => Vec::new(),
        Value::Float(f) if *f != 0.0 => {
            let mut out = vec![Value::Float(0.0)];
            if f.fract() != 0.0 {
                out.push(Value::Float(f.trunc()));
            }
            out
        }
        Value::Float(_) => Vec::new(),
        Value::Str(s) if !s.is_empty() => {
            let mut out = vec![Value::str(String::new())];
            let chars: Vec<char> = s.chars().collect();
            if chars.len() > 1 {
                let half: String = chars[..chars.len() / 2].iter().collect();
                out.push(Value::str(half));
                let drop_first: String = chars[1..].iter().collect();
                out.push(Value::str(drop_first));
            }
            // drop each single character, not just the front-half/drop-first
            // candidates above -- a REAL quality bug found+fixed (production-
            // hardening PR-it869), the SAME shape PR-it749 already fixed for
            // `List` (never extended to `Str`): only ever offering a
            // front-truncated or front-dropped candidate means a character
            // that must be removed from the MIDDLE or END of the string (to
            // reach the true minimal counterexample) can never be eliminated
            // -- the shrinker gets permanently stuck on a non-minimal result.
            // Confirmed live before this fix: `forall s: Str { expect
            // !(s.contains("q") && s.contains("z")) }` reported the
            // counterexample `"qrzgz"` (5 chars, deterministic across
            // reruns) instead of the true minimal `"qz"` (2 chars) -- the
            // extraneous `r`/`g`/trailing `z` sit at positions the shrinker
            // structurally could not remove.
            for i in 0..chars.len() {
                let mut smaller = chars.clone();
                smaller.remove(i);
                out.push(Value::str(smaller.into_iter().collect::<String>()));
            }
            out
        }
        Value::Str(_) => Vec::new(),
        Value::List(items) if !items.is_empty() => {
            let mut out = vec![Value::List(Rc::new(Vec::new()))];
            // drop each single element
            for i in 0..items.len() {
                let mut smaller: Vec<Value> = items.as_ref().clone();
                smaller.remove(i);
                out.push(Value::List(Rc::new(smaller)));
            }
            // shrink EVERY element, not just the first -- a REAL quality bug
            // found+fixed (production-hardening PR-it749): this used to only
            // ever offer candidates shrinking `items[0]`, so a property whose
            // failure depends on a NON-first element's magnitude/value could
            // never have that element minimized. Confirmed live before this
            // fix: `forall xs: List[Int] { expect xs.get(3).map(|v| v <= 100)
            // .unwrap_or(true) }` reported a counterexample with an unshrunk,
            // near-cap value at index 3 (e.g. `719630`) while the identical
            // property checked at index 0 correctly shrank to `[101]`. Same
            // class of gap PR-it694 already fixed for `Ctor` fields (`for i
            // in 0..fields.len()`, below) but never extended to `List`.
            for i in 0..items.len() {
                for c in shrink(&items[i]) {
                    let mut v2 = items.as_ref().clone();
                    v2[i] = c;
                    out.push(Value::List(Rc::new(v2)));
                }
            }
            out
        }
        Value::List(_) => Vec::new(),
        // Some(x) -> None, then Some(shrunk x)
        Value::Ctor { ty, variant, fields }
            if ty.as_str() == "Option" && variant.as_str() == "Some" =>
        {
            let mut out = vec![Value::none()];
            if let Some(inner) = fields.first() {
                for c in shrink(inner) {
                    out.push(Value::some(c));
                }
            }
            out
        }
        // record / enum: promote a same-typed field to replace the whole value,
        // THEN shrink one field at a time. The promotion is a generalization of
        // the `Option::Some -> None` case above to any user-defined recursive
        // ADT: a field whose OWN value is a `Ctor` of the SAME `ty` (e.g.
        // `Rec2(child: Chain)`'s `child`, itself a `Chain`) is structurally
        // guaranteed to be a valid, smaller sibling of the whole value --
        // `generate` could only ever have produced a same-typed value there.
        // Without this, shrinking a recursive type (a tree, a linked-list-as-
        // ADT, an expression AST, ...) could get permanently stuck mutating
        // fields IN PLACE at whatever depth the first failing case happened to
        // generate, converging on a needlessly deep, non-minimal counterexample
        // instead of a genuinely minimal one -- confirmed live before this fix
        // (production-hardening PR-it694): `type Chain = Base | Rec1(child:
        // Chain) | Rec2(child: Chain) | Rec3(child: Chain)` with `forall c:
        // Chain { expect false }` (unconditionally false, so `Base` is the
        // true minimal counterexample) reported `Rec2(Rec3(Base))` instead.
        // Promotions are tried FIRST (greedy shrinking keeps the first
        // still-failing candidate) since dropping a whole level converges
        // faster than a single field-level mutation.
        Value::Ctor { ty, variant, fields } if !fields.is_empty() => {
            let mut out = Vec::new();
            for f in fields.iter() {
                if matches!(f, Value::Ctor { ty: fty, .. } if fty == ty) {
                    out.push(f.clone());
                }
            }
            for i in 0..fields.len() {
                for c in shrink(&fields[i]) {
                    let mut nf = fields.as_ref().clone();
                    nf[i] = c;
                    out.push(Value::Ctor {
                        ty: ty.clone(),
                        variant: variant.clone(),
                        fields: Rc::new(nf),
                    });
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Human-readable rendering of a counterexample value (strings quoted so an
/// empty string is visible).
pub fn render(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{s}\""),
        other => format!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(SEED);
        let mut b = Rng::new(SEED);
        for _ in 0..50 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn shrink_int_moves_toward_zero() {
        assert!(shrink(&Value::Int(0)).is_empty());
        assert_eq!(shrink(&Value::Int(50))[0], Value::Int(0));
        // shrinking a big value offers 0 and the halfway point
        let cands = shrink(&Value::Int(1000));
        assert!(cands.contains(&Value::Int(0)));
        assert!(cands.contains(&Value::Int(500)));
    }

    /// `n.abs()` panics for `i64::MIN` (production-hardening PR-it636, found
    /// auditing this module) -- `shrink` must not panic on ANY `Value::Int`,
    /// including the one i64 value whose magnitude doesn't fit back in i64.
    #[test]
    fn shrink_int_does_not_panic_on_i64_min() {
        let cands = shrink(&Value::Int(i64::MIN));
        assert!(cands.contains(&Value::Int(0)));
        assert!(cands.contains(&Value::Int(i64::MIN / 2)));
        assert!(cands.contains(&Value::Int(i64::MIN + 1)));
    }

    #[test]
    fn shrink_str_and_list_reduce_size() {
        assert!(shrink(&Value::str("abc")).contains(&Value::str("")));
        let list = Value::List(Rc::new(vec![Value::Int(1), Value::Int(2)]));
        let cands = shrink(&list);
        assert!(cands.contains(&Value::List(Rc::new(Vec::new()))));
    }

    /// A REAL quality bug found+fixed (production-hardening PR-it749): `shrink`'s
    /// `List` arm used to only ever offer candidates shrinking `items[0]` -- an
    /// element at any OTHER position could never be individually minimized, since
    /// there was no equivalent of the `Ctor` arm's `for i in 0..fields.len()` loop
    /// (the sibling recursive-record case, fixed for the analogous gap under
    /// PR-it694). Confirmed live via `kupl test` before this fix: a property
    /// depending on `xs.get(3)`'s magnitude reported an unshrunk near-cap value at
    /// index 3 while the identical property on index 0 shrank correctly.
    #[test]
    fn shrink_list_offers_candidates_shrinking_every_element_not_just_the_first() {
        let list = Value::List(Rc::new(vec![Value::Int(5), Value::Int(5), Value::Int(42)]));
        let cands = shrink(&list);
        // a candidate shrinking element[0] toward 0 must exist (the pre-fix
        // behavior already covered this).
        assert!(
            cands.contains(&Value::List(Rc::new(vec![Value::Int(0), Value::Int(5), Value::Int(42)]))),
            "must still offer a shrunk-index-0 candidate: {cands:?}"
        );
        // a candidate shrinking element[1] (a NON-first position) toward 0 must
        // ALSO exist -- this is exactly what was missing before the fix.
        assert!(
            cands.contains(&Value::List(Rc::new(vec![Value::Int(5), Value::Int(0), Value::Int(42)]))),
            "must offer a shrunk-index-1 candidate: {cands:?}"
        );
        // and element[2] (the LAST position) too.
        assert!(
            cands.contains(&Value::List(Rc::new(vec![Value::Int(5), Value::Int(5), Value::Int(0)]))),
            "must offer a shrunk-index-2 candidate: {cands:?}"
        );
    }

    /// A REAL quality bug found+fixed (production-hardening PR-it869), the SAME
    /// shape `shrink_list_offers_candidates_shrinking_every_element_not_just_the_
    /// first` above already fixed for `List` (PR-it749) -- never extended to
    /// `Str`: only ever offering a front-truncated (`half`) or front-dropped
    /// (`drop_first`) candidate means a character that must be removed from the
    /// MIDDLE or END of a string can never be individually eliminated. Confirmed
    /// live via `kupl test` before this fix: `forall s: Str { expect
    /// !(s.contains("q") && s.contains("z")) }` reported the non-minimal
    /// counterexample `"qrzgz"` (5 chars, deterministic across reruns) instead of
    /// the true minimal `"qz"` (2 chars).
    #[test]
    fn shrink_str_offers_candidates_dropping_any_single_character_not_just_the_front() {
        let cands = shrink(&Value::str("qrz"));
        // dropping the FIRST character is already covered by the pre-fix
        // `drop_first` candidate.
        assert!(cands.contains(&Value::str("rz")), "must still offer a drop-first candidate: {cands:?}");
        // dropping a MIDDLE character must ALSO exist -- exactly what was
        // missing before the fix.
        assert!(cands.contains(&Value::str("qz")), "must offer a drop-middle-character candidate: {cands:?}");
        // and dropping the LAST character too.
        assert!(cands.contains(&Value::str("qr")), "must offer a drop-last-character candidate: {cands:?}");
    }

    #[test]
    fn generated_ints_stay_in_arithmetic_safe_range() {
        let mut rng = Rng::new(SEED);
        for _ in 0..10_000 {
            let n = gen_int(&mut rng);
            assert!(n.abs() <= 1_000_000, "gen_int out of safe range: {n}");
        }
    }

    fn name_ty(n: &str) -> TyExpr {
        TyExpr { kind: TyExprKind::Name(n.to_string()), span: crate::diag::Span::default() }
    }

    /// The deepest a `Value::Ctor` chain of self-referential fields (each
    /// variant's fields recursively checked) goes.
    fn ctor_chain_depth(v: &Value) -> usize {
        match v {
            Value::Ctor { fields, .. } => 1 + fields.iter().map(ctor_chain_depth).max().unwrap_or(0),
            _ => 0,
        }
    }

    /// A REAL bug found+fixed (production-hardening PR-it636): `generate`'s
    /// own doc comment claims "`depth` bounds recursion on nested
    /// collections/records so generation always terminates" -- but
    /// `gen_named` never checked `depth` at all before this fix, unlike its
    /// List/Option siblings. Uses a DELIBERATELY recursion-heavy type (3 of 4
    /// variants recurse, only 1 base case -- a 75% chance of recursing at
    /// each level, versus a balanced 50/50 type like the `Tree` in
    /// `examples/properties.kupl`'s own `Point` neighbor) so the fix's
    /// effect is actually exercised across 100 generated cases, not just
    /// plausible by chance. `gen_named`'s fix forces a nullary variant once
    /// `depth >= 4` (matching List/Option's own threshold), so the deepest a
    /// chain can go is 4 unconstrained levels (0..=3) plus one forced-nullary
    /// level at depth 4 -- asserts every generated value's ctor-chain depth
    /// is at most 5, structurally, not just "usually small."
    #[test]
    fn gen_named_terminates_for_a_recursion_heavy_self_referential_type() {
        let mut types: TypeDb = HashMap::new();
        types.insert(
            "Chain".to_string(),
            vec![
                ("Base".to_string(), vec![]),
                ("Rec1".to_string(), vec![("child".to_string(), name_ty("Chain"))]),
                ("Rec2".to_string(), vec![("child".to_string(), name_ty("Chain"))]),
                ("Rec3".to_string(), vec![("child".to_string(), name_ty("Chain"))]),
            ],
        );
        let ty = name_ty("Chain");
        let mut rng = Rng::new(SEED);
        for i in 0..CASES {
            let v = generate(&ty, &mut rng, &types, 0).expect("generates");
            let depth = ctor_chain_depth(&v);
            assert!(depth <= 5, "case {i}: generated a Chain {depth} levels deep -- depth cap not enforced");
        }
    }
}
