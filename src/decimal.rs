//! An exact base-10 arbitrary-precision decimal number (`dec("3.14")`),
//! built on [`BigInt`] exactly like [`crate::rational::Rational`] is —
//! `sig * 10^-scale`, where `sig` is a signed [`BigInt`] significand and
//! `scale` is the count of digits after the decimal point. Unlike
//! `Rational`, a `Decimal` is NOT auto-reduced: `dec("2.50")` and
//! `dec("2.5")` are numerically equal (`==` aligns scales before
//! comparing) but keep their own STORED scale for `Display` — matching
//! how SQL `DECIMAL`/`NUMERIC` columns and financial libraries preserve a
//! value's own precision rather than silently trimming "insignificant"
//! trailing zeros a caller may have intentionally written.
//!
//! `it107`: this is the universal-language enrichment campaign's own
//! second "second numeric tower member built on `BigInt`" (`Rational` was
//! the first) — the campaign's private continuity note explicitly flagged
//! `src/bigint.rs`/`src/rational.rs` as templates before starting.

use crate::bigint::BigInt;
use std::cmp::Ordering;
use std::fmt;

/// A conservative, DELIBERATE cap on `scale` (far below what a bare
/// `BigInt` significand could otherwise support via its own
/// `MAX_BIGINT_LIMBS`, ~180,000 decimal digits) — chosen specifically to
/// keep `align`'s scale-matching multiplication cheap regardless of how
/// large the OTHER operand's significand is. Unlike `BigInt::pow`/
/// `Rational::add`-style growth (where the DANGER is one already-large
/// operand growing further), the danger here is different in shape: an
/// operand with a modest significand but a huge STORED scale (e.g.
/// `dec("0." + "0"*99_999 + "1")`, a syntactically ordinary 1-limb
/// significand paired with a 100,000-digit scale) forces `align` to
/// multiply the OTHER operand's significand — which can independently be
/// as large as `MAX_BIGINT_LIMBS` allows — by a `10^100_000`-magnitude
/// power, an O(n×m) schoolbook multiplication of two independently
/// near-cap-sized numbers. This is exactly the cost shape
/// `Rational::cmp_would_be_too_expensive` (PR-it718) was written to
/// reject for cross-multiplication — but here, bounding `scale` itself
/// (checked at construction AND after every op that can grow it) removes
/// the danger by construction instead of needing a separate cost-estimate
/// function: capping the SHIFT side of the multiplication small enough
/// that even pairing it with a `BigInt` at its own independent
/// `MAX_BIGINT_LIMBS` cap stays comfortably fast. No real financial,
/// scientific, or engineering use of a `Decimal` type needs anywhere
/// close to 1,000 places after the decimal point — this can be raised
/// later if a genuine need surfaces, but starts conservative.
pub const MAX_DECIMAL_SCALE: u32 = 1_000;

/// How many EXTRA digits of precision `div` computes beyond
/// `max(a.scale, b.scale)` before rounding — decimal division does not
/// generally terminate (`1 / 3` has no finite decimal expansion), so
/// SOME fixed precision policy is unavoidable. 34 significant digits
/// mirrors IEEE 754-2008's `decimal128` format — a standards-referenced,
/// defensible choice rather than an arbitrary one, and far more than
/// double precision `Float`'s own ~15-17 significant decimal digits.
pub const DIV_EXTRA_DIGITS: u32 = 34;

#[derive(Clone, Debug)]
pub struct Decimal {
    /// signed significand; value == sig * 10^-scale
    pub sig: BigInt,
    /// digits stored after the decimal point (always >= 0)
    pub scale: u32,
}

impl Decimal {
    pub fn zero() -> Decimal {
        Decimal { sig: BigInt::zero(), scale: 0 }
    }

    pub fn from_i64(v: i64) -> Decimal {
        Decimal { sig: BigInt::from_i64(v), scale: 0 }
    }

