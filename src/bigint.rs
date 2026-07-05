//! A small, zero-dependency arbitrary-precision integer, shared by the
//! interpreter and the KVM. Sign-magnitude, with the magnitude stored as
//! little-endian base-1e9 limbs (each `u32` in `0..1_000_000_000`). Base 1e9 is
//! chosen so `to_string` is trivial and identical to a C port (native), which
//! matters for byte-identity across engines.
//!
//! Schoolbook add/sub/mul (a standard, well-known approach). Division, modulo,
//! and power are a later iteration.

use std::cmp::Ordering;

const BASE: u64 = 1_000_000_000;

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
    /// empty input or a non-digit character.
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim();
        let (neg, digits) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
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
    fn ordering() {
        assert!(b("100").cmp(&b("99")) == Ordering::Greater);
        assert!(b("-100").cmp(&b("-99")) == Ordering::Less);
        assert!(b("-1").cmp(&b("1")) == Ordering::Less);
        assert!(b("0").cmp(&b("0")) == Ordering::Equal);
        assert!(b("1000000000000").cmp(&b("999999999999")) == Ordering::Greater);
    }
}
