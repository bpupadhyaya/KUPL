//! Data encodings and a non-cryptographic hash, shared by the interpreter and
//! the KVM (zero dependencies, like `src/json.rs`). All pure and deterministic.
//!
//! `base64_*` and `hex_*` operate on a string's **UTF-8 bytes**; `*_decode`
//! returns `Err` on malformed input or if the decoded bytes are not valid
//! UTF-8. `hash_fnv` is FNV-1a (64-bit), returned as an `i64` bit-pattern.
//! The algorithms are mirrored byte-for-byte in `cgen.rs`.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decoded bytes -> a KUPL string. Rejects a NUL (K0008: KUPL strings are
/// NUL-free — the native C runtime would truncate at it, a cross-engine
/// divergence) and non-UTF-8. Shared by base64_decode and hex_decode.
fn bytes_to_string(out: Vec<u8>) -> Result<String, String> {
    if out.contains(&0) {
        return Err("decoded bytes contain a NUL byte".into());
    }
    String::from_utf8(out).map_err(|_| "decoded bytes are not valid UTF-8".into())
}

pub fn base64_encode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn base64_decode(s: &str) -> Result<String, String> {
    let raw: Vec<u8> = s.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();
    if raw.len() % 4 != 0 {
        return Err("invalid base64: length not a multiple of 4".into());
    }
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    for chunk in raw.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 {
            return Err("invalid base64: too much padding".into());
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' {
                if i < 4 - pad {
                    return Err("invalid base64: misplaced padding".into());
                }
                0
            } else {
                b64_value(c).ok_or("invalid base64: bad character")?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16 & 0xFF) as u8);
        if pad < 2 {
            out.push((n >> 8 & 0xFF) as u8);
        }
        if pad < 1 {
            out.push((n & 0xFF) as u8);
        }
    }
    bytes_to_string(out)
}

pub fn hex_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(s.len() * 2);
    for &b in s.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("invalid hex: odd length".into());
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = nibble(pair[0]).ok_or("invalid hex: bad digit")?;
        let lo = nibble(pair[1]).ok_or("invalid hex: bad digit")?;
        out.push((hi << 4) | lo);
    }
    bytes_to_string(out)
}

/// FNV-1a, 64-bit. Non-cryptographic; stable across engines and runs.
pub fn hash_fnv(s: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // 14695981039346656037
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // 1099511628211
    }
    h as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("hello"), "aGVsbG8=");
        assert_eq!(base64_encode("Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
    }

    #[test]
    fn base64_roundtrip_and_errors() {
        for s in ["", "a", "ab", "abc", "abcd", "the quick brown fox", "π≈3.14"] {
            assert_eq!(base64_decode(&base64_encode(s)).unwrap(), s);
        }
        assert!(base64_decode("abc").is_err()); // bad length
        assert!(base64_decode("****").is_err()); // bad chars
        assert!(base64_decode("a===").is_err()); // too much padding
    }

    #[test]
    fn hex_known_and_roundtrip() {
        assert_eq!(hex_encode("AB"), "4142");
        assert_eq!(hex_encode(""), "");
        assert_eq!(hex_encode("hello"), "68656c6c6f");
        for s in ["", "a", "hello", "π"] {
            assert_eq!(hex_decode(&hex_encode(s)).unwrap(), s);
        }
        // uppercase hex decodes too
        assert_eq!(hex_decode("4142").unwrap(), "AB");
        assert_eq!(hex_decode("4A4b").unwrap(), "JK");
        assert!(hex_decode("abc").is_err()); // odd length
        assert!(hex_decode("zz").is_err()); // bad digit
    }

    #[test]
    fn fnv_is_stable() {
        // known FNV-1a 64-bit vectors (as unsigned)
        assert_eq!(hash_fnv("") as u64, 0xcbf29ce484222325);
        assert_eq!(hash_fnv("a") as u64, 0xaf63dc4c8601ec8c);
        assert_eq!(hash_fnv("foobar") as u64, 0x85944171f73967e8);
        // same input → same hash; different input → (almost surely) different
        assert_eq!(hash_fnv("kupl"), hash_fnv("kupl"));
        assert_ne!(hash_fnv("kupl"), hash_fnv("kupI"));
    }
}
