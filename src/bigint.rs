//! A small, zero-dependency arbitrary-precision integer, shared by the
//! interpreter and the KVM. Sign-magnitude, with the magnitude stored as
//! little-endian base-1e9 limbs (each `u32` in `0..1_000_000_000`). Base 1e9 is
//! chosen so `to_string` is trivial and identical to a C port (native), which
//! matters for byte-identity across engines.
//!
//! Schoolbook add/sub/mul/div/mod (a standard, well-known approach — this
//! module previously said division/modulo/power were "a later iteration,"
//! but all three are implemented; the doc comment was simply stale, PR-it637).
//! Because multiplication is schoolbook (O(n²), not Karatsuba/Toom-Cook),
//! any operation whose result SIZE isn't bounded by its inputs' own size is a
//! potential denial-of-service: `pow`'s repeated squaring can turn a modest
//! exponent into an exponentially large result, and `from_str` can turn an
//! arbitrarily long caller-supplied digit string directly into an
//! arbitrarily large `BigInt` with no intermediate computation at all. Both
//! reject a request that would exceed `MAX_BIGINT_LIMBS` rather than exhaust
//! memory or pin a CPU core indefinitely (PR-it637/PR-it638).

use std::cmp::Ordering;

const BASE: u64 = 1_000_000_000;
/// A sanity cap on ANY single `BigInt`'s size, in limbs (~9 decimal digits
/// per limb) — generous for any plausible legitimate use (an RSA-2048 key is
/// ~617 decimal digits; this campaign's own tests exercise 400-digit
/// numbers), but far short of exhausting memory/CPU via schoolbook
/// operations. Enforced at every point a `BigInt` can newly EXCEED its
/// inputs' own combined size: `pow` (exponential growth from a modest
/// exponent) and `from_str` (an arbitrarily large caller-supplied string,
/// with no proportional computation of its own to "pay for" the size).
pub const MAX_BIGINT_LIMBS: u64 = 20_000;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BigInt {
    /// sign; always `false` for zero
    neg: bool,
    /// base-1e9 limbs, little-endian, normalized (no trailing zero limb);
    /// empty magnitude represents zero
    limbs: Vec<u32>,
}

impl BigInt {
    pub fn zero() -> Self {
        BigInt { neg: false, limbs: Vec::new() }
    }

    pub fn from_i64(n: i64) -> Self {
        if n == 0 {
            return Self::zero();
        }
        let neg = n < 0;
        let mut m = (n as i128).unsigned_abs(); // handles i64::MIN
        let mut limbs = Vec::new();
        while m > 0 {
            limbs.push((m % BASE as u128) as u32);
            m /= BASE as u128;
        }
        BigInt { neg, limbs }
    }

