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
    // Caret length must be measured in CHARACTERS, matching `col` (which
    // `line_col` computes via `char_indices`) -- `diag.span.end -
    // diag.span.start` is a BYTE length, which overshoots the character
    // count for any span covering multi-byte UTF-8 text, extending the
    // caret underline past the actual erroring source text (confirmed live:
    // a span over "日本語" -- 3 characters, 9 bytes -- produced 6 EXTRA
    // carets). `.get(..)` (not direct slicing) so a span that isn't on a
    // char boundary, or a reversed `end < start` span, degrades to 0 chars
    // instead of panicking (production-hardening PR-it655).
    let span_start = (diag.span.start as usize).min(src.len());
    let span_end = (diag.span.end as usize).min(src.len()).max(span_start);
    let span_chars = src.get(span_start..span_end).map_or(0, |s| s.chars().count());
    let line_chars_after_col = src_line.chars().count().saturating_sub(col - 1);
    let caret_len = span_chars.max(1).min(line_chars_after_col.max(1));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn carets(rendered: &str) -> usize {
        rendered.lines().find(|l| l.contains('^')).map_or(0, |l| l.matches('^').count())
    }

    #[test]
    fn caret_length_matches_ascii_span_exactly() {
        let src = "let x: Int = \"hi\"\n";
        // span covers `"hi"` (4 bytes == 4 chars for pure ASCII)
        let start = src.find('"').unwrap() as u32;
        let end = start + "\"hi\"".len() as u32;
        let d = Diag::error("K0000", "test", Span::new(start, end));
        let out = render(&d, src, "f");
        assert_eq!(carets(&out), 4, "{out}");
    }

    /// A REAL BUG found+fixed (production-hardening PR-it655): `diag.rs` had
    /// ZERO test coverage of any kind before this iteration, despite being
    /// the rendering path EVERY diagnostic in the compiler flows through.
    /// `caret_len` used `diag.span.end - diag.span.start` (a BYTE length)
    /// directly as the number of `^` characters to print, but `col` (and
    /// the visual position a caret needs to align with) is a CHARACTER
    /// column -- for a span covering multi-byte UTF-8 text, the byte length
    /// exceeds the character length, so the caret UNDERLINE overshot past
    /// the actual erroring source text. Confirmed live before fixing: a
    /// span over "日本語" (3 characters, 9 bytes) produced 6 EXTRA carets.
    #[test]
    fn caret_length_matches_character_count_not_byte_count_for_utf8() {
        let src = "let x: Int = \"日本語\" + \"y\"\n";
        // span covers the string literal `"日本語"` -- 5 characters (the two
        // quotes + 3 CJK characters), 11 bytes (quotes are 1 byte each, each
        // CJK character is 3 bytes: 2 + 3*3 = 11).
        let start = src.find('"').unwrap() as u32;
        let end = start + "\"日本語\"".len() as u32;
        let d = Diag::error("K0000", "test", Span::new(start, end));
        let out = render(&d, src, "f");
        assert_eq!(carets(&out), 5, "byte-length would wrongly give 11: {out}");
    }

    #[test]
    fn caret_length_is_clamped_to_the_remaining_line_not_beyond_it() {
        // a span that (incorrectly) extends past the end of its own line
        // must not print more carets than characters actually remain.
        let src = "let x = 1\nlet y = 2\n";
        let d = Diag::error("K0000", "test", Span::new(4, 9999));
        let out = render(&d, src, "f");
        // "x = 1" is 5 characters; the caret must not run past the line.
        assert_eq!(carets(&out), 5, "{out}");
    }

    #[test]
    fn a_reversed_span_does_not_panic_and_still_renders() {
        // `end < start` should never happen in practice (spans are built via
        // `Span::merge`'s min/max), but `Span::new` performs no validation --
        // rendering must degrade gracefully (a minimum 1-character caret),
        // never panic on the underflowing subtraction the old code did.
        let src = "let x = 1\n";
        let d = Diag::error("K0000", "test", Span::new(8, 2));
        let out = render(&d, src, "f");
        assert_eq!(carets(&out), 1, "{out}");
    }

    #[test]
    fn to_json_escapes_and_reports_every_diag_field() {
        let src = "let x = 1\n";
        let d = Diag::warning("K0100", "a \"quoted\" message", Span::new(4, 5));
        let out = to_json(&[d], src, "f.kupl");
        assert!(out.contains("\"severity\":\"warning\""), "{out}");
        assert!(out.contains("\"code\":\"K0100\""), "{out}");
        assert!(out.contains("a \\\"quoted\\\" message"), "{out}");
        assert!(out.contains("\"file\":\"f.kupl\""), "{out}");
        assert!(out.contains("\"start\":4") && out.contains("\"end\":5"), "{out}");
    }
}
