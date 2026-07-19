//! `kupl lsp` — a minimal Language Server Protocol server over stdio.
//!
//! Zero dependencies: Content-Length framing and a small JSON parser live
//! here. v0 capabilities: full-text document sync + push diagnostics on
//! open/change/save (multi-file aware — unsaved buffer contents override
//! what's on disk, `use`-dependencies come from disk).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::diag::{json_escape, line_col_utf16, Severity};

// ---------------- tiny JSON ----------------

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn index(&self, i: usize) -> Option<&Json> {
        match self {
            Json::Arr(items) => items.get(i),
            _ => None,
        }
    }
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Json::Num(n) if *n >= 0.0 => Some(*n as usize),
            _ => None,
        }
    }
}

pub fn parse_json(src: &str) -> Result<Json, String> {
    let bytes = src.as_bytes();
    let mut pos = 0usize;
    let v = parse_value(bytes, &mut pos, 0)?;
    skip_ws(bytes, &mut pos);
    // Trailing non-whitespace after the top-level value is an error, mirroring
    // json.rs's `parse` exactly (production-hardening PR-it792): this parser
    // used to silently accept and ignore trailing content -- confirmed live,
    // a mock ai-fun response of `"123abc"` for an `Int` shape SUCCEEDED (value
    // 123, "abc" silently dropped) on interp/KVM (both call THIS parser, per
    // ai.rs::convert), while native's `k_json_parse` (which mirrors json.rs's
    // OWN trailing-content check) correctly rejected it -- a genuine
    // crash-vs-succeed-shaped divergence (well, succeed-vs-reject), not just
    // message wording, and reachable via ordinary ai-fun mock testing.
    if pos != bytes.len() {
        return Err(format!("unexpected trailing characters at position {}", byte_to_char_pos(bytes, pos)));
    }
    Ok(v)
}

/// Number of UTF-8 characters in `b[..byte_pos]` -- this parser tracks BYTE
/// offsets internally (unlike json.rs's `Parser`, which is `Vec<char>`-
/// indexed), but json.rs's own position-bearing error messages (mirrored
/// exactly by native's `k_json_parse`/`kjp_cpos`, see cgen.rs) report CHAR
/// positions -- a raw byte position here would silently diverge from both on
/// any non-ASCII input (production-hardening PR-it792, spotted while fixing
/// the trailing-content/unexpected-character message gaps below).
fn byte_to_char_pos(b: &[u8], byte_pos: usize) -> usize {
    std::str::from_utf8(&b[..byte_pos.min(b.len())]).map(|s| s.chars().count()).unwrap_or(byte_pos)
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

/// Read exactly 4 hex digits starting at byte index `start` (the body of a
/// `\uXXXX` escape) into a code unit. Bounds-checked (`.get()`, not direct
/// slicing) so a truncated escape at the end of input is a clean parse
/// error instead of an out-of-bounds panic -- production-hardening PR-it765.
fn hex4(b: &[u8], start: usize) -> Result<u32, String> {
    let hex = b
        .get(start..start + 4)
        .and_then(|s| std::str::from_utf8(s).ok())
        .ok_or("bad \\u escape")?;
    u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape".to_string())
}

/// A robustness-audit finding (production-hardening PR-it620): this
/// recursive-descent parser had NO nesting-depth guard, unlike json.rs's
/// `parse` (the `json_parse` builtin's implementation, shared by interp/vm)
/// and cgen.rs's `kjp_value` (native's mirror) -- both of which already
/// bound recursion via `json::MAX_JSON_DEPTH`/`K_MAX_JSON_DEPTH` specifically
/// to prevent a stack overflow on untrusted deeply-nested input. THIS parser
/// (used for LSP JSON-RPC AND ai.rs's mock-response parsing) was the one gap
/// -- confirmed by direct reproduction: `parse_json("[".repeat(1000) + "]"
/// .repeat(1000))` overflowed the stack and aborted the process (SIGABRT),
/// not a catchable panic. Reuses `json::MAX_JSON_DEPTH` (not a new constant)
/// so all three parsers agree on the same limit, matching json.rs's own
/// documented intent ("the native backend enforces the same limit so all
/// engines agree").
fn parse_value(b: &[u8], pos: &mut usize, depth: usize) -> Result<Json, String> {
    skip_ws(b, pos);
    if *pos >= b.len() {
        // "...of input", not "...of JSON" -- matches json.rs's own wording
        // exactly (production-hardening PR-it792); this parser's OTHER
        // messages already do (see the literal-validation fix above), this
        // one site was missed.
        return Err("unexpected end of input".into());
    }
    match b[*pos] {
        b'{' => {
            let depth = depth + 1;
            if depth > crate::json::MAX_JSON_DEPTH {
                return Err("JSON nested too deeply".into());
            }
            *pos += 1;
            let mut pairs = Vec::new();
            skip_ws(b, pos);
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
                return Ok(Json::Obj(pairs));
            }
            loop {
                skip_ws(b, pos);
                let key = match parse_value(b, pos, depth)? {
                    Json::Str(s) => s,
                    _ => return Err("object key must be a string".into()),
                };
                skip_ws(b, pos);
                if *pos >= b.len() || b[*pos] != b':' {
                    return Err("expected ':'".into());
                }
                *pos += 1;
                let val = parse_value(b, pos, depth)?;
                pairs.push((key, val));
                skip_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b'}') => {
                        *pos += 1;
                        return Ok(Json::Obj(pairs));
                    }
                    _ => return Err("expected ',' or '}'".into()),
                }
            }
        }
        b'[' => {
            let depth = depth + 1;
            if depth > crate::json::MAX_JSON_DEPTH {
                return Err("JSON nested too deeply".into());
            }
            *pos += 1;
            let mut items = Vec::new();
            skip_ws(b, pos);
            if *pos < b.len() && b[*pos] == b']' {
                *pos += 1;
                return Ok(Json::Arr(items));
            }
            loop {
                items.push(parse_value(b, pos, depth)?);
                skip_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b']') => {
                        *pos += 1;
                        return Ok(Json::Arr(items));
                    }
                    _ => return Err("expected ',' or ']'".into()),
                }
            }
        }
        b'"' => {
            *pos += 1;
            let mut out = String::new();
            while *pos < b.len() {
                match b[*pos] {
                    b'"' => {
                        *pos += 1;
                        return Ok(Json::Str(out));
                    }
                    b'\\' => {
                        *pos += 1;
                        match b.get(*pos) {
                            Some(b'n') => out.push('\n'),
                            Some(b't') => out.push('\t'),
                            Some(b'r') => out.push('\r'),
                            Some(b'"') => out.push('"'),
                            Some(b'\\') => out.push('\\'),
                            Some(b'/') => out.push('/'),
                            Some(b'b') => out.push('\u{8}'),
                            Some(b'f') => out.push('\u{c}'),
                            Some(b'u') => {
                                // TWO real bugs found+fixed in this one arm (production-
                                // hardening PR-it765): (1) a genuine PROCESS-ABORTING PANIC
                                // on a truncated `\u` escape at the end of input -- the
                                // original direct `&b[*pos+1..*pos+5]` slice indexing panics
                                // ("range end index ... out of range") instead of returning
                                // a clean parse error, confirmed live via `parse_json("\"\\u12\"")`.
                                // Every OTHER literal-matching arm in this same function was
                                // already hardened against truncated/malformed input
                                // (PR-it545's `starts_with` bounds-safety comment above), but
                                // this ONE site was missed. (2) each `\uXXXX` escape was
                                // decoded INDEPENDENTLY with no surrogate-PAIR combination --
                                // unlike `json.rs`'s own, already-tested surrogate-pairing
                                // logic (this parser is a genuinely SEPARATE, independently-
                                // reimplemented JSON parser, used for LSP JSON-RPC AND ai.rs's
                                // mock-response parsing, per PR-it620's own doc comment above)
                                // -- a high surrogate (D800..=DBFF) followed by its low half
                                // (DC00..=DFFF) is required to form ONE astral code point
                                // (e.g. an emoji); decoding each half separately instead
                                // produced TWO U+FFFD replacement characters, confirmed live:
                                // `parse_json("\"\\uD83C\\uDF89\"")` (the emoji's own true
                                // surrogate pair) decoded to `"\u{FFFD}\u{FFFD}"` instead of
                                // the single correct emoji codepoint.
                                let hi = hex4(b, *pos + 1)?;
                                *pos += 4;
                                // A truncated/malformed LOW-surrogate escape (`\u` with fewer
                                // than 4 hex digits following) is a genuine parse error here
                                // too -- propagated via `?`, mirroring json.rs's own identical
                                // convention -- distinct from a VALID-but-out-of-range low
                                // half (any ordinary `\uXXXX` that isn't DC00..=DFFF), which
                                // falls back to U+FFFD for the unpaired high surrogate while
                                // leaving the candidate for the NEXT loop iteration to parse
                                // on its own.
                                let cp = if (0xD800..=0xDBFF).contains(&hi) {
                                    if b.get(*pos + 1) == Some(&b'\\') && b.get(*pos + 2) == Some(&b'u') {
                                        let lo = hex4(b, *pos + 3)?;
                                        if (0xDC00..=0xDFFF).contains(&lo) {
                                            *pos += 6;
                                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                        } else {
                                            0xFFFD
                                        }
                                    } else {
                                        0xFFFD
                                    }
                                } else {
                                    hi
                                };
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                            _ => return Err("bad escape".into()),
                        }
                        *pos += 1;
                    }
                    _ => {
                        // copy a full UTF-8 character
                        let s = std::str::from_utf8(&b[*pos..]).map_err(|_| "bad UTF-8")?;
                        let ch = s.chars().next().unwrap();
                        out.push(ch);
                        *pos += ch.len_utf8();
                    }
                }
            }
            Err("unterminated string".into())
        }
        b't' => {
            // A REAL BUG found+fixed (bug-hunt batch 153, PR-it545): the
            // literal-matching arms below used to just check the FIRST byte
            // (`t`/`f`/`n`) and blindly advance `pos` by the literal's length,
            // with no check that the following bytes actually spelled
            // "true"/"false"/"null" -- garbage input like "not json" (starts
            // with `n`) silently "parsed" as `Json::Null` instead of failing.
            // ai.rs reuses this parser (via `crate::lsp::parse_json`) for
            // ai-fun mock-response text, where malformed input is EXPECTED
            // and deliberately tested -- the leniency here (fine for
            // well-formed JSON-RPC messages, this parser's original purpose)
            // caused interp/KVM's ai-fun shape-mismatch message to read
            // "expected Int, model returned null" for input that isn't valid
            // JSON at all, while native's stricter C mirror (`k_json_parse`)
            // correctly reported "not valid JSON (invalid literal...)" for
            // the SAME input -- a real cross-engine behavioral divergence,
            // not just wording. `starts_with` is bounds-safe even if the
            // remaining input is shorter than the literal, so this also
            // closes a latent out-of-bounds risk in the unchecked `*pos +=
            // N` advance on truncated input.
            if b[*pos..].starts_with(b"true") {
                *pos += 4;
                Ok(Json::Bool(true))
            } else {
                Err("invalid literal (expected `true`)".into())
            }
        }
        b'f' => {
            if b[*pos..].starts_with(b"false") {
                *pos += 5;
                Ok(Json::Bool(false))
            } else {
                Err("invalid literal (expected `false`)".into())
            }
        }
        b'n' => {
            if b[*pos..].starts_with(b"null") {
                *pos += 4;
                Ok(Json::Null)
            } else {
                Err("invalid literal (expected `null`)".into())
            }
        }
        // Only ATTEMPT a number parse when the leading byte could plausibly
        // start one -- matches json.rs's own `value()` dispatch exactly
        // (production-hardening PR-it792): the OLD code here fell through to
        // a number-scan for ANY unrecognized byte, and an empty scan (e.g.
        // the very first byte is `X`, not digit/`-`) produced a bare
        // "invalid number" -- json.rs (and native's `k_json_parse`, which
        // mirrors it byte-for-byte) instead report "unexpected character
        // `X` at position N" for exactly this input, confirmed as a genuine
        // ai-fun-mock-response cross-engine wording divergence via live
        // repro (`KUPL_AI_MOCK_CLASSIFY=X`).
        _ if matches!(b[*pos], b'0'..=b'9' | b'-') => {
            let start = *pos;
            while *pos < b.len()
                && matches!(b[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                *pos += 1;
            }
            let s = std::str::from_utf8(&b[start..*pos]).unwrap_or("");
            // The embedded scanned text (`invalid number `{s}`` , not a bare
            // "invalid number") also mirrors json.rs exactly -- confirmed
            // via live repro (`KUPL_AI_MOCK_CLASSIFY=12.3.4`).
            s.parse::<f64>().map(Json::Num).map_err(|_| format!("invalid number `{s}`"))
        }
        _ => {
            let c = std::str::from_utf8(&b[*pos..]).ok().and_then(|s| s.chars().next());
            let cp = byte_to_char_pos(b, *pos);
            match c {
                Some(c) => Err(format!("unexpected character `{c}` at position {cp}")),
                None => Err(format!("unexpected character at position {cp}")),
            }
        }
    }
}

// ---------------- LSP server ----------------

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    // minimal percent-decoding (spaces etc.)
    let mut out = String::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Some(PathBuf::from(out))
}

/// Upper bound on a single JSON-RPC message body. Generous for real source files,
/// but refuses an absurd `Content-Length` before it becomes a pre-allocation DoS
/// (a malicious client could otherwise claim gigabytes and abort the server on the
/// `vec![0u8; content_length]` allocation).
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

fn read_message(stdin: &mut impl BufRead) -> Option<String> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if stdin.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    if content_length > MAX_MESSAGE_LEN {
        return None; // refuse absurd frame sizes rather than pre-allocate them
    }
    let mut buf = vec![0u8; content_length];
    stdin.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn send(out: &mut impl Write, body: &str) {
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

/// Compute diagnostics for a file (with unsaved buffers overriding disk) and
/// return them as an LSP `publishDiagnostics` notification body.
fn diagnostics_notification(path: &PathBuf, uri: &str, buffers: &HashMap<PathBuf, String>) -> String {
    let entry = path.display().to_string();
    let (diags, map) = match crate::loader::load_with(&entry, buffers) {
        Err((ds, map)) => (ds, map),
        Ok((program, map)) => {
            let (_, mut ds) = crate::check::check(&program);
            if !ds.iter().any(|d| d.severity == Severity::Error) {
                ds.extend(crate::effects::check_effects(&program));
            }
            (ds, map)
        }
    };

    // Only report diagnostics that belong to THIS file.
    let mut items = Vec::new();
    for d in &diags {
        let Some(file) = map
            .files
            .iter()
            .rev()
            .find(|f| d.span.start >= f.base)
        else {
            continue;
        };
        if PathBuf::from(&file.path) != *path {
            continue;
        }
        let local_start = d.span.start - file.base;
        let local_end = d.span.end.max(d.span.start + 1) - file.base;
        let (l1, c1) = line_col_utf16(&file.src, local_start);
        let (l2, c2) = line_col_utf16(&file.src, local_end);
        let severity = match d.severity {
            Severity::Error => 1,
            Severity::Warning => 2,
        };
        items.push(format!(
            "{{\"range\":{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}},\"severity\":{severity},\"code\":\"{}\",\"source\":\"kupl\",\"message\":\"{}\"}}",
            l1 - 1,
            c1 - 1,
            l2 - 1,
            c2 - 1,
            d.code,
            json_escape(&d.message)
        ));
    }
    format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{{\"uri\":\"{}\",\"diagnostics\":[{}]}}}}",
        json_escape(uri),
        items.join(",")
    )
}

// ---- language features: hover, definition, completion (read-only) ----

/// Byte offset of an LSP (line, character) position — both 0-based. Per the
/// LSP spec, `character` is a UTF-16 CODE UNIT offset -- every real client
/// (VS Code, etc.) sends positions this way. A REAL bug found+fixed
/// (production-hardening PR-it740): this used to treat `character` as a raw
/// BYTE offset instead, which is only correct for pure-ASCII lines. KUPL
/// explicitly supports non-ASCII identifiers (e.g. `café`, `日本` -- see the
/// fuzz tests below), so any line with a multi-byte UTF-8 character BEFORE
/// the target column made every position-based request (hover, goto-
/// definition, rename, completion, references) resolve to the WRONG
/// identifier -- silently, no panic, no error, just a wrong result -- once a
/// real editor's UTF-16-based `character` value was misread as a byte
/// count. Now correctly walks the target line by CHAR, converting the
/// UTF-16 unit count to the matching byte offset (`char::len_utf16` vs.
/// `char::len_utf8`); a run past the end of the line's UTF-16 length still
/// clamps to the line's full byte length, same defensive behavior as
/// before. (The output side was ALSO fixed, production-hardening PR-it764:
/// `diag::line_col`'s raw char count happens to equal the UTF-16 unit count
/// for every character KUPL's `is_ident` allows in an identifier
/// (alphanumeric BMP characters), so responses were already correctly
/// aligned with real clients for the common case -- but 4-byte/astral-plane
/// characters like emoji, which can only ever appear in a comment or string
/// literal, still under-counted every position on the SAME line. See
/// `diag::line_col_utf16`'s own doc comment for the fix and live repro.)
fn offset_at(text: &str, line: usize, character: usize) -> usize {
    let mut off = 0usize;
    for (n, l) in text.split_inclusive('\n').enumerate() {
        if n == line {
            let line_text = l.trim_end_matches('\n');
            let mut units = 0usize;
            let mut byte_off = 0usize;
            for ch in line_text.chars() {
                if units >= character {
                    break;
                }
                units += ch.len_utf16();
                byte_off += ch.len_utf8();
            }
            return off + byte_off;
        }
        off += l.len();
    }
    off.min(text.len())
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `s` is syntactically valid as a KUPL identifier -- non-empty,
/// every character passes `is_ident`, and the FIRST character additionally
/// isn't a digit (matching `lexer.rs::lex_ident`'s own dispatch condition,
/// `b'A'..=b'Z' | b'a'..=b'z' | b'_' | byte >= 0x80` -- a leading digit
/// routes to `lex_number` instead). A REAL bug found+fixed (production-
/// hardening PR-it767): `textDocument/rename`'s `newName` was accepted
/// VERBATIM with ZERO validation before being embedded into an outgoing
/// `WorkspaceEdit` -- every mainstream LSP client applies a rename edit
/// immediately and unconditionally, so an invalid `newName` silently
/// corrupted previously-working source with no error surfaced anywhere.
/// Live-confirmed BEFORE this fix via a raw LSP session: renaming to
/// `"123 bad-name!"` (not an identifier at all) and to `""` (empty) both
/// returned well-formed, "successful"-looking `WorkspaceEdit`s that would
/// produce invalid syntax (or delete the identifier entirely) if applied.
fn is_valid_new_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if is_ident(c) && !c.is_ascii_digit() => chars.all(is_ident),
        _ => false,
    }
}

/// The identifier token covering `offset`, as (name, start, end) byte offsets.
fn ident_at(text: &str, offset: usize) -> Option<(String, usize, usize)> {
    if offset > text.len() {
        return None;
    }
    // Snap to a char boundary — an editor-supplied position can land inside a
    // multi-byte UTF-8 character; slicing there would panic. Walk by whole `char`
    // so `start`/`end` are always boundaries (and non-ASCII identifiers work).
    let mut offset = offset;
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut start = offset;
    while let Some(c) = text[..start].chars().next_back() {
        if is_ident(c) {
            start -= c.len_utf8();
        } else {
            break;
        }
    }
    let mut end = offset;
    while let Some(c) = text[end..].chars().next() {
        if is_ident(c) {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    Some((text[start..end].to_string(), start, end))
}

/// Human-readable signature of a top-level item named `name`, if found.
/// Render one parameter as `name: Ty` or `name: Ty = default` (PR-it675): a
/// REAL hover/signatureHelp content-quality bug -- `ast::Param.default` (the
/// `x: Int = EXPR` syntax) was silently dropped by every LSP signature
/// renderer below, showing an incomplete/misleading signature for any
/// function using this documented language feature (e.g. a genuinely
/// optional `name: Str = "World"` parameter rendered as if it were
/// required). `fmt.rs`'s canonical formatter (the ONE place this project
/// treats as the source of truth for how KUPL source re-prints) already
/// renders defaults correctly; this mirrors that exact ` = {expr}` shape so
/// hover/signatureHelp stay consistent with `kupl fmt`'s own output.
fn param_str(p: &crate::ast::Param) -> String {
    use crate::fmt::{expr_str, ty_str};
    let mut s = format!("{}: {}", p.name, ty_str(&p.ty));
    if let Some(d) = &p.default {
        s.push_str(&format!(" = {}", expr_str(d, 0)));
    }
    s
}

/// Render a function declaration's signature (`fun name(params) -> ret uses effects`),
/// shared by top-level functions and component methods (exposed or private) so hover
/// shows the identical format regardless of where the function lives.
fn fun_sig_str(f: &crate::ast::FunDecl) -> String {
    use crate::fmt::ty_str;
    let params: Vec<String> = f.params.iter().map(param_str).collect();
    let ret = f.ret.as_ref().map(|r| format!(" -> {}", ty_str(r))).unwrap_or_default();
    let eff = if f.effects.is_empty() {
        String::new()
    } else {
        format!(" uses {}", f.effects.join(", "))
    };
    let kw = if f.ai.is_some() { "ai fun" } else { "fun" };
    format!("{kw} {}({}){ret}{eff}", f.name, params.join(", "))
}

/// Render a contract's body-less method signature (`expose fun name(params) ->
/// ret uses effects`) -- same shape as `fun_sig_str`, but `ast::FunSig` (a
/// contract method) has no body/`ai` field, unlike `ast::FunDecl`.
fn contract_sig_str(f: &crate::ast::FunSig) -> String {
    use crate::fmt::ty_str;
    let params: Vec<String> = f.params.iter().map(param_str).collect();
    let ret = f.ret.as_ref().map(|r| format!(" -> {}", ty_str(r))).unwrap_or_default();
    let eff = if f.effects.is_empty() {
        String::new()
    } else {
        format!(" uses {}", f.effects.join(", "))
    };
    format!("expose fun {}({}){ret}{eff}", f.name, params.join(", "))
}

fn item_signature(program: &crate::ast::Program, name: &str) -> Option<String> {
    use crate::ast::Item;
    use crate::fmt::ty_str;
    for item in &program.items {
        match item {
            Item::Fun(f) if f.name == name => return Some(fun_sig_str(f)),
            Item::Type(t) if t.name == name => {
                let variants: Vec<String> = t
                    .variants
                    .iter()
                    .map(|v| {
                        if v.fields.is_empty() {
                            v.name.clone()
                        } else {
                            let fs: Vec<String> = v
                                .fields
                                .iter()
                                .map(|p| format!("{}: {}", p.name, ty_str(&p.ty)))
                                .collect();
                            format!("{}({})", v.name, fs.join(", "))
                        }
                    })
                    .collect();
                return Some(format!("type {} = {}", t.name, variants.join(" | ")));
            }
            Item::Component(c) if c.name == name => {
                let head = if c.is_app { "app" } else { "component" };
                let intent =
                    c.intent.as_ref().map(|i| format!("\n{i}")).unwrap_or_default();
                return Some(format!("{head} {}{intent}", c.name));
            }
            Item::Contract(c) if c.name == name => {
                let intent =
                    c.intent.as_ref().map(|i| format!("\n{i}")).unwrap_or_default();
                return Some(format!("contract {}{intent}", c.name));
            }
            _ => {}
        }
        // constructor of a type?
        if let Item::Type(t) = item {
            for v in &t.variants {
                if v.name == name {
                    let fs: Vec<String> = v
                        .fields
                        .iter()
                        .map(|p| format!("{}: {}", p.name, ty_str(&p.ty)))
                        .collect();
                    let sig = if fs.is_empty() {
                        v.name.clone()
                    } else {
                        format!("{}({})", v.name, fs.join(", "))
                    };
                    return Some(format!("{sig}   // constructor of {}", t.name));
                }
            }
        }
        // A method (exposed or private) of a component -- before this fix, hovering
        // on ANY component method (its own declaration OR a `recv.method(...)` call
        // site) returned no hover at all, since item_signature only ever searched
        // TOP-LEVEL items; component methods live nested inside Item::Component's
        // `exposes`/`funs` lists (PR-it513).
        if let Item::Component(c) = item {
            if let Some(f) = c.exposes.iter().chain(&c.funs).find(|f| f.name == name) {
                return Some(format!("{}\n// method of component {}", fun_sig_str(f), c.name));
            }
        }
        // A contract's exposed method signature -- the same gap class as
        // component methods above, but never mirrored for `ContractDecl.sigs`:
        // hovering a contract method (its own declaration inside `contract { }`,
        // or a `recv.method(...)` call site on a contract-typed receiver)
        // returned no hover at all, since only the contract's OWN name was
        // ever matched, never its nested `sigs` list (PR-it571).
        if let Item::Contract(c) = item {
            if let Some(f) = c.sigs.iter().find(|f| f.name == name) {
                return Some(format!("{}\n// method of contract {}", contract_sig_str(f), c.name));
            }
        }
        // A component's own state field -- the SAME gap class as component
        // methods (PR-it513) and contract methods (PR-it571) above, but
        // never mirrored for `ComponentDecl.state`: hovering a state field
        // (its own declaration, or a bare reference inside a method body)
        // returned no hover at all, since only `exposes`/`funs` were ever
        // searched, never `state` (production-hardening PR-it871).
        // `item_completions`/`workspace_symbols` (this file) already treat
        // state fields as first-class named symbols; only hover/goto-
        // definition forgot them. Confirmed live before this fix via a real
        // `kupl lsp` JSON-RPC session: hovering `total` at `state total:
        // Int = 0` returned `null`, while hovering the sibling method
        // `bump` (fixed by PR-it513) correctly returned its signature.
        if let Item::Component(c) = item {
            if let Some(s) = c.state.iter().find(|s| s.name == name) {
                let ty = s.ty.as_ref().map(|t| format!(": {}", ty_str(t))).unwrap_or_default();
                return Some(format!("state {}{ty}\n// state field of component {}", s.name, c.name));
            }
        }
        // A component's own PROP -- the SAME gap class as state fields above
        // (PR-it871), never itself mirrored for `ComponentDecl.props`: hovering
        // a prop (its own declaration, or a bare reference inside a method
        // body -- props are scoped identically to state in `check.rs`'s
        // `put_props_and_state_in_scope`, just immutable) returned no hover at
        // all, since only `state`/`exposes`/`funs` were ever searched, never
        // `props` (production-hardening PR-it872). Confirmed live before this
        // fix via a real `kupl lsp` JSON-RPC session: hovering `label` at
        // `prop label: Str` returned `null`, while hovering the sibling
        // method `bump` correctly returned its signature.
        if let Item::Component(c) = item {
            if let Some(p) = c.props.iter().find(|p| p.name == name) {
                return Some(format!("prop {}: {}\n// prop of component {}", p.name, ty_str(&p.ty), c.name));
            }
        }
        // A component's own CHILD (`let child = Component(args)` inside the
        // body) -- the SAME gap class as state/props above (PR-it871/PR-it872),
        // never itself mirrored for `ComponentDecl.children`: hovering a child
        // (its own declaration, or a bare reference inside a method body, e.g.
        // `bulb.on_()`) returned no hover at all, since only `state`/`props`/
        // `exposes`/`funs` were ever searched, never `children`
        // (production-hardening PR-it873). Confirmed NOT a deliberate boundary:
        // `check.rs`'s `bind_component_env` scopes children in the EXACT SAME
        // function, via the same `ctx.scopes.insert`, as props/state --
        // `ctx.scopes.insert(&child.name, Ty::Component(child.component.clone()),
        // false)` -- so a child is just as much a first-class named identifier
        // as a prop or state field. Confirmed live before this fix via a real
        // `kupl lsp` JSON-RPC session: hovering `bulb` at `let bulb = Light()`
        // returned `null`, while hovering the sibling method `status`
        // correctly returned its signature.
        if let Item::Component(c) = item {
            if let Some(child) = c.children.iter().find(|ch| ch.name == name) {
                return Some(format!("let {}: {}\n// child of component {}", child.name, child.component, c.name));
            }
        }
    }
    None
}

/// A callable's parameter labels + full signature label, for `signatureHelp`.
/// Searches the SAME three sources as `item_signature`'s method/UFCS lookup --
/// top-level funs (covers plain calls AND UFCS, since `x.free_fn()` resolves
/// to a top-level `Fun` the same way a direct `free_fn(x)` call would),
/// component methods (`exposes`/`funs`), and contract methods (`sigs`).
fn signature_help_info(program: &crate::ast::Program, name: &str) -> Option<(String, Vec<String>)> {
    use crate::ast::Item;
    let params_of = |params: &[crate::ast::Param]| -> Vec<String> { params.iter().map(param_str).collect() };
    for item in &program.items {
        match item {
            Item::Fun(f) if f.name == name => return Some((fun_sig_str(f), params_of(&f.params))),
            Item::Component(c) => {
                if let Some(f) = c.exposes.iter().chain(&c.funs).find(|f| f.name == name) {
                    return Some((fun_sig_str(f), params_of(&f.params)));
                }
            }
            Item::Contract(c) => {
                if let Some(f) = c.sigs.iter().find(|f| f.name == name) {
                    return Some((contract_sig_str(f), params_of(&f.params)));
                }
            }
            _ => {}
        }
    }
    None
}

/// How many arguments (by SPAN) lie fully before `offset` -- the 0-based
/// index of the parameter currently being typed. A cursor still inside an
/// argument's own span counts that argument as active (not yet "past" it);
/// a cursor at/after a trailing comma counts as having moved to the next one.
fn active_param_index(spans: impl Iterator<Item = crate::diag::Span>, offset: usize) -> usize {
    spans.filter(|s| (s.end as usize) <= offset).count()
}

/// Find the INNERMOST `Call`/`MethodCall` expression whose span contains
/// `offset` (so `f(g(x, |), y)` at `|` resolves to `g`, not `f`), across
/// every function-shaped body in the program (top-level funs, component
/// exposes/funs/handlers, contract laws, top-level laws). Reuses
/// `effects::walk_block` -- the SAME shared expression walker used by effect
/// inference (including its match-arm-guard coverage, PR-it584) -- rather
/// than re-implementing a second, independent AST traversal that could drift
/// out of sync with it, per this campaign's own sibling-consistency lesson.
/// Returns (callee/method name, active parameter index).
fn find_enclosing_call(program: &crate::ast::Program, offset: usize) -> Option<(String, usize)> {
    use crate::ast::{ExprKind, Item};
    let mut best: Option<(u32, String, usize)> = None; // (span width, name, active index)
    let mut consider = |e: &crate::ast::Expr| {
        let sp = e.span;
        if (sp.start as usize) > offset || offset > (sp.end as usize) {
            return;
        }
        let width = sp.end - sp.start;
        let found = match &e.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::Ident(n) => {
                    Some((n.clone(), active_param_index(args.iter().map(|a| a.value.span), offset)))
                }
                _ => None,
            },
            ExprKind::MethodCall { name, args, .. } => {
                Some((name.clone(), active_param_index(args.iter().map(|a| a.span), offset)))
            }
            _ => None,
        };
        if let Some((n, idx)) = found {
            if best.as_ref().is_none_or(|(w, _, _)| width < *w) {
                best = Some((width, n, idx));
            }
        }
    };
    let mut visit_block = |block: &crate::ast::Block| crate::effects::walk_block(block, &mut consider);
    for item in &program.items {
        match item {
            Item::Fun(f) => visit_block(&f.body),
            Item::Component(c) => {
                for f in c.exposes.iter().chain(&c.funs) {
                    visit_block(&f.body);
                }
                for h in &c.handlers {
                    visit_block(&h.body);
                }
            }
            Item::Contract(c) => {
                for l in &c.laws {
                    visit_block(&l.body);
                }
            }
            Item::Law(l) => visit_block(&l.body),
            _ => {}
        }
    }
    best.map(|(_, name, idx)| (name, idx))
}

/// `signatureHelp` at an LSP position: the enclosing call's signature label,
/// its parameter labels, and which parameter is active (clamped into range),
/// or None if the cursor isn't inside a resolvable call's argument list.
///
/// A REAL bug found+fixed (production-hardening PR-it878, found via this
/// campaign's "re-audit a function with prior fix history" technique on the
/// `locally_bound`/local-vs-top-level-collision family -- fixed FIVE times
/// across `resolve_hover`/`resolve_hover_cross_file`/`resolve_definition`/
/// `resolve_definition_cross_file`/`occurrences_cross_file`, PR-it704/it739/
/// it741/it742/it743 -- but `signature_help_info`'s identical bare-name
/// program-item scan never received the SAME guard): a local variable
/// (`let`/parameter/lambda param) sharing a bare name with an unrelated
/// top-level `fun` used to show that unrelated `fun`'s signature/parameter
/// hints while typing a call to the LOCAL. Confirmed live before this fix:
/// `let greet = fn y { y }; greet("hi")` (shadowing an unrelated top-level
/// `fun greet(x: Str) -> Str`) reported signature help for the top-level
/// `greet`, not "no signature" as the local-closure call warrants -- matching
/// the SAME suppression convention `resolve_hover`'s own fix already
/// established (return `None` rather than attempting to resolve to the
/// local's own signature, a bigger feature not yet built).
pub fn resolve_signature_help(text: &str, line: usize, character: usize) -> Option<(String, Vec<String>, usize)> {
    let offset = offset_at(text, line, character);
    let (program, diags) = crate::parser::parse(text);
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return None;
    }
    let (name, active) = find_enclosing_call(&program, offset)?;
    if locally_bound(&program, offset, &name) {
        return None;
    }
    let (label, params) = signature_help_info(&program, &name)?;
    let active = if params.is_empty() { 0 } else { active.min(params.len() - 1) };
    Some((label, params, active))
}