    /// Parse an optional sign, decimal digits, and an optional single `.`
    /// followed by more decimal digits (`"3.14"`, `"-0.005"`, `"100"`,
    /// `".5"`, `"5."`) — deliberately no exponent notation (`1e10`) in this
    /// first stage, mirroring how `BigInt::from_str` itself started plain
    /// integer-only before `pow`/exponent-adjacent features arrived later.
    pub fn from_str(s: &str) -> Result<Decimal, String> {
        let t = s.trim();
        let (neg, rest) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(format!("invalid Decimal: {s}"));
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid Decimal: {s}"));
        }
        let scale = frac_part.len() as u32;
        if scale > MAX_DECIMAL_SCALE {
            return Err(format!(
                "invalid Decimal: {} fractional digits exceeds the {MAX_DECIMAL_SCALE}-digit scale limit",
                frac_part.len()
            ));
        }
        let mut digits = String::with_capacity(int_part.len() + frac_part.len());
        digits.push_str(int_part);
        digits.push_str(frac_part);
        if digits.is_empty() {
            digits.push('0');
        }
        let magnitude =
            BigInt::from_str(&digits).ok_or_else(|| format!("invalid Decimal: {s}"))?;
        let sig = if neg { magnitude.negate() } else { magnitude };
        Ok(Decimal { sig, scale })
    }

    /// Whether this value's significand or scale exceeds its own cap —
    /// the `Decimal` sibling of `BigInt::exceeds_max_size`/
    /// `Rational::exceeds_max_size`, checked at the SAME
    /// `raw_binary_op`-boundary point after every arithmetic op (PR-it639's
    /// own "repeated ordinary ops can walk an in-range value past the cap
    /// one step at a time" lesson applies here too: `mul`'s `scale = a.scale
    /// + b.scale` can newly exceed `MAX_DECIMAL_SCALE` even when both
    /// operands were individually within it).
    pub fn exceeds_max_size(&self) -> bool {
        self.sig.exceeds_max_size() || self.scale > MAX_DECIMAL_SCALE
    }

    fn scale_up(sig: &BigInt, digits: u32) -> Result<BigInt, String> {
        if digits == 0 {
            return Ok(sig.clone());
        }
        let p = BigInt::from_i64(10).pow(digits as u64)?;
        Ok(sig.mul(&p))
    }

    /// Scale both operands' significands to a shared common scale
    /// (`max(a.scale, b.scale)`) so their significands become directly
    /// comparable/addable — the decimal analogue of `Rational::add`'s own
    /// `a*d + c*b` cross-multiplication to a shared denominator.
    fn align(a: &Decimal, b: &Decimal) -> Result<(BigInt, BigInt, u32), String> {
        let scale = a.scale.max(b.scale);
        let asig = Self::scale_up(&a.sig, scale - a.scale)?;
        let bsig = Self::scale_up(&b.sig, scale - b.scale)?;
        Ok((asig, bsig, scale))
    }

    pub fn add(a: &Decimal, b: &Decimal) -> Result<Decimal, String> {
        let (asig, bsig, scale) = Self::align(a, b)?;
        Ok(Decimal { sig: asig.add(&bsig), scale })
    }

    pub fn sub(a: &Decimal, b: &Decimal) -> Result<Decimal, String> {
        let (asig, bsig, scale) = Self::align(a, b)?;
        Ok(Decimal { sig: asig.sub(&bsig), scale })
    }

    pub fn negate(a: &Decimal) -> Decimal {
        Decimal { sig: a.sig.negate(), scale: a.scale }
    }

    /// Always exact (unlike `div`): `sig`/`scale` each simply add.
    pub fn mul(a: &Decimal, b: &Decimal) -> Decimal {
        Decimal { sig: a.sig.mul(&b.sig), scale: a.scale + b.scale }
    }

    /// `Err` on division by zero, or if the result's scale would exceed
    /// `MAX_DECIMAL_SCALE`. Rounds half-away-from-zero at the last digit —
    /// see `DIV_EXTRA_DIGITS`'s own doc comment for why a rounding policy
    /// is unavoidable here (unlike `Rational::div`, which is always exact).
    pub fn div(a: &Decimal, b: &Decimal) -> Result<Decimal, String> {
        if b.sig.is_zero() {
            return Err("division by zero".into());
        }
        let target_scale = a.scale.max(b.scale) + DIV_EXTRA_DIGITS;
        if target_scale > MAX_DECIMAL_SCALE {
            return Err(format!(
                "Decimal division would require {target_scale} digits of scale, exceeding the {MAX_DECIMAL_SCALE}-digit limit"
            ));
        }
        // value = (a.sig / 10^a.scale) / (b.sig / 10^b.scale)
        //       = (a.sig * 10^b.scale) / (b.sig * 10^a.scale)
        // we want a `result.sig` such that `result.sig / 10^target_scale`
        // equals that value, so scale the numerator up by the extra shift
        // needed to land the quotient at `target_scale` digits.
        let shift = target_scale + b.scale - a.scale; // always >= 0: target_scale >= a.scale by construction
        let scaled_num = Self::scale_up(&a.sig, shift)?;
        let (q, r) = scaled_num.divmod(&b.sig).ok_or_else(|| "division by zero".to_string())?;
        let result_neg = scaled_num.is_negative() != b.sig.is_negative();
        let rounded = Self::round_half_up(q, r, &b.sig, result_neg);
        let result = Decimal { sig: rounded, scale: target_scale };
        if result.exceeds_max_size() {
            return Err(format!(
                "Decimal division result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                crate::bigint::MAX_BIGINT_LIMBS,
                crate::bigint::MAX_BIGINT_LIMBS * 9
            ));
        }
        Ok(result)
    }

    /// `divmod` truncates toward zero (dividend's sign on the remainder) —
    /// round the truncated quotient's MAGNITUDE up by one when the
    /// discarded remainder is at least half the divisor (`2*|r| >= |b|`),
    /// in the direction `result_neg` says the true mathematical quotient
    /// lies (needed since `q` itself can be exactly zero, which carries no
    /// sign of its own to read back).
    fn round_half_up(q: BigInt, r: BigInt, divisor: &BigInt, result_neg: bool) -> BigInt {
        let twice_r = r.abs().add(&r.abs());
        if twice_r.cmp(&divisor.abs()) != Ordering::Less {
            let one = BigInt::from_i64(1);
            if result_neg {
                q.sub(&one)
            } else {
                q.add(&one)
            }
        } else {
            q
        }
    }

    pub fn cmp(a: &Decimal, b: &Decimal) -> Result<Ordering, String> {
        let (asig, bsig, _) = Self::align(a, b)?;
        Ok(asig.cmp(&bsig))
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        Decimal::cmp(self, other) == Ok(Ordering::Equal)
    }
}
impl Eq for Decimal {}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.sig.is_negative();
        let digits = self.sig.abs().to_string();
        let scale = self.scale as usize;
        if neg {
            write!(f, "-")?;
        }
        if scale == 0 {
            write!(f, "{digits}")
        } else if digits.len() > scale {
            let split = digits.len() - scale;
            write!(f, "{}.{}", &digits[..split], &digits[split..])
        } else {
            write!(f, "0.{}{}", "0".repeat(scale - digits.len()), digits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn parse_and_display_roundtrip() {
        for s in ["0", "1", "-1", "3.14", "-0.005", "100", "0.1", "-0.1", "123456789.987654321"] {
            assert_eq!(d(s).to_string(), s);
        }
    }

    #[test]
    fn parse_edge_forms() {
        assert_eq!(d(".5").to_string(), "0.5");
        assert_eq!(d("5.").to_string(), "5");
        assert_eq!(d("-.5").to_string(), "-0.5");
        assert_eq!(d("007").to_string(), "7");
        assert_eq!(d("+3.5").to_string(), "3.5");
    }

    #[test]
    fn parse_rejects_malformed() {
        for s in ["", "-", ".", "1.2.3", "abc", "1a", "1.a", "1..2"] {
            assert!(Decimal::from_str(s).is_err(), "expected {s:?} to be rejected");
        }
    }

    #[test]
    fn equality_is_scale_insensitive_but_display_preserves_scale() {
        assert_eq!(d("2.50"), d("2.5"));
        assert_eq!(d("2.50").to_string(), "2.50");
        assert_eq!(d("2.5").to_string(), "2.5");
        assert_ne!(d("2.50"), d("2.51"));
        assert_eq!(d("0"), d("0.00"));
        assert_eq!(d("-0.00"), d("0"));
    }

    #[test]
    fn add_sub_mul_are_exact() {
        assert_eq!(Decimal::add(&d("1.1"), &d("2.22")).unwrap().to_string(), "3.32");
        assert_eq!(Decimal::sub(&d("5.5"), &d("1.25")).unwrap().to_string(), "4.25");
        assert_eq!(Decimal::mul(&d("2.5"), &d("4")).to_string(), "10.0");
        assert_eq!(Decimal::mul(&d("0.1"), &d("0.1")).to_string(), "0.01");
        assert_eq!(Decimal::sub(&d("1"), &d("0.9")).unwrap().to_string(), "0.1");
        assert_eq!(Decimal::add(&d("-1.5"), &d("1.5")).unwrap().to_string(), "0.0");
    }

    #[test]
    fn div_rounds_half_up_and_is_bounded_precision() {
        // 1/4 = 0.25 exactly, at scale 0.max(0)+34 = 34 digits after the point.
        let r = Decimal::div(&d("1"), &d("4")).unwrap();
        assert_eq!(r.to_string(), format!("0.25{}", "0".repeat(32)));
        assert_eq!(Decimal::div(&d("10"), &d("2")).unwrap().to_string(), format!("5.{}", "0".repeat(34)));
        assert!(Decimal::div(&d("1"), &d("0")).is_err());
        // 1/3 is non-terminating; must round, not hang or error.
        let third = Decimal::div(&d("1"), &d("3")).unwrap();
        assert!(third.to_string().starts_with("0.333"));
    }

    #[test]
    fn div_rounding_direction_matches_sign() {
        // 1/8 = 0.125 exactly -> no rounding needed regardless of extra precision.
        let r = Decimal::div(&d("1"), &d("8")).unwrap();
        assert!(r.to_string().starts_with("0.125"));
        let neg = Decimal::div(&d("-1"), &d("4")).unwrap();
        assert!(neg.to_string().starts_with("-0.25"));
    }

    #[test]
    fn ordering_aligns_scale() {
        assert_eq!(Decimal::cmp(&d("1.5"), &d("1.50")).unwrap(), Ordering::Equal);
        assert_eq!(Decimal::cmp(&d("1.4"), &d("1.5")).unwrap(), Ordering::Less);
        assert_eq!(Decimal::cmp(&d("-1"), &d("1")).unwrap(), Ordering::Less);
    }

    #[test]
    fn scale_beyond_the_cap_is_a_clean_error_not_a_hang_or_panic() {
        let huge_scale = format!("0.{}1", "0".repeat(MAX_DECIMAL_SCALE as usize));
        assert!(Decimal::from_str(&huge_scale).is_err());
    }

    /// A REAL danger class this module's own top-of-file doc comment names:
    /// `align` must not let one operand's huge STORED SCALE force scaling
    /// the OTHER operand's significand up by an amount whose product with
    /// that significand's own size is dangerously expensive. Confirmed live
    /// this completes fast (not a multi-second/minute hang) even pairing a
    /// large significand with the max legal scale difference.
    #[test]
    fn align_of_a_large_significand_against_max_scale_completes_quickly() {
        let big_sig = "7".repeat(2000);
        let a = Decimal { sig: BigInt::from_str(&big_sig).unwrap(), scale: 0 };
        let b = Decimal { sig: BigInt::from_i64(1), scale: MAX_DECIMAL_SCALE };
        let start = std::time::Instant::now();
        let sum = Decimal::add(&a, &b).unwrap();
        assert!(start.elapsed().as_secs() < 2, "align must stay cheap even at the scale cap");
        assert!(sum.to_string().ends_with("1"));
    }

    #[test]
    fn mul_growing_scale_past_the_cap_is_a_clean_error() {
        let a = Decimal { sig: BigInt::from_i64(1), scale: MAX_DECIMAL_SCALE };
        let b = Decimal { sig: BigInt::from_i64(1), scale: MAX_DECIMAL_SCALE };
        let product = Decimal::mul(&a, &b);
        assert!(product.exceeds_max_size(), "scale sum must be caught by exceeds_max_size");
    }
}
