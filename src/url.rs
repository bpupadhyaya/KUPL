//! URL percent-encoding and query strings, shared by the interpreter and the
//! KVM (zero dependencies, like `src/json.rs`). All pure and deterministic.
//!
//! `url_encode` leaves the RFC 3986 *unreserved* set (`A-Za-z0-9-_.~`) as-is and
//! percent-encodes every other byte, including space as `%20` (not `+`).
//! `url_decode` reverses `%XX` and also accepts `+` as a space; it returns
//! `Err` on a malformed `%` escape or non-UTF-8 result. Query helpers build and
//! parse `key=value&key=value` pairs, url-encoding/decoding each part.

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

pub fn url_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xF) as usize] as char);
        }
    }
    out
}

pub fn url_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err("invalid percent-encoding: truncated escape".into());
                }
                let hi = hex_val(bytes[i + 1]).ok_or("invalid percent-encoding: bad hex")?;
                let lo = hex_val(bytes[i + 2]).ok_or("invalid percent-encoding: bad hex")?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "decoded bytes are not valid UTF-8".into())
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse `a=1&b=2` into decoded `[[key, value], …]`. A bare `key` yields
/// `[key, ""]`; empty segments (from `&&` or a leading/trailing `&`) are
/// skipped. On a malformed escape the offending part decodes to its raw text.
pub fn query_parse(s: &str) -> Vec<Vec<String>> {
    let mut pairs = Vec::new();
    for seg in s.split('&') {
        if seg.is_empty() {
            continue;
        }
        let (k, v) = match seg.split_once('=') {
            Some((k, v)) => (k, v),
            None => (seg, ""),
        };
        let key = url_decode(k).unwrap_or_else(|_| k.to_string());
        let val = url_decode(v).unwrap_or_else(|_| v.to_string());
        pairs.push(vec![key, val]);
    }
    pairs
}

/// Build `a=1&b=2` from `[[key, value], …]`, url-encoding each part. Inner
/// lists shorter than 2 use `""` for the missing element.
pub fn query_build(pairs: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (i, pair) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        let empty = String::new();
        let k = pair.first().unwrap_or(&empty);
        let v = pair.get(1).unwrap_or(&empty);
        out.push_str(&url_encode(k));
        out.push('=');
        out.push_str(&url_encode(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_basics() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
        assert_eq!(url_encode("hello"), "hello");
        assert_eq!(url_encode("a-b_c.d~e"), "a-b_c.d~e"); // unreserved
        assert_eq!(url_encode("100%"), "100%25");
        assert_eq!(url_encode("π"), "%CF%80"); // UTF-8 bytes
    }

    #[test]
    fn decode_basics_and_errors() {
        assert_eq!(url_decode("a%20b%26c").unwrap(), "a b&c");
        assert_eq!(url_decode("a+b").unwrap(), "a b"); // + is space
        assert_eq!(url_decode("%CF%80").unwrap(), "π");
        assert!(url_decode("%2").is_err()); // truncated
        assert!(url_decode("%zz").is_err()); // bad hex
    }

    #[test]
    fn encode_decode_roundtrip() {
        for s in ["", "hello world", "a=b&c=d", "π≈3.14", "sp ace/slash?q#h"] {
            assert_eq!(url_decode(&url_encode(s)).unwrap(), s);
        }
    }

    #[test]
    fn query_roundtrip() {
        let q = "name=Ada%20Lovelace&role=engineer%2Blead&flag=";
        let parsed = query_parse(q);
        assert_eq!(
            parsed,
            vec![
                vec!["name".to_string(), "Ada Lovelace".to_string()],
                vec!["role".to_string(), "engineer+lead".to_string()],
                vec!["flag".to_string(), "".to_string()],
            ]
        );
        assert_eq!(query_build(&parsed), "name=Ada%20Lovelace&role=engineer%2Blead&flag=");
    }

    #[test]
    fn query_edge_cases() {
        assert_eq!(query_parse("a"), vec![vec!["a".to_string(), "".to_string()]]);
        assert_eq!(query_parse("&a=1&&b=2&"), vec![
            vec!["a".to_string(), "1".to_string()],
            vec!["b".to_string(), "2".to_string()],
        ]);
        assert_eq!(query_parse(""), Vec::<Vec<String>>::new());
    }
}
