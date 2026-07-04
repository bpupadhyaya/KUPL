//! Diagnostics: spans, errors, and human/machine-readable rendering.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl Diag {
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diag { severity: Severity::Error, code, message: message.into(), span }
    }
    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diag { severity: Severity::Warning, code, message: message.into(), span }
    }
}

/// Resolve a byte offset to 1-based (line, column).
pub fn line_col(src: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(src.len());
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

pub fn render(diag: &Diag, src: &str, file: &str) -> String {
    let (line, col) = line_col(src, diag.span.start);
    let sev = match diag.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let src_line = src.lines().nth(line - 1).unwrap_or("");
    let caret_len = ((diag.span.end - diag.span.start) as usize).max(1).min(src_line.len().saturating_sub(col - 1).max(1));
    let mut out = String::new();
    out.push_str(&format!(
        "{sev}[{code}]: {msg}\n  --> {file}:{line}:{col}\n",
        code = diag.code,
        msg = diag.message,
    ));
    out.push_str(&format!("   |\n{line:3}| {src_line}\n   | "));
    out.push_str(&" ".repeat(col - 1));
    out.push_str(&"^".repeat(caret_len));
    out.push('\n');
    out
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

/// Machine-readable diagnostics (`--json`): one JSON object per line would be
/// hostile to small consumers, so we emit a single document.
pub fn to_json(diags: &[Diag], src: &str, file: &str) -> String {
    let mut out = String::from("{\"diagnostics\":[");
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (line, col) = line_col(src, d.span.start);
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        out.push_str(&format!(
            "{{\"severity\":\"{sev}\",\"code\":\"{}\",\"message\":\"{}\",\"file\":\"{}\",\"span\":{{\"start\":{},\"end\":{},\"line\":{line},\"col\":{col}}}}}",
            d.code,
            json_escape(&d.message),
            json_escape(file),
            d.span.start,
            d.span.end,
        ));
    }
    out.push_str("]}");
    out
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
