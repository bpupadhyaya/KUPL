//! Exact rational numbers built on [`BigInt`], shared by the interpreter and the
//! KVM. Always stored in reduced form (numerator and denominator share no common
//! factor) with a strictly-positive denominator, so equality is structural and
//! `to_string` is canonical — identical across every engine.

use crate::bigint::{BigInt, MAX_BIGINT_LIMBS};
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rational {
    /// carries the sign
    pub num: BigInt,
    /// strictly positive
    pub den: BigInt,
}

/// Cap on `num`/`den`'s size (BOTH, not either -- see below) before
/// `Rational::new` will attempt a `gcd`-based reduction (PR-it718).
/// `BigInt::gcd`'s Euclidean algorithm is an uncapped internal building
/// block, like `add`/`sub`/`mul` -- but unlike those (whose per-call cost is
/// roughly linear/quadratic in operand size), gcd's cost for two "generic"
/// (non-power-of-ten, non-adversarially-crafted) large operands is
/// empirically MUCH worse: measured directly via `BigInt::gcd` on random
/// digit strings, 400 digits took ~130ms, 800 digits ~900ms, 1200 digits ~3s
/// (roughly cubic growth, consistent with O(steps) × O(per-step divmod
/// cost), where BOTH factors grow with operand size for "generic" inputs
/// unlike the FEW-step best case a hand-picked test input can accidentally
/// hit). Extrapolating, `rat(big("<~180,000-digit string>"),
/// big("<~180,000-digit string>"))` -- each operand individually well within
/// `BigInt::from_str`'s OWN cap (PR-it638) -- was confirmed LIVE to run for
/// OVER TWO MINUTES without completing before this fix, on completely
/// ordinary, syntactically unremarkable KUPL source.
///
/// Deliberately requires BOTH operands to exceed the cap, not either: gcd's
/// actual cost is driven by the interaction of the two sizes (Euclid's first
/// step is a SINGLE divmod whose cost is `~a_limbs * b_limbs`), so one huge
/// operand paired with a small one (e.g. `rat(big("<100k digits>"), 3)`, a
/// real pre-existing test case for a DIFFERENT cap, PR-it639's repeated-
/// multiplication test) is cheap regardless of the huge side's size --
/// rejecting on `||` would falsely reject that legitimate, fast case. 100
/// limbs (~900 digits) keeps the worst case (both sides large) bounded to
/// roughly ~1 second even un-optimized, while comfortably covering every
/// REAL rational-number use case (financial/scientific precision essentially
/// never needs more than a few dozen digits) — including this file's own
/// `to_f64` test, which legitimately constructs a `Rational` from two
/// coprime ~401-digit (45-limb) BigInts.
const MAX_GCD_INPUT_LIMBS: usize = 100;

impl Rational {
    /// Build a reduced rational. `Err` if the denominator is zero, OR if
    /// reducing it would require a `gcd` computation too expensive to
    /// attempt (PR-it718, see `MAX_GCD_INPUT_LIMBS`'s doc comment) -- checked
    /// BEFORE calling `gcd`, not after, since (unlike `add`/`sub`/`mul`'s
    /// post-hoc `exceeds_max_size` check at the `raw_binary_op` boundary) the
    /// danger here is the COST of the call itself, not the size of a result
    /// that could be checked after the fact.
    pub fn new(num: BigInt, den: BigInt) -> Result<Rational, String> {
        if den.is_zero() {
            return Err("division by zero".into());
        }
        if num.limb_count() > MAX_GCD_INPUT_LIMBS && den.limb_count() > MAX_GCD_INPUT_LIMBS {
            return Err(format!(
                "Rational construction would require a GCD reduction too large to compute \
                 (limit ~{MAX_GCD_INPUT_LIMBS} limbs, roughly {} decimal digits, when BOTH operands exceed it)",
                MAX_GCD_INPUT_LIMBS * 9
            ));
        }
        // move any sign onto the numerator so the denominator is positive
        let (num, den) = if den.is_negative() {
            (num.negate(), den.negate())
        } else {
            (num, den)
        };
        let g = num.gcd(&den);
        if g.is_zero() {
            // num == 0 -> canonical 0/1
            return Ok(Rational { num: BigInt::zero(), den: BigInt::from_i64(1) });
        }
        let num = num.divmod(&g).unwrap().0;
        let den = den.divmod(&g).unwrap().0;
        Ok(Rational { num, den })
    }

    pub fn from_ints(n: i64, d: i64) -> Result<Rational, String> {
        Rational::new(BigInt::from_i64(n), BigInt::from_i64(d))
    }

