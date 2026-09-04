//! ChaCha20-Poly1305 AEAD (RFC 8439), zero-dependency, for `distribution.rs`'s
//! `weight distributed` wire transport. Chosen over AES-GCM specifically
//! because it's the AEAD construction most commonly recommended for a
//! from-scratch software implementation: add/rotate/xor only (no S-boxes,
//! no table lookups that leak timing through cache behavior the way a naive
//! AES implementation's lookup tables do), and its authentication half
//! (Poly1305) is built here on `bigint.rs`'s already-tested arbitrary-
//! precision arithmetic rather than a hand-rolled fixed-width-limb field
//! implementation -- `distribution.rs` messages are small control-plane
//! values, not bulk data, so `BigInt`'s throughput is irrelevant, and
//! reusing an already-verified primitive is a strictly safer way to get
//! Poly1305's mod-(2^130-5) field arithmetic right than reproducing the
//! classic 26-bit/44-bit limb tricks from memory.
//!
//! Every primitive here is checked against the OFFICIAL RFC 8439 test
//! vectors in this file's own test module (the ChaCha20 block function
//! §2.3.2, Poly1305 §2.5.2, and the full AEAD construction §2.8.2) -- not
//! just internal self-consistency.

use crate::bigint::BigInt;

// ============================== ChaCha20 ==============================

const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(7);
}

/// One 64-byte ChaCha20 keystream block, RFC 8439 §2.3.
fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&CHACHA_CONST);
    for i in 0..8 {
        state[4 + i] = u32::from_le_bytes(key[i * 4..i * 4 + 4].try_into().unwrap());
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes(nonce[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let initial = state;
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }
    for i in 0..16 {
        state[i] = state[i].wrapping_add(initial[i]);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&state[i].to_le_bytes());
    }
    out
}

/// XOR `data` in place with the ChaCha20 keystream starting at
/// `initial_counter` -- encrypts or decrypts (the cipher is its own
/// inverse), RFC 8439 §2.4.
fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], initial_counter: u32, data: &mut [u8]) {
    let mut counter = initial_counter;
    for chunk in data.chunks_mut(64) {
        let block = chacha20_block(key, counter, nonce);
        for (b, k) in chunk.iter_mut().zip(block.iter()) {
            *b ^= k;
        }
        counter = counter.wrapping_add(1);
    }
}

// ============================== Poly1305 ==============================

/// RFC 8439 §2.5.1: clear specific bits of the first half of the one-time
/// key so `r` (once treated as a number) never exceeds 124 significant
/// bits -- required for the field arithmetic below to stay correct.
fn clamp_r(r: &[u8; 16]) -> [u8; 16] {
    let mut r = *r;
    r[3] &= 15;
    r[7] &= 15;
    r[11] &= 15;
    r[15] &= 15;
    r[4] &= 252;
    r[8] &= 252;
    r[12] &= 252;
    r
}

/// Little-endian bytes -> a non-negative `BigInt`, via repeated
/// multiply-by-256-and-add (Horner's method, most-significant byte first).
fn le_bytes_to_bigint(bytes: &[u8]) -> BigInt {
    let base = BigInt::from_i64(256);
    let mut acc = BigInt::zero();
    for &b in bytes.iter().rev() {
        acc = acc.mul(&base).add(&BigInt::from_i64(b as i64));
    }
    acc
}

/// The low `len` little-endian bytes of a non-negative `BigInt` -- via
/// repeated divmod-by-256, which extracts the number's low-order bytes
/// regardless of how large it is (equivalent to `v mod 256^len`, computed
/// one byte at a time rather than as a separate explicit reduction).
fn bigint_to_le_bytes(v: &BigInt, len: usize) -> Vec<u8> {
    let base = BigInt::from_i64(256);
    let mut n = v.clone();
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let (q, r) = n.divmod(&base).unwrap();
        let byte: u8 = r.to_decimal().parse().unwrap();
        out.push(byte);
        n = q;
    }
    out
}

fn poly1305_prime() -> BigInt {
    // 2^130 - 5
    BigInt::from_i64(2).pow(130).unwrap().sub(&BigInt::from_i64(5))
}

/// RFC 8439 §2.5.1: one-time authenticator over `msg`, using a 32-byte
/// one-time key (`r || s`, 16 bytes each) that MUST never be reused across
/// two different messages.
fn poly1305_mac(one_time_key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    let r_clamped = clamp_r(one_time_key[0..16].try_into().unwrap());
    let r = le_bytes_to_bigint(&r_clamped);
    let s = le_bytes_to_bigint(&one_time_key[16..32]);
    let p = poly1305_prime();

    let mut acc = BigInt::zero();
    for chunk in msg.chunks(16) {
        let mut block = chunk.to_vec();
        block.push(1); // the RFC's own "append a 1 byte" marker, at whatever length the real chunk is
        let n = le_bytes_to_bigint(&block);
        acc = acc.add(&n).mul(&r);
        acc = acc.divmod(&p).unwrap().1;
    }
    acc = acc.add(&s);
    let bytes = bigint_to_le_bytes(&acc, 16); // low 16 bytes == mod 2^128
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&bytes);
    tag
}

