//! Exact rational numbers built on [`BigInt`], shared by the interpreter and the
//! KVM. Always stored in reduced form (numerator and denominator share no common
//! factor) with a strictly-positive denominator, so equality is structural and
//! `to_string` is canonical — identical across every engine.

use crate::bigint::BigInt;
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rational {
    /// carries the sign
    pub num: BigInt,
    /// strictly positive
    pub den: BigInt,
}

impl Rational {
    /// Build a reduced rational. `Err` if the denominator is zero.
    pub fn new(num: BigInt, den: BigInt) -> Result<Rational, String> {
        if den.is_zero() {
            return Err("division by zero".into());
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

    pub fn add(&self, o: &Rational) -> Rational {
        // a/b + c/d = (a*d + c*b) / (b*d)
        let n = self.num.mul(&o.den).add(&o.num.mul(&self.den));
        Rational::new(n, self.den.mul(&o.den)).unwrap()
    }

    pub fn sub(&self, o: &Rational) -> Rational {
        let n = self.num.mul(&o.den).sub(&o.num.mul(&self.den));
        Rational::new(n, self.den.mul(&o.den)).unwrap()
    }

    pub fn negate(&self) -> Rational {
        Rational { num: self.num.negate(), den: self.den.clone() }
    }

    pub fn mul(&self, o: &Rational) -> Rational {
        Rational::new(self.num.mul(&o.num), self.den.mul(&o.den)).unwrap()
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
    /// side overflows: scale `num` by a fixed power of ten via EXACT BigInt
    /// arithmetic before dividing, so the RATIO's precision survives even
    /// though the individual operands don't fit in `f64`. A ratio that is
    /// genuinely astronomically large or small (not just individually
    /// oversized) still correctly reduces to `±inf` or `0.0` through this
    /// same path, since the scaled quotient itself then overflows or
    /// underflows on its own.
    pub fn to_f64(&self) -> f64 {
        let n_dec = self.num.to_decimal();
        let d_dec = self.den.to_decimal();
        if let (Ok(n), Ok(d)) = (n_dec.parse::<f64>(), d_dec.parse::<f64>()) {
            if n.is_finite() && d.is_finite() {
                return n / d;
            }
        }
        const SCALE_DIGITS: u32 = 30;
        let scale = BigInt::from_i64(10).pow(SCALE_DIGITS as u64);
        let scaled_num = self.num.mul(&scale);
        let (q, _) = scaled_num.divmod(&self.den).unwrap();
        let approx = q.to_decimal().parse::<f64>().unwrap_or(f64::INFINITY);
        approx / 10f64.powi(SCALE_DIGITS as i32)
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
        assert_eq!(r(1, 3).add(&r(1, 6)), r(1, 2)); // 1/3 + 1/6 = 1/2
        assert_eq!(r(1, 3).mul(&r(3, 1)), r(1, 1)); // 1/3 * 3 = 1
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
        let ten_400 = BigInt::from_i64(10).pow(400);
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
        let huge_over_one = Rational::new(BigInt::from_i64(10).pow(400), BigInt::from_i64(1)).unwrap();
        assert_eq!(huge_over_one.to_f64(), f64::INFINITY);
        let one_over_huge = Rational::new(BigInt::from_i64(1), BigInt::from_i64(10).pow(400)).unwrap();
        assert_eq!(one_over_huge.to_f64(), 0.0);

        // the ordinary, small-number fast path is unaffected by the fix.
        assert_eq!(r(1, 4).to_f64(), 0.25);
        assert_eq!(r(10, 1).to_f64(), 10.0);
    }
}
