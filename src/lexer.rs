//! The KUPL lexer: UTF-8 source -> token stream.
//!
//! Newlines are significant (statement terminators) except:
//!   - inside `(` … `)` and `[` … `]`
//!   - after a token that implies continuation (operator, comma, dot, open bracket)
//! Comments: `//` line comments and `/* … */` block comments (nesting allowed).

use crate::diag::{Diag, Span};
use crate::token::{keyword, StrPart, Tok, Token};

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Depth of `(`/`[` nesting; newlines are suppressed while > 0.
    paren_depth: usize,
    tokens: Vec<Token>,
    pub diags: Vec<Diag>,
}

pub fn lex(src: &str) -> (Vec<Token>, Vec<Diag>) {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        paren_depth: 0,
        tokens: Vec::new(),
        diags: Vec::new(),
    };
    lx.run();
    (lx.tokens, lx.diags)
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }
    fn span_from(&self, start: usize) -> Span {
        Span::new(start as u32, self.pos as u32)
    }
    fn push(&mut self, tok: Tok, start: usize) {
        let span = self.span_from(start);
        self.tokens.push(Token { tok, span });
    }
    fn last_suppresses_newline(&self) -> bool {
        self.tokens
            .last()
            .map(|t| t.tok.suppresses_newline())
            .unwrap_or(true)
    }

    fn run(&mut self) {
        while let Some(b) = self.peek() {
            let start = self.pos;
            match b {
                b' ' | b'\t' | b'\r' => {
                    self.bump();
                }
                b'\n' => {
                    self.bump();
                    if self.paren_depth == 0 && !self.last_suppresses_newline() {
                        self.push(Tok::Newline, start);
                    }
                }
                b'/' if self.peek2() == Some(b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                b'/' if self.peek2() == Some(b'*') => {
                    self.bump();
                    self.bump();
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.bump() {
                            None => {
                                self.diags.push(Diag::error(
                                    "K0002",
                                    "unterminated block comment",
                                    self.span_from(start),
                                ));
                                break;
                            }
                            Some(b'/') if self.peek() == Some(b'*') => {
                                self.bump();
                                depth += 1;
                            }
                            Some(b'*') if self.peek() == Some(b'/') => {
                                self.bump();
                                depth -= 1;
                            }
                            _ => {}
                        }
                    }
                }
                b'"' => self.lex_string(),
                b'0'..=b'9' => self.lex_number(),
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.lex_ident(),
                _ if b >= 0x80 => self.lex_ident(), // permit non-ASCII identifiers
                _ => self.lex_operator(),
            }
        }
        // Terminate a trailing unterminated statement.
        if !self.last_suppresses_newline() {
            let p = self.pos;
            self.push(Tok::Newline, p);
        }
        let p = self.pos;
        self.push(Tok::Eof, p);
    }

    fn lex_ident(&mut self) {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
                self.bump();
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        match keyword(text) {
            Some(kw) => self.push(kw, start),
            None => self.push(Tok::Ident(text.to_string()), start),
        }
    }

    fn lex_number(&mut self) {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9') | Some(b'_')) {
            self.bump();
        }
        // A float only if `.` is followed by a digit (so `1..5` stays a range).
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek2(), Some(b'0'..=b'9')) {
            is_float = true;
            self.bump();
            while matches!(self.peek(), Some(b'0'..=b'9') | Some(b'_')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.bump();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.bump();
            }
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                is_float = true;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.bump();
                }
            } else {
                self.pos = save; // not an exponent; `1e` -> `1` then ident `e`
            }
        }
        let text: String = self.src[start..self.pos].replace('_', "");
        if is_float {
            match text.parse::<f64>() {
                Ok(v) => self.push(Tok::Float(v), start),
                Err(_) => self.diags.push(Diag::error(
                    "K0003",
                    format!("invalid float literal `{text}`"),
                    self.span_from(start),
                )),
            }
        } else {
            match text.parse::<i64>() {
                Ok(v) => self.push(Tok::Int(v), start),
                Err(_) => self.diags.push(Diag::error(
                    "K0004",
                    format!("integer literal `{text}` does not fit in Int (64-bit)"),
                    self.span_from(start),
                )),
            }
        }
    }

    fn lex_string(&mut self) {
        let start = self.pos;
        self.bump(); // opening quote
        let mut parts: Vec<StrPart> = Vec::new();
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some(b'\n') => {
                    self.diags.push(Diag::error(
                        "K0005",
                        "unterminated string literal",
                        self.span_from(start),
                    ));
                    break;
                }
                Some(b'"') => break,
                Some(b'\\') => match self.bump() {
                    Some(b'n') => text.push('\n'),
                    Some(b't') => text.push('\t'),
                    Some(b'r') => text.push('\r'),
                    Some(b'\\') => text.push('\\'),
                    Some(b'"') => text.push('"'),
                    Some(b'{') => text.push('{'),
                    Some(b'}') => text.push('}'),
                    Some(b'0') => text.push('\0'),
                    other => {
                        self.diags.push(Diag::error(
                            "K0006",
                            format!(
                                "unknown escape sequence `\\{}`",
                                other.map(|c| c as char).unwrap_or(' ')
                            ),
                            self.span_from(self.pos.saturating_sub(2)),
                        ));
                    }
                },
                Some(b'{') => {
                    // interpolation: capture raw expression source until matching `}`
                    if !text.is_empty() {
                        parts.push(StrPart::Text(std::mem::take(&mut text)));
                    }
                    let expr_start = self.pos;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.bump() {
                            None | Some(b'\n') => {
                                self.diags.push(Diag::error(
                                    "K0007",
                                    "unterminated `{` interpolation in string",
                                    self.span_from(expr_start),
                                ));
                                depth = 0;
                            }
                            Some(b'{') => depth += 1,
                            Some(b'}') => depth -= 1,
                            // nested string literal: skip it whole, so quotes
                            // and braces inside it don't confuse the scan —
                            // `"{xs.join(", ")}"` works without escaping
                            Some(b'"') => loop {
                                match self.bump() {
                                    None | Some(b'\n') => {
                                        self.diags.push(Diag::error(
                                            "K0007",
                                            "unterminated `{` interpolation in string",
                                            self.span_from(expr_start),
                                        ));
                                        depth = 0;
                                        break;
                                    }
                                    Some(b'\\') => {
                                        self.bump();
                                    }
                                    Some(b'"') => break,
                                    _ => {}
                                }
                            },
                            _ => {}
                        }
                    }
                    let end = self.pos.saturating_sub(1);
                    let raw = self.src[expr_start..end].to_string();
                    parts.push(StrPart::Expr(raw, expr_start as u32));
                }
                Some(b) => {
                    // Copy UTF-8 continuation bytes verbatim.
                    text.push(b as char);
                    if b >= 0x80 {
                        // re-do properly: back up and take the full char
                        text.pop();
                        let ch_start = self.pos - 1;
                        let ch = self.src[ch_start..].chars().next().unwrap_or('\u{fffd}');
                        text.push(ch);
                        self.pos = ch_start + ch.len_utf8();
                    }
                }
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(StrPart::Text(text));
        }
        self.push(Tok::Str(parts), start);
    }

    fn lex_operator(&mut self) {
        let start = self.pos;
        let b = self.bump().unwrap();
        let two = |lx: &mut Self, tok: Tok| {
            lx.bump();
            tok
        };
        let tok = match b {
            b'+' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::PlusEq)
                } else {
                    Tok::Plus
                }
            }
            b'-' => match self.peek() {
                Some(b'=') => two(self, Tok::MinusEq),
                Some(b'>') => two(self, Tok::Arrow),
                _ => Tok::Minus,
            },
            b'*' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::StarEq)
                } else {
                    Tok::Star
                }
            }
            b'/' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::SlashEq)
                } else {
                    Tok::Slash
                }
            }
            b'%' => Tok::Percent,
            b'=' => match self.peek() {
                Some(b'=') => two(self, Tok::EqEq),
                Some(b'>') => two(self, Tok::FatArrow),
                _ => Tok::Eq,
            },
            b'!' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::NotEq)
                } else {
                    Tok::Bang
                }
            }
            b'<' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::LtEq)
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                if self.peek() == Some(b'=') {
                    two(self, Tok::GtEq)
                } else {
                    Tok::Gt
                }
            }
            b'&' => {
                if self.peek() == Some(b'&') {
                    two(self, Tok::AndAnd)
                } else {
                    self.diags.push(Diag::error(
                        "K0008",
                        "single `&` is not an operator (did you mean `&&`?)",
                        self.span_from(start),
                    ));
                    return;
                }
            }
            b'|' => match self.peek() {
                Some(b'|') => two(self, Tok::OrOr),
                Some(b'>') => two(self, Tok::PipeGt),
                _ => Tok::Pipe,
            },
            b'.' => {
                if self.peek() == Some(b'.') {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        two(self, Tok::DotDotEq)
                    } else {
                        Tok::DotDot
                    }
                } else {
                    Tok::Dot
                }
            }
            b'?' => Tok::Question,
            b',' => Tok::Comma,
            b':' => Tok::Colon,
            b'(' => {
                self.paren_depth += 1;
                Tok::LParen
            }
            b')' => {
                self.paren_depth = self.paren_depth.saturating_sub(1);
                Tok::RParen
            }
            b'[' => {
                self.paren_depth += 1;
                Tok::LBracket
            }
            b']' => {
                self.paren_depth = self.paren_depth.saturating_sub(1);
                Tok::RBracket
            }
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            other => {
                self.diags.push(Diag::error(
                    "K0001",
                    format!("unexpected character `{}`", other as char),
                    self.span_from(start),
                ));
                return;
            }
        };
        self.push(tok, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        let (toks, diags) = lex(src);
        assert!(diags.is_empty(), "unexpected diags: {diags:?}");
        toks.into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn basic_tokens() {
        let ks = kinds("let x = 41 + 1");
        assert_eq!(
            ks,
            vec![
                Tok::KwLet,
                Tok::Ident("x".into()),
                Tok::Eq,
                Tok::Int(41),
                Tok::Plus,
                Tok::Int(1),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn range_vs_float() {
        let ks = kinds("0..10 1.5");
        assert_eq!(
            ks,
            vec![
                Tok::Int(0),
                Tok::DotDot,
                Tok::Int(10),
                Tok::Float(1.5),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn newline_suppression_in_parens() {
        let ks = kinds("f(1,\n2)");
        assert!(!ks.contains(&Tok::Newline) || ks.iter().filter(|t| **t == Tok::Newline).count() == 1);
    }

    #[test]
    fn string_interpolation() {
        let (toks, diags) = lex("\"hi {name}!\"");
        assert!(diags.is_empty());
        match &toks[0].tok {
            Tok::Str(parts) => {
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0], StrPart::Text("hi ".into()));
                assert!(matches!(&parts[1], StrPart::Expr(s, _) if s == "name"));
                assert_eq!(parts[2], StrPart::Text("!".into()));
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn interpolation_with_nested_string() {
        // quotes inside an interpolation are a nested literal, no escaping:
        let (toks, diags) = lex("\"keywords: {ks.join(\", \")}\"");
        assert!(diags.is_empty(), "{diags:?}");
        match &toks[0].tok {
            Tok::Str(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[1], StrPart::Expr(s, _) if s == "ks.join(\", \")"));
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn comments_ignored() {
        let ks = kinds("1 // hello\n/* block /* nested */ */ 2");
        assert_eq!(
            ks,
            vec![Tok::Int(1), Tok::Newline, Tok::Int(2), Tok::Newline, Tok::Eof]
        );
    }
}