/// Zero-pad `x`'s own length up to the next multiple of 16 (RFC 8439's
/// `pad16` -- zero bytes if `x` is already a multiple of 16, including the
/// empty slice).
fn pad16_len(x_len: usize) -> usize {
    (16 - (x_len % 16)) % 16
}

// ================================ AEAD =================================

/// RFC 8439 §2.6: the block-0 keystream (counter=0) IS the one-time
/// Poly1305 key for this (key, nonce) pair -- its first 32 of 64 bytes.
fn poly1305_key_gen(key: &[u8; 32], nonce: &[u8; 12]) -> [u8; 32] {
    let block = chacha20_block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block[0..32]);
    otk
}

fn mac_data(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(aad.len() + ciphertext.len() + 40);
    data.extend_from_slice(aad);
    data.resize(data.len() + pad16_len(aad.len()), 0);
    data.extend_from_slice(ciphertext);
    data.resize(data.len() + pad16_len(ciphertext.len()), 0);
    data.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    data.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    data
}

/// RFC 8439 §2.8.1: encrypt `plaintext` under `key`/`nonce`, additionally
/// authenticating (but not encrypting) `aad`. Returns `ciphertext || tag`
/// (`plaintext.len() + 16` bytes) -- data-plane counter starts at 1 (block
/// 0's keystream is reserved for the Poly1305 one-time key above).
pub fn encrypt(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let otk = poly1305_key_gen(key, nonce);
    let mut ciphertext = plaintext.to_vec();
    chacha20_xor(key, nonce, 1, &mut ciphertext);
    let tag = poly1305_mac(&otk, &mac_data(aad, &ciphertext));
    ciphertext.extend_from_slice(&tag);
    ciphertext
}

