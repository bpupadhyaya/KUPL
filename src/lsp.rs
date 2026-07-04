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
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"capabilities\":{{\"textDocumentSync\":1}},\"serverInfo\":{{\"name\":\"kupl-lsp\",\"version\":\"{}\"}}}}}}",
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
