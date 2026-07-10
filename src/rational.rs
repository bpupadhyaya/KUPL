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

    /// Nearest `f64` (may be ±inf for values beyond `f64` range).
    pub fn to_f64(&self) -> f64 {
        let n = self.num.to_decimal().parse::<f64>().unwrap_or(f64::INFINITY);
        let d = self.den.to_decimal().parse::<f64>().unwrap_or(f64::INFINITY);
        n / d
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
}
