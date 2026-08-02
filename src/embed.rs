//! From-scratch, zero-dependency text embeddings + cosine similarity for
//! KUPL's AI-native stdlib -- closes the "embeddings + similarity as
//! stdlib" half of `docs/GAPS.md` Tier 1.5's prompt-context-builders gap
//! (the other half, `kupl context --json`, is a CLI change in run.rs, not
//! a language builtin).
//!
//! This is NOT a neural embedding -- there is no model, no weights, no
//! network call, nothing that would conflict with this codebase's own
//! zero-external-dependency principle. It is a classic **feature-hashing
//! bag-of-words vectorizer** (the "hashing trick": Weinberger et al. 2009,
//! the same technique behind Vowpal Wabbit / scikit-learn's
//! `HashingVectorizer`): tokenize into lowercased words, hash each word
//! into one of `dims` buckets via the SAME [`hash_fnv`] this codebase
//! already exposes as the `hash_fnv` builtin, accumulate raw term-
//! frequency counts, then L2-normalize so `cosine_similarity` is
//! meaningful across texts of different lengths. Deterministic and
//! byte-identical across every engine by construction -- reusing an
//! already-shared, already-mirrored hash function is what makes this
//! trivially portable to `cgen.rs`'s C runtime, unlike a real embedding
//! model would be.

use crate::encoding::hash_fnv;

/// Matches `interp::MAX_TENSOR_LEN`'s own cap -- a `text_embed` result is a
/// `List[Float]`, the same order of magnitude of "how big a numeric vector
/// is it reasonable to build in one call" as a `Tensor`.
pub const MAX_EMBED_DIMS: i64 = 100_000_000;

/// Hash-embed `s` into a `dims`-dimensional, L2-normalized vector.
///
/// Tokenization is deliberately ASCII-only (word = a maximal run of ASCII
/// `[0-9A-Za-z]`, lowercased via `[A-Z]` -> `[a-z]`; any other byte,
/// including every non-ASCII UTF-8 byte, is a separator) -- matching this
/// codebase's OWN existing, documented native-backend convention for text
/// processing (`kupl native`'s `to_upper`/`to_lower` and regex character
/// classes are both ASCII-oriented already, see `STDLIB.md`), rather than
/// introducing a NEW, wider Unicode-vs-ASCII divergence class between
/// interp/vm (which could easily support full Unicode casing/word
/// boundaries, like `Str.to_lower()` does there) and native (which would
/// need a from-scratch Unicode case-folding table to match). Operating
/// byte-wise like this also means native's own tokenizer needs no UTF-8
/// decoding at all -- non-ASCII bytes (0x80..=0xFF, every UTF-8
/// continuation AND lead byte) are simply >= 0x80, trivially distinguished
/// from ASCII without decoding a single codepoint.
pub fn text_embed(s: &str, dims: i64) -> Result<Vec<f64>, String> {
    if dims <= 0 {
        return Err("text_embed needs a positive dims".into());
    }
    if dims > MAX_EMBED_DIMS {
        return Err("text_embed dims too large".into());
    }
    let dims = dims as usize;
    let mut vec = vec![0.0f64; dims];
    let mut word = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            word.push(b.to_ascii_lowercase() as char);
        } else if !word.is_empty() {
            bump_bucket(&mut vec, &word, dims);
            word.clear();
        }
    }
    if !word.is_empty() {
        bump_bucket(&mut vec, &word, dims);
    }
    let norm: f64 = vec.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
    Ok(vec)
}

/// `h as u64` on a negative `i64` wraps to its two's-complement bit
/// pattern (matching a plain `(uint64_t)` cast in C exactly), giving a
/// uniformly-distributed, byte-identical bucket index on every engine --
/// deliberately NOT `h.unsigned_abs()`, which would collapse `h` and
/// `-h` to the SAME bucket, measurably skewing the hash's distribution.
fn bump_bucket(vec: &mut [f64], word: &str, dims: usize) {
    let h = hash_fnv(word);
    let bucket = (h as u64 as usize) % dims;
    vec[bucket] += 1.0;
}

/// Cosine similarity of two equal-length vectors, in `[-1.0, 1.0]`
/// (`[0.0, 1.0]` for the non-negative vectors `text_embed` produces).
/// `Err` on a length mismatch (mirrors `Tensor`'s own "length mismatch"
/// convention); `0.0` for a zero vector rather than `NaN` (an empty or
/// all-non-word-character `text_embed` input is a real, reachable case,
/// not an error condition worth panicking over).
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> Result<f64, String> {
    if a.len() != b.len() {
        return Err(format!(
            "cosine_similarity needs two vectors of the same length, found {} and {}",
            a.len(),
            b.len()
        ));
    }
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return Ok(0.0);
    }
    Ok(dot / (na * nb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_is_deterministic_and_normalized() {
        let a = text_embed("the quick brown fox", 64).unwrap();
        let b = text_embed("the quick brown fox", 64).unwrap();
        assert_eq!(a, b, "same input, same dims -> identical vector");
        let norm: f64 = a.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-9, "must be L2-normalized: {norm}");
    }

    #[test]
    fn embed_rejects_non_positive_dims() {
        assert!(text_embed("hi", 0).is_err());
        assert!(text_embed("hi", -1).is_err());
        assert!(text_embed("hi", MAX_EMBED_DIMS + 1).is_err());
    }

    #[test]
    fn embed_of_empty_or_wordless_text_is_the_zero_vector() {
        assert_eq!(text_embed("", 8).unwrap(), vec![0.0; 8]);
        assert_eq!(text_embed("!!! ... ---", 8).unwrap(), vec![0.0; 8]);
    }

    #[test]
    fn embed_is_case_insensitive() {
        assert_eq!(text_embed("Hello World", 64).unwrap(), text_embed("hello world", 64).unwrap());
    }

    #[test]
    fn similarity_of_identical_text_is_one() {
        let v = text_embed("similar texts share words", 128).unwrap();
        let sim = cosine_similarity(&v, &v).unwrap();
        assert!((sim - 1.0).abs() < 1e-9, "{sim}");
    }

    #[test]
    fn similarity_ranks_related_text_higher_than_unrelated() {
        let a = text_embed("the cat sat on the mat", 256).unwrap();
        let b = text_embed("a cat sat on a mat today", 256).unwrap();
        let c = text_embed("quantum physics and general relativity", 256).unwrap();
        let sim_ab = cosine_similarity(&a, &b).unwrap();
        let sim_ac = cosine_similarity(&a, &c).unwrap();
        assert!(sim_ab > sim_ac, "related text ({sim_ab}) should score higher than unrelated ({sim_ac})");
    }

    #[test]
    fn similarity_of_zero_vector_is_zero_not_nan() {
        let zero = vec![0.0; 8];
        let v = text_embed("hello", 8).unwrap();
        let sim = cosine_similarity(&zero, &v).unwrap();
        assert_eq!(sim, 0.0);
        assert!(!sim.is_nan());
    }

    #[test]
    fn similarity_rejects_mismatched_lengths() {
        assert!(cosine_similarity(&[1.0, 2.0], &[1.0, 2.0, 3.0]).is_err());
    }
}
