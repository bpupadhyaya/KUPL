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
    // A REAL bug found+fixed (production-hardening PR-it796, found via an
    // Explore-agent survey): padding validity was checked WITHIN each 4-byte
    // group independently, with no state carried BETWEEN groups -- so a
    // padded group followed by ANOTHER group (padded or not) was silently
    // accepted, e.g. `"YQ==YQ=="` decoded to `"aa"` instead of erroring, even
    // though RFC 4648 permits `=` only in the FINAL quantum of the whole
    // string. Confirmed live across all three engines (interp/KVM/native all
    // agreed on the SAME wrong `Ok` result -- a shared correctness bug, not a
    // cross-engine divergence). `seen_padding` tracks whether a PRIOR group
    // already used padding; any group reached after that point is rejected
    // regardless of its own content.
    let mut seen_padding = false;
    for chunk in raw.chunks(4) {
        if seen_padding {
            return Err("invalid base64: padding is only allowed in the final group".into());
        }
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 {
            return Err("invalid base64: too much padding".into());
        }
        if pad > 0 {
            seen_padding = true;
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

/// SHA-256 (FIPS 180-4), implemented from scratch since this codebase has
/// zero external dependencies (the same "no crate" constraint every other
/// primitive in this file already lives under). Relocated here from
/// `registry.rs` (its original home, where it backed ONLY that module's own
/// package-hash integrity check) so it can also be exposed as the KUPL
/// builtins `sha256`/`hmac_sha256`, mirrored byte-for-byte in `cgen.rs` like
/// every other function in this file. Correctness verified against FIPS
/// 180-4's own official test vectors (the empty string, `"abc"`, and NIST's
/// own two-block message) in this file's test module below.
fn sha256_bytes(msg: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    // Padding: append a `1` bit (0x80 byte, since messages here are always
    // byte-aligned), then zero bytes until the length is 56 mod 64, then
    // the ORIGINAL message length in bits as a big-endian 64-bit integer.
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut padded = msg.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// SHA-256 of a string's UTF-8 bytes, as a lowercase hex digest (64 chars).
/// Cryptographic (unlike `hash_fnv` above) — see this file's own KAT test
/// for FIPS 180-4 correctness verification.
pub fn sha256_hex(s: &str) -> String {
    bytes_to_hex(&sha256_bytes(s.as_bytes()))
}

/// Same as [`sha256_hex`], but over raw bytes directly -- for input that
/// isn't (and shouldn't be forced to be) valid UTF-8, e.g. `buildcache.rs`
/// hashing the running `kupl` executable's own bytes. A lossy UTF-8
/// round-trip (`String::from_utf8_lossy`) would replace invalid sequences
/// with U+FFFD, which is not injective -- two DIFFERENT binaries could
/// legally lossy-decode to the identical string and hash the same, exactly
/// the kind of collision a cache-invalidation key must not have.
pub fn sha256_hex_bytes(bytes: &[u8]) -> String {
    bytes_to_hex(&sha256_bytes(bytes))
}

/// HMAC-SHA256 (RFC 2104/4231), built entirely from [`sha256_bytes`] above —
/// no new algorithm, just the standard ipad/opad construction. Returns a
/// lowercase hex digest (64 chars).
pub fn hmac_sha256_hex(key: &str, msg: &str) -> String {
    const BLOCK_SIZE: usize = 64; // SHA-256's own block size
    let key_bytes = key.as_bytes();
    // A key longer than the block size is itself hashed down first; a
    // shorter (or equal-length) key is used as-is. Either way the result is
    // zero-padded out to exactly one block.
    let mut key_block = [0u8; BLOCK_SIZE];
    if key_bytes.len() > BLOCK_SIZE {
        key_block[..32].copy_from_slice(&sha256_bytes(key_bytes));
    } else {
        key_block[..key_bytes.len()].copy_from_slice(key_bytes);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner_input = ipad.to_vec();
    inner_input.extend_from_slice(msg.as_bytes());
    let inner_digest = sha256_bytes(&inner_input);

    let mut outer_input = opad.to_vec();
    outer_input.extend_from_slice(&inner_digest);
    bytes_to_hex(&sha256_bytes(&outer_input))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relocated from `registry.rs` (this function's original home) along
    /// with `sha256_hex` itself. Verified against FIPS 180-4's own official
    /// test vectors -- byte-for-byte against the canonical digest, not
    /// merely "produces some 64-hex-char string" -- covering both the
    /// single-block (`""`, `"abc"`, each under 56 bytes after padding) and
    /// multi-block (NIST's own 56-byte message, which pads out to exactly
    /// two 64-byte blocks) code paths, since a hand-rolled implementation's
    /// chunking/padding boundary is exactly where a subtle bug would most
    /// likely hide.
    #[test]
    fn sha256_hex_matches_official_fips_180_4_test_vectors() {
        assert_eq!(sha256_hex(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // NIST's own 56-byte two-block message.
        assert_eq!(
            sha256_hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// HMAC-SHA256 test vectors independently computed via `openssl dgst
    /// -sha256 -mac HMAC` (not transcribed from memory -- see this
    /// campaign's own established discipline of cross-checking any recalled
    /// test-vector value against a trusted, independent computation before
    /// implementing a new algorithm from scratch). Covers the RFC 2104
    /// short-key path, an empty message, and a key LONGER than SHA-256's
    /// own 64-byte block size (the "hash the key down first" branch, the
    /// one this implementation's own short-key path can't exercise).
    #[test]
    fn hmac_sha256_hex_matches_independently_computed_openssl_vectors() {
        assert_eq!(
            hmac_sha256_hex(&"\u{b}".repeat(20), "Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(hmac_sha256_hex("key", ""), "5d5d139563c95b5967b9bd9a8c9b233a9dedb45072794cd232dc1b74832607d0");
        assert_eq!(
            hmac_sha256_hex("secret", "The quick brown fox jumps over the lazy dog"),
            "54cd5b827c0ec938fa072a29b177469c843317b095591dc846767aa338bac600"
        );
        // A 100-byte key exercises the "key longer than the block size,
        // hash it down first" branch.
        assert_eq!(
            hmac_sha256_hex(&"a".repeat(100), "test message"),
            "ab45eb0153f3f1665e8308a64c0839576ab778161f7054c3779be9df5c1f6242"
        );
    }

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

    /// A REAL bug found+fixed (production-hardening PR-it796, found via an
    /// Explore-agent survey, live-confirmed before fixing): padding validity
    /// was checked WITHIN each 4-byte group independently, with no state
    /// carried between groups, so a padded group followed by ANOTHER group
    /// (padded or not) was silently accepted -- `base64_decode("YQ==YQ==")`
    /// decoded to `"aa"` instead of erroring, even though RFC 4648 permits
    /// `=` only in the FINAL quantum of the whole string. Confirmed live
    /// across all three engines before this fix (interp/KVM/native all
    /// agreed on the SAME wrong `Ok` result) -- a shared correctness bug in
    /// this module (mirrored byte-for-byte in `cgen.rs`), not a
    /// cross-engine divergence.
    #[test]
    fn base64_decode_rejects_padding_before_the_final_group() {
        assert!(base64_decode("YQ==YQ==").is_err(), "padding in a non-final group must be rejected");
        assert!(base64_decode("YQ==QQ==").is_err(), "padding in a non-final group must be rejected");
        assert!(base64_decode("YQ==YWJj").is_err(), "a group after a padded group must be rejected even with no padding of its own");
        // padding correctly placed ONLY in the final group still decodes.
        assert_eq!(base64_decode("SGVsbG8sIHdvcmxkIQ==").unwrap(), "Hello, world!");
        assert_eq!(base64_decode("YQ==").unwrap(), "a");
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