/// The inverse of [`encrypt`]. Verifies the 16-byte trailing tag in
/// constant time (mirroring `distribution.rs::tokens_match`'s own
/// XOR-accumulate discipline for the SAME reason: a naive `==` on a MAC
/// leaks how many leading bytes an attacker's forged tag got right,
/// through timing) BEFORE returning any plaintext -- a failed check
/// returns `Err` and the input is never decrypted into caller-visible
/// bytes at all.
pub fn decrypt(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext_and_tag: &[u8]) -> Result<Vec<u8>, String> {
    if ciphertext_and_tag.len() < 16 {
        return Err("aead: ciphertext shorter than the 16-byte tag".to_string());
    }
    let split = ciphertext_and_tag.len() - 16;
    let ciphertext = &ciphertext_and_tag[..split];
    let received_tag = &ciphertext_and_tag[split..];

    let otk = poly1305_key_gen(key, nonce);
    let expected_tag = poly1305_mac(&otk, &mac_data(aad, ciphertext));

    let mut diff = 0u8;
    for i in 0..16 {
        diff |= expected_tag[i] ^ received_tag[i];
    }
    if diff != 0 {
        return Err("aead: authentication tag mismatch (corrupted or tampered message)".to_string());
    }

    let mut plaintext = ciphertext.to_vec();
    chacha20_xor(key, nonce, 1, &mut plaintext);
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_to_bytes(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// RFC 8439 §2.3.2's own official ChaCha20 block test vector.
    #[test]
    fn chacha20_block_matches_the_official_rfc8439_test_vector() {
        let key: [u8; 32] = hex_to_bytes(
            "00:01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13:14:15:16:17:18:19:1a:1b:1c:1d:1e:1f",
        )
        .try_into()
        .unwrap();
        let nonce: [u8; 12] = hex_to_bytes("00:00:00:09:00:00:00:4a:00:00:00:00").try_into().unwrap();
        let block = chacha20_block(&key, 1, &nonce);
        let expected = hex_to_bytes(
            "10 f1 e7 e4 d1 3b 59 15 50 0f dd 1f a3 20 71 c4 c7 d1 f4 c7 33 c0 68 03 04 22 aa 9a c3 d4 6c 4e \
             d2 82 64 46 07 9f aa 09 14 c2 d7 05 d9 8b 02 a2 b5 12 9c d1 de 16 4e b9 cb d0 83 e8 a2 50 3c 4e",
        );
        assert_eq!(block.to_vec(), expected);
    }

    /// RFC 8439 §2.5.2's own official Poly1305 test vector.
    #[test]
    fn poly1305_mac_matches_the_official_rfc8439_test_vector() {
        let key: [u8; 32] = hex_to_bytes(
            "85:d6:be:78:57:55:6d:33:7f:44:52:fe:42:d5:06:a8:01:03:80:8a:fb:0d:b2:fd:4a:bf:f6:af:41:49:f5:1b",
        )
        .try_into()
        .unwrap();
        let msg = b"Cryptographic Forum Research Group";
        let tag = poly1305_mac(&key, msg);
        let expected = hex_to_bytes("a8:06:1d:c1:30:51:36:c6:c2:2b:8b:af:0c:01:27:a9");
        assert_eq!(tag.to_vec(), expected);
    }

    /// RFC 8439 §2.8.2's own official AEAD test vector -- encrypt, verify
    /// against the spec's own ciphertext AND tag, then confirm `decrypt`
    /// round-trips it back to the original plaintext.
    #[test]
    fn aead_encrypt_matches_the_official_rfc8439_test_vector_and_decrypt_round_trips() {
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let aad = hex_to_bytes("50:51:52:53:c0:c1:c2:c3:c4:c5:c6:c7");
        let key: [u8; 32] = hex_to_bytes(
            "80:81:82:83:84:85:86:87:88:89:8a:8b:8c:8d:8e:8f:90:91:92:93:94:95:96:97:98:99:9a:9b:9c:9d:9e:9f",
        )
        .try_into()
        .unwrap();
        let nonce: [u8; 12] = hex_to_bytes("07:00:00:00:40:41:42:43:44:45:46:47").try_into().unwrap();

        let out = encrypt(&key, &nonce, &aad, plaintext);
        let (ciphertext, tag) = out.split_at(out.len() - 16);

        let expected_ciphertext = hex_to_bytes(
            "d3 1a 8d 34 64 8e 60 db 7b 86 af bc 53 ef 7e c2 a4 ad ed 51 29 6e 08 fe a9 e2 b5 a7 36 ee 62 d6 \
             3d be a4 5e 8c a9 67 12 82 fa fb 69 da 92 72 8b 1a 71 de 0a 9e 06 0b 29 05 d6 a5 b6 7e cd 3b 36 \
             92 dd bd 7f 2d 77 8b 8c 98 03 ae e3 28 09 1b 58 fa b3 24 e4 fa d6 75 94 55 85 80 8b 48 31 d7 bc \
             3f f4 de f0 8e 4b 7a 9d e5 76 d2 65 86 ce c6 4b 61 16",
        );
        let expected_tag = hex_to_bytes("1a:e1:0b:59:4f:09:e2:6a:7e:90:2e:cb:d0:60:06:91");

        assert_eq!(ciphertext, expected_ciphertext.as_slice());
        assert_eq!(tag, expected_tag.as_slice());

        let decrypted = decrypt(&key, &nonce, &aad, &out).expect("must decrypt cleanly");
        assert_eq!(decrypted, plaintext);
    }

    /// A single flipped ciphertext bit must be rejected, not silently
    /// decrypted into corrupted plaintext -- the whole POINT of the tag.
    #[test]
    fn a_tampered_ciphertext_byte_is_rejected_not_silently_decrypted() {
        let key = [7u8; 32];
        let nonce = [1u8; 12];
        let mut out = encrypt(&key, &nonce, b"aad", b"secret payload");
        let last = out.len() - 1;
        out[0] ^= 0x01; // flip a bit in the ciphertext body, not the tag
        let _ = out[last]; // (tag untouched -- confirms the check catches a CIPHERTEXT tamper, not just a tag tamper)
        assert!(decrypt(&key, &nonce, b"aad", &out).is_err());
    }

    /// A tampered AAD byte (with the ciphertext+tag otherwise untouched)
    /// must also be rejected -- confirms AAD is genuinely authenticated,
    /// not just carried alongside.
    #[test]
    fn tampered_aad_is_rejected() {
        let key = [3u8; 32];
        let nonce = [9u8; 12];
        let out = encrypt(&key, &nonce, b"original-aad", b"payload");
        assert!(decrypt(&key, &nonce, b"different-aad", &out).is_err());
    }

    /// A round trip through every empty/tiny/multi-block edge case, since
    /// the RFC vector above only exercises one message length.
    #[test]
    fn round_trips_across_empty_and_multi_block_message_lengths() {
        let key = [42u8; 32];
        let nonce = [5u8; 12];
        for len in [0usize, 1, 15, 16, 17, 63, 64, 65, 200] {
            let plaintext: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let out = encrypt(&key, &nonce, b"aad", &plaintext);
            assert_eq!(out.len(), plaintext.len() + 16);
            let decrypted = decrypt(&key, &nonce, b"aad", &out).unwrap();
            assert_eq!(decrypted, plaintext, "length {len} failed to round-trip");
        }
    }

    /// The empty-AAD case specifically (a distinct code path through
    /// `pad16_len`/`mac_data`, worth its own explicit test).
    #[test]
    fn empty_aad_round_trips() {
        let key = [1u8; 32];
        let nonce = [2u8; 12];
        let out = encrypt(&key, &nonce, b"", b"no aad here");
        assert_eq!(decrypt(&key, &nonce, b"", &out).unwrap(), b"no aad here");
    }

    /// Two different nonces under the SAME key must produce different
    /// ciphertexts for the SAME plaintext -- a sanity check against a
    /// keystream-reuse bug (the single most catastrophic failure mode for
    /// any stream cipher).
    #[test]
    fn different_nonces_under_the_same_key_never_reuse_keystream() {
        let key = [9u8; 32];
        let out_a = encrypt(&key, &[1u8; 12], b"", b"same plaintext, same key");
        let out_b = encrypt(&key, &[2u8; 12], b"", b"same plaintext, same key");
        assert_ne!(out_a, out_b);
    }
}
