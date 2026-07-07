//! JSON parsing and serialization, shared by the interpreter and the KVM.
//!
//! Values map onto the built-in `Json` ADT (registered via the prelude):
//!   JNull | JBool(b) | JNum(n) | JStr(s) | JArr(items) | JObj(fields)
//! `json_parse` builds these `Value::Ctor`s directly (by name), and
//! `json_stringify` walks them back to text. Object key order is preserved
//! (parse order in, insertion order out), so round-trips are deterministic and
//! byte-identical across engines.

use crate::value::Value;
use std::rc::Rc;

fn ctor(variant: &str, fields: Vec<Value>) -> Value {
    Value::Ctor {
        ty: Rc::new("Json".into()),
        variant: Rc::new(variant.into()),
        fields: Rc::new(fields),
    }
}

/// Parse a JSON document into a `Json` value. Trailing non-whitespace is an error.
/// Maximum JSON nesting depth. Untrusted input must not be able to drive the
/// recursive-descent parser (or the recursive serializer/Display) past this —
/// otherwise deeply-nested input overflows the stack (a segfault on the native
/// C backend, where the stack is small). Real JSON nests only a few dozen deep;
/// the native backend enforces the same limit so all engines agree.
pub const MAX_JSON_DEPTH: usize = 500;

pub fn parse(input: &str) -> Result<Value, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut p = Parser { chars: &chars, pos: 0, depth: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!("unexpected trailing characters at position {}", p.pos));
    }
    Ok(v)
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Read exactly four hex digits (the body of a `\uXXXX` escape) into a code unit.
    fn hex4(&mut self) -> Result<u32, String> {
        let mut code = 0u32;
        for _ in 0..4 {
            let d = self.bump().ok_or("truncated \\u escape")?;
            code = code * 16 + d.to_digit(16).ok_or("invalid \\u escape")?;
        }
        Ok(code)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') | Some('[') => {
                // Bound nesting so untrusted deep input can't overflow the stack.
                self.depth += 1;
                if self.depth > MAX_JSON_DEPTH {
                    return Err("JSON nested too deeply".into());
                }
                let v = if self.peek() == Some('{') { self.object() } else { self.array() };
                self.depth -= 1;
                v
            }
            Some('"') => Ok(ctor("JStr", vec![Value::str(self.string()?)])),
            Some('t') | Some('f') => self.boolean(),
            Some('n') => self.null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!("unexpected character `{c}` at position {}", self.pos)),
            None => Err("unexpected end of input".into()),
        }
    }

    fn lit(&mut self, word: &str) -> Result<(), String> {
        for want in word.chars() {
            if self.bump() != Some(want) {
                return Err(format!("invalid literal (expected `{word}`)"));
            }
        }
        Ok(())
    }

    fn null(&mut self) -> Result<Value, String> {
        self.lit("null")?;
        Ok(ctor("JNull", vec![]))
    }

    fn boolean(&mut self) -> Result<Value, String> {
        if self.peek() == Some('t') {
            self.lit("true")?;
            Ok(ctor("JBool", vec![Value::Bool(true)]))
        } else {
            self.lit("false")?;
            Ok(ctor("JBool", vec![Value::Bool(false)]))
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        {
            self.pos += 1;
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(|n| ctor("JNum", vec![Value::Float(n)]))
            .map_err(|_| format!("invalid number `{s}`"))
    }

    fn string(&mut self) -> Result<String, String> {
        // assumes current char is the opening quote
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".into()),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\u{0008}'),
                    Some('f') => out.push('\u{000C}'),
                    Some('u') => {
                        let hi = self.hex4()?;
                        // A `\uXXXX` high surrogate (D800..=DBFF) must be paired with a
                        // following `\uYYYY` low surrogate (DC00..=DFFF) to form one astral
                        // code point (e.g. an emoji). An unpaired surrogate -> U+FFFD.
                        let cp = if (0xD800..=0xDBFF).contains(&hi) {
                            if self.chars.get(self.pos) == Some(&'\\')
                                && self.chars.get(self.pos + 1) == Some(&'u')
                            {
                                let save = self.pos;
                                self.pos += 2; // consume the `\u` of the candidate low half
                                let lo = self.hex4()?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    self.pos = save; // not a low surrogate — re-parse it
                                    0xFFFD
                                }
                            } else {
                                0xFFFD
                            }
                        } else {
                            hi
                        };
                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                    }
                    _ => return Err("invalid escape".into()),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(ctor("JArr", vec![Value::List(Rc::new(items))]));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                _ => return Err("expected `,` or `]` in array".into()),
            }
        }
        Ok(ctor("JArr", vec![Value::List(Rc::new(items))]))
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1; // consume '{'
        let mut pairs: Vec<(Value, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(ctor("JObj", vec![Value::Map(Rc::new(pairs))]));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err("expected string key in object".into());
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bump() != Some(':') {
                return Err("expected `:` in object".into());
            }
            let val = self.value()?;
            // last key wins, preserving first-seen position (Map semantics)
            let kv = Value::str(key);
            match pairs.iter_mut().find(|(k, _)| *k == kv) {
                Some(slot) => slot.1 = val,
                None => pairs.push((kv, val)),
            }
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                _ => return Err("expected `,` or `}` in object".into()),
            }
        }
        Ok(ctor("JObj", vec![Value::Map(Rc::new(pairs))]))
    }
}