    /// Parse an optional sign followed by decimal digits. Returns `None` on
    /// empty input, a non-digit character, or a digit string so long the
    /// resulting `BigInt` would exceed `MAX_BIGINT_LIMBS` (a REAL bug found+
    /// fixed, production-hardening PR-it638: unlike `pow`, this construction
    /// path has no computation of its own to "pay for" the result's size --
    /// an ordinary KUPL line like `big("9".repeat(50_000_000))` turns a
    /// modestly-sized string-building call directly into a multi-megabyte
    /// `BigInt`, in ONE step, with no intermediate cost proportional to the
    /// danger. Checked BEFORE building any limbs, on the decimal digit COUNT
    /// alone -- no need to build anything just to reject it).
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim();
        let (neg, digits) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if digits.len() as u64 > MAX_BIGINT_LIMBS * 9 {
            return None;
        }
        let bytes = digits.as_bytes();
        let mut limbs = Vec::new();
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(9);
            let chunk = std::str::from_utf8(&bytes[start..i]).unwrap();
            limbs.push(chunk.parse::<u32>().unwrap());
            i = start;
        }
        let mut b = BigInt { neg, limbs };
        b.normalize();
        Some(b)
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
        if self.limbs.is_empty() {
            self.neg = false;
        }
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// Whether this `BigInt`'s size exceeds `MAX_BIGINT_LIMBS`. `pow` and
    /// `from_str` already reject a request that would newly EXCEED the cap
    /// in one step (PR-it637/PR-it638) — but repeated ordinary arithmetic
    /// (`a * b` doubling roughly every multiplication, in a loop, e.g. a
    /// hand-written squaring loop) can still walk an already-in-range
    /// `BigInt` past the cap one legitimate-looking operation at a time,
    /// bypassing `pow`'s guard entirely without ever calling `pow` at all.
    /// Exposed so a CALLER (specifically `raw_binary_op`, the shared
    /// operator-dispatch boundary reached from ordinary KUPL `+`/`-`/`*`
    /// syntax) can check a RESULT after computing it and reject further
    /// growth, rather than this module capping every individual `add`/
    /// `sub`/`mul` call itself — those remain uncapped internal building
    /// blocks (used throughout this crate on values already known to be
    /// safely bounded), and only the KUPL-visible boundary needs the check
    /// (PR-it639).
    pub fn exceeds_max_size(&self) -> bool {
        self.limbs.len() as u64 > MAX_BIGINT_LIMBS
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    /// -1, 0, or 1.
    pub fn sign(&self) -> i64 {
        if self.limbs.is_empty() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    pub fn abs(&self) -> BigInt {
        BigInt { neg: false, limbs: self.limbs.clone() }
    }

    pub fn negate(&self) -> BigInt {
        if self.limbs.is_empty() {
            self.clone()
        } else {
            BigInt { neg: !self.neg, limbs: self.limbs.clone() }
        }
    }

    pub fn to_decimal(&self) -> String {
        if self.limbs.is_empty() {
            return "0".to_string();
        }
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        s.push_str(&self.limbs.last().unwrap().to_string());
        for limb in self.limbs.iter().rev().skip(1) {
            s.push_str(&format!("{limb:09}"));
        }
        s
    }

    /// Compare magnitudes only.
    fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
        if a.len() != b.len() {
            return a.len().cmp(&b.len());
        }
        for i in (0..a.len()).rev() {
            if a[i] != b[i] {
                return a[i].cmp(&b[i]);
            }
        }
        Ordering::Equal
    }

    fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        let mut carry = 0u64;
        for i in 0..a.len().max(b.len()) {
            let av = *a.get(i).unwrap_or(&0) as u64;
            let bv = *b.get(i).unwrap_or(&0) as u64;
            let s = av + bv + carry;
            out.push((s % BASE) as u32);
            carry = s / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }

    /// Subtract magnitudes, assuming `a >= b`.
    fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let av = a[i] as i64;
            let bv = *b.get(i).unwrap_or(&0) as i64;
            let mut d = av - bv - borrow;
            if d < 0 {
                d += BASE as i64;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(d as u32);
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    pub fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            let mut r = BigInt { neg: self.neg, limbs: Self::add_mag(&self.limbs, &o.limbs) };
            r.normalize();
            r
        } else {
            // signs differ: subtract smaller magnitude from larger
            match Self::cmp_mag(&self.limbs, &o.limbs) {
                Ordering::Equal => BigInt::zero(),
                Ordering::Greater => {
                    let mut r = BigInt { neg: self.neg, limbs: Self::sub_mag(&self.limbs, &o.limbs) };
                    r.normalize();
                    r
                }
                Ordering::Less => {
                    let mut r = BigInt { neg: o.neg, limbs: Self::sub_mag(&o.limbs, &self.limbs) };
                    r.normalize();
                    r
                }
            }
        }
    }

    pub fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.negate())
    }

    pub fn mul(&self, o: &BigInt) -> BigInt {
        if self.limbs.is_empty() || o.limbs.is_empty() {
            return BigInt::zero();
        }
        let mut out = vec![0u64; self.limbs.len() + o.limbs.len()];
        for (i, &av) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &bv) in o.limbs.iter().enumerate() {
                let cur = out[i + j] + av as u64 * bv as u64 + carry;
                out[i + j] = cur % BASE;
                carry = cur / BASE;
            }
            out[i + o.limbs.len()] += carry;
        }
        let mut limbs: Vec<u32> = out.into_iter().map(|x| x as u32).collect();
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        let mut r = BigInt { neg: self.neg != o.neg, limbs };
        r.normalize();
        r
    }

    /// Multiply a magnitude by a scalar `k` in `0..BASE`.
    fn mul_small(a: &[u32], k: u64) -> Vec<u32> {
        if k == 0 || a.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(a.len() + 1);
        let mut carry = 0u64;
        for &av in a {
            let cur = av as u64 * k + carry;
            out.push((cur % BASE) as u32);
            carry = cur / BASE;
        }
        while carry > 0 {
            out.push((carry % BASE) as u32);
            carry /= BASE;
        }
        out
    }

    /// Divide magnitudes: returns (quotient, remainder) magnitudes, `b` != 0.
    /// Long division processing dividend limbs high→low, choosing each base-1e9
    /// quotient digit by binary search — a simple, deterministic method that
    /// ports to C identically (byte-identity of the decimal result).
    fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
        if Self::cmp_mag(a, b) == Ordering::Less {
            return (Vec::new(), a.to_vec());
        }
        let mut quo = vec![0u32; a.len()];
        let mut rem: Vec<u32> = Vec::new();
        for i in (0..a.len()).rev() {
            // rem = rem * BASE + a[i]  (prepend the new limb)
            let mut next = Vec::with_capacity(rem.len() + 1);
            next.push(a[i]);
            next.extend_from_slice(&rem);
            while next.last() == Some(&0) {
                next.pop();
            }
            rem = next;
            // largest q in 0..BASE with b*q <= rem
            let (mut lo, mut hi) = (0u64, BASE - 1);
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                if Self::cmp_mag(&Self::mul_small(b, mid), &rem) != Ordering::Greater {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            quo[i] = lo as u32;
            if lo > 0 {
                rem = Self::sub_mag(&rem, &Self::mul_small(b, lo));
            }
        }
        while quo.last() == Some(&0) {
            quo.pop();
        }
        (quo, rem)
    }

    /// Truncated division: the quotient truncates toward zero and the remainder
    /// takes the DIVIDEND's sign (matching `Int` `/` and `%`). `None` on
    /// division by zero.
    pub fn divmod(&self, o: &BigInt) -> Option<(BigInt, BigInt)> {
        if o.limbs.is_empty() {
            return None;
        }
        let (q, r) = Self::divmod_mag(&self.limbs, &o.limbs);
        let mut quo = BigInt { neg: self.neg != o.neg, limbs: q };
        let mut rem = BigInt { neg: self.neg, limbs: r };
        quo.normalize();
        rem.normalize();
        Some((quo, rem))
    }

    /// The greatest common divisor of `|self|` and `|o|` — Euclid's algorithm.
    /// Always non-negative; `gcd(0, 0) == 0`, `gcd(n, 0) == |n|`.
    pub fn gcd(&self, o: &BigInt) -> BigInt {
        let mut a = self.abs();
        let mut b = o.abs();
        while !b.is_zero() {
            let (_, r) = a.divmod(&b).unwrap();
            a = b;
            b = r;
        }
        a
    }

    /// `self ^ exp` for a non-negative exponent, by repeated squaring. `Err`
    /// if the RESULT would be unreasonably large — a REAL bug found+fixed
    /// (production-hardening PR-it637): unlike `Int.pow` (which caps its
    /// exponent at `u32::MAX` AND uses `checked_pow`, failing fast the
    /// instant the i64 result would overflow), this function had NO limit at
    /// all before this fix — an ordinary, syntactically unremarkable KUPL
    /// line like `big(2).pow(1_000_000_000)` requests a result with roughly
    /// a BILLION bits, which this module's schoolbook (O(n²)) squaring either
    /// exhausts all available memory building, or pins a CPU core computing
    /// for an effectively unbounded time — with NO diagnostic, no clean
    /// panic, nothing a caller could catch or a user could debug. Estimates
    /// the result's limb count (`self`'s own limb count × `exp`, a safe
    /// upper bound — squaring roughly doubles a number's DIGIT count per
    /// squaring step, so the true result size stays under this estimate)
    /// BEFORE doing any of the expensive multiplication, and rejects
    /// anything past `MAX_BIGINT_LIMBS` cleanly instead of attempting it.
    /// `|self|` of 0 or 1 is exempt from the estimate entirely — `0^n` and
    /// `(±1)^n` stay O(1)-sized for ANY exponent, so a huge `exp` alone must
    /// never reject them (the estimate, based on limb COUNT alone, can't
    /// see that a magnitude-1 base never actually grows when multiplied).
    pub fn pow(&self, exp: u64) -> Result<BigInt, String> {
        let unit_magnitude = self.limbs.is_empty() || (self.limbs.len() == 1 && self.limbs[0] == 1);
        if !unit_magnitude {
            let base_limbs = self.limbs.len() as u64;
            let too_large = match base_limbs.checked_mul(exp) {
                Some(estimated) => estimated > MAX_BIGINT_LIMBS,
                None => true, // the estimate itself overflowed u64 -- certainly too large
            };
            if too_large {
                return Err(format!(
                    "BigInt.pow: result would be too large to compute (limit ~{MAX_BIGINT_LIMBS} limbs, roughly {} decimal digits)",
                    MAX_BIGINT_LIMBS * 9
                ));
            }
        }
        let mut result = BigInt::from_i64(1);
        let mut base = self.clone();
        let mut e = exp;
        while e > 0 {
            if e & 1 == 1 {
                result = result.mul(&base);
            }
            e >>= 1;
            if e > 0 {
                base = base.mul(&base);
            }
        }
        Ok(result)
    }

    pub fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.sign(), o.sign()) {
            (a, b) if a != b => a.cmp(&b),
            (s, _) => {
                let m = Self::cmp_mag(&self.limbs, &o.limbs);
                if s < 0 {
                    m.reverse()
                } else {
                    m
                }
            }
        }
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        BigInt::cmp(self, other)
    }
}
impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for BigInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_decimal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> BigInt {
        BigInt::from_str(s).unwrap()
    }

    #[test]
    fn roundtrip_and_display() {
        for s in ["0", "1", "-1", "999999999", "1000000000", "-1000000001",
                  "123456789012345678901234567890"] {
            assert_eq!(b(s).to_decimal(), s);
        }
        assert_eq!(BigInt::from_i64(0).to_decimal(), "0");
        assert_eq!(BigInt::from_i64(i64::MIN).to_decimal(), i64::MIN.to_string());
        assert_eq!(BigInt::from_str("007").unwrap().to_decimal(), "7");
        assert!(BigInt::from_str("").is_none());
        assert!(BigInt::from_str("12x").is_none());
    }

    /// A REAL bug found+fixed (production-hardening PR-it638) -- the SAME
    /// "check extremes" class as `pow`'s fix (it637), but at the OTHER end:
    /// `pow` can turn a modest exponent into an exponentially large result;
    /// `from_str` can turn an arbitrarily long caller-supplied STRING
    /// directly into an arbitrarily large `BigInt`, with no proportional
    /// computation of its own to "pay for" the size -- an ordinary KUPL line
    /// like `big("9".repeat(50_000_000))` used to succeed immediately,
    /// producing a multi-megabyte `BigInt` in one step from a single call.
    #[test]
    fn from_str_rejects_a_digit_string_too_long_to_represent() {
        // exactly at the cap: still fine.
        let at_cap = "9".repeat((MAX_BIGINT_LIMBS * 9) as usize);
        assert!(BigInt::from_str(&at_cap).is_some());
        // one digit past the cap: rejected -- and MUST return quickly (this
        // test itself would be slow/wasteful if rejection required building
        // the limbs first).
        let over_cap = "9".repeat((MAX_BIGINT_LIMBS * 9 + 1) as usize);
        assert!(BigInt::from_str(&over_cap).is_none());
        // a sign prefix doesn't let a caller sneak past the cap by one digit.
        let neg_over_cap = format!("-{over_cap}");
        assert!(BigInt::from_str(&neg_over_cap).is_none());
        // ordinary, legitimate large-but-reasonable strings are unaffected.
        assert!(BigInt::from_str(&"9".repeat(400)).is_some());
    }

    /// A REAL bug found+fixed (production-hardening PR-it639): `pow` (it637)
    /// and `from_str` (it638) already reject a request that would newly
    /// exceed `MAX_BIGINT_LIMBS` in ONE step -- but ordinary REPEATED
    /// multiplication (a hand-written squaring loop, `r = r * r` many times
    /// over) can walk an already-in-range `BigInt` past the cap one
    /// legitimate-looking `mul` call at a time, bypassing `pow`'s guard
    /// entirely without ever calling `pow`. `exceeds_max_size` is the
    /// primitive `raw_binary_op` (the shared KUPL-operator-dispatch
    /// boundary) uses to close that gap.
    #[test]
    fn exceeds_max_size_detects_growth_past_the_cap() {
        let at_cap = BigInt::from_str(&"9".repeat((MAX_BIGINT_LIMBS * 9) as usize)).unwrap();
        assert!(!at_cap.exceeds_max_size());
        let over_cap = at_cap.mul(&BigInt::from_i64(10));
        assert!(over_cap.exceeds_max_size());
        // small, ordinary values are never mistakenly flagged.
        assert!(!b("12345").exceeds_max_size());
        assert!(!BigInt::zero().exceeds_max_size());
    }

    #[test]
    fn arithmetic() {
        assert_eq!(b("2").mul(&b("3")).to_decimal(), "6");
        assert_eq!(b("999999999").add(&b("1")).to_decimal(), "1000000000");
        assert_eq!(b("1000000000").sub(&b("1")).to_decimal(), "999999999");
        assert_eq!(b("-5").add(&b("3")).to_decimal(), "-2");
        assert_eq!(b("5").add(&b("-5")).to_decimal(), "0");
        assert_eq!(b("-4").mul(&b("3")).to_decimal(), "-12");
        // 25! = 15511210043330985984000000
        let mut f = BigInt::from_i64(1);
        for i in 1..=25 {
            f = f.mul(&BigInt::from_i64(i));
        }
        assert_eq!(f.to_decimal(), "15511210043330985984000000");
    }

    #[test]
    fn division_and_power() {
        // q*d + r == dividend, and remainder has the dividend's sign
        for (n, d) in [("100", "7"), ("-100", "7"), ("100", "-7"), ("-100", "-7"),
                       ("1000000000000000000000", "7"), ("999999999999999999", "1000000000")] {
            let (nb, db) = (b(n), b(d));
            let (q, r) = nb.divmod(&db).unwrap();
            assert_eq!(q.mul(&db).add(&r), nb, "{n}/{d}");
            assert!(r.abs().cmp(&db.abs()) == Ordering::Less, "|r|<|d| for {n}/{d}");
        }
        assert_eq!(b("17").divmod(&b("5")).unwrap().1.to_decimal(), "2");
        assert_eq!(b("-17").divmod(&b("5")).unwrap().1.to_decimal(), "-2"); // dividend sign
        assert_eq!(b("100").divmod(&b("7")).unwrap().0.to_decimal(), "14");
        assert!(b("5").divmod(&b("0")).is_none());
        // 10^30 / 10^15 = 10^15
        assert_eq!(b("1000000000000000000000000000000").divmod(&b("1000000000000000")).unwrap().0.to_decimal(),
                   "1000000000000000");
        // powers
        assert_eq!(b("2").pow(10).unwrap().to_decimal(), "1024");
        assert_eq!(b("2").pow(128).unwrap().to_decimal(), "340282366920938463463374607431768211456");
        assert_eq!(b("10").pow(0).unwrap().to_decimal(), "1");
        assert_eq!(b("-3").pow(3).unwrap().to_decimal(), "-27");
    }

    /// A REAL bug found+fixed (production-hardening PR-it637): unlike
    /// `Int.pow` (capped at exponent `u32::MAX`, and `checked_pow` fails fast
    /// on overflow), `BigInt.pow` had NO limit — `big(2).pow(1_000_000_000)`
    /// (an entirely ordinary, syntactically unremarkable KUPL line) requests
    /// a result with roughly a BILLION bits, which this module's schoolbook
    /// (O(n²)) squaring either exhausts memory building or pins a CPU core
    /// computing for an unbounded time.
    #[test]
    fn pow_rejects_a_result_that_would_be_unreasonably_large() {
        // a genuinely huge exponent on a non-trivial base is rejected --
        // MUST return quickly (this test itself would hang/OOM if the cap
        // were not enforced BEFORE attempting the computation).
        assert!(b("2").pow(1_000_000_000).is_err());
        assert!(b("999999999").pow(u64::MAX).is_err());
        // a base whose limb count times a large exponent overflows u64
        // ITSELF must also be rejected, not silently wrap into "small enough".
        assert!(b("1000000000000000000").pow(u64::MAX).is_err());

        // ordinary, legitimate large-but-reasonable results are unaffected --
        // matches this campaign's own 400-digit test fixtures elsewhere.
        assert!(b("10").pow(400).is_ok());
        assert!(b("2").pow(1000).is_ok());
        // 0 and 1 to any power (even a huge one) are trivially tiny and must
        // never be rejected just because the EXPONENT is large.
        assert_eq!(b("0").pow(1_000_000_000).unwrap().to_decimal(), "0");
        assert_eq!(b("1").pow(u64::MAX).unwrap().to_decimal(), "1");
        assert_eq!(b("0").pow(0).unwrap().to_decimal(), "1"); // 0^0 == 1, by convention
    }

    #[test]
    fn gcd_euclid() {
        assert_eq!(b("12").gcd(&b("18")).to_decimal(), "6");
        assert_eq!(b("18").gcd(&b("12")).to_decimal(), "6");
        assert_eq!(b("-12").gcd(&b("18")).to_decimal(), "6"); // magnitude
        assert_eq!(b("17").gcd(&b("5")).to_decimal(), "1"); // coprime
        assert_eq!(b("100").gcd(&b("0")).to_decimal(), "100");
        assert_eq!(b("0").gcd(&b("0")).to_decimal(), "0");
        // large: gcd(2^100, 2^60) == 2^60
        assert_eq!(b("2").pow(100).unwrap().gcd(&b("2").pow(60).unwrap()), b("2").pow(60).unwrap());
    }

    #[test]
    fn ordering() {
        assert!(b("100").cmp(&b("99")) == Ordering::Greater);
        assert!(b("-100").cmp(&b("-99")) == Ordering::Less);
        assert!(b("-1").cmp(&b("1")) == Ordering::Less);
        assert!(b("0").cmp(&b("0")) == Ordering::Equal);
        assert!(b("1000000000000").cmp(&b("999999999999")) == Ordering::Greater);
    }
}