    /// Whether either component exceeds `bigint::MAX_BIGINT_LIMBS` (PR-it639)
    /// — `add`/`sub`/`mul` each cross-multiply numerator/denominator BigInts
    /// internally, so a `Rational`'s components can grow the SAME way a bare
    /// `BigInt`'s can via repeated arithmetic; exposed for the SAME
    /// KUPL-operator-boundary check `raw_binary_op` applies to `BigInt`.
    pub fn exceeds_max_size(&self) -> bool {
        self.num.exceeds_max_size() || self.den.exceeds_max_size()
    }

    /// A REAL bug found+fixed (production-hardening PR-it942): this used to
    /// `.unwrap()` `Rational::new`'s `Result` -- but `new` can legitimately
    /// return `Err` (PR-it718's `MAX_GCD_INPUT_LIMBS` guard, see its own doc
    /// comment above), so two ordinary, individually-under-cap `Rational`s
    /// whose cross-multiplication pushes BOTH the resulting numerator AND
    /// denominator past the limb cap turned this `Err` into a raw Rust
    /// panic -- a bogus "internal compiler error... this is a bug in KUPL,
    /// not your program" on interp/vm, actively misleading since the input
    /// IS ordinary (if extreme) user code, fully reachable through `kupl
    /// check`. Confirmed live via repeated squaring of a `rat(big(...),
    /// big(...))` built from two ~450-digit coprime repdigit strings (5
    /// iterations of `r = r * r`): interp/vm crashed with the bogus ICE
    /// message (no KUPL source span); native's independently-reimplemented
    /// `k_rat_mul` was ALREADY correct (calls `k_panic` directly, no
    /// Rust-style Result/unwrap indirection to lose), producing the clean,
    /// intended "Rational construction would require a GCD reduction too
    /// large to compute..." panic instead -- a genuine three-engine
    /// divergence, with native the one that was already right. Fixed by
    /// propagating the `Result` like `div`/`recip` already correctly do,
    /// instead of unwrapping it.
    pub fn add(&self, o: &Rational) -> Result<Rational, String> {
        // a/b + c/d = (a*d + c*b) / (b*d)
        let n = self.num.mul(&o.den).add(&o.num.mul(&self.den));
        Rational::new(n, self.den.mul(&o.den))
    }

    pub fn sub(&self, o: &Rational) -> Result<Rational, String> {
        let n = self.num.mul(&o.den).sub(&o.num.mul(&self.den));
        Rational::new(n, self.den.mul(&o.den))
    }

    pub fn negate(&self) -> Rational {
        Rational { num: self.num.negate(), den: self.den.clone() }
    }

    pub fn mul(&self, o: &Rational) -> Result<Rational, String> {
        Rational::new(self.num.mul(&o.num), self.den.mul(&o.den))
    }

    /// `Err` if `o` is zero.
    pub fn div(&self, o: &Rational) -> Result<Rational, String> {
        if o.num.is_zero() {
            return Err("division by zero".into());
        }
        Rational::new(self.num.mul(&o.den), self.den.mul(&o.num))
    }

    /// `Err` if `self` is zero.
    pub fn recip(&self) -> Result<Rational, String> {
        Rational::new(self.den.clone(), self.num.clone())
    }

    pub fn cmp(&self, o: &Rational) -> Ordering {
        // denominators are positive, so a/b <=> c/d reduces to a*d <=> c*b
        self.num.mul(&o.den).cmp(&o.num.mul(&self.den))
    }