/// Find a `FunDecl` (top-level or a component's `expose`d/private method) by
/// its exact span -- how a K0301 diagnostic's `info.decl.span` maps back to
/// the AST node whose signature needs editing.
fn find_fun_decl_by_span<'a>(
    program: &'a crate::ast::Program,
    span: crate::diag::Span,
) -> Option<&'a crate::ast::FunDecl> {
    use crate::ast::Item;
    for item in &program.items {
        match item {
            Item::Fun(f) if f.span == span => return Some(f),
            Item::Component(c) => {
                if let Some(f) = c.exposes.iter().chain(&c.funs).find(|f| f.span == span) {
                    return Some(f);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the effect name(s) quoted after `` `uses `` in a K0301 or K0302
/// message -- K0301's ("public but does not declare its effects — add
/// `uses X, Y`") is a comma-separated list; K0302's ("declares `uses X` but
/// never uses it") is always exactly one name, and this same rfind-based scan
/// finds it too. Parsing OUR OWN generated string is a common, low-risk
/// quick-fix pattern (the message format is fixed and covered by effects.rs's
/// own tests, which would catch drift immediately) rather than re-deriving
/// the effect set from scratch here.
fn extract_uses_names(message: &str) -> Option<String> {
    let marker = "`uses ";
    let start = message.rfind(marker)? + marker.len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// The byte offset right after a function's parameter list's closing `)`,
/// where a fresh `uses X` clause should be inserted (`fun name(params) uses
/// X -> ret { ... }`). Paren-depth-tracked so a default-value expression
/// containing its own parens (`x: Int = f(1)`) doesn't confuse the match.
fn insertion_point_after_params(text: &str, fun_span: crate::diag::Span) -> Option<usize> {
    let start = fun_span.start as usize;
    let bytes = text.as_bytes();
    let open_rel = text.get(start..)?.find('(')?;
    let mut i = start + open_rel;
    let mut depth = 0i32;
    loop {
        match bytes.get(i)? {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Locate an existing `uses X, Y` clause right after a function's parameter
/// list (reuses `insertion_point_after_params` for the "where does the clause
/// start" half rather than re-deriving that paren-depth-tracked search).
/// Returns `(range_start, range_end, effect_names)` where `range_start` is
/// right after `)` and `range_end` is right before the `->`/`{` that follows
/// -- i.e. the exact span that must be replaced to add, remove, or drop the
/// whole clause. `None` if the function has no `uses` clause at all (effect
/// names, unlike arbitrary identifiers, never contain `-` or `{`, so scanning
/// for the first occurrence of either safely finds the clause's end without
/// needing to parse the return type).
fn find_uses_clause_range(text: &str, fun_span: crate::diag::Span) -> Option<(usize, usize, Vec<String>)> {
    let after_params = insertion_point_after_params(text, fun_span)?;
    let rest = text.get(after_params..)?;
    let kw_rel = rest.len() - rest.trim_start().len();
    let trimmed = &rest[kw_rel..];
    if !trimmed.starts_with("uses") {
        return None;
    }
    let after_kw = &trimmed[4..];
    if after_kw.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None; // e.g. `usesomething`, not the `uses` keyword
    }
    let clause_start = after_params;
    let inner_start = after_params + kw_rel + 4;
    let end_rel = after_kw.find(['-', '{'])?;
    let clause_end = inner_start + end_rel;
    let effects: Vec<String> =
        text[inner_start..clause_end].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    Some((clause_start, clause_end, effects))
}

/// `codeAction`: quick-fixes derivable from the file's OWN current diagnostics
/// (computed directly, the same recipe `diagnostics_notification` already
/// uses: `check::check` then `effects::check_effects` if no hard errors --
/// reused here for consistency rather than a second, independently-written
/// diagnostics pipeline). v0 scope: THREE fixes, covering both directions of
/// the effects-declaration lint plus the case a fresh clause alone couldn't
/// handle. (1) K0301 on a function with NO existing `uses` clause at all --
/// insert the missing effect list as a fresh clause (zero-width insertion).
/// (2) K0301 on a function that ALREADY declares SOME effects but is missing
/// others (e.g. `uses io` needs to become `uses io, ai.call`) -- this was
/// PR-it587's deferred v0 gap ("K0301's diagnostic doesn't carry the existing
/// clause's span"), closed here by locating that span independently via
/// `find_uses_clause_range` (the same helper PR-it588 built for K0302) and
/// replacing the WHOLE clause with the union of the existing + missing
/// effects, rather than needing the diagnostic itself to carry that span.
/// (3) K0302 ("declares `uses X` but never uses it") -- drop just that one
/// effect name from the clause (or the whole clause, if it was the only
/// effect declared). Returns (title, replace-range start, replace-range end,
/// replacement text) -- a fresh-clause K0301 fix is a zero-width insertion
/// (start == end); the widening K0301 fix and the K0302 fix are both real
/// range replacements.
pub fn resolve_code_actions(text: &str, start_off: usize, end_off: usize) -> Vec<(String, usize, usize, String)> {
    let (program, mut diags) = crate::parser::parse(text);
    diags.extend(crate::check::check(&program).1);
    if !diags.iter().any(|d| d.severity == Severity::Error) {
        diags.extend(crate::effects::check_effects(&program));
    }
    let mut out = Vec::new();
    for d in &diags {
        if (d.span.end as usize) < start_off || (d.span.start as usize) > end_off {
            continue;
        }
        if d.code == "K0301" {
            let Some(f) = find_fun_decl_by_span(&program, d.span) else { continue };
            let Some(names) = extract_uses_names(&d.message) else { continue };
            if f.effects.is_empty() {
                let Some(insert_at) = insertion_point_after_params(text, f.span) else { continue };
                out.push((format!("Add `uses {names}`"), insert_at, insert_at, format!(" uses {names}")));
            } else {
                let Some((clause_start, clause_end, _)) = find_uses_clause_range(text, f.span) else { continue };
                let mut widened = f.effects.clone();
                for m in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !widened.iter().any(|e| e == m) {
                        widened.push(m.to_string());
                    }
                }
                out.push((
                    format!("Widen `uses` clause to add `{names}`"),
                    clause_start,
                    clause_end,
                    format!(" uses {} ", widened.join(", ")),
                ));
            }
        } else if d.code == "K0302" {
            let Some(f) = find_fun_decl_by_span(&program, d.span) else { continue };
            let Some(name) = extract_uses_names(&d.message) else { continue };
            let Some((clause_start, clause_end, effects)) = find_uses_clause_range(text, f.span) else { continue };
            if !effects.iter().any(|e| e == &name) {
                continue;
            }
            let remaining: Vec<&str> = effects.iter().map(String::as_str).filter(|e| *e != name).collect();
            let replacement =
                if remaining.is_empty() { " ".to_string() } else { format!(" uses {} ", remaining.join(", ")) };
            out.push((format!("Remove unused `uses {name}`"), clause_start, clause_end, replacement));
        }
    }
    out
}

/// The declaration range (l0, c0, l1, c1) of the top-level item named `name`,
/// as 0-based LSP positions pointing at the NAME token.
fn item_definition(text: &str, program: &crate::ast::Program, name: &str) -> Option<(usize, usize, usize, usize)> {
    use crate::ast::Item;
    let span = program.items.iter().find_map(|item| match item {
        Item::Fun(f) if f.name == name => Some(f.span),
        Item::Type(t) if t.name == name => Some(t.span),
        Item::Component(c) if c.name == name => Some(c.span),
        Item::Contract(c) if c.name == name => Some(c.span),
        Item::Type(t) => t.variants.iter().find(|v| v.name == name).map(|v| v.span),
        // A method (exposed or private) of a component -- same gap as item_signature
        // above: "go to definition" on a component method used to find nothing
        // because only TOP-LEVEL items were searched (PR-it513). Also a
        // component's own STATE field -- the SAME gap class, never mirrored
        // for `ComponentDecl.state` (production-hardening PR-it871): "go to
        // definition" on a state field found nothing at all, since only
        // `exposes`/`funs` were ever searched here.
        // Also a component's own PROP -- the SAME gap class as state above,
        // never mirrored for `ComponentDecl.props` (production-hardening
        // PR-it872): "go to definition" on a prop found nothing at all,
        // since only `exposes`/`funs`/`state` were ever searched here.
        // Also a component's own CHILD -- the SAME gap class again, never
        // mirrored for `ComponentDecl.children` (production-hardening
        // PR-it873): "go to definition" on a child reference found nothing at
        // all, since only `exposes`/`funs`/`state`/`props` were ever searched
        // here.
        Item::Component(c) => c
            .exposes
            .iter()
            .chain(&c.funs)
            .find(|f| f.name == name)
            .map(|f| f.span)
            .or_else(|| c.state.iter().find(|s| s.name == name).map(|s| s.span))
            .or_else(|| c.props.iter().find(|p| p.name == name).map(|p| p.span))
            .or_else(|| c.children.iter().find(|ch| ch.name == name).map(|ch| ch.span)),
        // A contract's exposed method signature -- same gap, never mirrored for
        // `ContractDecl.sigs` (PR-it571).
        Item::Contract(c) => c.sigs.iter().find(|f| f.name == name).map(|f| f.span),
        _ => None,
    })?;
    // locate the name token within the declaration for a precise range
    let decl_start = span.start as usize;
    let name_off = text.get(decl_start..).and_then(|s| s.find(name)).map(|i| decl_start + i)?;
    let (l0, c0) = crate::diag::line_col_utf16(text, name_off as u32);
    let (l1, c1) = crate::diag::line_col_utf16(text, (name_off + name.len()) as u32);
    Some((l0 - 1, c0 - 1, l1 - 1, c1 - 1))
}

/// Hover markdown for the symbol at an LSP position, or None.
///
/// A REAL bug found+fixed (production-hardening PR-it742): unlike
/// `resolve_definition_cross_file`/`occurrences_cross_file` (fixed by
/// PR-it704/PR-it739/PR-it741), this single-file entry point never checked
/// `locally_bound` at all -- hovering a LOCAL variable/parameter that shares
/// a bare name with an unrelated TOP-LEVEL declaration in the SAME file
/// showed that unrelated declaration's signature instead of no hover (or,
/// worse, a misleading one). Suppressing the lookup (returning `None`
/// rather than attempting to resolve to the local's own declaration site --
/// a bigger feature) matches the existing established behavior for the
/// cross-file case when no same-file match exists.
pub fn resolve_hover(text: &str, line: usize, character: usize) -> Option<String> {
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    if locally_bound(&program, off, &name) {
        return None;
    }
    let sig = item_signature(&program, &name)?;
    Some(format!("```kupl\n{sig}\n```"))
}

/// Whether `name` is bound as a parameter (or handler payload binder) of the
/// function/method/handler whose body CONTAINS `offset` -- production-
/// hardening PR-it704: `resolve_definition_cross_file`/`occurrences_cross_file`
/// only ever check whether `name` is a TOP-LEVEL item in the current file
/// (`item_definition` never looks at local bindings at all); when it isn't --
/// e.g. the cursor is on a plain function PARAMETER reference like `mean` in
/// `fun greet(mean: Str) { "hi {mean}" }` -- they used to fall through and
/// search every `use`d file for an UNRELATED top-level item sharing that same
/// bare name, silently jumping goto-definition to it or, far worse, including
/// it in a rename's `WorkspaceEdit` and corrupting a completely unrelated
/// declaration in another file. Beyond parameters/handler binders, this also
/// checks the whole function/method/handler BODY for a `let`/`var` local, a
/// `for` loop variable, a `match`/`@`-pattern binding, or a lambda parameter
/// sharing the searched name (production-hardening PR-it739, closing the gap
/// this comment used to leave as future work: a local `let mean = ...` inside
/// `fun report() { ... }` was still falling through to the cross-file search
/// and silently reaching an unrelated top-level `fun mean` in a `use`d
/// sibling file, both for goto-definition and for rename's `WorkspaceEdit`).
/// This is deliberately coarse (matches anywhere in the whole body, not
/// precise per-scope shadowing) -- the same explicitly-accepted imprecision
/// `occurrences`'s own doc comment already documents for same-file shadowing;
/// it only needs to answer "is this name EVER locally bound here," not
/// "which exact declaration does this specific use resolve to."
fn locally_bound(program: &crate::ast::Program, offset: usize, name: &str) -> bool {
    local_binding_scope(program, offset, name).is_some()
}

/// Like `locally_bound`, but on a match also returns the byte SPAN of the
/// enclosing function/method/handler body -- production-hardening PR-it741:
/// `occurrences_cross_file` used to call `locally_bound` only to decide
/// whether to ALSO search cross-file, but its SAME-FILE base list (plain
/// `occurrences(text, name)`) was computed unconditionally, with no scope
/// filtering at all. That meant a LOCAL variable sharing a name with an
/// unrelated TOP-LEVEL declaration IN THE SAME FILE (no `use` involved at
/// all -- the single most common case) still had its rename/references
/// request silently include and corrupt that unrelated top-level
/// declaration and all ITS OWN call sites -- the exact same severity bug
/// class PR-it704/PR-it739 fixed for the cross-file case, just one file
/// shorter. Returning the enclosing scope lets `occurrences_cross_file`
/// restrict the same-file occurrence list to the local's own scope instead
/// of the whole file.
fn local_binding_scope(program: &crate::ast::Program, offset: usize, name: &str) -> Option<crate::diag::Span> {
    use crate::ast::Item;
    let in_span = |span: crate::diag::Span| (span.start as usize) <= offset && offset <= (span.end as usize);
    for item in &program.items {
        match item {
            Item::Fun(f) if in_span(f.span) => {
                let bound = f.params.iter().any(|p| p.name == name) || block_binds_name(&f.body, name);
                return bound.then_some(f.span);
            }
            Item::Component(c) => {
                for f in c.exposes.iter().chain(&c.funs) {
                    if in_span(f.span) {
                        let bound = f.params.iter().any(|p| p.name == name) || block_binds_name(&f.body, name);
                        return bound.then_some(f.span);
                    }
                }
                for h in &c.handlers {
                    if in_span(h.span) {
                        let bound = h.param.as_deref() == Some(name) || block_binds_name(&h.body, name);
                        return bound.then_some(h.span);
                    }
                }
            }
            // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it855,
            // the THIRTY-FOURTH survey, found via the SAME "hardcoded item-kind list
            // forgot an entry" pattern PR-it854 fixed in repl.rs's `is_item`): a
            // top-level `law "..." { ... }` block's `body: Block` can contain `let`
            // bindings exactly like a `fun` body (`ast::Law { name, body: Block,
            // span }`), but this function only matched `Item::Fun`/`Item::Component`,
            // falling through to `_ => {}` for `Item::Law` and `Item::Contract`'s
            // nested laws -- so the it704/it739/it836 scope-safety net these two
            // functions provide never applied to law bodies at all. Live-confirmed
            // BEFORE this fix via a real `kupl lsp` rename session: renaming a LOCAL
            // `let helper` inside a `law` block also silently renamed an UNRELATED
            // top-level `fun helper` and its call site elsewhere in the file (and,
            // symmetrically, renaming the top-level `fun helper` also corrupted the
            // law's own unrelated local). Fixed by adding the identical
            // `block_binds_name`-based arm already used for `Item::Fun`.
            Item::Law(l) if in_span(l.span) => {
                return block_binds_name(&l.body, name).then_some(l.span);
            }
            Item::Contract(c) => {
                for l in &c.laws {
                    if in_span(l.span) {
                        return block_binds_name(&l.body, name).then_some(l.span);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Every function/method/handler span in `program` where `name` is locally
/// bound (a "shadow zone") -- the complement of `local_binding_scope`: that
/// function answers "is THIS specific offset inside a local binding of
/// `name`," this one answers "which regions of the file would SHADOW `name`
/// if some OTHER reference (e.g. a top-level declaration) were renamed."
/// Used by `occurrences_cross_file` to make the PR-it741 same-file scoping
/// fix symmetric: renaming a local must not reach an unrelated top-level
/// declaration (via `local_binding_scope`), and conversely renaming a
/// top-level declaration must not reach into a DIFFERENT function's
/// unrelated local of the same bare name (via this).
fn shadow_zones(program: &crate::ast::Program, name: &str) -> Vec<crate::diag::Span> {
    use crate::ast::Item;
    let mut zones = Vec::new();
    for item in &program.items {
        match item {
            Item::Fun(f) => {
                if f.params.iter().any(|p| p.name == name) || block_binds_name(&f.body, name) {
                    zones.push(f.span);
                }
            }
            Item::Component(c) => {
                for f in c.exposes.iter().chain(&c.funs) {
                    if f.params.iter().any(|p| p.name == name) || block_binds_name(&f.body, name) {
                        zones.push(f.span);
                    }
                }
                for h in &c.handlers {
                    if h.param.as_deref() == Some(name) || block_binds_name(&h.body, name) {
                        zones.push(h.span);
                    }
                }
            }
            // See the matching fix + doc comment in `local_binding_scope` above
            // (PR-it855): `Item::Law`/`Item::Contract`'s nested laws were missing
            // from this function too, the symmetric half of the same gap.
            Item::Law(l) => {
                if block_binds_name(&l.body, name) {
                    zones.push(l.span);
                }
            }
            Item::Contract(c) => {
                for l in &c.laws {
                    if block_binds_name(&l.body, name) {
                        zones.push(l.span);
                    }
                }
            }
            _ => {}
        }
    }
    zones
}

/// Whether `name` is already declared as a TOP-LEVEL item (a `fun`, a `type`
/// or one of its variant/constructor names, a `component`, or a `contract`)
/// somewhere in `program` -- a `law`'s own name is a string literal, not an
/// identifier, so it can never collide with a rename target and is excluded.
/// Used by the `textDocument/rename` handler (production-hardening PR-it787,
/// closing a gap carried forward since PR-it767/it780) to detect a genuine
/// collision BEFORE generating a rename edit that would otherwise silently
/// produce a duplicate top-level definition -- e.g. renaming `fun helper`
/// to `main` when `fun main` already exists in the SAME file. Deliberately
/// scoped to SAME-FILE top-level items only, not cross-file: a top-level
/// item in a DIFFERENT (`use`d) file lives in that file's own mangled
/// package namespace (`resolve.rs`'s `pkg$name` scheme) and is only ever
/// reachable via a qualified `pkg.name`, never a bare identifier, so it
/// cannot actually collide with a bare rename in THIS file. Also
/// deliberately does NOT check local bindings (function params/`let`s) --
/// a local legitimately shadowing a top-level name is ordinary, legal
/// scoping, not a collision.
fn top_level_item_named(program: &crate::ast::Program, name: &str) -> bool {
    use crate::ast::Item;
    program.items.iter().any(|item| match item {
        Item::Fun(f) => f.name == name,
        Item::Type(t) => t.name == name || t.variants.iter().any(|v| v.name == name),
        Item::Component(c) => c.name == name,
        Item::Contract(ct) => ct.name == name,
        Item::Law(_) => false,
    })
}

/// Whether `name` is bound anywhere inside `block` by a `let`/`var`, a `for`
/// loop variable, a `forall` property variable, a lambda parameter, or a
/// `match` pattern binding (`x`, `x @ pat`, or a `Ctor(x)` sub-pattern) --
/// the local-binding forms `locally_bound` didn't check before PR-it739.
/// Deliberately walks the WHOLE block regardless of `offset`, matching this
/// file's existing token-based, not-precisely-scope-aware approach.
fn block_binds_name(block: &crate::ast::Block, name: &str) -> bool {
    block.stmts.iter().any(|s| stmt_binds_name(s, name))
}

fn stmt_binds_name(stmt: &crate::ast::Stmt, name: &str) -> bool {
    use crate::ast::Stmt;
    match stmt {
        Stmt::Let { name: n, init, .. } => n == name || expr_binds_name(init, name),
        Stmt::Assign { target, value, .. } => expr_binds_name(target, name) || expr_binds_name(value, name),
        Stmt::Expr(e) => expr_binds_name(e, name),
        Stmt::Return(e, _) => e.as_ref().is_some_and(|e| expr_binds_name(e, name)),
        Stmt::While { cond, body, .. } => expr_binds_name(cond, name) || block_binds_name(body, name),
        Stmt::For { var, iter, body, .. } => {
            var == name || expr_binds_name(iter, name) || block_binds_name(body, name)
        }
        Stmt::Emit { arg, .. } => arg.as_ref().is_some_and(|e| expr_binds_name(e, name)),
        Stmt::Expect(e, _) => expr_binds_name(e, name),
        Stmt::Forall { vars, body, .. } => {
            vars.iter().any(|(v, _)| v == name) || block_binds_name(body, name)
        }
        Stmt::Break(_) | Stmt::Continue(_) => false,
    }
}

fn expr_binds_name(expr: &crate::ast::Expr, name: &str) -> bool {
    use crate::ast::{ExprKind, StrPiece};
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::SizedInt(_, _)
        | ExprKind::F32(_) => false,
        ExprKind::Str(pieces) => pieces.iter().any(|p| match p {
            StrPiece::Text(_) => false,
            StrPiece::Expr(e) => expr_binds_name(e, name),
        }),
        ExprKind::List(items) | ExprKind::Par(items) => items.iter().any(|e| expr_binds_name(e, name)),
        ExprKind::Call { callee, args } => {
            expr_binds_name(callee, name) || args.iter().any(|a| expr_binds_name(&a.value, name))
        }
        ExprKind::MethodCall { recv, args, .. } => {
            expr_binds_name(recv, name) || args.iter().any(|e| expr_binds_name(e, name))
        }
        ExprKind::Field { recv, .. } => expr_binds_name(recv, name),
        ExprKind::Binary { lhs, rhs, .. } => expr_binds_name(lhs, name) || expr_binds_name(rhs, name),
        ExprKind::Unary { operand, .. } => expr_binds_name(operand, name),
        ExprKind::If { cond, then_block, else_block } => {
            expr_binds_name(cond, name)
                || block_binds_name(then_block, name)
                || else_block.as_ref().is_some_and(|e| expr_binds_name(e, name))
        }
        ExprKind::BlockExpr(b) => block_binds_name(b, name),
        ExprKind::Match { scrutinee, arms } => {
            expr_binds_name(scrutinee, name)
                || arms.iter().any(|arm| {
                    pattern_binds_name(&arm.pattern, name)
                        || arm.guard.as_ref().is_some_and(|g| expr_binds_name(g, name))
                        || expr_binds_name(&arm.body, name)
                })
        }
        ExprKind::Lambda { params, body } => {
            params.iter().any(|p| p.name == name) || block_binds_name(body, name)
        }
        ExprKind::Range { lo, hi, .. } => expr_binds_name(lo, name) || expr_binds_name(hi, name),
        ExprKind::With { recv, updates } => {
            expr_binds_name(recv, name) || updates.iter().any(|(_, e)| expr_binds_name(e, name))
        }
        ExprKind::Try(e) | ExprKind::Await(e) => expr_binds_name(e, name),
    }
}

fn pattern_binds_name(pattern: &crate::ast::Pattern, name: &str) -> bool {
    use crate::ast::PatternKind;
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Int(_) | PatternKind::Bool(_) | PatternKind::Str(_) | PatternKind::Range { .. } => false,
        PatternKind::Bind(n) => n == name,
        PatternKind::Ctor { args, .. } => args.iter().any(|p| pattern_binds_name(p, name)),
        PatternKind::Or(alts) => alts.iter().any(|p| pattern_binds_name(p, name)),
        PatternKind::At { name: n, inner } => n == name || pattern_binds_name(inner, name),
    }
}

/// Resolve every `use` target in `program` to a local sibling-file path
/// relative to `dir` (the document's own directory). Shared by every
/// cross-file LSP fallback (hover/definition/completion) -- the same simple,
/// non-package resolution rule (dot-separated path segments + `.kupl`) each
/// would otherwise reimplement independently. `kupl.toml`-based package
/// dependencies are out of scope here; a `use` naming a package dependency
/// resolves to a nonexistent local path and is silently skipped by callers
/// (via `text_at_path` returning `None`), same as before this existed.
fn used_file_paths(program: &crate::ast::Program, dir: &std::path::Path) -> Vec<PathBuf> {
    program
        .uses
        .iter()
        .map(|(use_path, _span)| {
            let rel: PathBuf = use_path.split('.').collect();
            let mut fs_path = dir.join(rel);
            fs_path.set_extension("kupl");
            fs_path
        })
        .collect()
}

/// Cross-file hover: try the current file first (identical to `resolve_hover`),
/// then fall back to the same locally-`use`d sibling files that
/// `resolve_definition_cross_file` searches (see its doc comment for scope).
///
/// A REAL, previously-unfixed sibling gap (production-hardening PR-it742):
/// unlike `resolve_definition_cross_file` (fixed by PR-it704), this cross-file
/// fallback loop never checked `locally_bound` at all -- even after
/// `resolve_hover` itself gained the check above, a local reference that
/// ISN'T shadowed by a SAME-FILE top-level item (so `resolve_hover` returns
/// `None`) would still fall through here and show an UNRELATED top-level
/// item's signature from a `use`d file, the exact PR-it704 hazard class,
/// just for hover instead of goto-definition/rename -- apparently missed
/// when PR-it704 patched the other two cross-file entry points.
pub fn resolve_hover_cross_file(
    text: &str,
    line: usize,
    character: usize,
    dir: &std::path::Path,
    buffers: &HashMap<PathBuf, String>,
) -> Option<String> {
    if let Some(h) = resolve_hover(text, line, character) {
        return Some(h);
    }
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    if locally_bound(&program, off, &name) {
        return None;
    }
    for fs_path in used_file_paths(&program, dir) {
        let Some(other_text) = text_at_path(&fs_path, buffers) else { continue };
        let (other_program, _diags) = crate::parser::parse(&other_text);
        if let Some(sig) = item_signature(&other_program, &name) {
            return Some(format!("```kupl\n{sig}\n```"));
        }
    }
    None
}

/// Every occurrence of the identifier `name`, as 0-based LSP ranges. Uses the
/// LEXER, so it matches only real identifier tokens — never text inside string
/// literals or comments (an identifier inside a `{…}` interpolation IS a real
/// reference and is included). Token-based, NOT scope-aware: it finds every
/// same-named identifier in the file (a shadowing local or a same-named field
/// included) — the common simple-LSP behavior; scope-aware rename is future work.
pub fn occurrences(text: &str, name: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let line_index = LineIndex::build(text);
    collect_occurrences(text, 0, name, text, &line_index, &mut out);
    // token order is source order at each level, but interpolation occurrences are
    // discovered at their enclosing string token — sort so edits/refs are ascending.
    out.sort();
    out
}

/// Precomputed line-start byte offsets for O(log L) position resolution,
/// instead of `diag::line_col_utf16`'s O(L) full-document rescan on every
/// call (production-hardening PR-it835): a REAL, live-confirmed quadratic-
/// time bug -- `collect_occurrences` below calls `line_col_utf16` TWICE per
/// matching identifier, so for a document of length L with M occurrences of
/// the name being looked up, position resolution alone cost O(M*L),
/// genuinely quadratic. Live-confirmed via a standalone timing probe
/// (500/1000/2000/4000 occurrences of a common local variable): 3.9ms /
/// 11.8ms / 55.3ms / 197.0ms -- roughly 4x time per 2x size, the textbook
/// O(n^2) signature -- reached by entirely ordinary, non-adversarial editor
/// interactions: `textDocument/documentHighlight` fires automatically on
/// essentially every cursor placement on a symbol (not a rare, explicit
/// action like rename), and `references`/`rename` are one keystroke away.
/// Binary search over precomputed line-start offsets narrows a lookup to
/// O(log L) to find the right line, then a BOUNDED scan of just that ONE
/// line's own UTF-16 width (not the whole document) for the column --
/// turning the whole occurrence-collection-plus-position-conversion pass
/// into O(L log L) total, independent of match count. Deliberately scoped
/// to `collect_occurrences`'s specific hot path (built once in
/// `occurrences` above, threaded through the recursive calls below) rather
/// than changing `diag::line_col_utf16` itself, which has many OTHER
/// one-off callers throughout this file where a full index would be
/// needless overhead for a single lookup.
struct LineIndex {
    /// Byte offset of the start of each line; `line_starts[0] == 0`.
    line_starts: Vec<u32>,
}

impl LineIndex {
    fn build(src: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, ch) in src.char_indices() {
            if ch == '\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        LineIndex { line_starts }
    }

    /// Resolve a byte offset in `src` (the SAME string this index was built
    /// from) to 1-based (line, UTF-16 code-unit column) -- matches
    /// `diag::line_col_utf16`'s exact contract (including its clamping of
    /// an out-of-range offset to `src.len()`) byte-for-byte.
    fn resolve_utf16(&self, src: &str, offset: u32) -> (usize, usize) {
        let offset = (offset as usize).min(src.len());
        let line_idx = match self.line_starts.binary_search(&(offset as u32)) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line_idx] as usize;
        let mut col = 1;
        // `line_start` is always a valid char boundary (it's either 0 or
        // immediately follows a single-byte `\n`), but `offset` is NOT
        // guaranteed to be one -- a direct `src[line_start..offset]` slice
        // would panic if `offset` ever landed mid-character. Mirror
        // `line_col_utf16`'s own `char_indices()` + `if i >= offset { break }`
        // pattern instead (a REAL bug this fix's own differential test
        // caught before it shipped: a `🎉`-containing document with an
        // arbitrary byte offset landing inside the emoji's 4-byte encoding
        // panicked here on the first version of this function).
        for (i, ch) in src[line_start..].char_indices() {
            if line_start + i >= offset {
                break;
            }
            col += ch.len_utf16();
        }
        (line_idx + 1, col)
    }

    /// Resolve a byte offset to just its 1-based LINE number, with NO column
    /// computation -- cheaper than `resolve_utf16` for callers (like
    /// `folding_ranges`) that only ever use the line, via binary search
    /// alone with no per-line scan at all. `src_len` mirrors
    /// `resolve_utf16`'s own out-of-range clamping (to `.min(src_len)`).
    fn resolve_line(&self, src_len: usize, offset: u32) -> usize {
        let offset = (offset as usize).min(src_len);
        let line_idx = match self.line_starts.binary_search(&(offset as u32)) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        line_idx + 1
    }
}

/// Every occurrence of `name` in the current file, PLUS every occurrence in a
/// file this one reaches via `use` (a real correctness hazard fixed by
/// PR-it518: `textDocument/rename` advertises `renameProvider: true` but was
/// previously 100% single-file -- renaming a cross-file symbol from a CALL
/// SITE would silently rename ONLY that call, leaving the actual declaration
/// (in the `use`d file) untouched and the program broken).
///
/// Returns `(target_uri, l0, c0, l1, c1)` per occurrence, empty `target_uri`
/// meaning "the current file" (mirrors `resolve_definition_cross_file`'s
/// convention) so a caller building per-file edits can group by URI.
///
/// SCOPE (documented, not silently assumed): this searches ONE HOP outward
/// along the CURRENT file's own `use` statements -- the common case of
/// renaming from a call site correctly reaches the declaration. It does
/// NOT discover sibling importers (other files that also `use` the same
/// module) or reach callers when renaming FROM the declaration site itself;
/// either would require a project-wide reverse-dependency scan (enumerating
/// every `.kupl` file in the workspace), which is a genuinely bigger
/// feature and out of scope here.
///
/// CORRECTNESS (production-hardening PR-it704, correcting a claim this
/// comment used to make): this is NOT unconditionally "strictly additive...
/// never turns a correct rename into an incorrect one." A plain function
/// PARAMETER reference (never a top-level item, so invisible to this file's
/// own `occurrences`-based scoping) used to still trigger the cross-file
/// search, silently including and renaming an UNRELATED same-named
/// top-level item in a `use`d file. `offset` (the cursor's byte position in
/// `text`) is used to skip the cross-file search when `name` is a local
/// parameter/handler-binder in scope there -- see `locally_bound`.
///
/// CORRECTNESS, part 2 (production-hardening PR-it741): the SAME-FILE base
/// list used to be plain, unconditional `occurrences(text, name)` -- which
/// is deliberately token-based/not-scope-aware (see that function's own doc
/// comment) and so ALSO includes any unrelated TOP-LEVEL declaration
/// sharing the local's bare name, in the SAME file, no `use` required at
/// all. That's a plain function `mean` and an unrelated `let mean` local in
/// the SAME file getting merged into one rename -- the identical severity
/// bug class as PR-it704/PR-it739, just without a file boundary. Fixed
/// symmetrically: when `local_binding_scope` finds `name` locally bound at
/// `offset`, the same-file list is restricted to lines within that
/// enclosing function/method/handler's own span; otherwise (renaming a
/// top-level declaration, or a reference that ISN'T itself a local binding)
/// any occurrence falling inside a DIFFERENT function's `shadow_zones` --
/// i.e. a region where `name` is locally rebound to something else -- is
/// excluded, since that occurrence refers to the unrelated local, not the
/// top-level symbol being renamed. Both directions are coarse (line-range,
/// not exact-scope), matching this file's established imprecision
/// elsewhere, but enough to stop a rename from reaching a declaration in a
/// STRUCTURALLY UNRELATED part of the file.
pub fn occurrences_cross_file(
    text: &str,
    name: &str,
    offset: usize,
    dir: &std::path::Path,
    buffers: &HashMap<PathBuf, String>,
) -> Vec<(String, usize, usize, usize, usize)> {
    let (program, _diags) = crate::parser::parse(text);
    let (same_file_occ, local_scope) = scoped_occurrences(text, &program, name, offset);
    let mut out: Vec<(String, usize, usize, usize, usize)> =
        same_file_occ.into_iter().map(|(l0, c0, l1, c1)| (String::new(), l0, c0, l1, c1)).collect();
    if local_scope.is_some() {
        return out;
    }
    for fs_path in used_file_paths(&program, dir) {
        let Some(other_text) = text_at_path(&fs_path, buffers) else { continue };
        let uri = path_to_uri(&fs_path);
        // A REAL correctness bug in a MUTATING operation (production-hardening
        // PR-it876, a survey finding, independently re-verified live before
        // fixing): `scoped_occurrences` (called above for THIS file) already
        // filters same-file occurrences against `shadow_zones` when `name` is
        // a top-level symbol -- excluding any occurrence that falls inside a
        // DIFFERENT function's unrelated local of the same bare name -- the
        // SAME-file half of it704/it739's local-vs-top-level collision fix.
        // This cross-file loop never applied that SAME filtering to the OTHER
        // file's own occurrences: it called plain, unscoped `occurrences`
        // here, with no notion of the OTHER file's local shadows at all.
        // Confirmed live before this fix: renaming a genuine top-level
        // `mean(xs)` call (in `main.kupl`, `use stats`) returned FIVE
        // locations instead of the expected TWO (the call site + the real
        // `fun mean` declaration) -- the extra three were `stats.kupl`'s own
        // UNRELATED `let mean = 5.0` local inside a completely different
        // function `other()`, which would have been silently renamed too,
        // corrupting `other()`'s logic exactly like it704/it739's own
        // "corrupting a completely unrelated file" severity framing.
        let (other_program, _diags) = crate::parser::parse(&other_text);
        let other_zones = shadow_zones(&other_program, name);
        let other_line_index = LineIndex::build(&other_text);
        let other_line_range = |span: crate::diag::Span| {
            let start_line = other_line_index.resolve_line(other_text.len(), span.start);
            let end_line = other_line_index.resolve_line(other_text.len(), span.end);
            (start_line - 1)..=(end_line - 1)
        };
        out.extend(
            occurrences(&other_text, name)
                .into_iter()
                .filter(|(l0, ..)| !other_zones.iter().any(|z| other_line_range(*z).contains(l0)))
                .map(move |(l0, c0, l1, c1)| (uri.clone(), l0, c0, l1, c1)),
        );
    }
    out
}

/// Same-file occurrences of `name`, scoped by `local_binding_scope`/`shadow_zones`
/// exactly like `occurrences_cross_file`'s own same-file half -- factored out
/// (production-hardening PR-it743) so `resolve_document_highlight` shares the
/// IDENTICAL local-vs-top-level-collision protection instead of reimplementing
/// (or omitting) it. Also returns the resolved `local_binding_scope` result so
/// callers that need to know whether to ALSO search cross-file (like
/// `occurrences_cross_file`) don't have to recompute it.
fn scoped_occurrences(
    text: &str,
    program: &crate::ast::Program,
    name: &str,
    offset: usize,
) -> (Vec<(usize, usize, usize, usize)>, Option<crate::diag::Span>) {
    let local_scope = local_binding_scope(program, offset, name);
    // Built ONCE (production-hardening PR-it836, found auditing lsp.rs
    // further after the SAME class of bug it835 fixed): the OLD
    // `crate::diag::line_col(text, ...)` call here is O(L) per call, and
    // this closure runs inside a filter checked for EVERY occurrence
    // against EVERY shadow zone below -- an O(occurrences * zones) nested
    // loop that was ALSO paying an O(L) tax per comparison, giving O(M*Z*L)
    // overall. Live-confirmed CUBIC-looking scaling before this fix (100/
    // 200/400/800 functions each locally shadowing a common name, one
    // `documentHighlight` query on an unrelated top-level use of that same
    // name): 12.7ms/104.0ms/770.7ms/6.16s -- roughly 8x time per 2x size
    // (2^3), far worse than it835's quadratic finding. `line_range` only
    // ever uses the LINE component (the column is discarded, `_`), so
    // `LineIndex::resolve_line` -- binary search alone, no per-line
    // column scan -- replaces the O(L) rescan with O(log L), turning the
    // whole thing into O((M+Z) log L + M*Z): still quadratic in the
    // occurrence/zone counts themselves (a separate, more invasive
    // algorithmic-redesign concern deliberately NOT tackled here -- see
    // this fix's own commit/memory notes), but no longer ALSO
    // proportional to document length on top of that.
    let line_index = LineIndex::build(text);
    let line_range = |span: crate::diag::Span| {
        let start_line = line_index.resolve_line(text.len(), span.start);
        let end_line = line_index.resolve_line(text.len(), span.end);
        (start_line - 1)..=(end_line - 1)
    };
    let other_shadow_zones: Vec<crate::diag::Span> =
        if local_scope.is_none() { shadow_zones(program, name) } else { Vec::new() };
    let occ = occurrences(text, name)
        .into_iter()
        .filter(|(l0, ..)| match local_scope {
            Some(span) => line_range(span).contains(l0),
            None => !other_shadow_zones.iter().any(|z| line_range(*z).contains(l0)),
        })
        .collect();
    (occ, local_scope)
}

/// Scan `text` (a full document, or the raw source of a string-interpolation
/// `{expr}` at absolute byte offset `base`) for identifier uses of `name`,
/// recursing into nested interpolations. Positions are line/col in `full`.
fn collect_occurrences(
    text: &str,
    base: u32,
    name: &str,
    full: &str,
    line_index: &LineIndex,
    out: &mut Vec<(usize, usize, usize, usize)>,
) {
    let (tokens, _diags) = crate::lexer::lex(text);
    for t in &tokens {
        match &t.tok {
            crate::token::Tok::Ident(s) if s == name => {
                let (l0, c0) = line_index.resolve_utf16(full, base + t.span.start);
                let (l1, c1) = line_index.resolve_utf16(full, base + t.span.end);
                out.push((l0 - 1, c0 - 1, l1 - 1, c1 - 1));
            }
            // `"…{x}…"` — the interpolated expression is captured raw inside the
            // string token, so its identifier uses (real references, updated by a
            // rename) are found by scanning the expression source at its offset.
            crate::token::Tok::Str(parts) => {
                for p in parts {
                    if let crate::token::StrPart::Expr(raw, expr_start) = p {
                        collect_occurrences(raw, *expr_start, name, full, line_index, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The identifier under the cursor (for references/rename resolution).
fn ident_under(text: &str, line: usize, character: usize) -> Option<String> {
    let off = offset_at(text, line, character);
    ident_at(text, off).map(|(n, _, _)| n)
}

/// Definition location (0-based range) for the symbol at an LSP position.
///
/// Same PR-it742 fix as `resolve_hover` above: a local reference sharing a
/// bare name with an unrelated top-level declaration in the SAME file must
/// not navigate goto-definition to that unrelated declaration. This ALSO
/// fixes the corresponding gap in `resolve_definition_cross_file`, which
/// calls this function FIRST and only checks `locally_bound` itself in the
/// fallback branch reached when this returns `None` -- previously, when a
/// same-file top-level match existed, this function returned `Some(..)`
/// unconditionally and `resolve_definition_cross_file`'s own `locally_bound`
/// guard was never even reached.
pub fn resolve_definition(text: &str, line: usize, character: usize) -> Option<(usize, usize, usize, usize)> {
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    if locally_bound(&program, off, &name) {
        return None;
    }
    item_definition(text, &program, &name)
}

/// Cross-file go-to-definition: try the current file first (identical to
/// `resolve_definition`), then fall back to every file this document reaches
/// via its own `use` statements, resolved LOCALLY (relative to `dir`, the
/// document's own directory -- the common multi-file-module case demonstrated
/// by examples/multifile; `kupl.toml`-based package dependencies are out of
/// scope here and simply fall through to `None` on a miss, same as before).
/// Before this (PR-it516), a symbol pulled in via `use` -- e.g. `mean(xs)` in
/// a file that does `use lib.stats` -- had NO hover and NO go-to-definition
/// at all, since resolve_hover/resolve_definition only ever see the single
/// buffer they're handed.
///
/// Returns `(target_uri, l0, c0, l1, c1)` where an EMPTY `target_uri` means
/// "the current file" (the caller reuses the request's own uri); a non-empty
/// one names the OTHER file the definition actually lives in.
pub fn resolve_definition_cross_file(
    text: &str,
    line: usize,
    character: usize,
    dir: &std::path::Path,
    buffers: &HashMap<PathBuf, String>,
) -> Option<(String, usize, usize, usize, usize)> {
    if let Some((l0, c0, l1, c1)) = resolve_definition(text, line, character) {
        return Some((String::new(), l0, c0, l1, c1));
    }
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    if locally_bound(&program, off, &name) {
        return None;
    }
    for fs_path in used_file_paths(&program, dir) {
        let Some(other_text) = text_at_path(&fs_path, buffers) else { continue };
        let (other_program, _diags) = crate::parser::parse(&other_text);
        if let Some((l0, c0, l1, c1)) = item_definition(&other_text, &other_program, &name) {
            return Some((path_to_uri(&fs_path), l0, c0, l1, c1));
        }
    }
    None
}

/// Completion candidates from this document's own declared items (no keywords) --
/// the part of `completions` that's meaningful to merge across files.
fn item_completions(program: &crate::ast::Program) -> Vec<(String, u8, String)> {
    use crate::ast::Item;
    let mut out: Vec<(String, u8, String)> = Vec::new();
    for item in &program.items {
        match item {
            Item::Fun(f) => {
                let sig = item_signature(program, &f.name).unwrap_or_default();
                out.push((f.name.clone(), 3, sig)); // 3 = Function
            }
            Item::Type(t) => {
                out.push((t.name.clone(), 22, format!("type {}", t.name))); // 22 = Struct
                for v in &t.variants {
                    let sig = item_signature(program, &v.name).unwrap_or_default();
                    out.push((v.name.clone(), 4, sig)); // 4 = Constructor
                }
            }
            Item::Component(c) => {
                out.push((c.name.clone(), 7, format!("component {}", c.name))); // 7 = Class
                // Component methods (exposed or private) and state fields used to be
                // completely invisible to completion -- only the component's OWN name
                // was listed, the same gap class fixed in item_signature/item_definition
                // for hover/go-to-definition (PR-it513); extend the same nested search
                // here so typing `n` or `greet` inside a component body autocompletes
                // (PR-it514).
                for f in c.exposes.iter().chain(&c.funs) {
                    out.push((f.name.clone(), 3, fun_sig_str(f))); // 3 = Function
                }
                for s in &c.state {
                    out.push((s.name.clone(), 6, format!("state {}", s.name))); // 6 = Variable
                }
                // Props used to be completely invisible to completion, the SAME gap
                // class as state fields above -- typing a prop name inside a
                // component body (e.g. `label` in a method that just reads a prop)
                // got no completion for it at all, since only `state` was ever
                // pushed here, never `props` (production-hardening PR-it872).
                for p in &c.props {
                    out.push((p.name.clone(), 6, format!("prop {}", p.name))); // 6 = Variable
                }
                // Children used to be completely invisible to completion too, the
                // SAME gap class as props/state above -- typing a child's own name
                // inside a component body (e.g. `bulb` in a method that calls
                // `bulb.on_()`) got no completion for it at all, since only
                // `state`/`props` were ever pushed here, never `children`
                // (production-hardening PR-it873).
                for child in &c.children {
                    out.push((child.name.clone(), 6, format!("let {}: {}", child.name, child.component))); // 6 = Variable
                }
            }
            Item::Contract(c) => {
                out.push((c.name.clone(), 8, format!("contract {}", c.name))); // 8 = Interface
                // Contract method signatures used to be completely invisible to
                // completion -- only the contract's OWN name was listed, the same
                // gap class fixed for component methods/state above (PR-it571).
                for f in &c.sigs {
                    out.push((f.name.clone(), 3, contract_sig_str(f))); // 3 = Function
                }
            }
            _ => {}
        }
    }
    out
}

/// A completion candidate: (label, LSP CompletionItemKind, detail).
pub fn completions(text: &str) -> Vec<(String, u8, String)> {
    let (program, _diags) = crate::parser::parse(text);
    let mut out = item_completions(&program);
    // language keywords (14 = Keyword)
    for kw in [
        "fun", "type", "component", "app", "contract", "match", "if", "else", "for", "while",
        "let", "var", "return", "true", "false", "uses", "expose", "state", "on", "emit", "wire",
    ] {
        out.push((kw.to_string(), 14, String::new()));
    }
    out
}

/// Cross-file completion: this document's own candidates (identical to
/// `completions`, keywords included), PLUS the item-level candidates
/// (functions/types/constructors/components/contracts/methods/state) from
/// every locally-`use`d sibling file. Before this (PR-it517), a name pulled
/// in via `use` -- e.g. `mean`/`label` in a file that does `use lib.stats` /
/// `use util` -- never autocompleted at all, the same gap class already
/// fixed for hover/go-to-definition (PR-it516).
pub fn completions_cross_file(
    text: &str,
    dir: &std::path::Path,
    buffers: &HashMap<PathBuf, String>,
) -> Vec<(String, u8, String)> {
    let (program, _diags) = crate::parser::parse(text);
    let mut out = completions(text);
    for fs_path in used_file_paths(&program, dir) {
        let Some(other_text) = text_at_path(&fs_path, buffers) else { continue };
        let (other_program, _diags) = crate::parser::parse(&other_text);
        out.extend(item_completions(&other_program));
    }
    out
}

/// Extract (uri, line, character) from a textDocument/position params object.
fn position_of(params: &Json) -> Option<(&str, usize, usize)> {
    let uri = params.get("textDocument")?.get("uri")?.str()?;
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_usize()?;
    let ch = pos.get("character")?.as_usize()?;
    Some((uri, line, ch))
}

/// `textDocument/documentSymbol`: an outline of the file's items (functions,
/// types, components, contracts, top-level `law`s), for "Go to Symbol"/
/// breadcrumbs/outline-view support. `None` on parse errors (nothing safe to
/// outline). Components are expanded into NESTED children (state fields,
/// exposed/private methods) -- built that way from the start, rather than as
/// a top-level-only pass needing a follow-up fix, since exactly that gap
/// (searching only `program.items`, blind to `Item::Component`'s nested
/// members) was the root cause behind THREE separate real bugs already this
/// campaign (hover/definition it513, completions it514).
fn document_symbols(text: &str) -> Option<String> {
    let (program, diags) = crate::parser::parse(text);
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return None;
    }
    let line_index = LineIndex::build(text);
    let syms: Vec<String> = program.items.iter().map(|item| item_symbol(text, item, &line_index)).collect();
    Some(format!("[{}]", syms.join(",")))
}

/// LSP `Range` for a span, rendered inline as a JSON object literal.
///
/// Takes a precomputed `&LineIndex` (production-hardening PR-it836, a
/// follow-up to it835's `collect_occurrences` fix mining the SAME
/// per-symbol full-document-rescan shape): `item_symbol`/
/// `maybe_push_symbol_info` call this once per SYMBOL in a file (functions,
/// types+variants, components+their nested state/methods, contracts+sigs),
/// so `textDocument/documentSymbol` and `workspace/symbol` were ALSO O(S*L)
/// -- live-confirmed via a standalone timing probe (500/1000/2000/4000
/// functions in one file): `document_symbols` took 6.4ms/22.6ms/84.9ms/
/// 334.5ms, `folding_ranges` (a separate but analogous bug, see below) took
/// 5.0ms/21.5ms/74.5ms/308.6ms -- both ~3.5-4x time per 2x size, the same
/// O(n^2) signature as it835's finding. `LineIndex` is built ONCE per file
/// by each of this function's callers and threaded through, instead of
/// each of the S symbols in that file independently rescanning the whole
/// document.
fn lsp_range(line_index: &LineIndex, text: &str, span: crate::diag::Span) -> String {
    let (l0, c0) = line_index.resolve_utf16(text, span.start);
    let (l1, c1) = line_index.resolve_utf16(text, span.end);
    format!(
        "{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}}",
        l0 - 1,
        c0 - 1,
        l1 - 1,
        c1 - 1
    )
}

/// `DocumentSymbol` JSON. `range`/`selectionRange` are the same span here (no
/// separate name-only span is tracked on these AST nodes) -- valid per spec
/// (selectionRange must be contained in range; equal satisfies that trivially).
/// `detail` is LSP's own "more detail for this symbol, e.g. the signature of
/// a function" field (PR-it675 follow-up, PR-it676): hover/completion/
/// signatureHelp all show a callable's full signature (params, return type,
/// effects, and -- since it675 -- default values), but the outline/breadcrumb
/// view (`documentSymbol`) used to show ONLY bare names for every symbol,
/// even though the very field the LSP spec exists FOR was sitting right there
/// unused. Empty string omits the field entirely (an empty `"detail":""`
/// would be technically valid but visually noisy in most editors' outline
/// views for symbols with no natural signature, like a bare `type`/`state`).
fn symbol_json(name: &str, kind: u8, range: &str, detail: &str, children: &[String]) -> String {
    let children_part =
        if children.is_empty() { String::new() } else { format!(",\"children\":[{}]", children.join(",")) };
    let detail_part = if detail.is_empty() { String::new() } else { format!(",\"detail\":\"{}\"", json_escape(detail)) };
    format!(
        "{{\"name\":\"{}\",\"kind\":{kind},\"range\":{range},\"selectionRange\":{range}{detail_part}{children_part}}}",
        json_escape(name)
    )
}

/// A constructor's field-list signature, e.g. `Circle(r: Float)` -- empty for
/// a fieldless variant (`Nothing`), since that would just repeat the `name`
/// field verbatim, which `symbol_json` correctly treats as "nothing to add"
/// the same way it does for a `type`/`component`/`contract`'s own entry.
fn variant_detail(v: &crate::ast::Variant) -> String {
    if v.fields.is_empty() {
        String::new()
    } else {
        format!("{}({})", v.name, v.fields.iter().map(param_str).collect::<Vec<_>>().join(", "))
    }
}

/// LSP `SymbolKind` numeric codes used here: Method=6, Function=12, Field=8,
/// EnumMember=22, Enum=10, Class=5, Interface=11.
fn item_symbol(text: &str, item: &crate::ast::Item, line_index: &LineIndex) -> String {
    use crate::ast::Item;
    use crate::fmt::ty_str;
    match item {
        Item::Fun(f) => symbol_json(&f.name, 12, &lsp_range(line_index, text, f.span), &fun_sig_str(f), &[]),
        Item::Type(t) => {
            let children: Vec<String> = t
                .variants
                .iter()
                .map(|v| symbol_json(&v.name, 22, &lsp_range(line_index, text, v.span), &variant_detail(v), &[]))
                .collect();
            symbol_json(&t.name, 10, &lsp_range(line_index, text, t.span), "", &children)
        }
        Item::Contract(c) => {
            let children: Vec<String> = c
                .sigs
                .iter()
                .map(|s| symbol_json(&s.name, 6, &lsp_range(line_index, text, s.span), &contract_sig_str(s), &[]))
                .collect();
            symbol_json(&c.name, 11, &lsp_range(line_index, text, c.span), "", &children)
        }
        Item::Law(l) => symbol_json(&l.name, 12, &lsp_range(line_index, text, l.span), "", &[]),
        Item::Component(c) => {
            let mut children: Vec<String> = c
                .state
                .iter()
                .map(|s| {
                    let detail = s.ty.as_ref().map(ty_str).unwrap_or_default();
                    symbol_json(&s.name, 8, &lsp_range(line_index, text, s.span), &detail, &[])
                })
                .collect();
            // Props used to be entirely absent from the document/workspace symbol
            // outline, the SAME gap class as state fields above -- a component's
            // own declared props never appeared as child symbols at all, since
            // only `state`/`exposes`/`funs` were ever walked here, never `props`
            // (production-hardening PR-it872).
            children.extend(
                c.props
                    .iter()
                    .map(|p| symbol_json(&p.name, 8, &lsp_range(line_index, text, p.span), &ty_str(&p.ty), &[])),
            );
            // Children (`let child = Component(args)`) used to be entirely absent
            // from the outline too, the SAME gap class as props/state above -- a
            // component's own declared children never appeared as child symbols
            // at all, since only `state`/`props`/`exposes`/`funs` were ever walked
            // here, never `children` (production-hardening PR-it873).
            children.extend(
                c.children
                    .iter()
                    .map(|ch| symbol_json(&ch.name, 8, &lsp_range(line_index, text, ch.span), &ch.component, &[])),
            );
            children.extend(
                c.exposes
                    .iter()
                    .map(|f| symbol_json(&f.name, 6, &lsp_range(line_index, text, f.span), &fun_sig_str(f), &[])),
            );
            children.extend(
                c.funs
                    .iter()
                    .map(|f| symbol_json(&f.name, 6, &lsp_range(line_index, text, f.span), &fun_sig_str(f), &[])),
            );
            symbol_json(&c.name, 5, &lsp_range(line_index, text, c.span), "", &children)
        }
    }
}

/// Collect every span in `item` an editor could reasonably want a fold
/// chevron for -- deliberately WIDER than `item_symbol`'s children (which
/// only surfaces state fields + exposed/private methods for outline
/// purposes): a component's `on X { ... }` handlers and `example { ... }`
/// blocks have real, often-long bodies too, and a contract's `law "..." {
/// ... }` bodies (NOT currently in `item_symbol`'s children at all, since
/// outline and folding are different concerns -- a law's body doesn't need
/// its own outline entry to still deserve a fold arrow).
fn foldable_spans(item: &crate::ast::Item, out: &mut Vec<crate::diag::Span>) {
    use crate::ast::Item;
    match item {
        Item::Fun(f) => out.push(f.span),
        Item::Type(t) => out.push(t.span),
        Item::Contract(c) => {
            out.push(c.span);
            out.extend(c.laws.iter().map(|l| l.span));
        }
        Item::Law(l) => out.push(l.span),
        Item::Component(c) => {
            out.push(c.span);
            out.extend(c.exposes.iter().map(|f| f.span));
            out.extend(c.funs.iter().map(|f| f.span));
            out.extend(c.handlers.iter().map(|h| h.span));
            out.extend(c.examples.iter().map(|e| e.span));
        }
    }
}

/// `textDocument/foldingRange`: one `FoldingRange` per multi-line foldable
/// span (see `foldable_spans`) -- a single-line span is skipped, since
/// folding a declaration that's already on one line is meaningless (and some
/// clients render a same-line start/end fold as a visual glitch).
fn folding_ranges(text: &str) -> Option<String> {
    let (program, diags) = crate::parser::parse(text);
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return None;
    }
    let mut spans = Vec::new();
    for item in &program.items {
        foldable_spans(item, &mut spans);
    }
    let line_index = LineIndex::build(text);
    let ranges: Vec<String> = spans
        .into_iter()
        .filter_map(|span| {
            let l0 = line_index.resolve_line(text.len(), span.start);
            let l1 = line_index.resolve_line(text.len(), span.end);
            if l0 == l1 {
                return None;
            }
            Some(format!("{{\"startLine\":{},\"endLine\":{}}}", l0 - 1, l1 - 1))
        })
        .collect();
    Some(format!("[{}]", ranges.join(",")))
}

/// Below this many collected files, `collect_kupl_files` keeps recursing --
/// a hard ceiling so a huge tree can't hang the server.
const MAX_WORKSPACE_FILES: usize = 5000;

/// Recursively collect every `.kupl` file under `root`, skipping hidden
/// directories (`.git`, editor dirs) and `target` -- this repo's OWN build
/// output is enormous; scanning it would make `workspace/symbol` pathologically
/// slow on a real KUPL checkout, for files that were never source anyway.
///
/// A REAL bug found+fixed (production-hardening PR-it732): this used
/// `path.is_dir()`, which FOLLOWS symlinks (it calls `fs::metadata`, not
/// `symlink_metadata`) -- so a symlinked directory was walked exactly like
/// an ordinary one, including a directory symlinked back to itself or an
/// ancestor. A live revert-and-verify test (constructing exactly that
/// cycle) DISPROVED the initially-suspected stack-overflow crash, though:
/// this function builds its `root` argument by repeated `entry.path()`
/// string concatenation, so the constructed path grows by at least one
/// path component on EVERY recursive call regardless of what the
/// underlying symlinks resolve to at the OS level -- a cyclic symlink
/// therefore hits the OS's path-length limit (`ENAMETOOLONG`, already
/// handled cleanly by the `let Ok(entries) = ... else { return }` above)
/// after a few hundred to a couple thousand recursions, FAR below what's
/// needed to exhaust even a modest thread stack. So this is NOT the
/// uncatchable-crash class of bug that `json::MAX_JSON_DEPTH`/
/// `kx.rs::decode_shape`/`regex.rs`'s group-nesting cap fixed. It's still
/// worth fixing on its own, lower-severity terms: following a symlinked
/// directory means a workspace scan can silently re-visit and duplicate
/// content already reachable another way (confusing `workspace/symbol`
/// results with the same symbol reported twice under different paths), and
/// a self-referencing cycle still wastes hundreds of pointless syscalls
/// before erroring out. Standard directory-walking tools (ripgrep, fd,
/// most language servers) deliberately do NOT follow symlinks during a
/// workspace scan for exactly these reasons. Fixed by switching to
/// `entry.file_type()` (backed by `symlink_metadata`, which does NOT
/// follow symlinks) instead of `path.is_dir()` -- a symlinked directory now
/// reports `is_dir() == false` and is simply never recursed into.
fn collect_kupl_files(root: &std::path::Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_WORKSPACE_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_WORKSPACE_FILES {
            return;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            if name.starts_with('.') || name == "target" {
                continue;
            }
            collect_kupl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "kupl") {
            out.push(path);
        }
    }
}

/// `workspace/symbol`: every item (top-level or nested inside a component)
/// across every `.kupl` file under `root` whose name contains `query`
/// (case-insensitive substring -- the common simple-server convention),
/// rendered as FLAT `SymbolInformation` JSON. Genuinely different response
/// SHAPE from `document_symbols`' nested per-file `DocumentSymbol`s: each
/// entry here carries its own `location.uri` since results span many files.
/// A file with parse errors is silently skipped (nothing safe to index),
/// mirroring `document_symbols`'s own per-file gate.
fn workspace_symbols(root: &std::path::Path, query: &str, buffers: &HashMap<PathBuf, String>) -> String {
    let mut files = Vec::new();
    collect_kupl_files(root, &mut files);
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for path in files {
        let Some(text) = text_at_path(&path, buffers) else { continue };
        let (program, diags) = crate::parser::parse(&text);
        if diags.iter().any(|d| d.severity == Severity::Error) {
            continue;
        }
        let uri = path_to_uri(&path);
        // Built ONCE per file (production-hardening PR-it836), not once per
        // symbol -- see `lsp_range`'s own doc comment for the full
        // live-confirmed O(S*L) latency bug this closes.
        let line_index = LineIndex::build(&text);
        for item in &program.items {
            collect_workspace_symbol_matches(&text, &uri, item, &needle, &line_index, &mut out);
        }
    }
    format!("[{}]", out.join(","))
}

fn maybe_push_symbol_info(
    out: &mut Vec<String>,
    text: &str,
    uri: &str,
    name: &str,
    kind: u8,
    span: crate::diag::Span,
    needle: &str,
    line_index: &LineIndex,
) {
    if needle.is_empty() || name.to_lowercase().contains(needle) {
        out.push(format!(
            "{{\"name\":\"{}\",\"kind\":{kind},\"location\":{{\"uri\":\"{}\",\"range\":{}}}}}",
            json_escape(name),
            json_escape(uri),
            lsp_range(line_index, text, span)
        ));
    }
}

fn collect_workspace_symbol_matches(
    text: &str,
    uri: &str,
    item: &crate::ast::Item,
    needle: &str,
    line_index: &LineIndex,
    out: &mut Vec<String>,
) {
    use crate::ast::Item;
    match item {
        Item::Fun(f) => maybe_push_symbol_info(out, text, uri, &f.name, 12, f.span, needle, line_index),
        Item::Type(t) => {
            maybe_push_symbol_info(out, text, uri, &t.name, 10, t.span, needle, line_index);
            for v in &t.variants {
                maybe_push_symbol_info(out, text, uri, &v.name, 22, v.span, needle, line_index);
            }
        }
        Item::Contract(c) => {
            maybe_push_symbol_info(out, text, uri, &c.name, 11, c.span, needle, line_index);
            for s in &c.sigs {
                maybe_push_symbol_info(out, text, uri, &s.name, 6, s.span, needle, line_index);
            }
        }
        Item::Law(l) => maybe_push_symbol_info(out, text, uri, &l.name, 12, l.span, needle, line_index),
        Item::Component(c) => {
            maybe_push_symbol_info(out, text, uri, &c.name, 5, c.span, needle, line_index);
            for s in &c.state {
                maybe_push_symbol_info(out, text, uri, &s.name, 8, s.span, needle, line_index);
            }
            // Props used to be entirely absent from `workspace/symbol` search
            // results, the SAME gap class as state fields above -- searching for
            // a prop's own name across the workspace found nothing, since only
            // `state`/`exposes`/`funs` were ever walked here, never `props`
            // (production-hardening PR-it872).
            for p in &c.props {
                maybe_push_symbol_info(out, text, uri, &p.name, 8, p.span, needle, line_index);
            }
            // Children used to be entirely absent from `workspace/symbol` search
            // results too, the SAME gap class as props/state above -- searching
            // for a child's own name across the workspace found nothing, since
            // only `state`/`props`/`exposes`/`funs` were ever walked here, never
            // `children` (production-hardening PR-it873).
            for child in &c.children {
                maybe_push_symbol_info(out, text, uri, &child.name, 8, child.span, needle, line_index);
            }
            for f in &c.exposes {
                maybe_push_symbol_info(out, text, uri, &f.name, 6, f.span, needle, line_index);
            }
            for f in &c.funs {
                maybe_push_symbol_info(out, text, uri, &f.name, 6, f.span, needle, line_index);
            }
        }
    }
}

/// `textDocument/documentHighlight`: every occurrence of the identifier under
/// the cursor, WITHIN THE CURRENT DOCUMENT ONLY. Deliberately single-file,
/// unlike `references`/`rename` (it518's cross-file fix) -- the LSP spec
/// defines `documentHighlight` as highlighting occurrences in the current
/// document, so `occurrences` (not `occurrences_cross_file`) is the
/// spec-correct choice here, not a scope cut.
///
/// A REAL bug found+fixed (production-hardening PR-it743, closing out the
/// `locally_bound`/local-vs-top-level-collision family -- see PR-it704/it739/
/// it741/it742): this used to call plain `occurrences(text, &name)`
/// unconditionally, with zero scope filtering, so highlighting a LOCAL
/// variable sharing a bare name with an unrelated TOP-LEVEL declaration in
/// the SAME file also highlighted that unrelated declaration and its own
/// call sites. Cosmetic severity (a visual highlight only, never
/// destructive/navigational, unlike rename/goto-definition), but the same
/// root cause -- now shares `scoped_occurrences` with `occurrences_cross_file`
/// so this bug family's fix is applied uniformly across every call site.
fn resolve_document_highlight(text: &str, line: usize, character: usize) -> Option<String> {
    let off = offset_at(text, line, character);
    let name = ident_under(text, line, character)?;
    let (program, _diags) = crate::parser::parse(text);
    let (occ, _local_scope) = scoped_occurrences(text, &program, &name, off);
    let locs: Vec<String> = occ
        .into_iter()
        .map(|(l0, c0, l1, c1)| {
            format!("{{\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}}}}")
        })
        .collect();
    Some(format!("[{}]", locs.join(",")))
}

/// Current text of a document: the unsaved editor buffer if present, else disk.
fn doc_text(uri: &str, buffers: &HashMap<PathBuf, String>) -> Option<String> {
    let path = uri_to_path(uri)?;
    text_at_path(&path, buffers)
}

/// `textDocument/formatting`: `None` if the source has parse errors (nothing
/// safe to format, matches `kupl fmt`'s own gate). Otherwise a JSON array of
/// LSP `TextEdit`s (a single whole-document replacement, or `[]` if already
/// formatted).
///
/// SAFETY GATE: `[]` (a safe no-op) is also returned when the source contains
/// comments -- `fmt::format_program` renders from the AST, which has no
/// comment nodes, so it silently DROPS every comment (the CLI's `kupl fmt`
/// only gets away with this because it prints a `note:` the user sees before
/// deciding to `--write`). Format-on-save triggers with no such chance to
/// warn first: wiring this straight to `format_program` would mean opening
/// an editor with format-on-save enabled SILENTLY deletes every comment in
/// the file on the very first keystroke+save. That is a correctness hazard
/// on the same footing as it518's cross-file rename gap (a MUTATING LSP
/// operation firing incorrectly, not just a missing capability) -- so this
/// stays a no-op for commented files until the formatter preserves them.
fn resolve_formatting(text: &str) -> Option<String> {
    let (program, diags) = crate::parser::parse(text);
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return None;
    }
    if crate::fmt::source_has_comments(text) {
        return Some("[]".to_string());
    }
    let formatted = crate::fmt::format_program(&program);
    if formatted == text {
        return Some("[]".to_string());
    }
    // The SAME safety net as `kupl fmt --write` (production-hardening
    // PR-it837): `format_program` can, for at least one confirmed case (a
    // Float/F32 literal overflowing to infinity, silently accepted with no
    // diagnostic), render text that does NOT compile -- `inf` re-lexes as a
    // bare identifier, syntactically valid (a PLAIN re-parse would MISS
    // this), but fails "unknown name" (K0240) once the full checker runs.
    // Format-on-save applying such an edit would corrupt the buffer with NO
    // chance for the user to notice or undo (unlike the CLI, which at least
    // prints `formatted: <file>` after the fact). A safe no-op (`[]`, the
    // SAME return already used for the comments/already-formatted cases
    // just above) is far better than proposing a destructive edit.
    if crate::run::compile(&formatted).is_err() {
        return Some("[]".to_string());
    }
    let (end_line, end_col) = line_col_utf16(text, text.len() as u32);
    Some(format!(
        "[{{\"range\":{{\"start\":{{\"line\":0,\"character\":0}},\"end\":{{\"line\":{},\"character\":{}}}}},\"newText\":\"{}\"}}]",
        end_line - 1,
        end_col - 1,
        json_escape(&formatted)
    ))
}

/// Current text at a filesystem path: the unsaved editor buffer if that file
/// happens to be open, else disk. Shared by `doc_text` (uri-keyed, the normal
/// per-request entry point) and cross-file lookups (path-keyed, since a `use`
/// target is resolved to a path before we know whether it's open).
fn text_at_path(path: &std::path::Path, buffers: &HashMap<PathBuf, String>) -> Option<String> {
    if let Some(buf) = buffers.get(path) {
        return Some(buf.clone());
    }
    std::fs::read_to_string(path).ok()
}

/// The inverse of `uri_to_path`: percent-encode a filesystem path into a
/// `file://` URI. Only bytes outside the RFC 3986 "unreserved" set are
/// escaped, so ordinary paths round-trip through `uri_to_path` unchanged.
fn path_to_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn serve() -> i32 {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    // open editor buffers (unsaved contents)
    let mut buffers: HashMap<PathBuf, String> = HashMap::new();
    // workspace root, for `workspace/symbol`'s whole-project file enumeration
    // -- unset until `initialize` supplies `rootUri` (`rootPath` as a fallback
    // for older clients); `workspace/symbol` is a safe no-op ("[]") without it.
    let mut workspace_root: Option<PathBuf> = None;

    while let Some(body) = read_message(&mut stdin) {
        // A robustness-audit finding (production-hardening PR-it620): a
        // message whose top-level JSON fails to parse used to be silently
        // dropped (`continue`) -- fine for a malformed NOTIFICATION (no
        // response expected anyway), but for a REQUEST (has an `id`), the
        // client is left waiting forever for a reply that will never come.
        // This became newly reachable once `parse_json` gained a nesting-
        // depth guard (same iteration): a deeply-nested `params` value used
        // to crash the whole process (a stack overflow); after the guard,
        // it cleanly returns `Err` instead -- which then fell straight into
        // this SAME silent-drop path, turning a crash into an indefinite
        // hang instead of actually fixing it. Per the JSON-RPC 2.0 spec's
        // own convention for a parse error (code -32700): respond with
        // `id: null`, since a message that failed to parse can't reliably
        // have its own `id` extracted either.
        let Ok(msg) = parse_json(&body) else {
            send(&mut stdout, "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}");
            continue;
        };
        // A REAL, live-confirmed silent-hang-the-client bug found+fixed
        // (production-hardening PR-it755), the SAME root cause and severity
        // class as PR-it620's own parse-error fix above, just a DIFFERENT
        // malformed message SHAPE that fix didn't cover: valid JSON whose
        // TOP-LEVEL value is an array (a base JSON-RPC 2.0 "batch") rather
        // than an object. `Json::get`/`msg.get("method")`/`msg.get("id")`
        // only ever match `Json::Obj` (see `Json::get` above), so a
        // top-level `Json::Arr` makes BOTH return `None` -- `method`
        // silently defaults to `""`, which falls into the dispatch match's
        // catch-all `_` arm, and since `id` is ALSO `None` there, that
        // arm's own `if let Some(id) = id { ...respond... }` never fires
        // either: NO response is ever sent for the whole message, so any
        // request nested inside the batch (each with its OWN legitimate
        // `id`) is left waiting forever for a reply that will never come.
        // The LSP specification explicitly states batching is NOT
        // supported (unlike the base JSON-RPC 2.0 protocol it otherwise
        // follows), so a spec-compliant client should never send one --
        // but this server had no defensive check at all, silently
        // swallowing the message instead of reporting the violation.
        // Mirrors PR-it620's OWN precedent exactly: report a clean
        // JSON-RPC error (`id: null`, since a batch envelope has no single
        // `id` of its own to report) rather than staying silent.
        if matches!(msg, Json::Arr(_)) {
            send(
                &mut stdout,
                "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid Request: batch requests are not supported\"}}",
            );
            continue;
        }
        let method = msg.get("method").and_then(Json::str).unwrap_or("");
        let id = msg.get("id");

        match method {
            "initialize" => {
                let id = id.map(render_id).unwrap_or_else(|| "null".into());
                workspace_root = msg
                    .get("params")
                    .and_then(|p| p.get("rootUri"))
                    .and_then(Json::str)
                    .and_then(uri_to_path)
                    .or_else(|| {
                        msg.get("params")
                            .and_then(|p| p.get("rootPath"))
                            .and_then(Json::str)
                            .map(PathBuf::from)
                    });
                send(
                    &mut stdout,
                    &format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"capabilities\":{{\"textDocumentSync\":1,\"hoverProvider\":true,\"definitionProvider\":true,\"referencesProvider\":true,\"renameProvider\":true,\"documentFormattingProvider\":true,\"documentSymbolProvider\":true,\"documentHighlightProvider\":true,\"workspaceSymbolProvider\":true,\"completionProvider\":{{\"triggerCharacters\":[\".\"]}},\"signatureHelpProvider\":{{\"triggerCharacters\":[\"(\",\",\"]}},\"codeActionProvider\":{{\"codeActionKinds\":[\"quickfix\"]}},\"foldingRangeProvider\":true}},\"serverInfo\":{{\"name\":\"kupl-lsp\",\"version\":\"{}\"}}}}}}",
                        env!("CARGO_PKG_VERSION")
                    ),
                );
            }
            "shutdown" => {
                let id = id.map(render_id).unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":null}}"));
            }
            "exit" => return 0,
            "textDocument/didOpen" => {
                let doc = msg.get("params").and_then(|p| p.get("textDocument"));
                if let (Some(uri), Some(text)) = (
                    doc.and_then(|d| d.get("uri")).and_then(Json::str),
                    doc.and_then(|d| d.get("text")).and_then(Json::str),
                ) {
                    if let Some(path) = uri_to_path(uri) {
                        buffers.insert(path.clone(), text.to_string());
                        let note = diagnostics_notification(&path, uri, &buffers);
                        send(&mut stdout, &note);
                    }
                }
            }
            "textDocument/didChange" => {
                let params = msg.get("params");
                let uri = params
                    .and_then(|p| p.get("textDocument"))
                    .and_then(|d| d.get("uri"))
                    .and_then(Json::str);
                let change = params.and_then(|p| p.get("contentChanges")).and_then(|c| c.index(0));
                let text = change.and_then(|c| c.get("text")).and_then(Json::str);
                // A REAL, live-confirmed document-state-corruption bug
                // found+fixed (production-hardening PR-it754): this server
                // declares `textDocumentSync: 1` (full sync) at
                // `initialize`, which per the LSP spec obligates a
                // compliant client to send `contentChanges[0].text` as the
                // ENTIRE new document with NO `range` field -- but this
                // handler never verified that contract, unconditionally
                // treating whatever text arrived as the full buffer. A
                // client (or client bug) that sends an INCREMENTAL-style
                // edit instead (a `range` + a tiny replacement fragment --
                // exactly what `textDocumentSync: 2` clients send, and what
                // this server never asked for) silently overwrote the
                // ENTIRE document down to just that tiny fragment, with
                // zero error reported back to the client. Live-confirmed
                // BEFORE this fix: `didOpen` with a real multi-line
                // program, then a single incremental-style `didChange`
                // (`range` + `text: "9"`) left the server's buffer
                // containing only `"9"` -- the whole document silently
                // gone. A `range`-bearing entry is now rejected as a
                // protocol violation (buffer left untouched, matching this
                // handler's existing no-op behavior for any other
                // malformed/missing field) instead of corrupting state.
                let is_full_replacement = change.is_some_and(|c| c.get("range").is_none());
                if let (Some(uri), Some(text)) = (uri, text) {
                    if is_full_replacement {
                        if let Some(path) = uri_to_path(uri) {
                            buffers.insert(path.clone(), text.to_string());
                            let note = diagnostics_notification(&path, uri, &buffers);
                            send(&mut stdout, &note);
                        }
                    }
                }
            }
            "textDocument/didSave" => {
                let uri = msg
                    .get("params")
                    .and_then(|p| p.get("textDocument"))
                    .and_then(|d| d.get("uri"))
                    .and_then(Json::str);
                if let Some(uri) = uri {
                    if let Some(path) = uri_to_path(uri) {
                        buffers.remove(&path); // saved: disk is truth again
                        let note = diagnostics_notification(&path, uri, &buffers);
                        send(&mut stdout, &note);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = msg
                    .get("params")
                    .and_then(|p| p.get("textDocument"))
                    .and_then(|d| d.get("uri"))
                    .and_then(Json::str)
                {
                    if let Some(path) = uri_to_path(uri) {
                        buffers.remove(&path);
                    }
                }
            }
            "textDocument/hover" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    // Cross-file fallback (PR-it516): a symbol pulled in via `use` (e.g. a
                    // helper defined in another module) used to have no hover at all --
                    // resolve_hover only ever sees this one buffer's text.
                    let dir = uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    let md = resolve_hover_cross_file(&text, line, ch, &dir, &buffers)?;
                    Some(format!(
                        "{{\"contents\":{{\"kind\":\"markdown\",\"value\":\"{}\"}}}}",
                        json_escape(&md)
                    ))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/signatureHelp" => {
                // Parameter hints while typing a call's argument list (PR-it586): the
                // one commonly-expected LSP capability this server never implemented,
                // confirmed absent by an explicit method-inventory check.
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    let (label, params, active) = resolve_signature_help(&text, line, ch)?;
                    let param_json: Vec<String> = params
                        .iter()
                        .map(|p| format!("{{\"label\":\"{}\"}}", json_escape(p)))
                        .collect();
                    Some(format!(
                        "{{\"signatures\":[{{\"label\":\"{}\",\"parameters\":[{}]}}],\"activeSignature\":0,\"activeParameter\":{active}}}",
                        json_escape(&label),
                        param_json.join(",")
                    ))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/codeAction" => {
                // Quick-fixes: add a missing `uses <effect>` clause for K0301 (PR-it587),
                // and remove an unused one for K0302 (PR-it588) -- the other confirmed-
                // missing LSP capability alongside signatureHelp, now covering both halves
                // of the effects-declaration lint.
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let uri = p.get("textDocument")?.get("uri")?.str()?;
                    let range = p.get("range")?;
                    let s = range.get("start")?;
                    let e = range.get("end")?;
                    let (sl, sc) = (s.get("line")?.as_usize()?, s.get("character")?.as_usize()?);
                    let (el, ec) = (e.get("line")?.as_usize()?, e.get("character")?.as_usize()?);
                    let text = doc_text(uri, &buffers)?;
                    let start_off = offset_at(&text, sl, sc);
                    let end_off = offset_at(&text, el, ec);
                    let items: Vec<String> = resolve_code_actions(&text, start_off, end_off)
                        .into_iter()
                        .map(|(title, edit_start, edit_end, new_text)| {
                            let (sl, sc) = line_col_utf16(&text, edit_start as u32);
                            let (el, ec) = line_col_utf16(&text, edit_end as u32);
                            let start_pos = format!("{{\"line\":{},\"character\":{}}}", sl - 1, sc - 1);
                            let end_pos = format!("{{\"line\":{},\"character\":{}}}", el - 1, ec - 1);
                            format!(
                                "{{\"title\":\"{}\",\"kind\":\"quickfix\",\"edit\":{{\"changes\":{{\"{}\":[{{\"range\":{{\"start\":{start_pos},\"end\":{end_pos}}},\"newText\":\"{}\"}}]}}}}}}",
                                json_escape(&title),
                                json_escape(uri),
                                json_escape(&new_text)
                            )
                        })
                        .collect();
                    Some(format!("[{}]", items.join(",")))
                })()
                .unwrap_or_else(|| "[]".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/definition" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    // Cross-file fallback (PR-it516): same rationale as hover above -- an
                    // empty target_uri means "this file" (reuse the request's own uri),
                    // a non-empty one names the OTHER file the definition lives in.
                    let dir = uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    let (target_uri, l0, c0, l1, c1) = resolve_definition_cross_file(&text, line, ch, &dir, &buffers)?;
                    let target_uri = if target_uri.is_empty() { uri.to_string() } else { target_uri };
                    Some(format!(
                        "{{\"uri\":\"{}\",\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}}}}",
                        json_escape(&target_uri)
                    ))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/references" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    let name = ident_under(&text, line, ch)?;
                    // Cross-file fallback (PR-it518): same rationale as hover/definition/
                    // completion, plus a correctness angle -- see occurrences_cross_file's
                    // doc comment for why this matters for rename specifically.
                    let dir = uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    let off = offset_at(&text, line, ch);
                    let locs: Vec<String> = occurrences_cross_file(&text, &name, off, &dir, &buffers)
                        .into_iter()
                        .map(|(target_uri, l0, c0, l1, c1)| {
                            let u = if target_uri.is_empty() { uri.to_string() } else { target_uri };
                            format!(
                                "{{\"uri\":\"{}\",\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}}}}",
                                json_escape(&u)
                            )
                        })
                        .collect();
                    Some(format!("[{}]", locs.join(",")))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/documentHighlight" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    resolve_document_highlight(&text, line, ch)
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/rename" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let new_name = p.get("newName")?.str()?;
                    if !is_valid_new_identifier(new_name) {
                        return None;
                    }
                    let text = doc_text(uri, &buffers)?;
                    let name = ident_under(&text, line, ch)?;
                    // Cross-file fallback (PR-it518): a real correctness hazard, not just a
                    // scope gap -- renaming a cross-file symbol from a call site used to
                    // silently rename ONLY that call, leaving its actual declaration (in the
                    // `use`d file) untouched and the program broken. Group edits by target
                    // file into a proper multi-file WorkspaceEdit.
                    let dir = uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    let off = offset_at(&text, line, ch);
                    // A REAL gap found+fixed (production-hardening PR-it787, carried
                    // forward since PR-it767/it780 as the known, deliberately-deferred
                    // sub-case of rename validation): `is_valid_new_identifier` above
                    // only ever checked `new_name`'s own SYNTACTIC well-formedness --
                    // nothing checked whether `new_name` ALREADY names a DIFFERENT
                    // top-level item in this file. Renaming `fun helper` to `main` when
                    // `fun main` already exists used to silently produce a
                    // WorkspaceEdit that, once applied, left TWO top-level items named
                    // `main` -- a duplicate-definition compile error the editor gave
                    // zero warning about before applying the edit. Only checked when
                    // `name` itself is NOT locally bound (a top-level rename target) --
                    // a local shadowing a top-level name is ordinary, legal scoping,
                    // not a collision (see `top_level_item_named`'s own doc comment for
                    // why this is deliberately SAME-FILE-only, not cross-file).
                    if new_name != name {
                        let (program, _diags) = crate::parser::parse(&text);
                        if local_binding_scope(&program, off, &name).is_none()
                            && top_level_item_named(&program, new_name)
                        {
                            return None;
                        }
                    }
                    let mut by_file: Vec<(String, Vec<String>)> = Vec::new();
                    for (target_uri, l0, c0, l1, c1) in occurrences_cross_file(&text, &name, off, &dir, &buffers) {
                        let u = if target_uri.is_empty() { uri.to_string() } else { target_uri };
                        let edit = format!(
                            "{{\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}},\"newText\":\"{}\"}}",
                            json_escape(new_name)
                        );
                        match by_file.iter_mut().find(|(fu, _)| fu == &u) {
                            Some((_, edits)) => edits.push(edit),
                            None => by_file.push((u, vec![edit])),
                        }
                    }
                    let changes: Vec<String> = by_file
                        .into_iter()
                        .map(|(u, edits)| format!("\"{}\":[{}]", json_escape(&u), edits.join(",")))
                        .collect();
                    Some(format!("{{\"changes\":{{{}}}}}", changes.join(",")))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/completion" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let items = (|| {
                    let p = msg.get("params")?;
                    let uri = p.get("textDocument")?.get("uri")?.str()?;
                    let text = doc_text(uri, &buffers)?;
                    // Cross-file fallback (PR-it517): same rationale as hover/definition.
                    let dir = uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    Some(completions_cross_file(&text, &dir, &buffers))
                })()
                .unwrap_or_default();
                let entries: Vec<String> = items
                    .iter()
                    .map(|(label, kind, detail)| {
                        format!(
                            "{{\"label\":\"{}\",\"kind\":{kind},\"detail\":\"{}\"}}",
                            json_escape(label),
                            json_escape(detail)
                        )
                    })
                    .collect();
                send(
                    &mut stdout,
                    &format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{{\"isIncomplete\":false,\"items\":[{}]}}}}",
                        entries.join(",")
                    ),
                );
            }
            "textDocument/formatting" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let uri = p.get("textDocument")?.get("uri")?.str()?;
                    let text = doc_text(uri, &buffers)?;
                    resolve_formatting(&text)
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/documentSymbol" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let uri = p.get("textDocument")?.get("uri")?.str()?;
                    let text = doc_text(uri, &buffers)?;
                    document_symbols(&text)
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/foldingRange" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let uri = p.get("textDocument")?.get("uri")?.str()?;
                    let text = doc_text(uri, &buffers)?;
                    folding_ranges(&text)
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "workspace/symbol" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let query = msg.get("params").and_then(|p| p.get("query")).and_then(Json::str).unwrap_or("");
                let result = match &workspace_root {
                    Some(root) => workspace_symbols(root, query, &buffers),
                    None => "[]".to_string(),
                };
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            _ => {
                // politely answer unknown REQUESTS (those with an id)
                if let Some(id) = id {
                    let id = render_id(id);
                    send(
                        &mut stdout,
                        &format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":null}}"),
                    );
                }
            }
        }
    }
    0
}

fn render_id(id: &Json) -> String {
    match id {
        Json::Num(n) => {
            if n.fract() == 0.0 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Json::Str(s) => format!("\"{}\"", json_escape(s)),
        _ => "null".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // a small multi-item program for the language-feature tests
    const PROG: &str = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
                        type Shape = Circle(r: Float) | Square(s: Float)\n\
                        fun main() uses io {\n    print(add(1, 2))\n}\n";

    /// `LineIndex::resolve_utf16`'s own O(log L) result must be BYTE-FOR-BYTE
    /// identical to `diag::line_col_utf16`'s original O(L) full-rescan
    /// result, for EVERY offset in EVERY document shape -- this is the
    /// critical correctness guard for PR-it835's fix, since LSP positions
    /// feed directly into user-visible rename/highlight/reference edits and
    /// a subtle off-by-one here would corrupt every result silently, a far
    /// worse regression than the original latency bug. Checks EVERY byte
    /// offset (not just token boundaries) across: empty string, single
    /// line, multiple lines, blank lines, a trailing newline, NO trailing
    /// newline, and lines containing astral-plane characters (the exact
    /// UTF-16-surrogate-pair case `line_col_utf16`'s own doc comment
    /// flags as the trickiest -- PR-it764).
    #[test]
    fn line_index_matches_line_col_utf16_for_every_offset() {
        let docs = [
            "",
            "x",
            "fun main() {\n    let x = 0\n    print(x)\n}\n",
            "no\ntrailing\nnewline",
            "\n\n\n",
            "line one\n\nline three (blank line two)\n",
            "fun main() {\n    let s = \"🎉 emoji line\"\n    print(s)\n}",
            "🎉🎉🎉\nmore🎉text\n",
        ];
        for doc in docs {
            let index = LineIndex::build(doc);
            // every byte offset from 0 to len+2 (covers in-range, exactly-at-end,
            // and past-end clamping, matching `line_col_utf16`'s own `.min(len)`)
            for offset in 0..=(doc.len() as u32 + 2) {
                let expected = crate::diag::line_col_utf16(doc, offset);
                let actual = index.resolve_utf16(doc, offset);
                assert_eq!(
                    actual, expected,
                    "mismatch at offset {offset} in {doc:?}: LineIndex gave {actual:?}, line_col_utf16 gave {expected:?}"
                );
            }
        }
    }

    /// The analogous exhaustive differential check for `LineIndex::
    /// resolve_line` (production-hardening PR-it836's NEW method, added
    /// for `folding_ranges`/`scoped_occurrences`'s line-only callers) --
    /// must match `diag::line_col`'s own LINE component (its column is
    /// unused by either caller and deliberately not part of this
    /// method's contract) for every offset, across the same document
    /// shapes as the UTF-16 variant above.
    #[test]
    fn line_index_resolve_line_matches_line_col_for_every_offset() {
        let docs = [
            "",
            "x",
            "fun main() {\n    let x = 0\n    print(x)\n}\n",
            "no\ntrailing\nnewline",
            "\n\n\n",
            "line one\n\nline three (blank line two)\n",
            "fun main() {\n    let s = \"🎉 emoji line\"\n    print(s)\n}",
            "🎉🎉🎉\nmore🎉text\n",
        ];
        for doc in docs {
            let index = LineIndex::build(doc);
            for offset in 0..=(doc.len() as u32 + 2) {
                let (expected_line, _) = crate::diag::line_col(doc, offset);
                let actual_line = index.resolve_line(doc.len(), offset);
                assert_eq!(
                    actual_line, expected_line,
                    "mismatch at offset {offset} in {doc:?}: resolve_line gave {actual_line}, line_col gave {expected_line}"
                );
            }
        }
    }

    /// The actual bug fixed by PR-it835: `occurrences`'s real, end-to-end
    /// output (not just the position-resolution primitive in isolation)
    /// must be UNCHANGED by the O(n^2) -> O(n log n) rewrite, across the
    /// exact scenarios `collect_occurrences` handles -- multiple same-name
    /// occurrences spread across several lines, AND the recursive
    /// string-interpolation path (`"…{x}…"`), which is the one case where
    /// the SAME `LineIndex` (built once from the outermost `full` document)
    /// must still be threaded correctly through nested recursive calls
    /// operating on DIFFERENT `text`/`base` substrings.
    #[test]
    fn occurrences_output_unchanged_by_line_index_rewrite() {
        let src = "fun main() {\n    let x = 0\n    let y = x + 1\n    print(\"x is {x}, y is {y}\")\n}\n";
        let occ = occurrences(src, "x");
        // x: declared line 1 (0-indexed), used in `x + 1` line 2, and inside
        // the string interpolation `{x}` on line 3 -- three occurrences.
        assert_eq!(occ.len(), 3, "{occ:?}");
        assert_eq!(occ[0].0, 1, "declaration on line index 1: {occ:?}");
        assert_eq!(occ[1].0, 2, "use in `x + 1` on line index 2: {occ:?}");
        assert_eq!(occ[2].0, 3, "use inside string interpolation on line index 3: {occ:?}");
    }

    #[test]
    fn code_action_adds_a_missing_uses_clause_for_k0301() {
        // A NEW LSP capability added (PR-it587): a quick-fix for K0301 ("public but
        // does not declare its effects") that inserts the missing `uses <effect>`
        // clause automatically -- the other confirmed-missing capability alongside
        // signatureHelp (it586), from the same method-inventory check.
        let src = "pub fun outer(x: Int) -> Int {\n    print(to_str(x))\n    x\n}\n";
        let actions = resolve_code_actions(src, 0, src.len());
        assert_eq!(actions.len(), 1, "{actions:?}");
        let (title, start, end, new_text) = &actions[0];
        assert_eq!(title, "Add `uses io`");
        assert_eq!(new_text, " uses io");
        assert_eq!(start, end, "K0301's fix is a zero-width insertion");
        // applying the edit produces a program with NO K0301 (and no OTHER new errors).
        let mut fixed = src.to_string();
        fixed.insert_str(*start, new_text);
        let (program, mut diags) = crate::parser::parse(&fixed);
        diags.extend(crate::check::check(&program).1);
        if !diags.iter().any(|d| d.severity == Severity::Error) {
            diags.extend(crate::effects::check_effects(&program));
        }
        assert!(
            !diags.iter().any(|d| d.code == "K0301"),
            "the fix must actually resolve K0301: {fixed:?} -> {diags:?}"
        );
    }

    #[test]
    fn code_action_stays_empty_when_a_uses_clause_already_exists_or_nothing_is_wrong() {
        // No false positives: a function whose EXISTING `uses` clause already covers
        // everything it calls (so K0301 never fires -- distinct from the widening
        // case below, which covers a clause that's PRESENT but INCOMPLETE), and a
        // function with no K0301 at all, must both yield zero code actions.
        let already_declared = "pub fun outer(x: Int) uses io -> Int {\n    print(to_str(x))\n    x\n}\n";
        assert!(resolve_code_actions(already_declared, 0, already_declared.len()).is_empty());
        let clean = "fun outer(x: Int) -> Int {\n    x\n}\n";
        assert!(resolve_code_actions(clean, 0, clean.len()).is_empty());
    }

    #[test]
    fn code_action_widens_an_existing_uses_clause_for_k0301() {
        // PR-it587's deferred v0 gap, closed here (PR-it589): a function that ALREADY
        // declares `uses io` but is MISSING another required effect (calling an `ai
        // fun`, which requires `ai`) must get a fix that WIDENS the existing clause
        // (`uses io` -> `uses io, ai`) rather than being skipped as out of scope.
        let src = "ai fun helper() -> Str {\n    intent \"say hi\"\n}\n\
                   pub fun outer(x: Int) uses io -> Str {\n    print(to_str(x))\n    helper()\n}\n";
        let actions = resolve_code_actions(src, 0, src.len());
        assert_eq!(actions.len(), 1, "{actions:?}");
        let (title, start, end, new_text) = &actions[0];
        assert_eq!(title, "Widen `uses` clause to add `ai`");
        assert_ne!(start, end, "widening is a real range replacement, not a zero-width insertion");
        let mut fixed = src.to_string();
        fixed.replace_range(*start..*end, new_text);
        assert!(fixed.contains("uses io, ai"), "{fixed:?}");
        let (program, mut diags) = crate::parser::parse(&fixed);
        diags.extend(crate::check::check(&program).1);
        if !diags.iter().any(|d| d.severity == Severity::Error) {
            diags.extend(crate::effects::check_effects(&program));
        }
        assert!(
            !diags.iter().any(|d| d.code == "K0301"),
            "the fix must actually resolve K0301: {fixed:?} -> {diags:?}"
        );
    }

    #[test]
    fn code_action_removes_the_sole_unused_uses_clause_for_k0302() {
        // The symmetric follow-up to K0301's fix (PR-it588): K0302 ("declares `uses
        // X` but never uses it") on a function whose `uses` clause names only ONE
        // effect -- the fix must drop the WHOLE clause, not leave a dangling `uses`.
        let src = "pub fun outer(x: Int) uses io -> Int {\n    x\n}\n";
        let actions = resolve_code_actions(src, 0, src.len());
        assert_eq!(actions.len(), 1, "{actions:?}");
        let (title, start, end, new_text) = &actions[0];
        assert_eq!(title, "Remove unused `uses io`");
        let mut fixed = src.to_string();
        fixed.replace_range(*start..*end, new_text);
        assert_eq!(fixed, "pub fun outer(x: Int) -> Int {\n    x\n}\n");
        let (program, mut diags) = crate::parser::parse(&fixed);
        diags.extend(crate::check::check(&program).1);
        if !diags.iter().any(|d| d.severity == Severity::Error) {
            diags.extend(crate::effects::check_effects(&program));
        }
        assert!(
            !diags.iter().any(|d| d.code == "K0301" || d.code == "K0302"),
            "the fix must not leave K0301/K0302 behind: {fixed:?} -> {diags:?}"
        );
    }

    #[test]
    fn code_action_removes_just_one_of_several_unused_effects_for_k0302() {
        // Multiple declared effects, only one unused -- the fix must drop just that
        // ONE name and keep the rest of the clause intact.
        let src = "pub fun outer(x: Int) uses io, ai.call -> Int {\n    print(to_str(x))\n    x\n}\n";
        let actions = resolve_code_actions(src, 0, src.len());
        assert_eq!(actions.len(), 1, "{actions:?}");
        let (title, start, end, new_text) = &actions[0];
        assert_eq!(title, "Remove unused `uses ai.call`");
        let mut fixed = src.to_string();
        fixed.replace_range(*start..*end, new_text);
        assert_eq!(fixed, "pub fun outer(x: Int) uses io -> Int {\n    print(to_str(x))\n    x\n}\n");
        let (program, mut diags) = crate::parser::parse(&fixed);
        diags.extend(crate::check::check(&program).1);
        if !diags.iter().any(|d| d.severity == Severity::Error) {
            diags.extend(crate::effects::check_effects(&program));
        }
        assert!(
            !diags.iter().any(|d| d.code == "K0301" || d.code == "K0302"),
            "the fix must not leave K0301/K0302 behind: {fixed:?} -> {diags:?}"
        );
    }

    #[test]
    fn signature_help_reports_params_and_active_index() {
        // A NEW LSP capability added (PR-it586): parameter hints while typing a call's
        // argument list. This server implemented hover/definition/completion/rename/
        // symbols but never signatureHelp -- confirmed absent via an explicit method-
        // inventory check, a genuinely missing, commonly-expected capability rather
        // than a bug in an existing one.
        let call_line = PROG.lines().position(|l| l.contains("print(add(")).unwrap();
        let line = PROG.lines().nth(call_line).unwrap();
        let open_paren = line.find("add(").unwrap() + "add(".len();

        // cursor right after `add(`, before any argument -> parameter 0 active.
        let (label, params, active) = resolve_signature_help(PROG, call_line, open_paren)
            .expect("signature help inside add(...)'s argument list");
        assert_eq!(label, "fun add(a: Int, b: Int) -> Int");
        assert_eq!(params, vec!["a: Int".to_string(), "b: Int".to_string()]);
        assert_eq!(active, 0, "cursor before any argument must be on parameter 0");

        // cursor just after the comma (into the second argument) -> parameter 1 active.
        let comma = line.find(',').unwrap();
        let (_, _, active2) =
            resolve_signature_help(PROG, call_line, comma + 2).expect("signature help on 2nd arg");
        assert_eq!(active2, 1, "cursor past the comma must be on parameter 1");

        // outside any call entirely (the `type Shape = ...` line) -> None.
        let type_line = PROG.lines().position(|l| l.contains("type Shape")).unwrap();
        assert!(
            resolve_signature_help(PROG, type_line, 5).is_none(),
            "no active call on a line with no call at all"
        );
    }

    #[test]
    fn signature_help_resolves_the_innermost_nested_call_and_component_methods() {
        // Nested calls: signature help for `outer(inner(1, |), 3)` at `|` must resolve
        // to `inner`'s signature (the INNERMOST enclosing call), not `outer`'s --
        // `find_enclosing_call` picks the SMALLEST containing span for exactly this
        // reason. Also covers a component's `expose fun` method, both via a direct
        // `recv.method(...)` call site (PR-it586).
        let src = "fun inner(x: Int, y: Int) -> Int { x + y }\n\
                   fun outer(a: Int, b: Int) -> Int { a + b }\n\
                   component Greeter {\n    intent \"g\"\n    expose fun greet(name: Str, loud: Bool) -> Str { name }\n}\n\
                   fun main() uses io {\n    \
                   print(outer(inner(1, 2), 3))\n    \
                   let g = Greeter()\n    \
                   print(g.greet(\"x\", true))\n}\n";

        let nested_line = src.lines().position(|l| l.contains("outer(inner(")).unwrap();
        let line = src.lines().nth(nested_line).unwrap();
        let inner_open = line.find("inner(").unwrap() + "inner(".len();
        let (label, ..) =
            resolve_signature_help(src, nested_line, inner_open).expect("signature help inside inner(...)");
        assert!(label.starts_with("fun inner("), "must resolve the innermost call: {label}");

        let method_line = src.lines().position(|l| l.contains("g.greet(")).unwrap();
        let mline = src.lines().nth(method_line).unwrap();
        let greet_open = mline.find("greet(").unwrap() + "greet(".len();
        let (label2, params2, _) =
            resolve_signature_help(src, method_line, greet_open).expect("signature help on a method call");
        assert!(label2.contains("greet(name: Str, loud: Bool)"), "{label2}");
        assert_eq!(params2, vec!["name: Str".to_string(), "loud: Bool".to_string()]);
    }

    /// A REAL bug found+fixed (production-hardening PR-it878, the SIXTH
    /// sibling instance of the `locally_bound`/local-vs-top-level-collision
    /// class -- fixed FIVE times already across `resolve_hover`/
    /// `resolve_hover_cross_file`/`resolve_definition`/
    /// `resolve_definition_cross_file`/`occurrences_cross_file`, PR-it704/
    /// it739/it741/it742/it743, but `resolve_signature_help`'s identical
    /// bare-name scan never received the SAME guard): a local closure sharing
    /// a bare name with an unrelated top-level `fun` used to show that
    /// unrelated `fun`'s signature/parameter hints while typing a call to the
    /// LOCAL. Confirmed live before this fix (a scratch probe, later
    /// converted into this permanent test): `let greet = fn y { y };
    /// greet("hi")`, shadowing an unrelated top-level `fun greet(x: Str) ->
    /// Str`, reported `Some(("fun greet(x: Str) -> Str", ["x: Str"], 0))` --
    /// the WRONG signature -- instead of `None`.
    #[test]
    fn signature_help_suppressed_for_a_call_to_a_local_shadowing_an_unrelated_top_level_fun() {
        let src = "fun greet(x: Str) -> Str {\n    x\n}\nfun report() -> Str {\n    let greet = fn y { y }\n    greet(\"hi\")\n}\n";
        let call_line = src.lines().position(|l| l.contains("greet(\"hi\")")).unwrap();
        let line = src.lines().nth(call_line).unwrap();
        let open_paren = line.find("greet(").unwrap() + "greet(".len();
        assert!(
            resolve_signature_help(src, call_line, open_paren).is_none(),
            "must NOT show the unrelated top-level `greet`'s signature for a call to the local closure"
        );

        // Sanity: a GENUINE call to the top-level `greet` (no local shadow in scope)
        // still correctly resolves -- this fix must not be a blanket suppression.
        let src2 = "fun greet(x: Str) -> Str {\n    x\n}\nfun main() -> Str {\n    greet(\"hi\")\n}\n";
        let call_line2 = src2.lines().position(|l| l.contains("greet(\"hi\")")).unwrap();
        let line2 = src2.lines().nth(call_line2).unwrap();
        let open_paren2 = line2.find("greet(").unwrap() + "greet(".len();
        let (label, ..) = resolve_signature_help(src2, call_line2, open_paren2)
            .expect("a genuine top-level call must still resolve");
        assert!(label.starts_with("fun greet("), "{label}");
    }

    #[test]
    fn hover_and_definition_work_on_component_methods() {
        // A real LSP capability gap (PR-it513, bug-hunt batch 134): hovering on ANY
        // component method -- exposed or private, at its own declaration OR at a
        // `recv.method(...)` call site -- returned NO hover at all, and "go to
        // definition" found nothing either. Root cause: item_signature/item_definition
        // only ever searched TOP-LEVEL program items; component methods live nested
        // inside Item::Component's `exposes`/`funs` lists, which neither function
        // looked at. Only hovering on the component's OWN name (e.g. its constructor
        // call `Greeter()`) worked. Fixed by adding a component-method fallthrough to
        // both functions, sharing a new `fun_sig_str` helper with the existing
        // top-level-function case so the rendered signature is identical either way.
        let src = "component Greeter {\n    intent \"g\"\n    expose fun greet(name: Str) -> Str {\n        \"hi {name}\"\n    }\n    fun helper() -> Int {\n        5\n    }\n}\nfun main() {\n    let g = Greeter()\n    print(g.greet(\"x\"))\n}\n";

        // hover on the exposed method's own declaration
        let decl_line = src.lines().position(|l| l.contains("expose fun greet")).unwrap();
        let ch = src.lines().nth(decl_line).unwrap().find("greet").unwrap() + 1;
        let h_decl = resolve_hover(src, decl_line, ch).expect("hover on exposed method decl");
        assert!(h_decl.contains("fun greet(name: Str) -> Str"), "{h_decl}");
        assert!(h_decl.contains("method of component Greeter"), "{h_decl}");

        // hover on a PRIVATE (non-exposed) component method's declaration
        let helper_line = src.lines().position(|l| l.contains("fun helper")).unwrap();
        let ch2 = src.lines().nth(helper_line).unwrap().find("helper").unwrap() + 1;
        let h_priv = resolve_hover(src, helper_line, ch2).expect("hover on private method decl");
        assert!(h_priv.contains("fun helper() -> Int"), "{h_priv}");

        // hover on a `recv.method(...)` CALL SITE, not just the declaration
        let call_line = src.lines().position(|l| l.contains("g.greet")).unwrap();
        let ch3 = src.lines().nth(call_line).unwrap().find("greet").unwrap() + 1;
        let h_call = resolve_hover(src, call_line, ch3).expect("hover on method call site");
        assert!(h_call.contains("fun greet(name: Str) -> Str"), "{h_call}");

        // go-to-definition on the call site resolves to the method's OWN declaration line
        let (l0, c0, _, _) = resolve_definition(src, call_line, ch3).expect("definition of greet");
        assert_eq!(l0, decl_line, "definition should point at the `expose fun greet` line");
        assert_eq!(c0, src.lines().nth(decl_line).unwrap().find("greet").unwrap());

        // the component's own name still hovers as before (no regression)
        let comp_line = src.lines().position(|l| l.contains("let g = Greeter")).unwrap();
        let ch4 = src.lines().nth(comp_line).unwrap().find("Greeter").unwrap() + 1;
        let h_comp = resolve_hover(src, comp_line, ch4).expect("hover on component ctor call");
        assert!(h_comp.contains("component Greeter"), "{h_comp}");
    }

    /// A REAL LSP capability gap found+fixed (production-hardening PR-it871,
    /// an Explore survey finding, independently re-verified live before
    /// implementing): the SAME gap class as it513's component-method fix and
    /// it571's contract-method fix above, just never mirrored for
    /// `ComponentDecl.state` -- hovering a state field (its own declaration,
    /// or a bare reference inside a method body) returned NO hover at all,
    /// and "go to definition" found nothing either, since
    /// item_signature/item_definition's component fallthrough only ever
    /// searched `exposes`/`funs`, never `state`. `item_completions`/
    /// `workspace_symbols` already treated state fields as first-class named
    /// symbols; only hover/goto-definition forgot them. Confirmed live
    /// before this fix via a real `kupl lsp` JSON-RPC session: hovering
    /// `total` at `state total: Int = 0` returned `null`, while hovering
    /// the sibling method `bump` (already fixed by it513) correctly
    /// returned its signature.
    #[test]
    fn hover_and_definition_work_on_component_state_fields() {
        let src = "component Widget {\n    intent \"w\"\n    state total: Int = 0\n    expose fun bump() -> Int {\n        total += 1\n        total\n    }\n}\nfun main() {\n    let w = Widget()\n}\n";

        // hover on the state field's own declaration
        let decl_line = src.lines().position(|l| l.contains("state total")).unwrap();
        let ch = src.lines().nth(decl_line).unwrap().find("total").unwrap() + 1;
        let h_decl = resolve_hover(src, decl_line, ch).expect("hover on state field decl");
        assert!(h_decl.contains("state total: Int"), "{h_decl}");
        assert!(h_decl.contains("state field of component Widget"), "{h_decl}");

        // hover on a BARE reference to the state field inside a method body
        let ref_line = src.lines().position(|l| l.contains("total += 1")).unwrap();
        let ch2 = src.lines().nth(ref_line).unwrap().find("total").unwrap() + 1;
        let h_ref = resolve_hover(src, ref_line, ch2).expect("hover on state field reference");
        assert!(h_ref.contains("state total: Int"), "{h_ref}");

        // go-to-definition on the reference resolves to the field's OWN declaration line
        let (l0, c0, _, _) = resolve_definition(src, ref_line, ch2).expect("definition of total");
        assert_eq!(l0, decl_line, "definition should point at the `state total` line");
        assert_eq!(c0, src.lines().nth(decl_line).unwrap().find("total").unwrap());

        // the sibling method still hovers as before (no regression)
        let method_line = src.lines().position(|l| l.contains("expose fun bump")).unwrap();
        let ch3 = src.lines().nth(method_line).unwrap().find("bump").unwrap() + 1;
        let h_method = resolve_hover(src, method_line, ch3).expect("hover on sibling method decl");
        assert!(h_method.contains("fun bump() -> Int"), "{h_method}");
    }

    /// A REAL LSP capability gap found+fixed (production-hardening PR-it872, a
    /// carried-forward lead from it871's own survey, independently re-verified
    /// live before implementing): the SAME gap class as it871's state-field fix
    /// above (and it513's component-method fix before that), just never itself
    /// mirrored for `ComponentDecl.props` -- hovering a prop (its own
    /// declaration, or a bare reference inside a method body) returned NO hover
    /// at all, and "go to definition" found nothing either, since
    /// item_signature/item_definition's component fallthrough only ever
    /// searched `exposes`/`funs`/`state`, never `props`. Props are scoped
    /// identically to state in `check.rs`'s prop/state-in-scope setup (just
    /// immutable), so there is no architectural reason for hover/definition to
    /// treat them differently -- confirmed NOT a documented boundary:
    /// `item_completions`/`item_symbol`/`workspace_symbols` (this file) also
    /// never referenced `c.props` anywhere, the SAME oversight in four places
    /// at once, all fixed together here. Confirmed live before this fix via a
    /// real `kupl lsp` JSON-RPC session: hovering `label` at `prop label: Str`
    /// returned `null`, hovering the bare `label` reference inside `show()`'s
    /// body also returned `null`, goto-definition on that reference also
    /// returned `null`, and the completion list inside the component body was
    /// missing `label` entirely -- while hovering the sibling method `bump`
    /// (already fixed by PR-it513) correctly returned its signature.
    #[test]
    fn hover_and_definition_work_on_component_props() {
        let src = "component Widget {\n    intent \"w\"\n    prop label: Str\n    state total: Int = 0\n    expose fun bump() -> Int {\n        total += 1\n        total\n    }\n    expose fun show() -> Str {\n        label\n    }\n}\nfun main() {\n    let w = Widget(label: \"hi\")\n}\n";

        // hover on the prop's own declaration
        let decl_line = src.lines().position(|l| l.contains("prop label")).unwrap();
        let ch = src.lines().nth(decl_line).unwrap().find("label").unwrap() + 1;
        let h_decl = resolve_hover(src, decl_line, ch).expect("hover on prop decl");
        assert!(h_decl.contains("prop label: Str"), "{h_decl}");
        assert!(h_decl.contains("prop of component Widget"), "{h_decl}");

        // hover on a BARE reference to the prop inside a method body
        let ref_line = src.lines().position(|l| l.trim() == "label").unwrap();
        let ch2 = src.lines().nth(ref_line).unwrap().find("label").unwrap() + 1;
        let h_ref = resolve_hover(src, ref_line, ch2).expect("hover on prop reference");
        assert!(h_ref.contains("prop label: Str"), "{h_ref}");

        // go-to-definition on the reference resolves to the prop's OWN declaration line
        let (l0, c0, _, _) = resolve_definition(src, ref_line, ch2).expect("definition of label");
        assert_eq!(l0, decl_line, "definition should point at the `prop label` line");
        assert_eq!(c0, src.lines().nth(decl_line).unwrap().find("label").unwrap());

        // the sibling state field and method still hover as before (no regression)
        let state_line = src.lines().position(|l| l.contains("state total")).unwrap();
        let ch3 = src.lines().nth(state_line).unwrap().find("total").unwrap() + 1;
        let h_state = resolve_hover(src, state_line, ch3).expect("hover on sibling state field decl");
        assert!(h_state.contains("state total: Int"), "{h_state}");

        // the prop is also a completion candidate, not just hover/definition
        let labels: Vec<String> = completions(src).into_iter().map(|(l, ..)| l).collect();
        assert!(labels.contains(&"label".to_string()), "prop must be a completion candidate: {labels:?}");
    }

    /// A REAL LSP capability gap found+fixed (production-hardening PR-it873, a
    /// standalone check of `ComponentDecl`'s OTHER fields beyond `props`/`state`
    /// prompted by it872's own fix): the SAME gap class as it871's state-field
    /// fix and it872's prop fix above, just never itself mirrored for
    /// `ComponentDecl.children` -- hovering a child (`let child = Component(args)`
    /// inside a component body: its own declaration, or a bare reference like
    /// `bulb.on_()` inside a method body) returned NO hover at all, and "go to
    /// definition" found nothing either, since item_signature/item_definition's
    /// component fallthrough only ever searched `exposes`/`funs`/`state`/`props`,
    /// never `children`. Confirmed NOT a documented boundary before implementing:
    /// `check.rs`'s `bind_component_env` scopes children in the EXACT SAME
    /// function, via the same `ctx.scopes.insert`, as props/state -- a child is
    /// just as much a first-class named identifier as a prop or state field.
    /// By contrast, `ComponentDecl.ports`/`wires`/`supervises` were checked and
    /// confirmed genuinely DIFFERENT: `ports` are matched by string equality
    /// against `ComponentSig`'s `in_ports`/`out_ports` maps (used only in `wire`/
    /// `emit`/`on` trigger positions, never inserted into `ctx.scopes`), `wires`
    /// have no `name` field at all (they connect two existing (component, port)
    /// pairs), and `supervises` merely references an existing child's name (a
    /// directive, not a new named declaration) -- none of these are genuine
    /// "declaration missing from the symbol tables" gaps of the props/state/
    /// children shape, so they were correctly left alone. `handlers` (which DO
    /// appear in `lsp.rs`, 4 times) have no `name` field either and are already
    /// covered by the SEPARATE, already-exhausted `local_binding_scope`/
    /// `shadow_zones` machinery (it704/it739/it855) for their `param` binder.
    /// Confirmed live before this fix via a real `kupl lsp` JSON-RPC session:
    /// hovering `bulb` at `let bulb = Light()` returned `null`, hovering the bare
    /// `bulb` reference inside `status()`'s body (`bulb.on_()`) also returned
    /// `null`, goto-definition on that reference also returned `null`, and the
    /// completion list inside the component body was missing `bulb` entirely --
    /// while hovering the sibling method `status` correctly returned its
    /// signature (an existing, working case, no regression).
    #[test]
    fn hover_and_definition_work_on_component_children() {
        let src = "component Light {\n    intent \"l\"\n    expose fun on_() -> Bool {\n        true\n    }\n}\ncomponent Panel {\n    intent \"p\"\n    let bulb = Light()\n    expose fun status() -> Bool {\n        bulb.on_()\n    }\n}\nfun main() {\n    let p = Panel()\n}\n";

        // hover on the child's own declaration
        let decl_line = src.lines().position(|l| l.contains("let bulb")).unwrap();
        let ch = src.lines().nth(decl_line).unwrap().find("bulb").unwrap() + 1;
        let h_decl = resolve_hover(src, decl_line, ch).expect("hover on child decl");
        assert!(h_decl.contains("let bulb: Light"), "{h_decl}");
        assert!(h_decl.contains("child of component Panel"), "{h_decl}");

        // hover on a BARE reference to the child inside a method body
        let ref_line = src.lines().position(|l| l.trim() == "bulb.on_()").unwrap();
        let ch2 = src.lines().nth(ref_line).unwrap().find("bulb").unwrap() + 1;
        let h_ref = resolve_hover(src, ref_line, ch2).expect("hover on child reference");
        assert!(h_ref.contains("let bulb: Light"), "{h_ref}");

        // go-to-definition on the reference resolves to the child's OWN declaration line
        let (l0, c0, _, _) = resolve_definition(src, ref_line, ch2).expect("definition of bulb");
        assert_eq!(l0, decl_line, "definition should point at the `let bulb` line");
        assert_eq!(c0, src.lines().nth(decl_line).unwrap().find("bulb").unwrap());

        // the sibling method still hovers as before (no regression)
        let method_line = src.lines().position(|l| l.contains("expose fun status")).unwrap();
        let ch3 = src.lines().nth(method_line).unwrap().find("status").unwrap() + 1;
        let h_method = resolve_hover(src, method_line, ch3).expect("hover on sibling method decl");
        assert!(h_method.contains("fun status() -> Bool"), "{h_method}");

        // the child is also a completion candidate, not just hover/definition
        let labels: Vec<String> = completions(src).into_iter().map(|(l, ..)| l).collect();
        assert!(labels.contains(&"bulb".to_string()), "child must be a completion candidate: {labels:?}");
    }

    #[test]
    fn hover_and_definition_work_on_contract_methods() {
        // The exact same gap class as it513's component-method fix (above), just never
        // mirrored for `ContractDecl.sigs`: hovering on a contract's exposed method --
        // its own declaration inside `contract { }`, OR a `recv.method(...)` call site
        // on a contract-typed receiver -- returned NO hover at all, and "go to
        // definition" found nothing either, since item_signature/item_definition's
        // component-method fallthrough only ever looked at Item::Component, never
        // Item::Contract's own `sigs` list. Only hovering on the contract's OWN name
        // worked (PR-it571). `FunSig` (a contract method) has no body/`ai` field unlike
        // `FunDecl`, so a small analogous `contract_sig_str` formatter was added rather
        // than reusing `fun_sig_str` directly.
        let src = "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n}\nfun use_it(s: Store) -> Int {\n    s.get(\"x\")\n}\n";

        // hover on the method's own declaration inside the contract
        let decl_line = src.lines().position(|l| l.contains("expose fun get")).unwrap();
        let ch = src.lines().nth(decl_line).unwrap().find("get").unwrap() + 1;
        let h_decl = resolve_hover(src, decl_line, ch).expect("hover on contract method decl");
        assert!(h_decl.contains("expose fun get(k: Str) -> Int"), "{h_decl}");
        assert!(h_decl.contains("method of contract Store"), "{h_decl}");

        // hover on a `recv.method(...)` CALL SITE on a contract-typed receiver
        let call_line = src.lines().position(|l| l.contains("s.get")).unwrap();
        let ch2 = src.lines().nth(call_line).unwrap().find("get").unwrap() + 1;
        let h_call = resolve_hover(src, call_line, ch2).expect("hover on contract method call site");
        assert!(h_call.contains("expose fun get(k: Str) -> Int"), "{h_call}");

        // go-to-definition on the call site resolves to the method's OWN declaration line
        let (l0, c0, _, _) = resolve_definition(src, call_line, ch2).expect("definition of get");
        assert_eq!(l0, decl_line, "definition should point at the `expose fun get` line");
        assert_eq!(c0, src.lines().nth(decl_line).unwrap().find("get").unwrap());

        // the contract's own name still hovers as before (no regression)
        let param_line = src.lines().position(|l| l.contains("s: Store")).unwrap();
        let ch3 = src.lines().nth(param_line).unwrap().find("Store").unwrap() + 1;
        let h_contract = resolve_hover(src, param_line, ch3).expect("hover on contract type name");
        assert!(h_contract.contains("contract Store"), "{h_contract}");
    }

    #[test]
    fn completions_include_contract_methods() {
        // Same gap class as it514's component-method/state completion fix, mirrored for
        // contracts: a contract's exposed method signatures were completely invisible to
        // completion -- only the contract's OWN name was listed (PR-it571).
        let src = "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n}\n";
        let items = completions(src);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(labels.contains(&"Store"), "the contract's own name is still listed: {labels:?}");
        assert!(labels.contains(&"get"), "contract method must be a completion candidate: {labels:?}");
        let get = items.iter().find(|(l, _, _)| l == "get").unwrap();
        assert_eq!(get.1, 3, "method completion kind must be Function (3)");
        assert!(get.2.contains("expose fun get(k: Str) -> Int"), "{get:?}");
    }

    #[test]
    fn hover_and_definition_reach_across_use_imports() {
        // A real, well-scoped LSP capability gap (PR-it516): resolve_hover/resolve_definition
        // only ever see the ONE buffer they're handed, so a symbol pulled in via `use` (e.g.
        // `mean(xs)` in a file that does `use lib.stats`) had NO hover and NO go-to-definition
        // at all -- even though `kupl run`/`kupl check`/`kupl build` had already been fixed
        // (PR-it507) to resolve the SAME `use` imports for compilation. Fixed by adding
        // resolve_hover_cross_file/resolve_definition_cross_file: try the current file first
        // (identical to the plain functions, so single-file behavior is unchanged), then walk
        // this file's own `use` statements (resolved locally, relative to the document's
        // directory -- the examples/multifile case) and search each target file in turn.
        //
        // Uses the REAL examples/multifile/main.kupl (`use util` / `use lib.stats`) and its
        // sibling files on disk -- exercising the actual filesystem-resolution path, not just
        // an in-memory fixture.
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let main_path = manifest_dir.join("examples/multifile/main.kupl");
        let dir = main_path.parent().unwrap();
        let text = std::fs::read_to_string(&main_path).expect("read examples/multifile/main.kupl");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();

        // `mean` lives in lib/stats.kupl, reached via `use lib.stats`.
        let mean_line = text.lines().position(|l| l.contains("let m = mean")).unwrap();
        let ch = text.lines().nth(mean_line).unwrap().find("mean").unwrap() + 1;
        let hover = resolve_hover_cross_file(&text, mean_line, ch, dir, &empty_buffers)
            .expect("cross-file hover on `mean` must find lib/stats.kupl's definition");
        assert!(hover.contains("fun mean(xs: List[Int]) -> Float"), "{hover}");
        let (target_uri, l0, c0, _, _) = resolve_definition_cross_file(&text, mean_line, ch, dir, &empty_buffers)
            .expect("cross-file definition on `mean` must find lib/stats.kupl");
        assert!(target_uri.starts_with("file://") && target_uri.ends_with("lib/stats.kupl"), "{target_uri}");
        assert_eq!(l0, 0, "mean is declared on line 0 of lib/stats.kupl");
        assert_eq!(c0, 4, "the name starts after `fun `");

        // `label` lives in util.kupl, reached via `use util`.
        let label_line = text.lines().position(|l| l.contains("({label(m)})")).unwrap();
        let ch2 = text.lines().nth(label_line).unwrap().find("label").unwrap() + 1;
        let hover2 = resolve_hover_cross_file(&text, label_line, ch2, dir, &empty_buffers)
            .expect("cross-file hover on `label` must find util.kupl's definition");
        assert!(hover2.contains("fun label(x: Float) -> Str"), "{hover2}");

        // Same-file symbols are completely unaffected -- the cross-file fallback only kicks
        // in when the current-file search misses.
        let comp_line = text.lines().position(|l| l.contains("component Reporter")).unwrap();
        let ch3 = text.lines().nth(comp_line).unwrap().find("Reporter").unwrap() + 1;
        let h_local = resolve_hover_cross_file(&text, comp_line, ch3, dir, &empty_buffers)
            .expect("same-file component hover unaffected");
        assert!(h_local.contains("component Reporter"), "{h_local}");

        // An unresolvable identifier (not in this file, not in any `use`d file) still cleanly
        // returns None -- no panic on a missing/unreadable sibling file.
        assert!(resolve_hover_cross_file("fun probe() -> Int {\n    zzz_nonexistent\n}\n", 1, 5, dir, &empty_buffers).is_none());
    }

    #[test]
    fn completions_reach_across_use_imports() {
        // Same gap class as it516's hover/definition fix, applied to completions: a name
        // pulled in via `use` (e.g. `mean`/`label` in examples/multifile/main.kupl, which does
        // `use lib.stats` / `use util`) never autocompleted -- `completions` only ever looked
        // at the current buffer. Fixed by `completions_cross_file`, reusing the SAME
        // used_file_paths/text_at_path helpers as the hover/definition fix (PR-it517).
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let main_path = manifest_dir.join("examples/multifile/main.kupl");
        let dir = main_path.parent().unwrap();
        let text = std::fs::read_to_string(&main_path).expect("read examples/multifile/main.kupl");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();

        // plain (non-cross-file) completions still miss them -- confirms the gap existed and
        // that completions() itself is unchanged (no regression on single-file behavior).
        let plain = completions(&text);
        let local_only: Vec<&str> = plain.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(!local_only.contains(&"mean"), "plain completions must NOT see cross-file names: {local_only:?}");

        let items = completions_cross_file(&text, dir, &empty_buffers);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(labels.contains(&"mean"), "mean (lib/stats.kupl via `use lib.stats`) must autocomplete: {labels:?}");
        assert!(labels.contains(&"label"), "label (util.kupl via `use util`) must autocomplete: {labels:?}");
        // the real signature is carried as detail, like any other function completion.
        let mean = items.iter().find(|(l, _, _)| l == "mean").unwrap();
        assert_eq!(mean.1, 3, "kind must be Function (3)");
        assert!(mean.2.contains("fun mean(xs: List[Int]) -> Float"), "{mean:?}");
        // this document's own names are still present (no regression).
        assert!(labels.contains(&"Reporter"), "{labels:?}");
    }

    #[test]
    fn occurrences_and_rename_reach_across_use_imports() {
        // A REAL correctness hazard, not just a scope gap (PR-it518): `textDocument/rename`
        // advertises `renameProvider: true`, but renaming a cross-file symbol FROM A CALL SITE
        // used to silently rename ONLY that call, leaving the actual declaration (in the
        // `use`d file) completely untouched -- the resulting program would call an undefined
        // name. Confirmed empirically first: plain `occurrences(main_text, "mean")` in
        // examples/multifile/main.kupl returns exactly ONE location (the call site), not the
        // declaration in lib/stats.kupl.
        //
        // Fixed by occurrences_cross_file: current-file occurrences PLUS occurrences in every
        // file reached via this file's own `use` statements (one hop outward -- see its doc
        // comment for the documented, NOT-fully-solved remaining direction: renaming FROM the
        // declaration site doesn't reach back out to callers, which would need a project-wide
        // reverse-dependency scan).
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let main_path = manifest_dir.join("examples/multifile/main.kupl");
        let dir = main_path.parent().unwrap();
        let text = std::fs::read_to_string(&main_path).expect("read examples/multifile/main.kupl");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();

        // plain (non-cross-file) occurrences confirms the hazard: only the call site.
        let local_only = occurrences(&text, "mean");
        assert_eq!(local_only.len(), 1, "plain occurrences must NOT see the cross-file declaration: {local_only:?}");

        let mean_off = text.find("mean(xs)").unwrap();
        let locs = occurrences_cross_file(&text, "mean", mean_off, dir, &empty_buffers);
        assert_eq!(locs.len(), 2, "call site (this file) + declaration (lib/stats.kupl): {locs:?}");
        let local = locs.iter().filter(|(u, ..)| u.is_empty()).count();
        let cross = locs.iter().filter(|(u, ..)| !u.is_empty()).count();
        assert_eq!(local, 1, "exactly one same-file occurrence (the call site): {locs:?}");
        assert_eq!(cross, 1, "exactly one cross-file occurrence (the declaration): {locs:?}");
        let (cross_uri, l0, c0, _, _) = locs.iter().find(|(u, ..)| !u.is_empty()).unwrap().clone();
        assert!(cross_uri.starts_with("file://") && cross_uri.ends_with("lib/stats.kupl"), "{cross_uri}");
        assert_eq!((l0, c0), (0, 4), "declaration is `fun mean` on line 0, name starts after `fun `");

        // A same-file-only symbol (no `use` involvement) is completely unaffected.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() {\n    print(add(1, 2))\n}\n";
        let add_off = src.find("add(1, 2)").unwrap();
        let same_file = occurrences_cross_file(src, "add", add_off, dir, &empty_buffers);
        assert_eq!(same_file.len(), 2, "decl + call, both same-file: {same_file:?}");
        assert!(same_file.iter().all(|(u, ..)| u.is_empty()), "{same_file:?}");
    }

    /// A REAL correctness bug in a MUTATING operation (production-hardening
    /// PR-it876, the THIRD sibling instance of it704/it739's local-vs-top-level
    /// collision class, found via this campaign's "re-audit a function with
    /// prior fix history" technique): `occurrences_cross_file`'s cross-file loop
    /// never applied `shadow_zones` filtering to the OTHER file's own
    /// occurrences -- only THIS file's occurrences were scoped (via
    /// `scoped_occurrences`, it704/it739's fix). Renaming a genuine top-level
    /// symbol from a real cross-file call site used to ALSO sweep up a
    /// completely unrelated LOCAL variable of the same bare name inside a
    /// different function in the USED file, corrupting that function's logic if
    /// the rename were actually applied.
    #[test]
    fn cross_file_rename_does_not_reach_an_unrelated_local_in_the_used_file() {
        let dir = std::path::Path::new("/fake/lsp-it876");
        let main_text = "use stats\nfun report(xs: List[Int]) -> Float {\n    mean(xs)\n}\n";
        let stats_text = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\nfun other() -> Float {\n    let mean = 5.0\n    print(mean)\n    mean\n}\n";
        let mut buffers: HashMap<PathBuf, String> = HashMap::new();
        buffers.insert(dir.join("stats.kupl"), stats_text.to_string());

        let off = main_text.find("mean(xs)").unwrap();
        let locs = occurrences_cross_file(main_text, "mean", off, dir, &buffers);
        assert_eq!(
            locs.len(),
            2,
            "must be exactly the call site (this file) + the real `fun mean` declaration (stats.kupl), \
             NOT `other()`'s unrelated local: {locs:?}"
        );
        let local = locs.iter().filter(|(u, ..)| u.is_empty()).count();
        let cross = locs.iter().filter(|(u, ..)| !u.is_empty()).count();
        assert_eq!(local, 1, "exactly one same-file occurrence (the call site): {locs:?}");
        assert_eq!(cross, 1, "exactly one cross-file occurrence (the real declaration, not the unrelated local): {locs:?}");
        let (cross_uri, l0, c0, ..) = locs.iter().find(|(u, ..)| !u.is_empty()).unwrap().clone();
        assert!(cross_uri.ends_with("stats.kupl"), "{cross_uri}");
        assert_eq!((l0, c0), (0, 4), "must be `fun mean`'s OWN declaration, not `other()`'s `let mean`: {locs:?}");
    }

    /// A REAL bug (production-hardening PR-it704): `resolve_definition_cross_file`/
    /// `occurrences_cross_file` only ever checked whether `name` is a TOP-LEVEL
    /// item in the current file before falling back cross-file -- never whether
    /// it's a plain local PARAMETER, since `item_definition`/`occurrences` never
    /// model local scope at all. A parameter reference sharing text with an
    /// unrelated top-level item in a `use`d file used to silently jump
    /// goto-definition to that unrelated declaration and, far worse, a rename
    /// would include and rename it too, corrupting a completely unrelated file
    /// with no warning -- directly contradicting `occurrences_cross_file`'s own
    /// (now-corrected) doc comment claim that cross-file expansion "never turns
    /// a correct rename into an incorrect one." Found via a sixteenth
    /// research-subagent dispatch, live-reproduced before this fix.
    #[test]
    fn locally_bound_parameter_suppresses_cross_file_goto_definition_and_rename() {
        let dir = std::path::Path::new("/fake/lsp-it704");
        let main_text = "use stats\nfun greet(mean: Str) -> Str {\n    \"hi {mean}\"\n}\nfun main() { greet(\"x\") }\n";
        let stats_text = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\n";
        let mut buffers: HashMap<PathBuf, String> = HashMap::new();
        buffers.insert(dir.join("stats.kupl"), stats_text.to_string());

        // The cursor is on `mean` INSIDE the interpolation `"hi {mean}"` -- a
        // reference to the PARAMETER, never a call to the imported `fun mean`.
        let off = main_text.find("{mean}").unwrap() + 1;
        let line = main_text[..off].matches('\n').count();
        let line_start = main_text[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let ch = off - line_start;

        // goto-definition must NOT jump into stats.kupl.
        assert!(
            resolve_definition_cross_file(main_text, line, ch, dir, &buffers).is_none(),
            "a local parameter reference must not resolve to an unrelated cross-file declaration"
        );

        // rename must ONLY touch the current file's occurrences of the parameter.
        let locs = occurrences_cross_file(main_text, "mean", off, dir, &buffers);
        assert!(locs.iter().all(|(u, ..)| u.is_empty()), "must not reach into stats.kupl: {locs:?}");
        assert_eq!(locs.len(), 2, "the parameter declaration + its one interpolation use: {locs:?}");

        // Sanity: a GENUINE cross-file reference (a real call to the imported
        // function, never a local of any kind) still resolves correctly -- this
        // fix must not be a blanket "never cross a file boundary" regression.
        let caller_text = "use stats\nfun report(xs: List[Int]) -> Float {\n    mean(xs)\n}\n";
        let off2 = caller_text.find("mean(xs)").unwrap();
        let call_line = caller_text[..off2].matches('\n').count();
        let call_line_start = caller_text[..off2].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let call_ch = off2 - call_line_start;
        let (target_uri, ..) = resolve_definition_cross_file(caller_text, call_line, call_ch, dir, &buffers)
            .expect("a genuine cross-file call must still resolve");
        assert!(target_uri.ends_with("stats.kupl"), "{target_uri}");
    }

    /// A REAL bug (production-hardening PR-it739): PR-it704's `locally_bound` fix only
    /// ever checked function/method PARAMETERS and handler payload binders -- its own
    /// doc comment explicitly flagged `let`/`var` locals, `match` bindings, and lambda
    /// parameters as an unfixed residual gap. A `let mean = 5.0` local inside
    /// `fun report()` shares its bare name with an unrelated top-level `fun mean` in a
    /// `use`d sibling file; renaming/goto-definition on the LOCAL used to still fall
    /// through to the cross-file search (since `occurrences`/`item_definition` have no
    /// notion of local scope), silently jumping goto-definition into the wrong file and,
    /// far worse, including that unrelated declaration in a rename's `WorkspaceEdit` --
    /// corrupting `stats.kupl` when the user only meant to rename a local variable.
    /// Live-reproduced by a research subagent before this fix (compiled `libkupl.rlib`,
    /// called `occurrences_cross_file` directly, observed the cross-file hit).
    #[test]
    fn locally_bound_let_local_suppresses_cross_file_goto_definition_and_rename() {
        let dir = std::path::Path::new("/fake/lsp-it739");
        let main_text = "use stats\nfun report() -> Float {\n    let mean = 5.0\n    print(mean)\n    mean\n}\n";
        let stats_text = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\n";
        let mut buffers: HashMap<PathBuf, String> = HashMap::new();
        buffers.insert(dir.join("stats.kupl"), stats_text.to_string());

        // The cursor is on the `let mean` declaration itself.
        let off = main_text.find("mean = 5.0").unwrap();
        let line = main_text[..off].matches('\n').count();
        let line_start = main_text[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let ch = off - line_start;

        // goto-definition must NOT jump into stats.kupl.
        assert!(
            resolve_definition_cross_file(main_text, line, ch, dir, &buffers).is_none(),
            "a local `let` reference must not resolve to an unrelated cross-file declaration"
        );

        // rename must ONLY touch the current file's occurrences of the local (decl + 2 uses).
        let locs = occurrences_cross_file(main_text, "mean", off, dir, &buffers);
        assert!(locs.iter().all(|(u, ..)| u.is_empty()), "must not reach into stats.kupl: {locs:?}");
        assert_eq!(locs.len(), 3, "declaration + print(mean) + trailing mean, all same-file: {locs:?}");

        // Sanity: a genuine cross-file call to the imported `mean` function is unaffected.
        let caller_text = "use stats\nfun report2(xs: List[Int]) -> Float {\n    mean(xs)\n}\n";
        let off2 = caller_text.find("mean(xs)").unwrap();
        let call_line = caller_text[..off2].matches('\n').count();
        let call_line_start = caller_text[..off2].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let call_ch = off2 - call_line_start;
        let (target_uri, ..) = resolve_definition_cross_file(caller_text, call_line, call_ch, dir, &buffers)
            .expect("a genuine cross-file call must still resolve");
        assert!(target_uri.ends_with("stats.kupl"), "{target_uri}");
    }

    /// A REAL bug (production-hardening PR-it741): the SAME-FILE analog of PR-it739
    /// -- no `use`/cross-file boundary needed at all. `occurrences_cross_file`'s base
    /// same-file list used to be plain, unconditional `occurrences(text, name)`, which
    /// is purely token-based (matches every identifier token with the given text,
    /// regardless of what it actually refers to). A local `let mean = 5.0` inside one
    /// function sharing a bare name with an UNRELATED top-level `fun mean` elsewhere in
    /// the SAME file got merged into ONE rename: renaming the local also silently
    /// renamed the unrelated function's declaration AND its own call site (in a
    /// DIFFERENT function), corrupting a completely different part of the program the
    /// user never touched. The fix is symmetric (see `shadow_zones`): renaming the
    /// top-level `fun mean` itself must also skip an unrelated local of the same name
    /// shadowed in a different function. Live-verified before this fix: plain
    /// `occurrences(src, "mean")` returned all 4 locations merged into one group,
    /// regardless of which one the rename was invoked from.
    ///
    /// NOTE on precision: the fix is line-range-based, not exact-scope (matching this
    /// file's established coarse-imprecision elsewhere -- see `block_binds_name`'s doc
    /// comment) -- it protects against a DIFFERENT function/scope entirely, which is
    /// the common, realistic case exercised here (the unrelated top-level call site
    /// lives in `helper()`, a separate function from the local's own `main()`). It does
    /// NOT precisely separate "before" vs. "after" a shadowing point WITHIN the SAME
    /// function (an intentionally out-of-scope, rarer edge case).
    #[test]
    fn local_binding_scope_suppresses_same_file_rename_into_an_unrelated_top_level_declaration() {
        let src = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\nfun helper() {\n    print(mean([1, 2, 3]))\n}\nfun main() {\n    let mean = 5.0\n    print(mean)\n}\n";

        // Plain (scope-blind) occurrences confirms the hazard still exists at that
        // primitive layer (by design -- see `occurrences`'s own doc comment): all 4
        // are merged with no scope awareness.
        let all = occurrences(src, "mean");
        assert_eq!(all.len(), 4, "unrelated fun decl + its call + local decl + local use: {all:?}");

        // The cursor is on the LOCAL `let mean` declaration inside `main()`.
        let off = src.find("mean = 5.0").unwrap();
        let dir = std::path::Path::new("/fake/lsp-it741");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();
        let locs = occurrences_cross_file(src, "mean", off, dir, &empty_buffers);
        assert_eq!(locs.len(), 2, "only the local's own decl + use, NOT the unrelated fun: {locs:?}");
        // Neither location may fall on line 0 (the unrelated `fun mean` declaration)
        // or line 4 (its call site inside `helper()`'s `print(mean([1, 2, 3]))`).
        assert!(locs.iter().all(|(_, l0, ..)| *l0 != 0 && *l0 != 4), "must not touch the unrelated fun: {locs:?}");

        // Renaming the TOP-LEVEL `fun mean` itself is symmetrically protected: still
        // finds only its own declaration + call site, not the unrelated local in main().
        let fun_off = src.find("fun mean").unwrap() + 4;
        let fun_locs = occurrences_cross_file(src, "mean", fun_off, dir, &empty_buffers);
        assert_eq!(fun_locs.len(), 2, "fun decl + its one call site: {fun_locs:?}");
        assert!(fun_locs.iter().all(|(_, l0, ..)| *l0 == 0 || *l0 == 4), "{fun_locs:?}");
    }

    /// A REAL, LIVE-CONFIRMED bug (production-hardening PR-it855, the THIRTY-FOURTH
    /// survey, found via the SAME "hardcoded item-kind list forgot an entry" pattern
    /// PR-it854 fixed in repl.rs's `is_item`): `local_binding_scope`/`shadow_zones`
    /// (the exact PR-it704/it739/it836 scope-safety-net functions this test's own
    /// sibling above exercises for `fun`/`component`) only matched `Item::Fun`/
    /// `Item::Component`, falling through to `_ => {}` for a top-level `law "..." {
    /// ... }` block -- even though `Law.body: Block` can contain `let` bindings just
    /// like a function body. Live-confirmed BEFORE this fix via a real `kupl lsp`
    /// rename session: renaming the LOCAL `let helper` inside a `law` block ALSO
    /// silently renamed an UNRELATED top-level `fun helper` and its call site
    /// elsewhere in the file, and symmetrically renaming the top-level `fun helper`
    /// corrupted the law's own unrelated local. This test is the direct Rust-level
    /// mirror of that live session.
    #[test]
    fn local_binding_scope_suppresses_same_file_rename_into_an_unrelated_top_level_declaration_through_a_law() {
        let src = "fun helper() -> Int { 1 }\nfun caller() -> Int { helper() }\nlaw \"shadowed\" {\n    let helper = 2\n    expect helper == 2\n}\n";

        let all = occurrences(src, "helper");
        assert_eq!(all.len(), 4, "unrelated fun decl + its call + local let + local use: {all:?}");

        // The cursor is on the LOCAL `let helper` declaration inside the law.
        let off = src.find("helper = 2").unwrap();
        let dir = std::path::Path::new("/fake/lsp-it855");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();
        let locs = occurrences_cross_file(src, "helper", off, dir, &empty_buffers);
        assert_eq!(locs.len(), 2, "only the law's own local decl + use, NOT the unrelated fun: {locs:?}");
        // Neither location may fall on line 0 (the unrelated `fun helper` declaration)
        // or line 1 (its call site inside `caller()`).
        assert!(locs.iter().all(|(_, l0, ..)| *l0 != 0 && *l0 != 1), "must not touch the unrelated fun: {locs:?}");

        // Renaming the TOP-LEVEL `fun helper` itself is symmetrically protected:
        // still finds only its own declaration + call site, not the law's local.
        let fun_off = src.find("fun helper").unwrap() + 4;
        let fun_locs = occurrences_cross_file(src, "helper", fun_off, dir, &empty_buffers);
        assert_eq!(fun_locs.len(), 2, "fun decl + its one call site: {fun_locs:?}");
        assert!(fun_locs.iter().all(|(_, l0, ..)| *l0 == 0 || *l0 == 1), "{fun_locs:?}");
    }

    /// Sibling of the test above (PR-it855), covering the SECOND code path the fix
    /// touched: a law nested INSIDE a `contract` block (`ContractDecl.laws`), not a
    /// standalone top-level `law`. Same shape, different AST route into `Item::Contract`.
    #[test]
    fn local_binding_scope_suppresses_same_file_rename_into_an_unrelated_top_level_declaration_through_a_contract_law() {
        let src = "fun helper() -> Int { 1 }\nfun caller() -> Int { helper() }\ncontract Store {\n    intent \"x\"\n    expose fun get() -> Int\n    law \"shadowed\" {\n        let helper = 2\n        expect helper == 2\n    }\n}\n";

        let off = src.find("helper = 2").unwrap();
        let dir = std::path::Path::new("/fake/lsp-it855-contract");
        let empty_buffers: HashMap<PathBuf, String> = HashMap::new();
        let locs = occurrences_cross_file(src, "helper", off, dir, &empty_buffers);
        assert_eq!(locs.len(), 2, "only the contract law's own local decl + use: {locs:?}");
        assert!(locs.iter().all(|(_, l0, ..)| *l0 != 0 && *l0 != 1), "must not touch the unrelated fun: {locs:?}");

        let fun_off = src.find("fun helper").unwrap() + 4;
        let fun_locs = occurrences_cross_file(src, "helper", fun_off, dir, &empty_buffers);
        assert_eq!(fun_locs.len(), 2, "fun decl + its one call site: {fun_locs:?}");
        assert!(fun_locs.iter().all(|(_, l0, ..)| *l0 == 0 || *l0 == 1), "{fun_locs:?}");
    }

    /// A REAL bug (production-hardening PR-it742): unlike the cross-file rename/
    /// goto-definition entry points (PR-it704/PR-it739), the SINGLE-FILE `resolve_hover`/
    /// `resolve_definition` never checked `locally_bound` at all -- hovering or
    /// goto-definition on a LOCAL variable sharing a bare name with an unrelated
    /// TOP-LEVEL declaration in the SAME file showed/navigated to that unrelated
    /// declaration instead of nothing. Also fixes a previously-unfixed SIBLING gap:
    /// `resolve_hover_cross_file`'s cross-file fallback loop never checked
    /// `locally_bound` either (unlike `resolve_definition_cross_file`, which PR-it704
    /// DID patch) -- a local not shadowed by any SAME-file top-level item could still
    /// reach an unrelated top-level item's signature in a `use`d file.
    #[test]
    fn locally_bound_suppresses_same_file_hover_and_definition_into_an_unrelated_top_level_declaration() {
        let src = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\nfun main() {\n    let mean = 5.0\n    print(mean)\n}\n";
        let off = src.find("mean = 5.0").unwrap();
        let line = src[..off].matches('\n').count();
        let line_start = src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let ch = off - line_start;

        // Hovering the local must NOT show the unrelated `fun mean`'s signature.
        assert!(
            resolve_hover(src, line, ch).is_none(),
            "hovering a local must not show an unrelated same-file top-level declaration"
        );
        // Goto-definition on the local must NOT navigate to the unrelated `fun mean`.
        assert!(
            resolve_definition(src, line, ch).is_none(),
            "goto-definition on a local must not navigate to an unrelated same-file top-level declaration"
        );

        // Sanity: hovering/goto-def on the TOP-LEVEL `fun mean` itself is unaffected.
        let fun_off = src.find("fun mean").unwrap() + 4;
        let fun_line = src[..fun_off].matches('\n').count();
        let fun_line_start = src[..fun_off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let fun_ch = fun_off - fun_line_start;
        assert!(resolve_hover(src, fun_line, fun_ch).is_some(), "the real top-level fn must still hover");
        assert!(resolve_definition(src, fun_line, fun_ch).is_some(), "the real top-level fn must still goto-def");

        // Cross-file hover sibling gap: a local NOT shadowed by any same-file top-level
        // item must not reach into a `use`d file's unrelated top-level item either.
        let dir = std::path::Path::new("/fake/lsp-it742");
        let main_text = "use stats\nfun report() -> Float {\n    let mean = 5.0\n    print(mean)\n    mean\n}\n";
        let stats_text = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\n";
        let mut buffers: HashMap<PathBuf, String> = HashMap::new();
        buffers.insert(dir.join("stats.kupl"), stats_text.to_string());
        let off2 = main_text.find("mean = 5.0").unwrap();
        let line2 = main_text[..off2].matches('\n').count();
        let line2_start = main_text[..off2].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let ch2 = off2 - line2_start;
        assert!(
            resolve_hover_cross_file(main_text, line2, ch2, dir, &buffers).is_none(),
            "a local not shadowed same-file must not reach an unrelated cross-file declaration via hover"
        );
    }

    #[test]
    fn completions_include_component_methods_and_state() {
        // The same gap class as it513's hover/go-to-definition fix, found by applying the
        // same scratch-probe methodology to `completions`: component methods (exposed or
        // private) and state fields were completely invisible to completion -- only the
        // component's OWN name was listed, since `completions` matched `Item::Component`
        // and pushed just the component name, never looking inside `c.exposes`/`c.funs`/
        // `c.state`. Typing `n` or `greet` inside a component body (the most common place
        // to type in a component-heavy KUPL program) got no completions for its own
        // members. Fixed by extending the Component arm to also emit each exposed/private
        // method (kind 3 = Function, reusing the shared fun_sig_str detail) and each state
        // field (kind 6 = Variable) (PR-it514).
        let src = "component Greeter {\n    intent \"g\"\n    state n: Int = 0\n    expose fun greet(name: Str) -> Str {\n        \"hi {name}\"\n    }\n    fun helper() -> Int {\n        5\n    }\n}\n";
        let items = completions(src);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(labels.contains(&"Greeter"), "the component's own name is still listed: {labels:?}");
        assert!(labels.contains(&"greet"), "exposed method must be a completion candidate: {labels:?}");
        assert!(labels.contains(&"helper"), "private method must be a completion candidate: {labels:?}");
        assert!(labels.contains(&"n"), "state field must be a completion candidate: {labels:?}");
        // the exposed method's completion carries its real signature as detail, like a
        // top-level function does.
        let greet = items.iter().find(|(l, _, _)| l == "greet").unwrap();
        assert_eq!(greet.1, 3, "method completion kind must be Function (3)");
        assert!(greet.2.contains("fun greet(name: Str) -> Str"), "{greet:?}");
        let n = items.iter().find(|(l, _, _)| l == "n").unwrap();
        assert_eq!(n.1, 6, "state field completion kind must be Variable (6)");
    }

    #[test]
    fn dispatch_helpers_reject_malformed_params_without_panic() {
        // A hostile/buggy editor can send any JSON as request params. The param
        // extractors must return None (never panic/unwrap) so the handler replies
        // with a clean `null` result rather than crashing the whole LSP server.
        let bad = [
            "{}",                                                     // no textDocument
            "{\"textDocument\":{}}",                                  // no uri
            "{\"textDocument\":{\"uri\":\"file:///a\"}}",             // no position
            "{\"textDocument\":{\"uri\":42},\"position\":{\"line\":0,\"character\":0}}", // uri not a string
            "{\"textDocument\":{\"uri\":\"file:///a\"},\"position\":{\"line\":\"x\",\"character\":0}}", // line not a number
            "{\"textDocument\":{\"uri\":\"file:///a\"},\"position\":{\"line\":-3,\"character\":0}}",     // negative line
            "{\"position\":{\"line\":0,\"character\":0}}",            // no textDocument at all
            "null",
            "[]",
            "\"just a string\"",
            "42",
        ];
        for c in bad {
            let j = parse_json(c).unwrap_or(Json::Null);
            assert_eq!(position_of(&j), None, "expected None for params: {c}");
        }
        // a well-formed request still parses
        let good = parse_json(
            "{\"textDocument\":{\"uri\":\"file:///a\"},\"position\":{\"line\":2,\"character\":5}}",
        )
        .unwrap();
        assert_eq!(position_of(&good), Some(("file:///a", 2, 5)));
        // doc_text on an unopened / malformed uri -> None (no unwrap on a missing doc)
        let empty: HashMap<PathBuf, String> = HashMap::new();
        assert_eq!(doc_text("not a uri", &empty), None);
        assert_eq!(doc_text("file:///no/such/path/xyz-kupl-lsp.kupl", &empty), None);
    }

    #[test]
    fn frame_reader_handles_malformed_frames() {
        use std::io::Cursor;
        let rd = |bytes: &str| read_message(&mut Cursor::new(bytes.as_bytes().to_vec()));
        // well-formed
        assert_eq!(rd("Content-Length: 2\r\n\r\n{}").as_deref(), Some("{}"));
        // no body at all -> EOF -> None (not a hang, not a panic)
        assert_eq!(rd(""), None);
        // header block with no Content-Length -> 0-length body (downstream JSON
        // parse rejects it); the point is it returns gracefully, no panic/hang
        assert_eq!(rd("garbage without a colon\r\n\r\n").as_deref(), Some(""));
        // garbage / negative / overflowing Content-Length -> parse fails -> None
        assert_eq!(rd("Content-Length: abc\r\n\r\n{}"), None);
        assert_eq!(rd("Content-Length: -5\r\n\r\n{}"), None);
        assert_eq!(rd("Content-Length: 999999999999999999999999\r\n\r\n{}"), None);
        // ABSURD but parseable length -> refused by the cap, NOT pre-allocated
        assert_eq!(rd("Content-Length: 999999999999\r\n\r\n{}"), None);
        // length larger than the actual body -> read_exact hits EOF -> None (no hang)
        assert_eq!(rd("Content-Length: 100\r\n\r\n{}"), None);
        // Content-Length: 0 -> empty body, handled
        assert_eq!(rd("Content-Length: 0\r\n\r\n").as_deref(), Some(""));
    }

    #[test]
    fn position_handlers_never_panic_on_edge_input() {
        // The LSP runs on live, mid-edit, malformed buffers with editor-supplied
        // positions that may be out of range or land mid-multibyte-UTF8. No handler
        // may panic (a crashed LSP kills editor features).
        let big = "fun f(){}\n".repeat(500);
        let docs = [
            "",
            "fun",                              // truncated
            "fun main() { print(",             // mid-edit
            "let café = 1\nlet 日本 = 2\n",      // multibyte identifiers/text
            "// 🎉🎉🎉 comment\nfun f() {}\n",   // emoji (4-byte) in a line
            "\"unterminated {interp",
            big.as_str(),                        // large-ish
        ];
        for doc in docs {
            for line in [0usize, 1, 2, 5, 100, usize::MAX] {
                for ch in [0usize, 1, 3, 4, 5, 50, 10_000, usize::MAX] {
                    // must return (Some/None), never panic — incl. positions that
                    // land mid-codepoint or past end of line/file
                    let _ = resolve_hover(doc, line, ch);
                    let _ = resolve_definition(doc, line, ch);
                }
            }
            let _ = completions(doc);
            let _ = occurrences(doc, "f");
        }
    }

    /// A robustness-audit finding (production-hardening PR-it618): the LSP
    /// methods added LATER (signatureHelp/codeAction/foldingRange, it586-590)
    /// were never added to `position_handlers_never_panic_on_edge_input`'s
    /// fuzz loop above -- that test only calls the four OLDER handlers
    /// (hover/definition/completions/occurrences). The newer ones take a
    /// DIFFERENT parameter shape too (`resolve_code_actions` takes raw BYTE
    /// OFFSETS, not line/character), so even re-running the exact same test
    /// wouldn't have exercised the right kind of adversarial input for it.
    /// Extends the same never-panic discipline to all three. No bug found —
    /// `resolve_signature_help` routes through the already-hardened
    /// `offset_at` (proven safe by the test above) before touching the AST;
    /// `resolve_code_actions`'s `start_off`/`end_off` are only ever used in
    /// numeric `<`/`>` comparisons against diagnostic spans, never as a
    /// slice index, so they're safe for ANY usize value including
    /// `usize::MAX`; `folding_ranges` only takes `text`. Locking this in as
    /// a permanent regression test now that it's been checked, rather than
    /// leaving the newer methods with weaker coverage than the older ones.
    #[test]
    fn newer_lsp_methods_never_panic_on_edge_input() {
        let big = "fun f(){}\n".repeat(500);
        let docs = [
            "",
            "fun",
            "fun main() { print(",
            "let café = 1\nlet 日本 = 2\n",
            "// 🎉🎉🎉 comment\nfun f() {}\n",
            "\"unterminated {interp",
            "fun f(x: Int",                    // truncated mid-signature
            "fun f(x: Int) uses io { g(",     // truncated mid-call
            big.as_str(),
        ];
        for doc in docs {
            for line in [0usize, 1, 2, 5, 100, usize::MAX] {
                for ch in [0usize, 1, 3, 4, 5, 50, 10_000, usize::MAX] {
                    let _ = resolve_signature_help(doc, line, ch);
                }
            }
            for off in [0usize, 1, 3, doc.len(), doc.len() + 1, 10_000, usize::MAX] {
                for end_off in [0usize, off, off.saturating_add(1), usize::MAX] {
                    let _ = resolve_code_actions(doc, off, end_off);
                }
            }
            let _ = folding_ranges(doc);
        }
    }

    #[test]
    fn parse_json_literal_keywords_are_validated_not_just_sniffed() {
        // A REAL BUG found+fixed (bug-hunt batch 153, PR-it545): the
        // true/false/null arms of `parse_value` used to check only the FIRST
        // byte and blindly advance `pos` by the literal's length, with no
        // check that the rest actually spelled the keyword -- garbage like
        // "not json" (starts with `n`) silently "parsed" as `Json::Null`
        // instead of failing. `ai.rs` reuses THIS parser (via
        // `crate::lsp::parse_json`) for ai-fun mock-response text, where
        // malformed input is deliberately tested (see
        // `ai::tests::shape_mismatch_message_is_kupl_syntax_not_rust_debug`
        // and friends) -- the leniency here (harmless for well-formed
        // JSON-RPC messages, this parser's original purpose) caused
        // interp/KVM's ai-fun error message to read "expected Int, model
        // returned null" for input that isn't valid JSON at all, while
        // native's stricter C mirror correctly reported "not valid JSON".
        // Valid literals still parse; garbage (incl. TRUNCATED input
        // shorter than the literal, e.g. a lone "t") now fails cleanly
        // instead of over-reading past the end of the buffer.
        assert_eq!(parse_json("true"), Ok(Json::Bool(true)));
        assert_eq!(parse_json("false"), Ok(Json::Bool(false)));
        assert_eq!(parse_json("null"), Ok(Json::Null));
        assert_eq!(parse_json("not json"), Err("invalid literal (expected `null`)".into()));
        assert_eq!(parse_json("tomato"), Err("invalid literal (expected `true`)".into()));
        assert_eq!(parse_json("foobar"), Err("invalid literal (expected `false`)".into()));
        assert_eq!(parse_json("t"), Err("invalid literal (expected `true`)".into()));
    }

    /// A REAL cross-engine wording+behavior gap found+fixed (production-
    /// hardening PR-it792), the SAME root cause as the literal-validation
    /// fix just above (this parser is a genuinely separate, independently
    /// reimplemented JSON parser from `json.rs`, used for LSP JSON-RPC AND
    /// `ai.rs`'s mock-response parsing) but THREE more previously-missed
    /// divergences from `json.rs`'s reference behavior, all confirmed live
    /// via `KUPL_AI_MOCK_<NAME>` against interp vs native's `k_json_parse`
    /// (which DOES mirror `json.rs` byte-for-byte): (1) trailing content
    /// after a valid top-level value used to be silently ACCEPTED here
    /// (`"123abc"` parsed as `123`, dropping `"abc"`) while native correctly
    /// REJECTED it -- a genuine succeed-vs-error divergence, not just
    /// wording; (2) an unrecognized leading byte (e.g. `X`) fell through to
    /// an always-attempted number scan, producing a bare `"invalid number"`
    /// instead of json.rs's `"unexpected character `X` at position N"`; (3)
    /// a numeric-shaped but unparseable token (`"12.3.4"`) produced a bare
    /// `"invalid number"` instead of json.rs's `"invalid number `12.3.4`"`
    /// (the scanned text embedded). Also fixes a smaller, independently-
    /// spotted wording slip: `"unexpected end of JSON"` -> `"unexpected end
    /// of input"`, matching json.rs exactly.
    #[test]
    fn parse_json_matches_json_rs_on_trailing_content_and_unexpected_characters() {
        assert_eq!(
            parse_json("123abc"),
            Err("unexpected trailing characters at position 3".into())
        );
        assert_eq!(parse_json("X"), Err("unexpected character `X` at position 0".into()));
        assert_eq!(parse_json("12.3.4"), Err("invalid number `12.3.4`".into()));
        assert_eq!(parse_json(""), Err("unexpected end of input".into()));
        // a non-ASCII unrecognized character reports a CHAR position, not a
        // byte position -- "é" is two bytes but the FIRST character.
        assert_eq!(parse_json("é"), Err("unexpected character `é` at position 0".into()));
        // trailing content after whitespace is still trailing content.
        assert_eq!(
            parse_json("true  garbage"),
            Err("unexpected trailing characters at position 6".into())
        );
        // genuinely well-formed input is completely unaffected.
        assert_eq!(parse_json("  42  "), Ok(Json::Num(42.0)));
    }

    /// A REAL, SEVERE robustness bug found+fixed (production-hardening
    /// PR-it620): this parser had NO recursion-depth guard at all, unlike
    /// json.rs's `parse` (the `json_parse` builtin, shared by interp/vm) and
    /// cgen.rs's `kjp_value` (native's mirror) -- both of which were ALREADY
    /// protected. Confirmed via direct reproduction BEFORE this fix: a
    /// document with 1,000 levels of `[` nesting overflowed the native stack
    /// and aborted the WHOLE TEST PROCESS (SIGABRT) -- not a catchable Rust
    /// panic, so `std::panic::set_hook`'s "internal compiler error" safety
    /// net (main.rs) can't help either; a stack overflow bypasses it
    /// entirely. This is used for LSP JSON-RPC (a malicious/buggy editor
    /// could send a deeply-nested `params` value) AND ai.rs's mock-response
    /// parsing -- a genuine crash-the-process DoS on an externally-facing
    /// surface, not just a missing test. Fixed by reusing
    /// `json::MAX_JSON_DEPTH` (matching the OTHER two parsers' limit exactly,
    /// rather than inventing a new one) and threading a depth counter through
    /// every recursive `parse_value` call. 100,000 levels of nesting (100x
    /// the limit) must now fail with a clean `Err`, not crash -- this test
    /// itself is the proof the fix actually prevents the stack overflow that
    /// used to happen at this depth.
    #[test]
    fn deeply_nested_json_is_rejected_not_a_stack_overflow() {
        let nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(parse_json(&nested), Err("JSON nested too deeply".into()));
        // well within the limit still parses fine.
        let shallow = format!("{}1{}", "[".repeat(50), "]".repeat(50));
        assert!(parse_json(&shallow).is_ok());
        // exactly at the boundary: MAX_JSON_DEPTH nested arrays is still ok,
        // one more is rejected -- pins the boundary precisely, not just "very
        // deep nesting eventually fails somewhere".
        let at_limit = format!("{}1{}", "[".repeat(crate::json::MAX_JSON_DEPTH), "]".repeat(crate::json::MAX_JSON_DEPTH));
        assert!(parse_json(&at_limit).is_ok(), "exactly MAX_JSON_DEPTH nested arrays must still parse");
        let over_limit =
            format!("{}1{}", "[".repeat(crate::json::MAX_JSON_DEPTH + 1), "]".repeat(crate::json::MAX_JSON_DEPTH + 1));
        assert_eq!(parse_json(&over_limit), Err("JSON nested too deeply".into()));
    }

    /// A narrow adversarial follow-up on PR-it620's own fix (production-
    /// hardening PR-it621), per that iteration's own guidance: candidate (1)
    /// was "does the fix correctly handle a NOTIFICATION (no `id` at all)
    /// whose JSON fails to parse". A NOTIFICATION is only distinguishable
    /// from a REQUEST by the ABSENCE of an `id` field on an otherwise-valid
    /// JSON-RPC envelope -- but when the top-level JSON fails to parse (the
    /// deep-nesting case), the server can never see that far into the
    /// message to know whether an `id` was present or not. The JSON-RPC 2.0
    /// spec's own convention (id:null for a parse error) exists precisely
    /// because of this: the server must always report a parse error, since
    /// silently guessing "this was probably a notification, drop it" could
    /// just as easily swallow a REQUEST's reply forever (the exact bug
    /// PR-it620 already fixed for the well-formed-envelope case). This test
    /// spawns the REAL `kupl lsp` process (following the it619 REPL
    /// subprocess-test pattern: background-thread stdin writer, so a large
    /// adversarial write can't deadlock against the child's own output) and
    /// sends a message SHAPED like a notification (`textDocument/didChange`,
    /// no `id` field) whose `params` is nested past `MAX_JSON_DEPTH` -- and
    /// confirms the server still replies with the spec-mandated id:null /
    /// -32700 parse-error response (not silence, not a hang, not a crash),
    /// and remains alive and fully functional for a subsequent normal
    /// request afterward. No bug found: this is the same unconditional
    /// parse-error path PR-it620 already wired up, which never branches on
    /// whether an `id` was present (it can't -- parsing failed before
    /// reaching that field) -- confirming intended, spec-compliant behavior
    /// with a live process-level regression test rather than leaving this
    /// specific shape untested.
    #[test]
    fn deeply_nested_notification_shaped_message_gets_a_parse_error_reply_not_silence() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let evil_params = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
        // note: no "id" field at all -- an otherwise-valid NOTIFICATION shape.
        let notification = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{evil_params}}}"#
        );
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let folding = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/foldingRange","params":{"textDocument":{"uri":"file:///nonexistent.kupl"}}}"#;

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            for body in [init, &notification, folding] {
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            }
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung reading a deeply-nested notification-shaped message");
        let stdout = String::from_utf8_lossy(&out.stdout);

        // 3 framed messages went in (init request, evil notification, folding
        // request); the server must reply to the request (id:1), then to the
        // evil message it can't distinguish from a request (id:null,
        // -32700), then to the final request (id:2) -- proving it's still
        // alive and fully functional afterward, not just not-crashed.
        let bodies: Vec<&str> = stdout
            .split("Content-Length:")
            .filter(|s| !s.trim().is_empty())
            .map(|chunk| chunk.split("\r\n\r\n").nth(1).unwrap_or("").trim())
            .collect();
        assert!(bodies.len() >= 2, "expected at least 2 responses, got {bodies:?}");
        let parse_error_reply = bodies.iter().find(|b| b.contains("-32700"));
        assert!(
            parse_error_reply.is_some(),
            "expected a -32700 parse-error reply among responses: {bodies:?}"
        );
        let reply = parse_error_reply.unwrap();
        assert!(reply.contains("\"id\":null"), "parse error reply must use id:null: {reply}");
        let final_reply = bodies.last().unwrap();
        assert!(
            final_reply.contains("\"id\":2"),
            "server must still answer a normal request after the adversarial notification: {final_reply}"
        );
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl lsp panicked: {stdout}"
        );
    }

    /// A REAL, live-confirmed document-state-corruption bug found+fixed
    /// (production-hardening PR-it754): this server declares
    /// `textDocumentSync: 1` (full sync) at `initialize`, which per the LSP
    /// spec obligates a compliant client to send `contentChanges[0].text`
    /// as the ENTIRE new document with NO `range` field on every
    /// `didChange` -- but the handler never verified that contract,
    /// unconditionally treating whatever text arrived as the full buffer.
    /// A client that sends an INCREMENTAL-style edit instead (a `range` +
    /// a tiny replacement fragment -- what `textDocumentSync: 2` clients
    /// send, never requested here) silently overwrote the ENTIRE document
    /// down to just that fragment, with zero error reported to the client.
    /// Spawns the REAL `kupl lsp` process (following this file's own
    /// established subprocess-test pattern): opens a real multi-line
    /// program, sends a single incremental-style `didChange` (a `range` +
    /// `text: "9"`), then requests `textDocument/documentSymbol` for the
    /// SAME uri -- if the buffer were corrupted to just `"9"` (a bare
    /// top-level integer, a parse error), `document_symbols` returns `None`
    /// (`"result":null`); if the buffer survived intact, the original
    /// function's name is still in the returned symbol list. Also confirms
    /// the server stays alive and answers a normal request afterward.
    #[test]
    fn an_incremental_style_didchange_does_not_corrupt_the_full_sync_document_buffer() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let full_src = "fun example_function_marker_it754() -> Int {\n    42\n}\n";
        let did_open = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///corruption_test_it754.kupl","text":{full_src:?}}}}}}}"#
        );
        // an INCREMENTAL-style edit: carries a `range`, and `text` is a tiny
        // replacement fragment, not the full document -- exactly what a
        // textDocumentSync:2 client would send, never requested by this
        // server's own declared textDocumentSync:1 capability.
        let incremental_change = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///corruption_test_it754.kupl","version":2},"contentChanges":[{"range":{"start":{"line":1,"character":4},"end":{"line":1,"character":6}},"rangeLength":2,"text":"9"}]}}"#;
        let symbol_req = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///corruption_test_it754.kupl"}}}"#;

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            for body in [init.to_string(), did_open, incremental_change.to_string(), symbol_req.to_string()] {
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            }
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung after an incremental-style didChange");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl lsp panicked: {stdout}"
        );

        let bodies: Vec<&str> = stdout
            .split("Content-Length:")
            .filter(|s| !s.trim().is_empty())
            .map(|chunk| chunk.split("\r\n\r\n").nth(1).unwrap_or("").trim())
            .collect();
        let symbol_reply = bodies
            .iter()
            .find(|b| b.contains("\"id\":2"))
            .expect("server must still answer the documentSymbol request after the incremental-style didChange");
        assert!(
            symbol_reply.contains("example_function_marker_it754"),
            "the original document must survive an incremental-style didChange intact -- \
             the buffer was corrupted down to just the malformed edit's fragment: {symbol_reply}"
        );
    }

    /// A REAL, live-confirmed silent-hang-the-client bug found+fixed
    /// (production-hardening PR-it755) -- the SAME root cause and severity
    /// class as PR-it620's own parse-error fix (a message this server can't
    /// process used to be silently dropped instead of cleanly erroring),
    /// just a DIFFERENT malformed message SHAPE: valid JSON whose TOP-LEVEL
    /// value is an ARRAY (a base JSON-RPC 2.0 "batch") rather than an
    /// object. `Json::get` only ever matches `Json::Obj`, so a top-level
    /// `Json::Arr` made both `method` and `id` extraction return `None` --
    /// `method` defaulted to `""` (falling into the dispatch match's
    /// catch-all arm), and since `id` was ALSO `None` there, that arm's own
    /// `if let Some(id) = id { ...respond... }` never fired: NO response
    /// was ever sent for the whole batch, so a request nested inside it
    /// (with its OWN legitimate `id`) waited forever for a reply that would
    /// never come. The LSP specification explicitly states batching is NOT
    /// supported (unlike the base JSON-RPC 2.0 protocol it otherwise
    /// follows), so a compliant client should never send one -- but this
    /// server had no defensive check at all. Spawns the REAL `kupl lsp`
    /// process (this file's own established subprocess-test pattern):
    /// sends `initialize`, then a top-level ARRAY containing one
    /// well-formed request, then a normal request -- confirms the batch
    /// gets a clean `-32600`/`id:null` error reply (not silence), and the
    /// server stays alive and answers the normal request afterward.
    #[test]
    fn a_top_level_json_rpc_batch_array_gets_a_clean_error_reply_not_silence() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        // a top-level ARRAY (a JSON-RPC batch) containing one well-formed
        // request with its OWN id (100) -- the LSP spec forbids this shape.
        let batch = r#"[{"jsonrpc":"2.0","id":100,"method":"textDocument/foldingRange","params":{"textDocument":{"uri":"file:///nonexistent.kupl"}}}]"#;
        let folding = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/foldingRange","params":{"textDocument":{"uri":"file:///nonexistent.kupl"}}}"#;

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            for body in [init, batch, folding] {
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            }
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung reading a top-level JSON-RPC batch array");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl lsp panicked: {stdout}"
        );

        let bodies: Vec<&str> = stdout
            .split("Content-Length:")
            .filter(|s| !s.trim().is_empty())
            .map(|chunk| chunk.split("\r\n\r\n").nth(1).unwrap_or("").trim())
            .collect();
        assert!(bodies.len() >= 2, "expected at least 2 responses (init + batch error), got {bodies:?}");
        let batch_reply = bodies.iter().find(|b| b.contains("-32600"));
        assert!(
            batch_reply.is_some(),
            "expected a -32600 Invalid Request reply for the batch array, got no such reply -- \
             the batch (and any request nested inside it, id:100) was silently dropped: {bodies:?}"
        );
        assert!(
            batch_reply.unwrap().contains("\"id\":null"),
            "batch rejection reply must use id:null (no single id belongs to the whole batch): {}",
            batch_reply.unwrap()
        );
        let final_reply = bodies.last().unwrap();
        assert!(
            final_reply.contains("\"id\":2"),
            "server must still answer a normal request after the batch array: {final_reply}"
        );
    }

    /// A permanent regression guard, per PR-it648 (no bug found this iteration --
    /// applying the "completeness claim vs actual implementation" methodology
    /// (`sdiff.rs` it643/it644/it646, `manifest_json` it647) to the LSP's
    /// `initialize` capability advertisement came back CLEAN: every
    /// `"textDocument/..."`/`"workspace/..."` dispatch match arm this module
    /// implements (hover, signatureHelp, codeAction, definition, references,
    /// documentHighlight, rename, completion, formatting, documentSymbol,
    /// foldingRange, workspace/symbol -- confirmed via a full read of the
    /// dispatch match block, not a skim) has a corresponding capability flag,
    /// and no capability is advertised without a matching handler. This test
    /// closes the gap of there being ZERO prior coverage of that claim -- so a
    /// FUTURE regression (a new method added without advertising it, or a
    /// capability flag left behind after a handler is removed) is caught
    /// automatically rather than relying on the same manual audit recurring.
    #[test]
    fn initialize_advertises_every_provider_capability_it_actually_implements() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", init.len(), init);
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung answering initialize");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let body = stdout.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        let v = parse_json(body).expect("initialize response must be valid JSON");
        let caps = v.get("result").and_then(|r| r.get("capabilities")).expect("capabilities object");

        // one entry per implemented `textDocument/x` or `workspace/x` request
        // handler (lifecycle methods `initialize`/`shutdown`/`exit` and the four
        // `did*` NOTIFICATIONS -- covered collectively by `textDocumentSync` --
        // are intentionally excluded, matching LSP's own convention).
        for key in [
            "hoverProvider",
            "definitionProvider",
            "referencesProvider",
            "renameProvider",
            "documentFormattingProvider",
            "documentSymbolProvider",
            "documentHighlightProvider",
            "workspaceSymbolProvider",
            "completionProvider",
            "signatureHelpProvider",
            "codeActionProvider",
            "foldingRangeProvider",
        ] {
            assert!(caps.get(key).is_some(), "missing advertised capability `{key}`: {caps:?}");
        }
        assert_eq!(
            caps.get("textDocumentSync").and_then(Json::as_usize),
            Some(1),
            "textDocumentSync must be full-sync (1), covering didOpen/didChange/didSave/didClose"
        );
        // the ONLY code action kind any handler ever emits is "quickfix" (grepped
        // the full `textDocument/codeAction` handler to confirm) -- advertising a
        // kind with no handler support (or vice versa) would mislead a client.
        let kinds = caps.get("codeActionProvider").and_then(|c| c.get("codeActionKinds"));
        assert_eq!(
            kinds.and_then(|k| k.index(0)).and_then(Json::str),
            Some("quickfix"),
            "codeActionKinds: {kinds:?}"
        );
    }

    fn wait_with_timeout_lsp(
        child: std::process::Child,
        timeout: std::time::Duration,
    ) -> Option<std::process::Output> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        rx.recv_timeout(timeout).ok().and_then(Result::ok)
    }

    #[test]
    fn hover_shows_fun_signature() {
        // position on `add` inside main's body (line 4, char ~10)
        let line = PROG.lines().position(|l| l.contains("print(add")).unwrap();
        let ch = PROG.lines().nth(line).unwrap().find("add").unwrap() + 1;
        let h = resolve_hover(PROG, line, ch).expect("hover on add");
        assert!(h.contains("fun add(a: Int, b: Int) -> Int"), "hover: {h}");
        // hover on a type name
        let tl = PROG.lines().position(|l| l.starts_with("type Shape")).unwrap();
        let h2 = resolve_hover(PROG, tl, 6).expect("hover on Shape");
        assert!(h2.contains("type Shape = Circle(r: Float) | Square(s: Float)"), "{h2}");
    }

    /// A REAL hover/signatureHelp content-quality bug (PR-it675): `ast::Param.default`
    /// (`x: Int = EXPR`) was parsed and used at call sites, and `fmt.rs`'s canonical
    /// formatter already renders it correctly -- but every LSP signature renderer
    /// (`fun_sig_str`, `contract_sig_str`, `signature_help_info`'s `params_of`) dropped
    /// it silently, showing a genuinely optional parameter as if it were required.
    #[test]
    fn hover_and_signature_help_show_parameter_default_values() {
        let src = "fun greet(name: Str = \"World\", loud: Bool = false) -> Str {\n    name\n}\n\
                   fun main() -> Str {\n    greet()\n}\n";
        let line = src.lines().position(|l| l.starts_with("fun greet")).unwrap();
        let ch = src.lines().nth(line).unwrap().find("greet").unwrap() + 1;
        let h = resolve_hover(src, line, ch).expect("hover on greet");
        assert!(
            h.contains("fun greet(name: Str = \"World\", loud: Bool = false) -> Str"),
            "hover must show BOTH default values: {h}"
        );
        // signatureHelp's per-parameter labels must also carry the default.
        let (program, _diags) = crate::parser::parse(src);
        let (label, params) = signature_help_info(&program, "greet").expect("signature help");
        assert!(label.contains("= \"World\"") && label.contains("= false"), "label: {label}");
        assert_eq!(params, vec!["name: Str = \"World\"", "loud: Bool = false"]);
    }

    #[test]
    fn definition_points_at_declaration() {
        // definition of `add` from its call site -> the `fun add` line
        let call_line = PROG.lines().position(|l| l.contains("print(add")).unwrap();
        let ch = PROG.lines().nth(call_line).unwrap().find("add").unwrap() + 1;
        let (l0, c0, _l1, _c1) = resolve_definition(PROG, call_line, ch).expect("definition");
        assert_eq!(l0, 0, "add is declared on line 0");
        assert_eq!(c0, 4, "the name starts after `fun `");
    }

    #[test]
    fn completion_lists_names_and_keywords() {
        let items = completions(PROG);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(labels.contains(&"add"));
        assert!(labels.contains(&"Shape"));
        assert!(labels.contains(&"Circle")); // constructor
        assert!(labels.contains(&"match")); // keyword
        // `add` completion carries its signature detail and Function kind
        let add = items.iter().find(|(l, _, _)| l == "add").unwrap();
        assert_eq!(add.1, 3);
        assert!(add.2.contains("-> Int"));
    }

    #[test]
    fn occurrences_skips_strings_and_comments() {
        // `add` appears: decl (l0), the call in main, plus a string + a comment
        // that must NOT match.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
                   // add is a helper\n\
                   fun main() uses io {\n    print(\"call add here\")\n    print(add(1, 2))\n}\n";
        let occ = occurrences(src, "add");
        // exactly two real identifier occurrences: the `fun add` decl and the
        // `add(1, 2)` call — NOT the comment, NOT the string literal.
        assert_eq!(occ.len(), 2, "occ: {occ:?}");
        assert_eq!(occ[0].0, 0); // declaration on line 0
    }

    #[test]
    fn references_and_rename() {
        // references from any occurrence returns all of them (decl + uses)
        let refs = occurrences(PROG, "add");
        assert!(refs.len() >= 2, "add is declared once and called once: {refs:?}");
        // ident under the call site resolves to `add`
        let call_line = PROG.lines().position(|l| l.contains("print(add")).unwrap();
        let ch = PROG.lines().nth(call_line).unwrap().find("add").unwrap() + 1;
        assert_eq!(ident_under(PROG, call_line, ch).as_deref(), Some("add"));
        // rename would produce one edit per occurrence (same count)
        assert_eq!(occurrences(PROG, "add").len(), refs.len());
    }

    #[test]
    fn occurrences_and_rename_work_on_component_methods() {
        // Follow-up to it513/it514 (which found item_signature/item_definition/completions
        // all shared the same bug: they only ever searched TOP-LEVEL program.items, blind to
        // methods nested inside Item::Component). occurrences (and therefore rename, which is
        // built entirely on occurrences) is architecturally DIFFERENT -- it works over the
        // LEXER's flat token stream, not the AST's item list, so it has no notion of
        // "top-level" at all and was never susceptible to that bug class. Locked here as a
        // genuine CLEAN finding (PR-it515), not an assumption: verified both the declaration
        // site (inside `expose fun greet`) and the call site (`g.greet(...)`) are found, and
        // that `ident_under` at the call site resolves to the same name rename would target.
        let src = "component Greeter {\n    intent \"g\"\n    expose fun greet(name: Str) -> Str {\n        \"hi {name}\"\n    }\n}\nfun main() {\n    let g = Greeter()\n    print(g.greet(\"x\"))\n}\n";
        let occ = occurrences(src, "greet");
        assert_eq!(occ.len(), 2, "declaration + call site, both found via the token stream: {occ:?}");
        let decl_line = src.lines().position(|l| l.contains("expose fun greet")).unwrap();
        let call_line = src.lines().position(|l| l.contains("g.greet")).unwrap();
        assert!(occ.iter().any(|(l, _, _, _)| *l == decl_line), "declaration site missing: {occ:?}");
        assert!(occ.iter().any(|(l, _, _, _)| *l == call_line), "call site missing: {occ:?}");
        let ch = src.lines().nth(call_line).unwrap().find("greet").unwrap() + 1;
        assert_eq!(ident_under(src, call_line, ch).as_deref(), Some("greet"));
        // rename would produce one edit per occurrence (same count) -- unaffected by which
        // component the method lives in.
        assert_eq!(occurrences(src, "greet").len(), occ.len());
    }

    #[test]
    fn is_valid_new_identifier_rejects_syntactically_invalid_names() {
        // rejected: empty, contains a space/punctuation, starts with a digit
        for bad in ["", "123 bad-name!", "9lives", "bad-name", "has space", "!", "a.b"] {
            assert!(!is_valid_new_identifier(bad), "must reject {bad:?} as a new identifier");
        }
        // accepted: ordinary ASCII, underscore-led, non-ASCII (café/日本 -- KUPL
        // explicitly supports these as real identifiers, matching `lexer.rs::
        // lex_ident`'s own `byte >= 0x80` dispatch condition)
        for good in ["foo", "_valid", "café_helper", "日本語", "a1", "CamelCase"] {
            assert!(is_valid_new_identifier(good), "must accept {good:?} as a new identifier");
        }
    }

    /// A REAL bug found+fixed (production-hardening PR-it767, from a fresh
    /// Explore survey): `textDocument/rename`'s `newName` was accepted
    /// VERBATIM with zero validation before being embedded into an outgoing
    /// `WorkspaceEdit` -- every mainstream LSP client applies a rename edit
    /// immediately and unconditionally once the server returns one, so an
    /// invalid `newName` silently produced a "successful"-looking response
    /// that would corrupt previously-working source if applied, with no
    /// error surfaced anywhere. Live-confirmed BEFORE this fix via a raw LSP
    /// stdio session: renaming `foo` to `"123 bad-name!"` (not an identifier
    /// at all) and to `""` (empty) both returned well-formed `WorkspaceEdit`s
    /// (2 edits each, covering the declaration and the call site) instead of
    /// an error/`null`. Spawns the REAL `kupl lsp` process (this file's own
    /// established subprocess-test pattern) to exercise the actual wire-level
    /// handler, not just the underlying pure `is_valid_new_identifier` check
    /// (covered in isolation by the test above).
    #[test]
    fn rename_with_an_invalid_new_name_returns_null_not_a_corrupting_edit() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let src = "fun foo(x: Int) -> Int {\n    x + 1\n}\n\nfun main() {\n    print(\"{foo(1)}\")\n}\n";
        let did_open = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///rename_invalid_it767.kupl","text":{src:?}}}}}}}"#
        );
        let rename_bad = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_invalid_it767.kupl"},"position":{"line":0,"character":4},"newName":"123 bad-name!"}}"#;
        let rename_empty = r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_invalid_it767.kupl"},"position":{"line":0,"character":4},"newName":""}}"#;
        let rename_ok = r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_invalid_it767.kupl"},"position":{"line":0,"character":4},"newName":"bar"}}"#;

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            for body in [init.to_string(), did_open, rename_bad.to_string(), rename_empty.to_string(), rename_ok.to_string()] {
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            }
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung on a rename request");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl lsp panicked: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":2,"result":null"#),
            "renaming to a syntactically invalid name must return null, not a corrupting edit: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":3,"result":null"#),
            "renaming to an empty name must return null, not a corrupting edit: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":4,"result":{"changes""#),
            "a genuinely valid rename must still work: {stdout}"
        );
    }

    /// A REAL gap found+fixed (production-hardening PR-it787): carried
    /// forward, deliberately deferred, and never actually attempted since
    /// PR-it767 (which fixed `newName`'s own SYNTACTIC validation but
    /// explicitly left this sub-case open) and reconfirmed by an Explore
    /// survey at PR-it780 -- `textDocument/rename` never checked whether
    /// `newName` ALREADY names a DIFFERENT top-level item in the SAME file.
    /// Live-confirmed BEFORE this fix via a raw LSP stdio session (this
    /// file's own established subprocess pattern, mirroring the test right
    /// above): renaming `fun helper` to `main`, when `fun main` already
    /// exists in the same file, returned a well-formed, non-null
    /// `WorkspaceEdit` that a client would apply immediately and
    /// unconditionally, silently producing a file with TWO top-level items
    /// named `main` -- a duplicate-definition compile error with zero
    /// warning before the edit landed. Also confirms two things the fix must
    /// NOT break: renaming to a genuinely free name still works, and a
    /// same-name "rename" (`newName` identical to the current name) is not
    /// mistaken for a self-collision.
    #[test]
    fn rename_into_an_existing_top_level_name_returns_null_not_a_duplicate_definition() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let mut child = std::process::Command::new(&bin)
            .arg("lsp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl lsp spawns");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let src = "fun helper() -> Int {\n    1\n}\n\nfun main() {\n    print(helper())\n}\n";
        let did_open = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///rename_collision_it787.kupl","text":{src:?}}}}}}}"#
        );
        // cursor on `helper`'s own declaration (line 0, "fun " is 4 chars).
        let rename_collides = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_collision_it787.kupl"},"position":{"line":0,"character":4},"newName":"main"}}"#;
        let rename_same_name = r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_collision_it787.kupl"},"position":{"line":0,"character":4},"newName":"helper"}}"#;
        let rename_ok = r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///rename_collision_it787.kupl"},"position":{"line":0,"character":4},"newName":"compute"}}"#;

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            for body in [init.to_string(), did_open, rename_collides.to_string(), rename_same_name.to_string(), rename_ok.to_string()] {
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            }
        });

        let out = wait_with_timeout_lsp(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl lsp hung on a rename request");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl lsp panicked: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":2,"result":null"#),
            "renaming into an EXISTING top-level name must return null, not a duplicate-definition edit: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":3,"result":{"changes""#),
            "a same-name \"rename\" must not be mistaken for a self-collision: {stdout}"
        );
        assert!(
            stdout.contains(r#""id":4,"result":{"changes""#),
            "renaming to a genuinely free name must still work: {stdout}"
        );
    }

    #[test]
    fn references_include_string_interpolation() {
        // A variable used inside a `"{x}"` interpolation is a REAL reference that
        // rename must update — but plain string TEXT and comments must not be touched.
        // Before PR-it94, occurrences only scanned bare Ident tokens and silently
        // missed interpolation uses, so a rename left `{x}` pointing at the old name.
        let src = "fun greet(x: Str) -> Str {\n    let y = x\n    \"hi {x}, the letter x\" // x here\n}\n";
        let refs = occurrences(src, "x");
        // param `x`, `= x`, and `{x}` = 3; the plain "letter x" text and the `// x`
        // comment are NOT identifiers, so they're excluded.
        assert_eq!(refs.len(), 3, "param + use + interpolation only: {refs:?}");
        // the interpolation occurrence is on the string's line (0-based line 2).
        assert!(refs.iter().any(|(l, _, _, _)| *l == 2), "interpolation ref on line 2: {refs:?}");
    }

    #[test]
    fn offset_and_ident_at() {
        assert_eq!(offset_at("ab\ncd", 1, 1), 4); // 'd'
        let (n, _, _) = ident_at("let foo = 1", 5).unwrap();
        assert_eq!(n, "foo");
    }

    /// A REAL bug found+fixed (production-hardening PR-it740): `offset_at` used
    /// to treat the LSP `character` field as a raw BYTE offset, but the LSP spec
    /// (and every real client -- VS Code, etc.) sends it as a UTF-16 CODE UNIT
    /// offset. On any line with a multi-byte UTF-8 character BEFORE the target
    /// column, the two counts diverge, so a real client's request landed on the
    /// WRONG identifier -- silently, no panic, just a wrong result.
    #[test]
    fn offset_at_treats_character_as_utf16_units_not_bytes() {
        let text = "let café = mean\n";
        // "let café = " is 11 UTF-16 units (l,e,t,sp,c,a,f,é,sp,=,sp -- é is ONE
        // unit) but 12 BYTES (é is TWO UTF-8 bytes), so "mean" starts at UTF-16
        // unit 11 / byte 12. A real client's cursor-at-start-of-"mean" request
        // sends character=11; the OLD byte-based code read that as byte offset
        // 11, landing on the space one byte short of "mean" (bytes: ...café" is
        // c=4,a=5,f=6,é=7-8, sp=9, '='=10, sp=11, 'm'=12), so `ident_at` found
        // no identifier there at all (a space has no adjacent ident chars).
        assert_eq!(offset_at(text, 0, 11), 12, "'mean' starts at byte 12 but UTF-16 unit 11");
        assert_eq!(
            ident_under(text, 0, 11).as_deref(),
            Some("mean"),
            "a real client's UTF-16 character=11 must resolve to 'mean', not land on the space before it"
        );

        // Sanity: the identifier BEFORE the multi-byte char is unaffected --
        // character=4 (UTF-16 unit, right before 'c') is also byte offset 4,
        // since everything up to that point is single-byte ASCII.
        assert_eq!(offset_at(text, 0, 4), 4);
        assert_eq!(ident_under(text, 0, 4).as_deref(), Some("café"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it764, the exact
    /// residual gap PR-it740's own doc comment above left open): the ENCODE
    /// side (`diag::line_col`, used to build every LSP position this server
    /// SENDS back) computed its column as a raw Unicode-scalar CHAR count,
    /// which happens to equal the UTF-16 unit count for every BMP character
    /// (café's é, 日本's CJK characters -- all exactly 1 UTF-16 unit each,
    /// covering PR-it740's own fix) -- but UNDER-counts by 1 for every
    /// ASTRAL-PLANE character (anything above U+FFFF, e.g. an emoji, encoded
    /// as a UTF-16 SURROGATE PAIR = 2 units) preceding the target position on
    /// the same line. `is_ident` categorically excludes such characters from
    /// ever being part of an identifier, so this couldn't misdirect a
    /// hover/rename/definition request the way PR-it740's decode-side bug
    /// did -- but it silently shifted every published diagnostic position on
    /// any line containing an astral character, for every real client. Live-
    /// confirmed BEFORE this fix (via a raw-UTF-8 JSON-RPC message, matching
    /// what a real client actually sends over the wire): `fun main() { let s
    /// = "🎉"; bogus_undefined_name }` published its `K0240` diagnostic at
    /// `character: 26`, one short of the correct UTF-16 offset `27`.
    #[test]
    fn published_diagnostic_positions_count_an_astral_plane_character_as_two_utf16_units() {
        let dir = std::env::temp_dir().join(format!("kupl-lsp-astral-it764-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("astral.kupl");
        // "🎉" (U+1F389) is ONE Rust char / Unicode scalar value, but TWO
        // UTF-16 code units (a surrogate pair) -- the diagnostic for
        // `bogus_undefined_name`, on the SAME line, must count it as such.
        let src = "fun main() { let s = \"\u{1F389}\"; bogus_undefined_name }\n";
        std::fs::write(&file, src).unwrap();
        let uri = format!("file://{}", file.display());
        let note = diagnostics_notification(&file, &uri, &HashMap::new());
        assert!(note.contains("K0240"), "{note}");
        assert!(
            note.contains("\"character\":27"),
            "the astral-plane emoji must count as 2 UTF-16 units, putting the diagnostic at character 27, not 26: {note}"
        );
        assert!(
            !note.contains("\"character\":26"),
            "must not under-count by treating the emoji as a single UTF-16 unit: {note}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_roundtrip() {
        let v = parse_json(r#"{"a": [1, 2.5, "x\ny", true, null], "b": {"c": -3}}"#).unwrap();
        assert_eq!(v.get("b").and_then(|b| b.get("c")), Some(&Json::Num(-3.0)));
        assert_eq!(
            v.get("a").and_then(|a| a.index(2)).and_then(Json::str),
            Some("x\ny")
        );
    }

    /// A REAL bug found+fixed (production-hardening PR-it765): this parser's
    /// `\u` escape handling decoded each `\uXXXX` INDEPENDENTLY, with no
    /// surrogate-PAIR combination logic -- unlike `json.rs`'s own, already-
    /// tested surrogate-pairing logic (this is a genuinely SEPARATE,
    /// independently-reimplemented JSON parser, used for LSP JSON-RPC AND
    /// ai.rs's mock-response parsing, per PR-it620's own doc comment on
    /// `parse_value`). A high surrogate (D800..=DBFF) followed by its low
    /// half (DC00..=DFFF) is required to form ONE astral code point (e.g.
    /// an emoji); decoding each half separately instead produced TWO
    /// U+FFFD replacement characters. Live-confirmed BEFORE this fix:
    /// `parse_json("\"\\uD83C\\uDF89\"")` (the emoji's own true surrogate
    /// pair) decoded to `"\u{FFFD}\u{FFFD}"` instead of the single correct
    /// emoji codepoint -- this is EXACTLY what made PR-it764's own first
    /// test attempt misleadingly appear to pass (a real client that
    /// `\u`-escapes an astral character in outgoing JSON would have hit
    /// this bug too, masking the `diag::line_col` astral under-counting
    /// bug PR-it764 actually fixed, by an unrelated coincidence of two
    /// replacement characters summing to the same UTF-16 unit count as one
    /// real emoji).
    #[test]
    fn u_escape_surrogate_pairs_combine_into_one_astral_code_point() {
        let v = parse_json("\"\\uD83C\\uDF89\"").unwrap();
        assert_eq!(v, Json::Str("🎉".to_string()), "a real UTF-16 surrogate pair must decode to the ONE emoji codepoint it encodes");

        // an UNPAIRED high surrogate (no following low surrogate) still
        // decodes to U+FFFD, same as before this fix -- not a regression.
        let v2 = parse_json("\"\\uD83Cx\"").unwrap();
        assert_eq!(v2, Json::Str("\u{FFFD}x".to_string()));
    }

    /// A REAL, genuinely process-ABORTING bug found+fixed (production-
    /// hardening PR-it765, the SAME `\u` escape arm as the test above): a
    /// truncated `\u` escape at the end of input -- e.g. a message cut off
    /// mid-escape, or a client bug -- panicked via direct `&b[*pos+1..
    /// *pos+5]` slice indexing ("range end index ... out of range for
    /// slice") instead of returning a clean parse error, confirmed live
    /// BEFORE this fix via `parse_json("\"\\u12\"")`. Every OTHER literal-
    /// matching arm in this same function was already hardened against
    /// malformed/truncated input (see PR-it545's `starts_with` bounds-
    /// safety comment on the `true`/`false`/`null` arms above) -- this ONE
    /// `\u` escape site was the gap. Since this parser backs `kupl lsp`'s
    /// entire incoming JSON-RPC message decode path, an adversarial or
    /// simply buggy client sending a truncated `\u` escape would crash the
    /// WHOLE server process (`main.rs` runs the whole CLI single-threaded,
    /// so an uncaught Rust panic here is a genuine process abort, not a
    /// catchable per-request error), losing every other open document's
    /// unsaved state too.
    #[test]
    fn a_truncated_u_escape_is_a_clean_parse_error_not_a_panic() {
        assert!(parse_json("\"\\u12\"").is_err(), "a truncated \\u escape must be a clean error, not panic");
        assert!(parse_json("\"\\u\"").is_err(), "a completely empty \\u escape must also be a clean error");
        assert!(parse_json("\"\\uD83C\\u\"").is_err(), "a truncated LOW surrogate half must also be a clean error, not panic");
    }

    #[test]
    fn diagnostics_for_buffer_override() {
        let dir = std::env::temp_dir().join(format!("kupl-lsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.kupl");
        std::fs::write(&file, "fun ok() -> Int {\n    1\n}\n").unwrap();

        // buffer has an error even though the disk file is fine
        let mut buffers = HashMap::new();
        buffers.insert(file.clone(), "fun bad() -> Int {\n    \"str\"\n}\n".to_string());
        let uri = format!("file://{}", file.display());
        let note = diagnostics_notification(&file, &uri, &buffers);
        assert!(note.contains("publishDiagnostics"));
        assert!(note.contains("K0200"), "{note}");

        // saved state: clean
        let note2 = diagnostics_notification(&file, &uri, &HashMap::new());
        assert!(note2.contains("\"diagnostics\":[]"), "{note2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn formatting_reformats_comment_free_source_and_is_idempotent() {
        // A real LSP capability gap (bug-hunt batch 141, PR-it529): `kupl fmt`
        // has existed as a CLI command all along, but the LSP server never
        // advertised `documentFormattingProvider` or handled
        // `textDocument/formatting` at all -- so no editor's "Format Document"
        // command (or format-on-save) could ever reach it, only the CLI.
        let messy = "fun add(a:Int,b:Int)->Int{\n  a+b\n}\n";
        let edits = resolve_formatting(messy).expect("parses cleanly, should format");
        assert!(edits.contains("fun add(a: Int, b: Int) -> Int"), "{edits}");
        assert!(edits.contains("\"line\":0,\"character\":0"), "whole-document range should start at (0,0): {edits}");
        // end-of-range is the position right after the LAST character (3 lines +
        // trailing newline -> end line index 3, column 0)
        assert!(edits.contains("\"line\":3,\"character\":0"), "{edits}");

        // running the formatter's OWN output back through itself is a no-op —
        // format-on-save must not thrash a file it just formatted
        let formatted = crate::fmt::format_program(&crate::parser::parse(messy).0);
        assert_eq!(resolve_formatting(&formatted), Some("[]".to_string()), "already-formatted source must be a no-op, not a spurious edit");
    }

    #[test]
    fn formatting_never_touches_a_file_with_comments() {
        // SAFETY GATE (same class as it518's rename hazard): `format_program`
        // renders from the AST, which drops comments entirely. Format-on-save
        // fires with no CLI-style warning the user could see first, so
        // formatting a commented file must be a SAFE NO-OP, never a silent
        // comment-deleting edit.
        let commented = "// keeps the sum\nfun add(a:Int,b:Int)->Int{\n  a+b\n}\n";
        assert!(crate::fmt::source_has_comments(commented));
        assert_eq!(resolve_formatting(commented), Some("[]".to_string()), "must not silently drop comments via format-on-save");

        // unparseable source: nothing safe to format at all
        assert_eq!(resolve_formatting("fun add(a: Int, b: Int -> Int {\n    a + b\n}\n"), None);
    }

    /// A REAL, live-confirmed DATA-LOSS bug found+fixed (production-hardening
    /// PR-it837): a `Float`/`F32` literal whose magnitude overflows to
    /// infinity (`1e400`, `1e40f32`, ...) is silently accepted by the lexer
    /// with ZERO diagnostic anywhere in the pipeline -- `let x: Float =
    /// 1e400` runs fine, printing `inf`. But `fmt::format_program` renders
    /// the resulting non-finite value via `Display`, producing the bare text
    /// `inf`/`-inf`/`inff32` -- NOT valid KUPL syntax (the lexer has no
    /// `inf`/`nan` literal form; it re-lexes as an ordinary IDENTIFIER,
    /// syntactically fine but an "unknown name" once the checker runs,
    /// K0240). `textDocument/formatting` used to propose this AS a
    /// format-on-save edit; the CLI's `kupl fmt --write` used to overwrite
    /// the file with it UNCONDITIONALLY, permanently destroying the
    /// original with no backup. Both are now a safe no-op (`[]`) here / a
    /// refused write with the file left untouched (see `main.rs`'s own
    /// `--write` handler and its matching test in `main.rs`'s own test
    /// module). A PLAIN re-parse alone would NOT have caught this -- `inf`
    /// as a bare identifier parses cleanly; only the FULL `run::compile`
    /// pipeline (parser + checker) surfaces K0240.
    #[test]
    fn formatting_never_proposes_a_non_reparseable_edit_for_an_overflowing_float_literal() {
        let src = "fun main() uses io {\n    let x: Float = 1e400\n    print(\"{x}\")\n}\n";
        // confirm the ROOT CAUSE precisely: the ORIGINAL source is valid and
        // runs fine (silently becoming `inf`), but format_program's rendering
        // of that Float value does NOT compile.
        assert!(crate::run::compile(src).is_ok(), "the original 1e400 literal must compile fine");
        let (program, _diags) = crate::parser::parse(src);
        let formatted = crate::fmt::format_program(&program);
        assert!(formatted.contains("inf"), "sanity: the bug's exact mechanism -- {formatted}");
        assert!(crate::run::compile(&formatted).is_err(), "sanity: the formatted `inf` text must NOT compile -- {formatted}");
        // the actual fix: resolve_formatting must refuse to propose this edit
        assert_eq!(
            resolve_formatting(src),
            Some("[]".to_string()),
            "must not propose a format-on-save edit that would corrupt the file"
        );

        // the F32 variant shares the identical mechanism.
        let src_f32 = "fun main() uses io { let x = 1e40f32 ; print(\"{x}\") }\n";
        assert!(crate::run::compile(src_f32).is_ok());
        assert_eq!(resolve_formatting(src_f32), Some("[]".to_string()));
    }

    #[test]
    fn document_symbols_outline_includes_nested_component_members() {
        // A real LSP capability gap (bug-hunt batch 142, PR-it530): no
        // `textDocument/documentSymbol` support at all -- so "Go to Symbol in
        // File" / breadcrumbs / outline-view had nothing to show for any
        // `.kupl` file, despite hover/definition/completion all working.
        // Built NESTED from the start (component state/methods as children of
        // the component symbol) rather than top-level-only, since exactly that
        // gap (blind to `Item::Component`'s nested members) caused THREE real
        // bugs already this campaign (it513/it514) -- an outline that only
        // shows component NAMES, none of their methods, would repeat the same
        // mistake in a fourth place.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\ntype Shape = Circle(r: Float) | Square(s: Float)\ncomponent Sensor {\n    intent \"s\"\n    expose fun read() -> Int {\n        0\n    }\n}\ncomponent Greeter {\n    intent \"g\"\n    prop label: Str\n    state n: Int = 0\n    let temp = Sensor()\n    expose fun greet(name: Str) -> Str {\n        \"hi {name}\"\n    }\n    fun helper() -> Int {\n        5\n    }\n}\ncontract Store {\n    expose fun get(k: Str) -> Int\n}\n";
        let syms = document_symbols(src).expect("parses cleanly, should outline");

        // top-level items present with the right kinds
        assert!(syms.contains("\"name\":\"add\",\"kind\":12"), "{syms}"); // Function
        assert!(syms.contains("\"name\":\"Shape\",\"kind\":10"), "{syms}"); // Enum
        assert!(syms.contains("\"name\":\"Greeter\",\"kind\":5"), "{syms}"); // Class
        assert!(syms.contains("\"name\":\"Store\",\"kind\":11"), "{syms}"); // Interface

        // ADT variants nested under the type
        assert!(syms.contains("\"name\":\"Circle\",\"kind\":22"), "{syms}"); // EnumMember
        assert!(syms.contains("\"name\":\"Square\",\"kind\":22"), "{syms}");

        // component props (production-hardening PR-it872, the SAME gap class as
        // state below, just never itself mirrored for `ComponentDecl.props`) +
        // state + a CHILD (production-hardening PR-it873, the SAME gap class
        // again, never itself mirrored for `ComponentDecl.children`) + BOTH
        // exposed and private methods, all nested under the component
        assert!(syms.contains("\"name\":\"label\",\"kind\":8"), "{syms}"); // Field
        assert!(syms.contains("\"name\":\"n\",\"kind\":8"), "{syms}"); // Field
        assert!(syms.contains("\"name\":\"temp\",\"kind\":8"), "{syms}"); // Field (child)
        assert!(syms.contains("\"name\":\"greet\",\"kind\":6"), "{syms}"); // Method
        assert!(syms.contains("\"name\":\"helper\",\"kind\":6"), "{syms}");

        // contract signature nested under the contract
        assert!(syms.contains("\"name\":\"get\",\"kind\":6"), "{syms}");

        // unparseable source: nothing safe to outline
        assert_eq!(document_symbols("fun add(a: Int, b: Int -> Int {\n    a + b\n}\n"), None);
    }

    /// A REAL outline content-quality gap (PR-it676, follow-up to it675):
    /// hover/completion/signatureHelp all show a callable's full signature,
    /// but `documentSymbol`'s outline/breadcrumb entries showed ONLY bare
    /// names -- never populating LSP's own `detail` field, whose spec
    /// wording is literally "more detail for this symbol, e.g. the signature
    /// of a function". Fixed by threading each item's already-correct
    /// signature string (or declared type, for state fields) through.
    #[test]
    fn document_symbols_populate_the_detail_field_with_signatures() {
        let src = "fun add(a: Int, b: Int = 1) -> Int {\n    a + b\n}\ntype Shape = Circle(r: Float) | Nothing\ncomponent Greeter {\n    intent \"g\"\n    state n: Int = 0\n    expose fun greet(name: Str) -> Str {\n        \"hi {name}\"\n    }\n}\ncontract Store {\n    expose fun get(k: Str) -> Int\n}\n";
        let syms = document_symbols(src).expect("parses cleanly, should outline");

        // top-level function: full signature, including the default value.
        assert!(syms.contains("\"detail\":\"fun add(a: Int, b: Int = 1) -> Int\""), "{syms}");
        // ADT variant: constructor field list.
        assert!(syms.contains("\"detail\":\"Circle(r: Float)\""), "{syms}");
        // component method: full signature, same as hover would show.
        assert!(syms.contains("\"detail\":\"fun greet(name: Str) -> Str\""), "{syms}");
        // component state field: its declared type.
        assert!(syms.contains("\"name\":\"n\",\"kind\":8"), "{syms}");
        assert!(syms.contains("\"detail\":\"Int\""), "{syms}");
        // contract method: full signature.
        assert!(syms.contains("\"detail\":\"expose fun get(k: Str) -> Int\""), "{syms}");
        // Exactly 5 symbols have a natural signature/type here (add, Circle,
        // greet, n, get) -- a fieldless variant (`Nothing`) and every item
        // with no natural signature (Shape/Greeter/Store's OWN entries) must
        // NOT carry a spurious `"detail"` key at all, not even an empty one.
        assert_eq!(syms.matches("\"detail\"").count(), 5, "{syms}");
    }

    #[test]
    fn folding_ranges_cover_every_multiline_construct_but_skip_one_liners() {
        // A NEW LSP capability (PR-it590): `textDocument/foldingRange`, the last
        // confirmed-missing method from a quick spec-vs-implementation inventory
        // (codeLens/inlayHint/documentLink/pull-mode-diagnostics were all considered
        // and are either not well-scoped for KUPL yet or lower value; foldingRange is
        // the one every general-purpose editor expects out of the box). Deliberately
        // WIDER than `item_symbol`'s outline children: component `on X` handlers and
        // `example { ... }` blocks, and contract `law "..." { ... }` bodies, are all
        // real multi-line bodies worth folding even though none of them appear as
        // documentSymbol children today (outline and folding are different concerns).
        let src = "component Counter {\n    intent \"c\"\n    in click: Event\n    state n: Int = 0\n    \
                   on click {\n        n += 1\n    }\n    example {\n        send click\n        expect n == 1\n    }\n}\n\
                   contract Store {\n    expose fun get(k: Str) -> Int\n    law \"roundtrip\" {\n        expect get(\"x\") == 0\n    }\n}\n";
        let ranges = folding_ranges(src).expect("parses cleanly, should have folding ranges");

        // component body, its `on click` handler, and its `example` block
        assert!(ranges.contains("{\"startLine\":0,\"endLine\":11}"), "{ranges}"); // component Counter
        assert!(ranges.contains("{\"startLine\":4,\"endLine\":6}"), "{ranges}"); // on click
        assert!(ranges.contains("{\"startLine\":7,\"endLine\":10}"), "{ranges}"); // example

        // contract body and its law -- but NOT the single-line `expose fun get(...)` sig
        assert!(ranges.contains("{\"startLine\":12,\"endLine\":17}"), "{ranges}"); // contract Store
        assert!(ranges.contains("{\"startLine\":14,\"endLine\":16}"), "{ranges}"); // law "roundtrip"
        assert!(!ranges.contains("\"startLine\":13"), "a single-line sig has nothing to fold: {ranges}");

        // exactly those five ranges, nothing else
        assert_eq!(ranges.matches("\"startLine\"").count(), 5, "{ranges}");

        // single-line top-level items produce no folding range at all
        let one_liners = "type Shape = Circle(r: Float) | Square(s: Float)\n";
        assert_eq!(folding_ranges(one_liners), Some("[]".to_string()));

        // unparseable source: nothing safe to fold
        assert_eq!(folding_ranges("fun add(a: Int, b: Int -> Int {\n    a + b\n}\n"), None);

        // A REAL coverage gap found+closed (production-hardening PR-it653):
        // `foldable_spans`' match is exhaustive over ALL 5 `Item` variants
        // (`Fun`/`Type`/`Component`/`Contract`/`Law`), so a top-level `fun` and
        // a multi-line `type` genuinely DO fold in the implementation -- but
        // this test's own name claims "every multiline construct" while only
        // ever exercising `Component`/`Contract`/`Law`, never `Fun` or `Type`.
        let top_level = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
                          type Row = {\n    name: Str,\n    age: Int\n}\n";
        let top_ranges = folding_ranges(top_level).expect("parses cleanly");
        assert!(top_ranges.contains("{\"startLine\":0,\"endLine\":2}"), "fun add: {top_ranges}");
        assert!(top_ranges.contains("{\"startLine\":3,\"endLine\":6}"), "multi-line type Row: {top_ranges}");
        assert_eq!(top_ranges.matches("\"startLine\"").count(), 2, "{top_ranges}");
    }

    #[test]
    fn document_highlight_finds_every_occurrence_in_current_file_only() {
        // A real LSP capability gap (bug-hunt batch 143, PR-it531): no
        // `textDocument/documentHighlight` support at all -- editors couldn't
        // highlight "every use of the symbol under my cursor" as the user
        // moves around a file, a standard feature every mainstream LSP server
        // provides. Reuses the already-tested `occurrences`/`ident_under`
        // exactly as `references` does, but deliberately stays SINGLE-FILE
        // (not `occurrences_cross_file`) since that is the LSP spec's own
        // definition of this request -- unlike `references`/`rename`
        // (it518), reaching into `use`-imported files here would be scope
        // creep, not a fix.
        let src = "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun main() uses io {\n    print(add(1, 2))\n    print(add(3, 4))\n}\n";
        let decl_line = src.lines().position(|l| l.contains("fun add")).unwrap();
        let decl_ch = src.lines().nth(decl_line).unwrap().find("add").unwrap() + 1;
        let highlights = resolve_document_highlight(src, decl_line, decl_ch).expect("cursor is on an identifier");
        // declaration + both call sites = 3 occurrences, none carrying a "uri"
        // field (documentHighlight ranges are implicitly this document)
        assert_eq!(highlights.matches("\"range\":").count(), 3, "{highlights}");
        assert!(!highlights.contains("\"uri\""), "documentHighlight must not carry cross-file uris: {highlights}");

        // cursor on the `fun` KEYWORD: `ident_under` extracts it as a word (it's
        // character-class-based, not token-aware), but `occurrences` searches the
        // LEXED token stream for `Tok::Ident` -- "fun" always lexes as `Tok::KwFun`,
        // never an identifier, so this correctly returns zero highlights, not a crash.
        assert_eq!(resolve_document_highlight(src, 0, 0), Some("[]".to_string()));
    }

    /// A REAL bug (production-hardening PR-it743), closing out the `locally_bound`/
    /// local-vs-top-level-collision family (PR-it704/it739/it741/it742): documentHighlight
    /// was the one remaining call site still using plain, unscoped `occurrences`.
    /// Highlighting a LOCAL variable used to also highlight an unrelated TOP-LEVEL
    /// declaration sharing its bare name (and vice versa) elsewhere in the SAME file.
    /// Cosmetic severity (a visual highlight, not a destructive rename), but the same
    /// root cause -- now fixed via the shared `scoped_occurrences` helper.
    #[test]
    fn document_highlight_does_not_merge_a_local_with_an_unrelated_top_level_declaration() {
        let src = "fun mean(xs: List[Int]) -> Float {\n    xs.sum().to_float() / xs.len().to_float()\n}\nfun helper() {\n    print(mean([1, 2, 3]))\n}\nfun main() {\n    let mean = 5.0\n    print(mean)\n}\n";

        // Highlighting the LOCAL `mean` must show only its own decl + use (2), not the
        // unrelated `fun mean`'s decl + call site.
        let off = src.find("mean = 5.0").unwrap();
        let line = src[..off].matches('\n').count();
        let line_start = src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let ch = off - line_start;
        let local_highlights =
            resolve_document_highlight(src, line, ch).expect("cursor is on an identifier");
        assert_eq!(local_highlights.matches("\"range\":").count(), 2, "{local_highlights}");

        // Highlighting the TOP-LEVEL `fun mean` must show only its own decl + call site
        // (2), not the unrelated local's decl + use in main().
        let fun_off = src.find("fun mean").unwrap() + 4;
        let fun_line = src[..fun_off].matches('\n').count();
        let fun_line_start = src[..fun_off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let fun_ch = fun_off - fun_line_start;
        let fun_highlights =
            resolve_document_highlight(src, fun_line, fun_ch).expect("cursor is on an identifier");
        assert_eq!(fun_highlights.matches("\"range\":").count(), 2, "{fun_highlights}");
    }

    #[test]
    fn workspace_symbol_searches_every_file_under_the_root() {
        // A real LSP capability gap (bug-hunt batch 144, PR-it532): no
        // `workspace/symbol` support -- editors' "Go to Symbol in Workspace"
        // (searching by name across the WHOLE project, not just the open
        // file) had nothing to query. This is the natural whole-project
        // analog to it530's per-document `textDocument/documentSymbol`, but a
        // genuinely different response SHAPE (flat `SymbolInformation` with
        // its own `location.uri` per entry, since matches span many files) --
        // not just documentSymbol run in a loop.
        let dir = std::env::temp_dir().join(format!("kupl-lsp-ws-test-{}", std::process::id()));
        let nested = dir.join("lib");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("main.kupl"), "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n").unwrap();
        std::fs::write(nested.join("util.kupl"), "fun addTwo(a: Int) -> Int {\n    a + 2\n}\n").unwrap();
        // a broken file must be silently skipped, not abort the whole search
        std::fs::write(dir.join("broken.kupl"), "fun bad(a: Int -> Int {\n    a\n}\n").unwrap();
        // a component's own PROP must also be a searchable workspace symbol (production-
        // hardening PR-it872, the SAME gap class as document-symbol nesting, just never
        // itself mirrored for `ComponentDecl.props` in the workspace-wide search path
        // either): searching for `addr` used to find nothing at all here.
        // a component's own CHILD must also be a searchable workspace symbol
        // (production-hardening PR-it873, the SAME gap class again, never itself
        // mirrored for `ComponentDecl.children` in the workspace-wide search path
        // either): searching for `addOn` used to find nothing at all here.
        std::fs::write(
            dir.join("comp.kupl"),
            "component Base {\n    intent \"b2\"\n    expose fun ping() -> Int {\n        0\n    }\n}\ncomponent Box {\n    intent \"b\"\n    prop addr: Str\n    let addOn = Base()\n    expose fun show() -> Str {\n        addr\n    }\n}\n",
        )
        .unwrap();

        let matches = workspace_symbols(&dir, "add", &HashMap::new());
        assert!(matches.contains("\"name\":\"add\""), "{matches}");
        assert!(matches.contains("\"name\":\"addTwo\""), "{matches}"); // found in the NESTED file
        assert!(matches.contains("\"name\":\"addr\""), "{matches}"); // the component's own prop
        assert!(matches.contains("\"name\":\"addOn\""), "{matches}"); // the component's own child
        assert!(matches.contains("main.kupl"), "{matches}");
        assert!(matches.contains("lib/util.kupl") || matches.contains("lib%2Futil.kupl"), "{matches}");
        // case-insensitive substring match, not exact-name
        assert_eq!(workspace_symbols(&dir, "ADD", &HashMap::new()), matches, "query matching must be case-insensitive");

        // a query matching nothing returns an empty (not null/error) result
        assert_eq!(workspace_symbols(&dir, "zzz_nonexistent", &HashMap::new()), "[]");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it732): `collect_kupl_files`
    /// used `path.is_dir()`, which FOLLOWS symlinks -- so a symlinked directory
    /// was scanned exactly like an ordinary one, letting the same content be
    /// silently re-visited (and reported twice, under two different paths) via
    /// whatever symlink happened to reach it. `sibling/marked.kupl` lives OUTSIDE
    /// `dir` (the workspace root) and is reachable ONLY through `dir/link`, a
    /// symlink to `sibling` -- with the fix, `link` reports `is_dir() == false`
    /// (via `entry.file_type()`, which does NOT follow symlinks) and is simply
    /// never recursed into, so `marked` is correctly NOT found. (An initial
    /// draft of this fix was suspected to also close an uncatchable stack-overflow
    /// crash via a self-referencing symlink cycle -- a live revert-and-verify
    /// DISPROVED that: `collect_kupl_files` builds its `root` argument by
    /// repeated path-string concatenation, so a cyclic symlink hits the OS's
    /// path-length limit `ENAMETOOLONG` -- already handled cleanly -- after a
    /// few hundred/thousand recursions, far below what's needed to overflow a
    /// thread stack. See the long comment on `collect_kupl_files` itself.)
    #[test]
    fn workspace_symbol_search_does_not_follow_a_symlinked_directory() {
        let dir = std::env::temp_dir().join(format!("kupl-lsp-symlink-test-{}", std::process::id()));
        let sibling = std::env::temp_dir().join(format!("kupl-lsp-symlink-sibling-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(dir.join("real.kupl"), "fun real(a: Int) -> Int {\n    a\n}\n").unwrap();
        std::fs::write(sibling.join("hidden.kupl"), "fun marked(a: Int) -> Int {\n    a\n}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sibling, dir.join("link")).unwrap();
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_dir(&sibling, dir.join("link"));

        let matches = workspace_symbols(&dir, "real", &HashMap::new());
        assert!(matches.contains("\"name\":\"real\""), "{matches}");
        // the symlinked directory is never entered, so its content is invisible
        assert_eq!(workspace_symbols(&dir, "marked", &HashMap::new()), "[]");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    /// A directory symlinked back to itself (or an ancestor) must not hang or
    /// crash the scan -- see the long comment on `collect_kupl_files` for why
    /// this was never actually an unbounded-recursion crash risk in the first
    /// place (the function's own path-growing recursion structure is
    /// self-bounding via the OS's path-length limit), but it's still worth a
    /// permanent regression test confirming the pathological case terminates
    /// cleanly and doesn't affect finding real content elsewhere in the tree.
    #[test]
    fn workspace_symbol_search_survives_a_self_referencing_symlink_cycle() {
        let dir = std::env::temp_dir().join(format!("kupl-lsp-symlink-cycle-test-{}", std::process::id()));
        let cyclic = dir.join("cyclic");
        std::fs::create_dir_all(&cyclic).unwrap();
        std::fs::write(dir.join("real.kupl"), "fun real(a: Int) -> Int {\n    a\n}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cyclic, cyclic.join("loop")).unwrap();
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_dir(&cyclic, cyclic.join("loop"));

        let matches = workspace_symbols(&dir, "real", &HashMap::new());
        assert!(matches.contains("\"name\":\"real\""), "{matches}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