/// Serialize a `Json` value to compact text. A non-`Json` value is an error
/// (the checker prevents this, so it only guards internal misuse).
pub fn stringify(v: &Value) -> Result<String, String> {
    let mut out = String::new();
    write_value(v, &mut out)?;
    Ok(out)
}

fn write_value(v: &Value, out: &mut String) -> Result<(), String> {
    let Value::Ctor { variant, fields, .. } = v else {
        return Err(format!("json_stringify needs a Json value, found {}", v.type_name()));
    };
    match variant.as_str() {
        "JNull" => out.push_str("null"),
        "JBool" => match fields.first() {
            Some(Value::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
            _ => return Err("malformed JBool".into()),
        },
        "JNum" => match fields.first() {
            Some(Value::Float(n)) => out.push_str(&format_num(*n)),
            _ => return Err("malformed JNum".into()),
        },
        "JStr" => match fields.first() {
            Some(Value::Str(s)) => write_string(s, out),
            _ => return Err("malformed JStr".into()),
        },
        "JArr" => match fields.first() {
            Some(Value::List(items)) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_value(item, out)?;
                }
                out.push(']');
            }
            _ => return Err("malformed JArr".into()),
        },
        "JObj" => match fields.first() {
            Some(Value::Map(pairs)) => {
                out.push('{');
                for (i, (k, val)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match k {
                        Value::Str(s) => write_string(s, out),
                        other => return Err(format!("JObj key must be Str, found {}", other.type_name())),
                    }
                    out.push(':');
                    write_value(val, out)?;
                }
                out.push('}');
            }
            _ => return Err("malformed JObj".into()),
        },
        other => return Err(format!("`{other}` is not a Json constructor")),
    }
    Ok(())
}

/// Whole floats render without a decimal point (`1`, not `1.0`) so parsed
/// integers round-trip; everything else uses the shortest round-trip form.
fn format_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &str) -> String {
        stringify(&parse(s).expect("parse")).expect("stringify")
    }

    #[test]
    fn scalars_and_whitespace() {
        assert_eq!(roundtrip("  null  "), "null");
        assert_eq!(roundtrip("true"), "true");
        assert_eq!(roundtrip("-12"), "-12");
        assert_eq!(roundtrip("3.5"), "3.5");
        assert_eq!(roundtrip("\"hi\""), "\"hi\"");
    }

    #[test]
    fn nested_structures_preserve_order() {
        assert_eq!(
            roundtrip("{ \"b\": 1, \"a\": [true, null, {\"x\": 2}] }"),
            "{\"b\":1,\"a\":[true,null,{\"x\":2}]}"
        );
    }

    #[test]
    fn string_escapes() {
        assert_eq!(roundtrip("\"a\\nb\\t\\\"c\\\"\""), "\"a\\nb\\t\\\"c\\\"\"");
        // \u escape decodes then re-encodes as the literal character
        assert_eq!(roundtrip("\"\\u0041\""), "\"A\"");
    }

    #[test]
    fn integral_floats_have_no_decimal() {
        assert_eq!(roundtrip("[1, 2.0, 2.5]"), "[1,2,2.5]");
    }

    #[test]
    fn errors_are_reported() {
        assert!(parse("{bad}").is_err());
        assert!(parse("[1, 2").is_err());
        assert!(parse("").is_err());
        assert!(parse("nul").is_err());
        assert!(parse("[1] extra").is_err());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // At the limit still parses; one deeper is a clean error (not a stack
        // overflow). Guards untrusted deeply-nested input across all engines.
        let ok = "[".repeat(MAX_JSON_DEPTH) + &"]".repeat(MAX_JSON_DEPTH);
        assert!(parse(&ok).is_ok());
        let deep = "[".repeat(MAX_JSON_DEPTH + 1) + &"]".repeat(MAX_JSON_DEPTH + 1);
        assert_eq!(parse(&deep).unwrap_err(), "JSON nested too deeply");
    }

    #[test]
    fn large_array_parses() {
        // No fixed element cap (the native parser used to cap at 4096).
        let big = "[".to_string() + &"1,".repeat(9999) + "1]";
        assert!(parse(&big).is_ok());
    }
}