    /// Whether `self.cmp(o)` would perform a BigInt multiplication large
    /// enough to be a de-facto hang (PR-it718). `cmp`'s cross-multiplication
    /// (`num*den` vs `num*den`) is, like `add`/`sub`/`mul`, an uncapped
    /// internal building block -- but unlike those (whose RESULT is checked
    /// against `MAX_BIGINT_LIMBS` at the `raw_binary_op` boundary AFTER
    /// computing, since a single arithmetic op's growth is small and bounded
    /// to check after the fact), `cmp` never stores a result: checking
    /// afterward would mean already having paid the cost this check exists to
    /// avoid. Estimated the SAME way `BigInt::pow` estimates its result size
    /// BEFORE the expensive squaring (PR-it637) -- `self.num`/`o.den` (and
    /// `o.num`/`self.den`) are each individually already known to be
    /// `<= MAX_BIGINT_LIMBS` (every `Rational` in existence was built by a
    /// size-checked operation), so their SUM is a safe, cheap O(1) upper
    /// bound on the product's limb count, needing no multiplication at all
    /// to compute.
    ///
    /// A REAL, LIVE-CONFIRMED bug (PR-it718): two Rationals each built from
    /// an ordinary, individually-legal `big("...")` string near the
    /// `MAX_BIGINT_LIMBS` cap (~180,000 digits, itself accepted by
    /// `BigInt::from_str`'s own cap, PR-it638) and passed to `rat(...)`,
    /// then compared with a single `<` -- confirmed LIVE to run for OVER TWO
    /// MINUTES of CPU time without completing (a debug build; still a
    /// multi-second hang even fully optimized) BEFORE this fix, because
    /// `raw_binary_op`'s `Lt`/`Le`/`Gt`/`Ge` arms and `list_order` (backing
    /// `.min()`/`.max()`/`.min_by()`/`.max_by()`/`.sort()`) each call
    /// `Rational::cmp` directly, entirely bypassing the size-cap check that
    /// gates `+`/`-`/`*`/`/`. An ordinary comparison operator silently
    /// costing minutes of CPU time on syntactically unremarkable, fully
    /// legal input is a real production-readiness gap -- reachable through EVERY
    /// engine (interp/KVM share `raw_binary_op`/`list_order`; native's
    /// independent `k_rat_cmp` has the identical unguarded pattern).
    pub fn cmp_would_be_too_expensive(&self, o: &Rational) -> bool {
        self.num.limb_count() + o.den.limb_count() > MAX_BIGINT_LIMBS as usize
            || o.num.limb_count() + self.den.limb_count() > MAX_BIGINT_LIMBS as usize
    }

