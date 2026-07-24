//! The KUPL lexer: UTF-8 source -> token stream.
//!
//! Newlines are significant (statement terminators) except:
//!   - inside `(` … `)` and `[` … `]`
//!   - after a token that implies continuation (operator, comma, dot, open bracket)
//! Comments: `//` line comments and `/* … */` block comments (nesting allowed).

use crate::diag::{Diag, Span};
use crate::token::{keyword, StrPart, Tok, Token};

/// A numeric type suffix on a literal: an integer width, `f32`, or none.
enum NumSuffix {
    Int(crate::value::IntW),
    F32,
    None,
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Depth of `(`/`[` nesting; newlines are suppressed while > 0.
    paren_depth: usize,
    /// Saved paren depths, pushed on `{` and popped on `}`. A brace-delimited
    /// block (closure body, `match`/`if` block, statement block) starts a fresh
    /// statement context where newlines are significant again, even when the
    /// block sits inside an open `(`/`[` — e.g. `xs.fold(seed, fn a { match a {
    /// ...newline-separated arms... } })`. Without this a `match` with
    /// newline-separated arms inside a call-argument closure fails to parse.
    paren_stack: Vec<usize>,
    tokens: Vec<Token>,
    pub diags: Vec<Diag>,
}

pub fn lex(src: &str) -> (Vec<Token>, Vec<Diag>) {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        paren_depth: 0,
        paren_stack: Vec::new(),
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
        // Hex (`0xFF`) and binary (`0b1010`) literals, with `_` separators.
        // Parsed as u64 then reinterpreted as i64, so full 64-bit bit patterns
        // are writable (`0xFFFFFFFFFFFFFFFF` == -1).
        if self.peek() == Some(b'0') && matches!(self.peek2(), Some(b'x') | Some(b'X') | Some(b'b') | Some(b'B')) {
            self.bump(); // '0'
            let radix: u32 = if matches!(self.peek(), Some(b'b') | Some(b'B')) { 2 } else { 16 };
            self.bump(); // 'x' / 'b'
            let digits_start = self.pos;
            while matches!(self.peek(), Some(c) if c == b'_'
                || (radix == 16 && c.is_ascii_hexdigit())
                || (radix == 2 && matches!(c, b'0' | b'1')))
            {
                self.bump();
            }
            // A hex digit run is a maximal munch, and `f`/`F` are themselves valid
            // hex digits -- `f32` is the ONLY numeric suffix this collides with
            // (every int-width suffix starts with `i`/`u`, neither a hex digit, so
            // those already fall out of the loop on their own). Back off a genuine
            // trailing `f32` so `peek_num_suffix` below can recognize it, matching
            // the decimal path's own long-standing `10f32` support -- but only when
            // a real (non-empty, underscore-stripped) hex digit body remains before
            // it, so a bare `0xf32` still lexes as the plain hex integer 0xf32
            // rather than backing into an "empty hex literal" error.
            if radix == 16
                && self.pos - digits_start >= 3
                && &self.src[self.pos - 3..self.pos] == "f32"
            {
                let head: String = self.src[digits_start..self.pos - 3].replace('_', "");
                if !head.is_empty() {
                    self.pos -= 3;
                }
            }
            let text: String = self.src[digits_start..self.pos].replace('_', "");
            if text.is_empty() {
                self.diags.push(Diag::error(
                    "K0004",
                    format!("empty {} literal", if radix == 16 { "hex" } else { "binary" }),
                    self.span_from(start),
                ));
                return;
            }
            let radixed = u64::from_str_radix(&text, radix);
            let fit_err = |lx: &mut Self, text: &str, radix: u32| {
                let kind = if radix == 16 { "hex" } else { "binary" };
                let prefix = if radix == 16 { "0x" } else { "0b" };
                // A hex/binary literal must fit 64 bits (they map to Int, and `0xFFFF..`
                // patterns are common). For a larger value, point at big(...) -- and show
                // the exact decimal for it when the value still fits in i128.
                let hint = match i128::from_str_radix(text, radix) {
                    Ok(v) => format!(
                        " — a {kind} literal must fit 64 bits; use `big(\"{v}\")` for an arbitrary-precision BigInt"
                    ),
                    Err(_) => format!(
                        " — a {kind} literal must fit 64 bits; for a larger value use a decimal `big(\"...\")` BigInt"
                    ),
                };
                lx.diags.push(Diag::error(
                    "K0004",
                    format!("{kind} integer literal `{prefix}{text}` does not fit in Int (64-bit){hint}"),
                    lx.span_from(start),
                ));
            };
            match (self.peek_num_suffix(), radixed) {
                (NumSuffix::Int(w), Ok(v)) => self.emit_sized(v as i128, w, start),
                (NumSuffix::F32, Ok(v)) => self.push(Tok::F32Lit(v as f32), start),
                (NumSuffix::None, Ok(v)) => self.push(Tok::Int(v as i64), start),
                // A REAL bug found+fixed (PR-it604, a lexer.rs sweep mirroring check.rs/
                // parser.rs's it581-603 findings): this used to fall straight through to
                // `fit_err` even when an EXPLICIT width suffix (`u8`, `i16`, ...) was
                // present, silently dropping it -- the message talked about `Int
                // (64-bit)` and `big(...)`, neither of which addresses what the user
                // actually wrote. Route a suffixed literal through `emit_sized`'s
                // existing width-aware "out of range for `W` (its range is X..Y)"
                // template instead (reused, not reimplemented) whenever the value at
                // least fits `i128` -- the SAME magnitude-then-cast-then-range-check
                // shape the `Ok(v)` arm above already uses for values that fit `u64`.
                (NumSuffix::Int(w), Err(_)) => match i128::from_str_radix(&text, radix) {
                    Ok(v) => self.emit_sized(v, w, start),
                    Err(_) => fit_err(self, &text, radix),
                },
                (_, Err(_)) => fit_err(self, &text, radix),
            }
            return;
        }
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
        // an `f32` suffix applies to any numeric body (`1.5f32`, `1e3f32`, `10f32`)
        match self.peek_num_suffix() {
            NumSuffix::F32 => {
                match text.parse::<f64>() {
                    Ok(v) => self.push(Tok::F32Lit(v as f32), start),
                    Err(_) => self.diags.push(Diag::error(
                        "K0003",
                        format!("invalid float literal `{text}`"),
                        self.span_from(start),
                    )),
                }
                return;
            }
            NumSuffix::Int(w) => {
                if is_float {
                    self.diags.push(Diag::error(
                        "K0009",
                        format!("integer width suffix `{}` on a float literal", w.name()),
                        self.span_from(start),
                    ));
                } else {
                    match text.parse::<i128>() {
                        Ok(v) => self.emit_sized(v, w, start),
                        // A REAL bug found+fixed (PR-it604, same lexer.rs sweep): unlike
                        // `emit_sized`'s own "out of range" message (used a few lines up
                        // for a value that DOES fit `i128`), this bare fallback -- for a
                        // decimal literal so large it doesn't even fit `i128` -- named
                        // the width but dropped its range AND the `big(...)` fix,
                        // even though `w.min()`/`w.max()` are directly available without
                        // needing the (here, unrepresentable) overflowing value itself.
                        Err(_) => self.diags.push(Diag::error(
                            "K0009",
                            format!(
                                "literal `{text}` out of range for `{}` (its range is {}..{}) — \
                                 too large for any fixed-width integer; use `big(\"{text}\")` for an arbitrary-precision BigInt",
                                w.name(),
                                w.min(),
                                w.max()
                            ),
                            self.span_from(start),
                        )),
                    }
                }
                return;
            }
            NumSuffix::None => {}
        }
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
                    format!(
                        "integer literal `{text}` does not fit in Int (64-bit) — use `big(\"{text}\")` for an arbitrary-precision BigInt"
                    ),
                    self.span_from(start),
                )),
            }
        }
    }

    /// After a number body, consume a numeric type suffix if present: an integer
    /// width (`i8`…`u64`) or `f32`. A trailing identifier run that is not exactly
    /// one of those is left alone (so `123index` stays `123` then `index`).
    fn peek_num_suffix(&mut self) -> NumSuffix {
        if !matches!(self.peek(), Some(b'i') | Some(b'u') | Some(b'f')) {
            return NumSuffix::None;
        }
        let save = self.pos;
        let s = self.pos;
        while matches!(self.peek(),
            Some(b'0'..=b'9') | Some(b'a'..=b'z') | Some(b'A'..=b'Z') | Some(b'_'))
        {
            self.bump();
        }
        let run = &self.src[s..self.pos];
        if let Some(w) = crate::value::IntW::from_name(run) {
            NumSuffix::Int(w)
        } else if run == "f32" {
            NumSuffix::F32
        } else {
            self.pos = save; // not a numeric suffix — put it back
            NumSuffix::None
        }
    }

    /// Emit a sized-int token, range-checking at lex time (K0009 on overflow).
    fn emit_sized(&mut self, v: i128, w: crate::value::IntW, start: usize) {
        if w.check_range(v) {
            self.push(Tok::SizedInt(v, w), start);
        } else {
            let hint = match w.widen_to_fit(v) {
                Some(wider) if wider != w => {
                    format!(
                        " — `{}` holds it (its range is {}..{})",
                        wider.name(),
                        wider.min(),
                        wider.max()
                    )
                }
                _ => " — too large for any fixed-width integer; use the default `Int` \
                      or a `big(...)` BigInt"
                    .to_string(),
            };
            self.diags.push(Diag::error(
                "K0009",
                format!(
                    "literal `{v}` out of range for `{}` (its range is {}..{}){hint}",
                    w.name(),
                    w.min(),
                    w.max()
                ),
                self.span_from(start),
            ));
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
                Some(b'\\') => {
                    // Position of the backslash itself -- captured BEFORE
                    // consuming the escaped character below, so every
                    // diagnostic in this arm anchors correctly regardless
                    // of how many bytes that character turns out to need
                    // (production-hardening PR-it934, replacing the
                    // original single-byte-assuming `self.pos.saturating_
                    // sub(2)` computations this arm used to repeat).
                    let esc_start = self.pos - 1;
                    match self.bump() {
                    Some(b'n') => text.push('\n'),
                    Some(b't') => text.push('\t'),
                    Some(b'r') => text.push('\r'),
                    Some(b'\\') => text.push('\\'),
                    Some(b'"') => text.push('"'),
                    Some(b'{') => text.push('{'),
                    Some(b'}') => text.push('}'),
                    Some(b'0') => {
                        // A `Str` is NUL-free UTF-8 text (so it maps cleanly to the
                        // native C `char*` representation across all engines). Binary
                        // data with embedded NULs belongs in a `List[Int]` of bytes.
                        self.diags.push(Diag::error(
                            "K0008",
                            "NUL (`\\0`) is not allowed in a string literal — `Str` is \
                             NUL-free UTF-8 text; use a byte list for binary data",
                            self.span_from(esc_start),
                        ));
                    }
                    // True EOF right after the backslash -- production-
                    // hardening PR-it934. Emitting NOTHING here (rather
                    // than a misleading "unknown escape sequence `\ `"
                    // with a phantom space character that doesn't exist
                    // anywhere in the source) is correct: the outer loop's
                    // own EOF check, on its very next iteration, reports
                    // the accurate K0005 "unterminated string literal".
                    None => {}
                    Some(c) => {
                        // A REAL, uncatchable-crash bug found+fixed
                        // (production-hardening PR-it934, a sibling of
                        // PR-it924's own interpolation-scanner fix -- same
                        // bug CLASS, different site, never covered by that
                        // fix since it was scoped to the `{...}`
                        // interpolation boundary scanner specifically, not
                        // this string-literal escape-handling code). The
                        // `self.bump()` above (fetching the escaped
                        // character) only ever consumes ONE raw byte -- for
                        // a multi-byte UTF-8 character (`c >= 0x80`),
                        // that's just its LEAD byte, leaving the
                        // character's remaining continuation byte(s)
                        // "orphaned" for the NEXT loop iteration to consume
                        // as if they started a fresh, independent byte
                        // sequence -- corrupting `self.pos`'s alignment to
                        // a valid char boundary and eventually panicking
                        // when `self.src` is later sliced at that now-
                        // invalid position (the plain-text scan's own "re-
                        // do properly" logic below, which assumes whatever
                        // it's handed IS a valid boundary). Live-confirmed:
                        // `"\π"` (backslash immediately followed by a
                        // 2-byte UTF-8 character) crashed with "internal
                        // compiler error [src/lexer.rs:590]" instead of a
                        // clean K0006. Fixed by reconstructing the FULL
                        // character here too, mirroring the established
                        // "re-do properly" pattern below exactly, before
                        // reporting it in the diagnostic (so the message
                        // also shows the real character instead of a mis-
                        // decoded single byte).
                        let ch = if c >= 0x80 {
                            let ch_start = self.pos - 1;
                            let ch = self.src[ch_start..].chars().next().unwrap_or('\u{fffd}');
                            self.pos = ch_start + ch.len_utf8();
                            ch
                        } else {
                            c as char
                        };
                        self.diags.push(Diag::error(
                            "K0006",
                            format!(
                                "unknown escape sequence `\\{ch}` — the valid escapes are \
                                 `\\n`, `\\t`, `\\r`, `\\\\`, `\\\"`, `\\{{` and `\\}}` \
                                 (a literal backslash is `\\\\`)"
                            ),
                            self.span_from(esc_start),
                        ));
                    }
                    }
                }
                Some(b'{') if self.peek() == Some(b'{') => {
                    // `{{` is a literal `{` (so JSON/CSS/`{…}` templates can be
                    // written directly); only a single `{` opens interpolation.
                    self.bump();
                    text.push('{');
                }
                Some(b'}') => {
                    // `}}` collapses to a literal `}`, symmetric with `{{`; a lone
                    // `}` in text is already literal.
                    if self.peek() == Some(b'}') {
                        self.bump();
                    }
                    text.push('}');
                }
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
                                // A REAL bug found+fixed (PR-it604, same lexer.rs sweep):
                                // this used to span from `expr_start` (right AFTER the
                                // opening `{`), excluding the very delimiter the message
                                // names ("unterminated `{` interpolation") -- unlike its
                                // sibling unterminated-construct diagnostics (K0002 block
                                // comment, K0005 string literal), which both anchor at
                                // their construct's own opening delimiter. `expr_start -
                                // 1` is the `{`'s own position, already in hand; `
                                // expr_start` ITSELF must stay unchanged since it's also
                                // used below for `StrPart::Expr`'s raw-source offset.
                                self.diags.push(Diag::error(
                                    "K0007",
                                    "unterminated `{` interpolation in string",
                                    self.span_from(expr_start.saturating_sub(1)),
                                ));
                                depth = 0;
                            }
                            Some(b'{') => depth += 1,
                            Some(b'}') => depth -= 1,
                            // `//` line comment inside an interpolation: consume to
                            // end of line (mirrors `run()`'s own top-level handling)
                            // without counting any `{`/`}` inside toward `depth` --
                            // otherwise a stray unmatched brace in the comment text
                            // silently mis-splits the interpolation boundary.
                            Some(b'/') if self.peek() == Some(b'/') => {
                                while let Some(c) = self.peek() {
                                    if c == b'\n' {
                                        break;
                                    }
                                    self.bump();
                                }
                            }
                            // `/* */` block comment inside an interpolation (nesting
                            // allowed, mirrors `run()`'s own top-level handling) --
                            // same rationale as the `//` arm above.
                            Some(b'/') if self.peek() == Some(b'*') => {
                                self.bump();
                                let mut cdepth = 1usize;
                                while cdepth > 0 {
                                    match self.bump() {
                                        None => {
                                            self.diags.push(Diag::error(
                                                "K0007",
                                                "unterminated `{` interpolation in string",
                                                self.span_from(expr_start.saturating_sub(1)),
                                            ));
                                            depth = 0;
                                            break;
                                        }
                                        Some(b'/') if self.peek() == Some(b'*') => {
                                            self.bump();
                                            cdepth += 1;
                                        }
                                        Some(b'*') if self.peek() == Some(b'/') => {
                                            self.bump();
                                            cdepth -= 1;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            // nested string literal: skip it whole, so quotes
                            // and braces inside it don't confuse the scan —
                            // `"{xs.join(", ")}"` works without escaping
                            Some(b'"') => loop {
                                match self.bump() {
                                    None | Some(b'\n') => {
                                        self.diags.push(Diag::error(
                                            "K0007",
                                            "unterminated `{` interpolation in string",
                                            self.span_from(expr_start.saturating_sub(1)),
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
                    // `self.pos - 1` is the byte just before the closing `}`. At
                    // EOF the loop never advances, so clamp to `expr_start` to keep
                    // the range non-inverted (a degenerate `{` at end-of-input).
                    //
                    // A REAL bug found+fixed (production-hardening PR-it924, a
                    // two-phase self-scoping survey finding): when the loop exits
                    // via true EOF (the `None` arm above, `self.pos` unmoved from
                    // wherever the last successfully-consumed byte left it) rather
                    // than a real closing `}`, `self.pos - 1` can land STRICTLY
                    // INSIDE a multi-byte UTF-8 character if the source is
                    // truncated mid-character (e.g. `"{π` with no closing brace) --
                    // this inner byte-at-a-time scan (unlike the outer plain-text
                    // scan just below, which explicitly reassembles a full
                    // character before pushing it) has no character-boundary
                    // awareness at all, so a raw byte offset landing mid-character
                    // is entirely possible. `self.src[expr_start..end]` (a `&str`
                    // slice) then panics ("byte index N is not a char boundary")
                    // instead of the intended clean K0007 diagnostic already
                    // pushed above. (The `Some(b'\n')` exit arm can NEVER hit this:
                    // `\n` is ASCII and can only ever appear between UTF-8
                    // characters, never as a continuation byte, so `self.pos`
                    // there is always already a valid boundary.) Live-confirmed:
                    // `kupl check` on a file containing `"{` followed by a raw
                    // 2-byte UTF-8 character (e.g. `π`) and nothing else crashed
                    // with "internal compiler error [src/lexer.rs:539]" (exit
                    // 101) instead of K0007 (exit 1), identically via `check`,
                    // `run`, and `native` (this lexer is shared front-end code).
                    // Fixed by rounding `end` DOWN to the nearest valid char
                    // boundary at or before the naive clamp -- `expr_start` is
                    // itself always a valid boundary (right after the single-byte
                    // `{` that opened this interpolation), so the loop below is
                    // guaranteed to terminate.
                    let mut end = self.pos.saturating_sub(1).max(expr_start);
                    while end > expr_start && !self.src.is_char_boundary(end) {
                        end -= 1;
                    }
                    let raw = self.src[expr_start..end].to_string();
                    parts.push(StrPart::Expr(raw, expr_start as u32));
                }
                Some(0) => {
                    // A raw NUL byte in the source: `Str` is NUL-free (see K0008).
                    self.diags.push(Diag::error(
                        "K0008",
                        "NUL byte is not allowed in a string literal — `Str` is \
                         NUL-free UTF-8 text; use a byte list for binary data",
                        self.span_from(self.pos.saturating_sub(1)),
                    ));
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
                    // A distinct code from K0008's NUL-in-string errors (PR-it670): both
                    // used to share K0008, silently violating DIAGNOSTICS.md's own stated
                    // invariant that "codes are never reused with a different meaning" --
                    // which matters for `kupl check --json`/LSP consumers that key
                    // remediation logic off the code, not just the message text.
                    self.diags.push(Diag::error(
                        "K0010",
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
            b'@' => Tok::At,
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
            b'{' => {
                // A block re-opens a statement context: save the surrounding
                // paren depth and reset to 0 so newlines inside the block (e.g.
                // match arms) are significant again.
                self.paren_stack.push(self.paren_depth);
                self.paren_depth = 0;
                Tok::LBrace
            }
            b'}' => {
                // Restore the paren depth that was in effect before this block.
                self.paren_depth = self.paren_stack.pop().unwrap_or(0);
                Tok::RBrace
            }
            // `;` is an explicit statement separator, equivalent to a newline —
            // so `{ a; b }` on one line works (and the formatter's inline blocks
            // parse). It carries no other meaning.
            b';' => Tok::Newline,
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

    /// `{{`/`}}` are literal braces; a single `{` still opens interpolation.
    #[test]
    fn literal_brace_escaping() {
        fn parts(src: &str) -> Vec<StrPart> {
            let (toks, diags) = lex(src);
            assert!(diags.is_empty(), "unexpected diags: {diags:?}");
            match &toks[0].tok {
                Tok::Str(p) => p.clone(),
                other => panic!("expected a string, got {other:?}"),
            }
        }
        assert_eq!(parts(r#""{{""#), vec![StrPart::Text("{".into())]);
        assert_eq!(parts(r#""}}""#), vec![StrPart::Text("}".into())]);
        assert_eq!(parts(r#""{{x}}""#), vec![StrPart::Text("{x}".into())]);
        assert_eq!(parts(r#""{{\"a\":1}}""#), vec![StrPart::Text("{\"a\":1}".into())]);
        // `{{ {n} }}` -> literal `{ `, interpolate n, literal ` }`
        assert_eq!(
            parts(r#""{{ {n} }}""#),
            vec![
                StrPart::Text("{ ".into()),
                StrPart::Expr("n".into(), 5),
                StrPart::Text(" }".into()),
            ]
        );
        // a plain single-brace interpolation is unchanged
        assert_eq!(parts(r#""{n}""#), vec![StrPart::Expr("n".into(), 2)]);
    }

    #[test]
    fn f32_literals() {
        assert_eq!(kinds("1.5f32"), vec![Tok::F32Lit(1.5), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("1e3f32"), vec![Tok::F32Lit(1000.0), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("10f32"), vec![Tok::F32Lit(10.0), Tok::Newline, Tok::Eof]);
        // bare float is still Float
        assert_eq!(kinds("1.5"), vec![Tok::Float(1.5), Tok::Newline, Tok::Eof]);
    }

    #[test]
    fn hex_and_binary_f32_suffix_is_disambiguated_from_hex_digits() {
        // A REAL bug found+fixed (PR-it1131): `f`/`F` are themselves valid hex
        // digits, so a hex literal's maximal-munch digit loop used to swallow a
        // trailing `f32` suffix as three more hex digits, silently producing a
        // large wrong `Int` instead of the `F32Lit` the design (and the decimal
        // path's own `10f32` support) clearly intended.
        assert_eq!(kinds("0xFFf32"), vec![Tok::F32Lit(255.0), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0xAf32"), vec![Tok::F32Lit(10.0), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0x1f32"), vec![Tok::F32Lit(1.0), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0x0f32"), vec![Tok::F32Lit(0.0), Tok::Newline, Tok::Eof]);
        // binary was never affected (`f` halts a binary digit run on its own) --
        // locked in here so a future regression in the shared branch is caught.
        assert_eq!(kinds("0b101f32"), vec![Tok::F32Lit(5.0), Tok::Newline, Tok::Eof]);
        // no hex digit body remains once `f32` backs off -- stays a plain hex Int,
        // not an "empty hex literal" error and not F32Lit(0.0).
        assert_eq!(kinds("0xf32"), vec![Tok::Int(0xf32), Tok::Newline, Tok::Eof]);
        // an explicit int-width suffix on hex was never ambiguous (`i`/`u` are not
        // hex digits) -- unaffected by this fix, reconfirmed here.
        use crate::value::IntW;
        assert_eq!(kinds("0xFFu8"), vec![Tok::SizedInt(255, IntW::U8), Tok::Newline, Tok::Eof]);
    }

    #[test]
    fn sized_int_literals() {
        use crate::value::IntW;
        assert_eq!(kinds("255u8"), vec![Tok::SizedInt(255, IntW::U8), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("1000i16"), vec![Tok::SizedInt(1000, IntW::I16), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0xFFu8"), vec![Tok::SizedInt(255, IntW::U8), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0b101u8"), vec![Tok::SizedInt(5, IntW::U8), Tok::Newline, Tok::Eof]);
        // bare number is still a plain Int
        assert_eq!(kinds("1000"), vec![Tok::Int(1000), Tok::Newline, Tok::Eof]);
        // out-of-range suffix is a K0009 diagnostic
        let (_, diags) = lex("256u8");
        assert!(diags.iter().any(|d| d.code == "K0009"), "{diags:?}");
        // a trailing identifier that isn't a width name is NOT consumed as a suffix
        let toks: Vec<Tok> = lex("123index").0.into_iter().map(|t| t.tok).collect();
        assert_eq!(toks, vec![Tok::Int(123), Tok::Ident("index".into()), Tok::Newline, Tok::Eof]);
    }

    #[test]
    fn sized_overflow_suggests_a_wider_width() {
        // K0009 for an over-wide literal should not just say "out of range"; it should show
        // the offending width's range AND name the narrowest width that would hold the value.
        let d = |src: &str| {
            lex(src)
                .1
                .into_iter()
                .find(|d| d.code == "K0009")
                .expect("K0009")
                .message
        };
        // 256 overflows u8 (0..255) -> suggest u16.
        let m = d("256u8");
        assert!(m.contains("out of range for `u8`") && m.contains("0..255"), "shows u8 range: {m}");
        assert!(m.contains("`u16` holds it") && m.contains("0..65535"), "suggests u16: {m}");
        // 128 overflows i8 (-128..127) -> suggest i16 (same signedness family).
        let s = d("128i8");
        assert!(s.contains("`i16` holds it"), "signed literal suggests i16: {s}");
        // 70000 overflows u16 -> suggest u32.
        assert!(d("70000u16").contains("`u32` holds it"), "u16 overflow suggests u32");
    }

    /// A REAL bug found+fixed (production-hardening PR-it1059, a background
    /// close-read survey finding): `IntW::widen_to_fit` only ever searched
    /// the SAME signedness family as the declared width, so a signed literal
    /// (e.g. `i8`) whose value exceeds `i64::MAX` but fits `u64` exactly used
    /// to fall through to the generic "too large for any fixed-width
    /// integer; use the default `Int`" hint -- both claims false, since
    /// `u64` holds the value and the bare unsuffixed literal ALSO overflows
    /// `Int`'s own 64-bit range (K0004). Live-confirmed BEFORE this fix via
    /// `18446744073709551615i8`.
    #[test]
    fn sized_overflow_beyond_every_signed_width_still_suggests_a_holding_unsigned_one() {
        let d = |src: &str| {
            lex(src)
                .1
                .into_iter()
                .find(|d| d.code == "K0009")
                .expect("K0009")
                .message
        };
        // 18446744073709551615 (u64::MAX) overflows every signed width (i64::MAX is
        // smaller) but fits u64 exactly -- must suggest u64, not "too large for any
        // fixed-width integer".
        let m = d("18446744073709551615i8");
        assert!(m.contains("out of range for `i8`"), "shows i8 range: {m}");
        assert!(
            m.contains("`u64` holds it") && m.contains("0..18446744073709551615"),
            "suggests u64 instead of falsely claiming no fixed width holds it: {m}"
        );
        assert!(
            !m.contains("too large for any fixed-width integer"),
            "must not show the generic hint when u64 genuinely holds the value: {m}"
        );
        // A value truly beyond every fixed width (including u64) must still show the
        // generic hint -- confirms the fallback doesn't over-fire.
        let m2 = d("999999999999999999999i8");
        assert!(
            m2.contains("too large for any fixed-width integer"),
            "a value beyond u64::MAX too must keep the generic hint: {m2}"
        );
    }

    #[test]
    fn suffixed_overflow_never_silently_drops_the_declared_width() {
        // Two REAL bugs found+fixed (PR-it604, a lexer.rs sweep mirroring check.rs/
        // parser.rs's it581-603 findings): an explicit width suffix (`u8`) on an
        // overflowing literal used to be silently dropped from the error message in
        // two shapes -- a hex/binary literal too big to fit u64 (fell through to the
        // generic "does not fit in Int (64-bit)" / "big(...)" message, ignoring the
        // suffix entirely), and a DECIMAL literal too big to fit even i128 (named the
        // width but dropped its range and the big(...) fix that its sibling,
        // `emit_sized`'s own message, always includes).
        let (_, diags) = lex("0x1FFFFFFFFFFFFFFFFu8"); // too big for u64, fits i128
        let d = diags.iter().find(|d| d.code == "K0009").expect("K0009, not the generic K0004");
        assert!(
            d.message.contains("out of range for `u8`") && d.message.contains("0..255"),
            "must name the declared width and its range, not silently drop the suffix: {}",
            d.message
        );

        let (_, diags) = lex("99999999999999999999999999999999999999999999u8"); // too big even for i128
        let d = diags.iter().find(|d| d.code == "K0009").expect("K0009");
        assert!(
            d.message.contains("out of range for `u8`") && d.message.contains("0..255"),
            "must show u8's own range even when the value overflows i128: {}",
            d.message
        );
        assert!(
            d.message.contains("big(\"99999999999999999999999999999999999999999999\")"),
            "must still name the big(...) fix: {}",
            d.message
        );
    }

    #[test]
    fn unterminated_interpolation_span_covers_its_opening_brace() {
        // A REAL bug found+fixed (PR-it604, same sweep): "unterminated `{`
        // interpolation in string" used to underline starting ONE BYTE AFTER the
        // `{` it names, unlike its sibling unterminated-construct diagnostics
        // (K0002 block comment, K0005 string literal), which both anchor at their
        // construct's own opening delimiter.
        let src = "\"{abc";
        let (_, diags) = lex(src);
        let d = diags.iter().find(|d| d.code == "K0007").expect("K0007");
        let text = &src[d.span.start as usize..d.span.end as usize];
        assert!(text.starts_with('{'), "span must include the opening `{{`: {text:?}");
    }

    /// A decimal literal too large for i64 is a K0004, and the message now NAMES the fix:
    /// wrap the digits in `big("...")` for an arbitrary-precision BigInt (it287). The suggested
    /// call echoes the exact digits (underscores stripped) so it can be copy-pasted verbatim.
    #[test]
    fn int_overflow_literal_suggests_big() {
        let (_, diags) = lex("99999999999999999999999");
        let d = diags.iter().find(|d| d.code == "K0004").expect("expected K0004");
        assert!(d.message.contains("does not fit in Int (64-bit)"), "{}", d.message);
        assert!(
            d.message.contains("big(\"99999999999999999999999\")"),
            "message should name the big(...) fix with the literal digits: {}",
            d.message
        );
        // underscores in the source literal are stripped from the suggested call
        let (_, du) = lex("9_223_372_036_854_775_808");
        let d2 = du.iter().find(|d| d.code == "K0004").expect("expected K0004");
        assert!(
            d2.message.contains("big(\"9223372036854775808\")"),
            "underscores must be stripped in the suggestion: {}",
            d2.message
        );
    }

    #[test]
    fn radixed_overflow_names_literal_and_big() {
        // A hex/binary literal wider than 64 bits is a K0004; the message should name the
        // radix and the literal, and -- when the value still fits i128 -- suggest big(...)
        // with the exact DECIMAL value (big takes a decimal string, not the hex text).
        let (_, dh) = lex("0x1FFFFFFFFFFFFFFFF"); // 2^65-1
        let h = dh.iter().find(|d| d.code == "K0004").expect("K0004 hex");
        assert!(h.message.contains("hex integer literal `0x1FFFFFFFFFFFFFFFF`"), "names hex literal: {}", h.message);
        assert!(h.message.contains("big(\"36893488147419103231\")"), "suggests big with decimal: {}", h.message);
        // A binary literal for the same value gets the same decimal suggestion.
        let (_, db) = lex("0b11111111111111111111111111111111111111111111111111111111111111111");
        let b = db.iter().find(|d| d.code == "K0004").expect("K0004 bin");
        assert!(b.message.contains("binary integer literal") && b.message.contains("big(\"36893488147419103231\")"), "binary big: {}", b.message);
        // Too large even for i128 -> a generic big(...) hint, no bogus decimal.
        let (_, dbig) = lex("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
        let big = dbig.iter().find(|d| d.code == "K0004").expect("K0004 huge");
        assert!(big.message.contains("for a larger value use a decimal `big(\"...\")`"), "generic hint: {}", big.message);
    }

    #[test]
    fn integer_literal_forms() {
        // hex, binary, and underscore separators all yield plain i64 Ints
        assert_eq!(kinds("0xFF"), vec![Tok::Int(255), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0xff"), vec![Tok::Int(255), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0b1010"), vec![Tok::Int(10), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("0xDEAD_BEEF"), vec![Tok::Int(3735928559), Tok::Newline, Tok::Eof]);
        assert_eq!(kinds("1_000_000"), vec![Tok::Int(1_000_000), Tok::Newline, Tok::Eof]);
        // full 64-bit pattern reinterpreted as i64
        assert_eq!(kinds("0xFFFFFFFFFFFFFFFF"), vec![Tok::Int(-1), Tok::Newline, Tok::Eof]);
        // `0` alone and a bare `0b`-less number still work
        assert_eq!(kinds("0"), vec![Tok::Int(0), Tok::Newline, Tok::Eof]);
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

    /// A REAL bug found+fixed (production-hardening PR-it966): a newline
    /// right after `..`, `..=`, `!`, or `@` used to terminate the statement
    /// like a closing delimiter, even though every other operator already
    /// suppressed it. Each of these should lex identically to its same-line
    /// form -- no stray `Newline` token between the operator and its operand.
    #[test]
    fn newline_suppression_after_range_bang_and_at() {
        assert_eq!(kinds("0 ..\n5"), kinds("0 .. 5"));
        assert_eq!(kinds("0 ..=\n5"), kinds("0 ..= 5"));
        assert_eq!(kinds("!\ntrue"), kinds("! true"));
        assert_eq!(kinds("whole @\nCircle"), kinds("whole @ Circle"));
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
    fn unterminated_interpolation_at_eof_does_not_panic() {
        // regression: a string ending in `{` at EOF used to build an inverted
        // byte range (expr_start > end) and panic slicing self.src. It must
        // instead produce a clean unterminated-interpolation diagnostic.
        // none of these may panic (the point of the test is reaching this line)
        for src in ["\"{", "\"abc{", "\"{ ", "\"x{y", "\"{{{\"", "\"{"] {
            let _ = lex(src);
        }
        // the reduced trigger emits a clean unterminated-interpolation diagnostic
        let (_t, diags) = lex("\"{");
        assert!(diags.iter().any(|d| d.code == "K0007"), "expected K0007, got {diags:?}");
    }

    #[test]
    fn unterminated_interpolation_at_eof_mid_multibyte_char_does_not_panic() {
        // regression: PR-it924, the SAME class of bug as
        // `unterminated_interpolation_at_eof_does_not_panic` above (an unclosed
        // `{` at EOF forcing an invalid byte range for `self.src[expr_start..end]`)
        // but for a case that fix's own ASCII-only fixtures never exercised: the
        // interpolation's raw content is truncated by true EOF strictly INSIDE a
        // multi-byte UTF-8 character, so the naive `self.pos - 1` clamp lands on
        // a non-char-boundary index instead of just an inverted range.
        // none of these may panic (the point of the test is reaching this line)
        for src in ["\"{π", "\"{日本語", "\"{a日", "\"{😀", "\"{a\u{1F600}"] {
            let _ = lex(src);
        }
        let (_t, diags) = lex("\"{π");
        assert!(diags.iter().any(|d| d.code == "K0007"), "expected K0007, got {diags:?}");
    }

    #[test]
    fn an_unknown_escape_sequence_followed_by_a_multibyte_char_does_not_panic() {
        // A REAL, uncatchable-crash bug found+fixed (production-hardening
        // PR-it934, a sibling of PR-it924's own interpolation-scanner fix
        // -- same bug CLASS (a byte-level scan losing UTF-8 char-boundary
        // awareness), different site (string-literal ESCAPE handling, not
        // the `{...}` interpolation boundary scanner), never covered by
        // that earlier fix. `lex_string`'s unknown-escape fallback used to
        // consume only ONE raw byte for the escaped character via its own
        // `self.bump()` -- for a multi-byte UTF-8 character, that's just
        // its lead byte, leaving the remaining continuation byte(s)
        // "orphaned" for the next loop iteration to treat as the start of
        // a fresh, independent byte sequence, corrupting `self.pos`'s
        // alignment to a valid char boundary. Live-confirmed BEFORE this
        // fix: `"\π"` (a backslash immediately followed by a 2-byte UTF-8
        // character) panicked with a byte-index-not-a-char-boundary error
        // instead of a clean K0006. None of these may panic (the point of
        // the test is reaching the assertions below).
        for src in ["\"\\π\"", "\"\\日本語\"", "\"a\\日b\"", "\"\\😀\"", "\"a\\\u{1F600}b\""] {
            let _ = lex(src);
        }
        // the diagnostic must show the REAL character, not a mis-decoded
        // single byte or a phantom placeholder.
        let (_t, diags) = lex("\"\\π\"");
        let d = diags.iter().find(|d| d.code == "K0006").expect(&format!("expected K0006, got {diags:?}"));
        assert!(d.message.contains('π'), "expected the real character π in the message, got {:?}", d.message);
    }

    #[test]
    fn a_backslash_right_at_true_eof_reports_only_unterminated_string_not_a_phantom_escape() {
        // A REAL, live-confirmed misleading-diagnostic bug found+fixed
        // (production-hardening PR-it934, found alongside the multibyte-
        // panic sibling above during the same close-read): when a string
        // literal ends abruptly right after a lone backslash (true EOF,
        // `self.bump()` returning `None` while fetching the escaped
        // character), the unknown-escape fallback used to synthesize a
        // PHANTOM space character in its own diagnostic message (`other.
        // map(|c| c as char).unwrap_or(' ')`) even though no space exists
        // anywhere in the source. Live-confirmed BEFORE this fix:
        // `"a\` (ending right there, no closing quote) reported `unknown
        // escape sequence \` followed by a literal space, alongside the
        // separately-correct K0005 "unterminated string literal" --
        // confusing and simply false. Fixed by emitting NO escape
        // diagnostic at all when the escaped-character fetch hits true
        // EOF, letting the outer loop's own EOF check (which fires on its
        // very next iteration) report K0005 alone, exactly as a plain
        // (non-backslash) EOF inside a string already does.
        let (_t, diags) = lex("\"a\\");
        assert!(diags.iter().any(|d| d.code == "K0005"), "expected K0005, got {diags:?}");
        assert!(
            !diags.iter().any(|d| d.code == "K0006"),
            "must NOT report a phantom unknown-escape diagnostic for true EOF: {diags:?}"
        );
    }

    #[test]
    fn interpolation_line_comment_cannot_reach_a_closing_brace() {
        // A REAL bug found+fixed (PR-it893, close-read sweep): the `{...}`
        // interpolation-boundary scanner counted `{`/`}` bytes with no
        // knowledge of `//`/`/* */` comments, unlike `run()`'s own top-level
        // dispatcher. Since the captured raw fragment is LATER re-lexed by
        // `parse_expr_fragment` via the full comment-aware lexer, the two
        // passes could disagree on where the interpolation actually ends:
        // `"{a // b}c}"` used to close at the FIRST `}` (right after `b`,
        // inside what should be a comment), silently re-absorbing `c}` as
        // ordinary trailing string text with zero diagnostics from either
        // pass. A `//` comment runs to the next physical newline, and an
        // interpolation can never span a newline (K0007), so a `}` after
        // `//` can NEVER validly close the interpolation on the same line
        // -- the fixed scanner now treats the whole rest of the line as
        // comment, correctly reaching a clean K0007 in both the case that
        // used to (by luck) parse "correctly" and the case that used to
        // silently corrupt the output, rather than the two disagreeing.
        for src in ["\"{a // b}c}\"", "\"{a // this is a trailing comment, no brace inside}\""] {
            let (_toks, diags) = lex(src);
            assert!(
                diags.iter().any(|d| d.code == "K0007"),
                "a `//` comment inside a single-line interpolation can never reach a real \
                 closing `}}` before end-of-line, so this must be a clean K0007, not a silent \
                 misparse: {src:?} -> {diags:?}"
            );
        }
    }

    #[test]
    fn interpolation_block_comment_is_skipped_without_disturbing_brace_depth() {
        // Companion to the line-comment case above: unlike `//`, a `/* */`
        // block comment is self-delimiting, so it can be skipped as one
        // atomic unit (nesting allowed, mirrors `run()`'s own top-level
        // handling) and the interpolation can still close correctly
        // afterward, even when the comment's own text contains a stray
        // unmatched brace.
        let (toks, diags) = lex("\"{a /* b}c */}\"");
        assert!(diags.is_empty(), "{diags:?}");
        match &toks[0].tok {
            Tok::Str(parts) => {
                assert_eq!(parts.len(), 1);
                assert!(
                    matches!(&parts[0], StrPart::Expr(s, _) if s == "a /* b}c */"),
                    "must close at the true final `}}`, past the whole block comment: {parts:?}"
                );
            }
            other => panic!("expected string, got {other:?}"),
        }
        // a block comment may itself span multiple physical lines, same as
        // at the top level -- the newline inside it must not trip the
        // interpolation's own no-newline rule.
        let (toks2, diags2) = lex("\"{a /* b}\nc */}\"");
        assert!(diags2.is_empty(), "{diags2:?}");
        match &toks2[0].tok {
            Tok::Str(parts) => {
                assert!(matches!(&parts[0], StrPart::Expr(s, _) if s == "a /* b}\nc */"));
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn unknown_escape_lists_the_valid_escapes() {
        // K0006 should not just name the offending escape; it should tell the user which
        // escapes ARE valid so they can fix it without consulting docs. `\q` is unknown.
        let (_t, diags) = lex("\"bad\\q\"");
        let m = &diags
            .iter()
            .find(|d| d.code == "K0006")
            .expect("K0006")
            .message;
        assert!(m.contains("`\\q`"), "names the bad escape: {m}");
        // the valid set is enumerated (spot-check the newline and brace escapes) and the
        // backslash-doubling hint is present.
        assert!(m.contains("`\\n`"), "lists \\n: {m}");
        assert!(m.contains("`\\{`"), "lists brace escape: {m}");
        assert!(m.contains("a literal backslash is"), "backslash hint: {m}");
    }

    #[test]
    fn nul_in_string_is_rejected() {
        // `Str` is NUL-free UTF-8 text (maps to native char*): both the `\0` escape
        // and a raw NUL byte in the source are compile errors (K0008), consistently.
        let (_t, diags) = lex("\"a\\0b\"");
        assert!(diags.iter().any(|d| d.code == "K0008"), "escape: {diags:?}");
        let (_t2, diags2) = lex("\"a\0b\"");
        assert!(diags2.iter().any(|d| d.code == "K0008"), "raw byte: {diags2:?}");
    }

    /// A REAL code-collision bug (PR-it670): a single `&` (not `&&`) used to be
    /// reported under the SAME code as `nul_in_string_is_rejected`'s K0008 --
    /// two entirely unrelated diagnostics (a bad-token lexer error vs. a
    /// string-content rule) sharing one code, directly violating
    /// `docs/reference/DIAGNOSTICS.md`'s own stated invariant that codes are
    /// "never reused with a different meaning". Now K0010, its own code.
    #[test]
    fn single_ampersand_is_rejected_with_its_own_code_not_the_nul_in_string_one() {
        let (_t, diags) = lex("a & b");
        let d = diags.iter().find(|d| d.code == "K0010").expect("K0010");
        assert!(d.message.contains("did you mean `&&`?"), "{}", d.message);
        // must NOT collide with the unrelated NUL-in-string diagnostic's code.
        assert!(!diags.iter().any(|d| d.code == "K0008"), "must not reuse K0008: {diags:?}");
    }

    #[test]
    fn semicolon_is_a_statement_separator() {
        // `;` lexes to a Newline token, so `a; b` separates statements on one line.
        let ks = kinds("let a = 1; let b = 2");
        assert!(ks.contains(&Tok::Newline));
        // it appears where the `;` was (between the two `let`s), not just at EOF
        let n_newlines = ks.iter().filter(|t| **t == Tok::Newline).count();
        assert!(n_newlines >= 2, "expected a separator at `;` and at EOF: {ks:?}");
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
