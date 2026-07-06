//! `kupl lsp` — a minimal Language Server Protocol server over stdio.
//!
//! Zero dependencies: Content-Length framing and a small JSON parser live
//! here. v0 capabilities: full-text document sync + push diagnostics on
//! open/change/save (multi-file aware — unsaved buffer contents override
//! what's on disk, `use`-dependencies come from disk).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::diag::{json_escape, line_col, Severity};

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
    let v = parse_value(bytes, &mut pos)?;
    Ok(v)
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_value(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    skip_ws(b, pos);
    if *pos >= b.len() {
        return Err("unexpected end of JSON".into());
    }
    match b[*pos] {
        b'{' => {
            *pos += 1;
            let mut pairs = Vec::new();
            skip_ws(b, pos);
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
                return Ok(Json::Obj(pairs));
            }
            loop {
                skip_ws(b, pos);
                let key = match parse_value(b, pos)? {
                    Json::Str(s) => s,
                    _ => return Err("object key must be a string".into()),
                };
                skip_ws(b, pos);
                if *pos >= b.len() || b[*pos] != b':' {
                    return Err("expected ':'".into());
                }
                *pos += 1;
                let val = parse_value(b, pos)?;
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
            *pos += 1;
            let mut items = Vec::new();
            skip_ws(b, pos);
            if *pos < b.len() && b[*pos] == b']' {
                *pos += 1;
                return Ok(Json::Arr(items));
            }
            loop {
                items.push(parse_value(b, pos)?);
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
                                let hex = std::str::from_utf8(&b[*pos + 1..*pos + 5])
                                    .map_err(|_| "bad \\u escape")?;
                                let cp =
                                    u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape")?;
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                *pos += 4;
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
            *pos += 4;
            Ok(Json::Bool(true))
        }
        b'f' => {
            *pos += 5;
            Ok(Json::Bool(false))
        }
        b'n' => {
            *pos += 4;
            Ok(Json::Null)
        }
        _ => {
            let start = *pos;
            while *pos < b.len()
                && matches!(b[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                *pos += 1;
            }
            std::str::from_utf8(&b[start..*pos])
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .map(Json::Num)
                .ok_or_else(|| "invalid number".into())
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
        let (l1, c1) = line_col(&file.src, local_start);
        let (l2, c2) = line_col(&file.src, local_end);
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

/// Byte offset of an LSP (line, character) position — both 0-based. Character
/// columns are treated as byte columns (correct for ASCII; a documented
/// approximation for multi-byte lines).
fn offset_at(text: &str, line: usize, character: usize) -> usize {
    let mut off = 0usize;
    for (n, l) in text.split_inclusive('\n').enumerate() {
        if n == line {
            return off + character.min(l.trim_end_matches('\n').len());
        }
        off += l.len();
    }
    off.min(text.len())
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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
fn item_signature(program: &crate::ast::Program, name: &str) -> Option<String> {
    use crate::ast::Item;
    use crate::fmt::ty_str;
    for item in &program.items {
        match item {
            Item::Fun(f) if f.name == name => {
                let params: Vec<String> =
                    f.params.iter().map(|p| format!("{}: {}", p.name, ty_str(&p.ty))).collect();
                let ret = f.ret.as_ref().map(|r| format!(" -> {}", ty_str(r))).unwrap_or_default();
                let eff = if f.effects.is_empty() {
                    String::new()
                } else {
                    format!(" uses {}", f.effects.join(", "))
                };
                let kw = if f.ai.is_some() { "ai fun" } else { "fun" };
                return Some(format!("{kw} {}({}){ret}{eff}", f.name, params.join(", ")));
            }
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
    }
    None
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
        _ => None,
    })?;
    // locate the name token within the declaration for a precise range
    let decl_start = span.start as usize;
    let name_off = text.get(decl_start..).and_then(|s| s.find(name)).map(|i| decl_start + i)?;
    let (l0, c0) = crate::diag::line_col(text, name_off as u32);
    let (l1, c1) = crate::diag::line_col(text, (name_off + name.len()) as u32);
    Some((l0 - 1, c0 - 1, l1 - 1, c1 - 1))
}

/// Hover markdown for the symbol at an LSP position, or None.
pub fn resolve_hover(text: &str, line: usize, character: usize) -> Option<String> {
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    let sig = item_signature(&program, &name)?;
    Some(format!("```kupl\n{sig}\n```"))
}

/// Every occurrence of the identifier `name`, as 0-based LSP ranges. Uses the
/// LEXER, so it matches only real identifier tokens — never text inside string
/// literals or comments (an identifier inside a `{…}` interpolation IS a real
/// reference and is included). Token-based, NOT scope-aware: it finds every
/// same-named identifier in the file (a shadowing local or a same-named field
/// included) — the common simple-LSP behavior; scope-aware rename is future work.
pub fn occurrences(text: &str, name: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    collect_occurrences(text, 0, name, text, &mut out);
    // token order is source order at each level, but interpolation occurrences are
    // discovered at their enclosing string token — sort so edits/refs are ascending.
    out.sort();
    out
}

/// Scan `text` (a full document, or the raw source of a string-interpolation
/// `{expr}` at absolute byte offset `base`) for identifier uses of `name`,
/// recursing into nested interpolations. Positions are line/col in `full`.
fn collect_occurrences(
    text: &str,
    base: u32,
    name: &str,
    full: &str,
    out: &mut Vec<(usize, usize, usize, usize)>,
) {
    let (tokens, _diags) = crate::lexer::lex(text);
    for t in &tokens {
        match &t.tok {
            crate::token::Tok::Ident(s) if s == name => {
                let (l0, c0) = crate::diag::line_col(full, base + t.span.start);
                let (l1, c1) = crate::diag::line_col(full, base + t.span.end);
                out.push((l0 - 1, c0 - 1, l1 - 1, c1 - 1));
            }
            // `"…{x}…"` — the interpolated expression is captured raw inside the
            // string token, so its identifier uses (real references, updated by a
            // rename) are found by scanning the expression source at its offset.
            crate::token::Tok::Str(parts) => {
                for p in parts {
                    if let crate::token::StrPart::Expr(raw, expr_start) = p {
                        collect_occurrences(raw, *expr_start, name, full, out);
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
pub fn resolve_definition(text: &str, line: usize, character: usize) -> Option<(usize, usize, usize, usize)> {
    let off = offset_at(text, line, character);
    let (name, _, _) = ident_at(text, off)?;
    let (program, _diags) = crate::parser::parse(text);
    item_definition(text, &program, &name)
}

/// A completion candidate: (label, LSP CompletionItemKind, detail).
pub fn completions(text: &str) -> Vec<(String, u8, String)> {
    use crate::ast::Item;
    let (program, _diags) = crate::parser::parse(text);
    let mut out: Vec<(String, u8, String)> = Vec::new();
    for item in &program.items {
        match item {
            Item::Fun(f) => {
                let sig = item_signature(&program, &f.name).unwrap_or_default();
                out.push((f.name.clone(), 3, sig)); // 3 = Function
            }
            Item::Type(t) => {
                out.push((t.name.clone(), 22, format!("type {}", t.name))); // 22 = Struct
                for v in &t.variants {
                    let sig = item_signature(&program, &v.name).unwrap_or_default();
                    out.push((v.name.clone(), 4, sig)); // 4 = Constructor
                }
            }
            Item::Component(c) => out.push((c.name.clone(), 7, format!("component {}", c.name))), // 7 = Class
            Item::Contract(c) => out.push((c.name.clone(), 8, format!("contract {}", c.name))), // 8 = Interface
            _ => {}
        }
    }
    // language keywords (14 = Keyword)
    for kw in [
        "fun", "type", "component", "app", "contract", "match", "if", "else", "for", "while",
        "let", "var", "return", "true", "false", "uses", "expose", "state", "on", "emit", "wire",
    ] {
        out.push((kw.to_string(), 14, String::new()));
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

/// Current text of a document: the unsaved editor buffer if present, else disk.
fn doc_text(uri: &str, buffers: &HashMap<PathBuf, String>) -> Option<String> {
    let path = uri_to_path(uri)?;
    if let Some(buf) = buffers.get(&path) {
        return Some(buf.clone());
    }
    std::fs::read_to_string(&path).ok()
}

pub fn serve() -> i32 {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    // open editor buffers (unsaved contents)
    let mut buffers: HashMap<PathBuf, String> = HashMap::new();

    while let Some(body) = read_message(&mut stdin) {
        let Ok(msg) = parse_json(&body) else { continue };
        let method = msg.get("method").and_then(Json::str).unwrap_or("");
        let id = msg.get("id");

        match method {
            "initialize" => {
                let id = id.map(render_id).unwrap_or_else(|| "null".into());
                send(
                    &mut stdout,
                    &format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"capabilities\":{{\"textDocumentSync\":1,\"hoverProvider\":true,\"definitionProvider\":true,\"referencesProvider\":true,\"renameProvider\":true,\"completionProvider\":{{\"triggerCharacters\":[\".\"]}}}},\"serverInfo\":{{\"name\":\"kupl-lsp\",\"version\":\"{}\"}}}}}}",
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
                let text = params
                    .and_then(|p| p.get("contentChanges"))
                    .and_then(|c| c.index(0))
                    .and_then(|c| c.get("text"))
                    .and_then(Json::str);
                if let (Some(uri), Some(text)) = (uri, text) {
                    if let Some(path) = uri_to_path(uri) {
                        buffers.insert(path.clone(), text.to_string());
                        let note = diagnostics_notification(&path, uri, &buffers);
                        send(&mut stdout, &note);
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
                    let md = resolve_hover(&text, line, ch)?;
                    Some(format!(
                        "{{\"contents\":{{\"kind\":\"markdown\",\"value\":\"{}\"}}}}",
                        json_escape(&md)
                    ))
                })()
                .unwrap_or_else(|| "null".into());
                send(&mut stdout, &format!("{{\"jsonrpc\":\"2.0\",\"id\":{rid},\"result\":{result}}}"));
            }
            "textDocument/definition" => {
                let rid = id.map(render_id).unwrap_or_else(|| "null".into());
                let result = (|| {
                    let p = msg.get("params")?;
                    let (uri, line, ch) = position_of(p)?;
                    let text = doc_text(uri, &buffers)?;
                    let (l0, c0, l1, c1) = resolve_definition(&text, line, ch)?;
                    Some(format!(
                        "{{\"uri\":\"{}\",\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}}}}",
                        json_escape(uri)
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
                    let locs: Vec<String> = occurrences(&text, &name)
                        .into_iter()
                        .map(|(l0, c0, l1, c1)| {
                            format!(
                                "{{\"uri\":\"{}\",\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}}}}",
                                json_escape(uri)
                            )
                        })
                        .collect();
                    Some(format!("[{}]", locs.join(",")))
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
                    let text = doc_text(uri, &buffers)?;
                    let name = ident_under(&text, line, ch)?;
                    let edits: Vec<String> = occurrences(&text, &name)
                        .into_iter()
                        .map(|(l0, c0, l1, c1)| {
                            format!(
                                "{{\"range\":{{\"start\":{{\"line\":{l0},\"character\":{c0}}},\"end\":{{\"line\":{l1},\"character\":{c1}}}}},\"newText\":\"{}\"}}",
                                json_escape(new_name)
                            )
                        })
                        .collect();
                    Some(format!(
                        "{{\"changes\":{{\"{}\":[{}]}}}}",
                        json_escape(uri),
                        edits.join(",")
                    ))
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
                    Some(completions(&text))
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

    #[test]
    fn json_roundtrip() {
        let v = parse_json(r#"{"a": [1, 2.5, "x\ny", true, null], "b": {"c": -3}}"#).unwrap();
        assert_eq!(v.get("b").and_then(|b| b.get("c")), Some(&Json::Num(-3.0)));
        assert_eq!(
            v.get("a").and_then(|a| a.index(2)).and_then(Json::str),
            Some("x\ny")
        );
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
}