    /// Nearest `f64` (may be ±inf for values genuinely beyond `f64`'s range).
    ///
    /// A REAL bug found+fixed (production-hardening PR-it627): converting
    /// `num` and `den` to `f64` SEPARATELY and then dividing is wrong when
    /// BOTH individually overflow `f64`'s range but their RATIO doesn't --
    /// e.g. two ~400-digit, coprime BigInts differing by only 2
    /// (`(10^400+1)/(10^400+3)`, whose true value is ~1.0) each parse to
    /// `+inf`, so the old code computed `inf / inf = NaN` -- silently wrong,
    /// and not the documented "nearest f64" for a value that IS close to 1.
    /// Confirmed via a live repro through the real `rat`/`to_float` builtins
    /// before touching any code. Fixed with a fast path (direct
    /// parse-and-divide, unchanged, for the common case where both operands
    /// fit `f64` on their own) and a slow path taken only when at least one
    /// side overflows.
    ///
    /// A SECOND REAL bug found+fixed (production-hardening PR-it1070, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): the original PR-it627 slow path scaled by a FIXED
    /// `10^30` and then divided the resulting decimal string's `f64` by a
    /// literal `10f64.powi(30)` -- correct only when the true ratio's
    /// magnitude happens to land within about 30 orders of magnitude of 1.
    /// For a ratio genuinely near (but not beyond) `f64`'s actual exponent
    /// boundaries -- e.g. `num=1, den=10^320+1` (true value ~1e-320, a
    /// perfectly valid subnormal) or `num=(10^400+7)*10^290+1,
    /// den=10^400+7` (true value ~1e290, comfortably finite) -- the fixed
    /// `10^30` scale is nowhere near enough to bring the SCALED quotient
    /// into a safely-representable range, so it silently rounds to `0.0` or
    /// `inf` respectively, even though the true value is neither. Live-
    /// confirmed before this fix: both cases above returned the wrong
    /// value identically on `kupl run`, `kupl run --vm`, AND `kupl native`
    /// (cgen.rs's `k_rat_to_f64` has the IDENTICAL fixed-scale defect),
    /// while genuinely-out-of-range ratios (`10^400/1`, `1/10^400`) and the
    /// original near-1 case both still correctly returned `inf`/`0.0`/`1.0`.
    /// Fixed by computing a DYNAMIC scale from `num`/`den`'s own decimal
    /// digit counts (already computed as strings above, for free) so the
    /// scaled integer quotient always has a comfortable, roughly-constant
    /// number of significant digits regardless of how far apart `num` and
    /// `den`'s magnitudes are -- then, instead of dividing by a literal
    /// `f64` power of ten (which can itself independently overflow or
    /// underflow even when the true ratio wouldn't), the quotient's digits
    /// are combined with the correct decimal EXPONENT into a scientific-
    /// notation string and handed to Rust's own correctly-rounded
    /// `f64::from_str`, which applies the full valid exponent range in one
    /// exact step. A digit-count gap so extreme that even a generous safety
    /// margin puts it unambiguously beyond `f64`'s range (~10^308 max,
    /// ~10^-324 smallest subnormal) is detected and returned directly,
    /// without ever needing to build a huge power-of-ten `BigInt`.
    pub fn to_f64(&self) -> f64 {
        let n_dec = self.num.to_decimal();
        let d_dec = self.den.to_decimal();
        if let (Ok(n), Ok(d)) = (n_dec.parse::<f64>(), d_dec.parse::<f64>()) {
            if n.is_finite() && d.is_finite() {
                return n / d;
            }
        }
        if self.num.is_zero() {
            return 0.0;
        }
        let neg = self.num.is_negative();
        let num_abs = self.num.abs();
        let num_digits = num_abs.to_decimal().len() as i64;
        let den_digits = self.den.to_decimal().len() as i64;
        // log10(num/den) is within +-1 of (num_digits - den_digits); a wide
        // safety margin beyond f64's actual exponent range means the ratio
        // is UNAMBIGUOUSLY out of range here.
        let approx_exp = num_digits - den_digits;
        if approx_exp > 320 {
            return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
        }
        if approx_exp < -330 {
            return 0.0;
        }
        // The true ratio's magnitude is within a few hundred orders of
        // f64's representable range either way, so `scale` below is always
        // small (well under a thousand) even though num/den individually
        // can be up to MAX_BIGINT_LIMBS-sized -- safe to `pow` unconditionally.
        const EXTRA_DIGITS: i64 = 25;
        let scale = den_digits - num_digits + EXTRA_DIGITS;
        let q_dec = if scale >= 0 {
            let factor = BigInt::from_i64(10).pow(scale as u64).expect("bounded scale");
            num_abs.mul(&factor).divmod(&self.den).unwrap().0.to_decimal()
        } else {
            let factor = BigInt::from_i64(10).pow((-scale) as u64).expect("bounded scale");
            num_abs.divmod(&self.den.mul(&factor)).unwrap().0.to_decimal()
        };
        if q_dec == "0" {
            return 0.0;
        }
        // `q_dec` is an unsigned decimal-integer string; its true value is
        // `q_dec * 10^-scale`.
        let qd = q_dec.len() as i64;
        let exp = (qd - 1) - scale;
        let mantissa = if qd > 1 { format!("{}.{}", &q_dec[..1], &q_dec[1..]) } else { q_dec };
        let sci = format!("{}{}e{}", if neg { "-" } else { "" }, mantissa, exp);
        sci.parse::<f64>().unwrap_or(if neg { f64::NEG_INFINITY } else { f64::INFINITY })
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == BigInt::from_i64(1) {
            write!(f, "{}", self.num.to_decimal())
        } else {
            write!(f, "{}/{}", self.num.to_decimal(), self.den.to_decimal())
        }
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        Rational::cmp(self, other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(n: i64, d: i64) -> Rational {
        Rational::from_ints(n, d).unwrap()
    }
    #[test]
    fn reduces_and_arithmetic() {
        assert_eq!(r(2, 4), r(1, 2)); // reduction
        assert_eq!(r(1, -2), r(-1, 2)); // sign normalization
        assert_eq!(r(1, 3).add(&r(1, 6)).unwrap(), r(1, 2)); // 1/3 + 1/6 = 1/2
        assert_eq!(r(1, 3).mul(&r(3, 1)).unwrap(), r(1, 1)); // 1/3 * 3 = 1
        assert_eq!(r(1, 3).div(&r(1, 2)).unwrap(), r(2, 3)); // (1/3)/(1/2) = 2/3
        assert_eq!(r(3, 7).recip().unwrap(), r(7, 3));
        assert_eq!(r(1, 1).to_string(), "1"); // integer prints bare
        assert_eq!(r(3, 4).to_string(), "3/4");
        assert!(r(1, 3).cmp(&r(1, 2)) == Ordering::Less);
        assert_eq!(r(1, 4).to_f64(), 0.25);
        assert!(r(1, 2).div(&r(0, 1)).is_err());
    }

    /// A REAL bug found+fixed (production-hardening PR-it627): the doc
    /// comment's OWN claim — "Nearest f64 (may be ±inf for values beyond
    /// f64 range)" — was violated by `to_f64`'s old implementation
    /// (`num.to_f64() / den.to_f64()`, converting each side SEPARATELY):
    /// for two coprime BigInts that are EACH individually too large for
    /// `f64` (both parse to `+inf`) but whose RATIO is perfectly
    /// representable, this computed `inf / inf = NaN` — not "the nearest
    /// f64" to a value that's actually close to 1. Found by taking the
    /// module's own claim as a spec and constructing exactly the case it
    /// implies should work but doesn't: two huge, nearly-equal, coprime
    /// numerators/denominators. Confirmed live through the real `rat`/
    /// `to_float` KUPL builtins (interp, vm, AND native all agreed on the
    /// pre-fix NaN before this fix, and all three agree on the post-fix
    /// ~1.0 after it) before writing this unit-level test.
    #[test]
    fn to_f64_does_not_produce_nan_when_num_and_den_are_individually_oversized_but_the_ratio_is_not() {
        // (10^400 + 1) / (10^400 + 3) -- coprime (differ by 2, and neither
        // is a multiple of the other), each individually WAY beyond f64's
        // ~1.8e308 range, but the ratio itself is extremely close to 1.
        let ten_400 = BigInt::from_i64(10).pow(400).unwrap();
        let n = ten_400.add(&BigInt::from_i64(1));
        let d = ten_400.add(&BigInt::from_i64(3));
        let ratio = Rational::new(n, d).unwrap();
        let f = ratio.to_f64();
        assert!(f.is_finite(), "expected a finite value close to 1.0, got {f}");
        assert!((f - 1.0).abs() < 1e-10, "expected ~1.0, got {f}");

        // a genuinely astronomical ratio (not just individually oversized)
        // must still correctly reduce to +inf / 0.0, matching the doc
        // comment's own documented "may be ±inf for values beyond range" —
        // the fix must not accidentally turn THESE cases finite.
        let huge_over_one = Rational::new(BigInt::from_i64(10).pow(400).unwrap(), BigInt::from_i64(1)).unwrap();
        assert_eq!(huge_over_one.to_f64(), f64::INFINITY);
        let one_over_huge = Rational::new(BigInt::from_i64(1), BigInt::from_i64(10).pow(400).unwrap()).unwrap();
        assert_eq!(one_over_huge.to_f64(), 0.0);

        // the ordinary, small-number fast path is unaffected by the fix.
        assert_eq!(r(1, 4).to_f64(), 0.25);
        assert_eq!(r(10, 1).to_f64(), 10.0);
    }

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
    /// PR-it1070, an Explore survey finding, independently re-verified
    /// live before implementing): the PR-it627 fix above scaled by a FIXED
    /// `10^30` regardless of how far the true ratio's magnitude sits from
    /// f64's actual overflow/underflow boundaries, so a ratio genuinely
    /// NEAR (but not beyond) those boundaries -- not just individually
    /// oversized like the near-1 case above -- silently rounded to `0.0`
    /// or `inf` instead of its true finite/representable value. Live-
    /// confirmed via `kupl run`/`run --vm`/`native` before this fix: both
    /// cases below returned the wrong value identically on all three
    /// engines (cgen.rs's `k_rat_to_f64` has the identical fixed-scale
    /// defect).
    #[test]
    fn to_f64_uses_a_dynamic_scale_not_a_fixed_one_so_near_boundary_ratios_are_not_silently_rounded_to_zero_or_inf() {
        // num=1, den=10^320+1: true value ~1e-320, a valid representable
        // subnormal -- the old fixed 10^30 scale left it far too small to
        // survive the scaled integer division, silently giving 0.0.
        let d1 = BigInt::from_i64(10).pow(320).unwrap().add(&BigInt::from_i64(1));
        let case1 = Rational::new(BigInt::from_i64(1), d1.clone()).unwrap();
        let f1 = case1.to_f64();
        assert!(f1 > 0.0 && f1.is_finite(), "expected a tiny nonzero subnormal, got {f1}");
        assert!((f1 - 1e-320).abs() / 1e-320 < 1e-6, "expected ~1e-320, got {f1}");

        // num=(10^400+7)*10^290+1, den=10^400+7: true value ~1e290,
        // comfortably finite -- the old fixed scale pushed the scaled
        // quotient itself past f64::MAX, silently giving inf.
        let ten_400_7 = BigInt::from_i64(10).pow(400).unwrap().add(&BigInt::from_i64(7));
        let n2 = ten_400_7.mul(&BigInt::from_i64(10).pow(290).unwrap()).add(&BigInt::from_i64(1));
        let case2 = Rational::new(n2, ten_400_7).unwrap();
        let f2 = case2.to_f64();
        assert!(f2.is_finite(), "expected a finite value ~1e290, got {f2}");
        assert!((f2 - 1e290).abs() / 1e290 < 1e-6, "expected ~1e290, got {f2}");

        // a negative numerator must carry its sign through the new path too.
        let neg_case = Rational::new(BigInt::from_i64(1).negate(), d1).unwrap();
        assert!(neg_case.to_f64() < 0.0, "sign must survive the dynamic-scale path");
    }
}
