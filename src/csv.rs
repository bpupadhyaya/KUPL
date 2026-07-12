//! A small, self-contained RFC 4180 CSV reader/writer, shared by the
//! interpreter and the KVM (zero dependencies, like `src/json.rs`). Pure.
//!
//! Grammar: records separated by `\n` or `\r\n`; fields separated by `,`. A
//! field may be quoted with `"…"`, in which case it can contain commas,
//! newlines, and doubled quotes (`""` → `"`). On output we quote a field iff it
//! contains `,`, `"`, `\n`, or `\r`, and emit `\n` between records. A single
//! trailing newline does not produce an extra empty record; a blank line in the
//! middle is a one-field record containing the empty string.

/// Parse CSV text into rows of string fields.
pub fn parse(input: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = input.chars().collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    // Whether the CURRENT field was opened with `"..."`, even if its content is
    // empty (PR-it678, real silent-data-loss bug: the final flush below used to
    // check only `!field.is_empty() || !row.is_empty()`, which can't tell "we
    // never saw any field content" (input ended right after a newline -- the
    // genuinely-desired skip) apart from "we saw a real, deliberately-empty
    // quoted field (`""`) that happens to be its row's ONLY field" -- BOTH leave
    // `field`/`row` empty. This silently dropped the entire last row of ANY
    // multi-row CSV whenever that row was a lone `""`, not just a
    // whole-document-is-just-`""` edge case as first characterized in it667.
    let mut field_was_quoted = false;
    let mut i = 0;
    let n = chars.len();

    while i < n {
        let c = chars[i];
        if c == '"' {
            // quoted field: consume until the closing quote (with "" escapes)
            i += 1;
            loop {
                if i >= n {
                    break;
                }
                let q = chars[i];
                if q == '"' {
                    if i + 1 < n && chars[i + 1] == '"' {
                        field.push('"');
                        i += 2;
                    } else {
                        i += 1; // closing quote
                        field_was_quoted = true;
                        break;
                    }
                } else {
                    field.push(q);
                    i += 1;
                }
            }
        } else if c == ',' {
            row.push(std::mem::take(&mut field));
            field_was_quoted = false;
            i += 1;
        } else if c == '\n' || c == '\r' {
            // end of record (handle CRLF as one terminator)
            if c == '\r' && i + 1 < n && chars[i + 1] == '\n' {
                i += 1;
            }
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
            field_was_quoted = false;
            i += 1;
        } else {
            field.push(c);
            i += 1;
        }
    }
    // flush the final field/record unless the input ended exactly on a newline
    // (in which case `field`/`row` are empty and we skip the phantom record) --
    // a CLOSED quoted field counts as real content even when empty (see above).
    if !field.is_empty() || !row.is_empty() || field_was_quoted {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Serialize rows of fields to CSV text (records joined with `\n`).
pub fn stringify(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push('\n');
        }
        for (c, field) in row.iter().enumerate() {
            if c > 0 {
                out.push(',');
            }
            write_field(field, &mut out);
        }
    }
    out
}

fn write_field(field: &str, out: &mut String) {
    let needs_quote = field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r');
    if needs_quote {
        out.push('"');
        for c in field.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(field);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Vec<Vec<String>> {
        parse(s)
    }

    #[test]
    fn simple_rows() {
        assert_eq!(p("a,b,c\n1,2,3"), vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]);
        // trailing newline: no phantom row
        assert_eq!(p("a,b\n"), vec![vec!["a", "b"]]);
        // CRLF endings
        assert_eq!(p("a,b\r\nc,d\r\n"), vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn quoted_fields() {
        assert_eq!(p("\"a,b\",c"), vec![vec!["a,b", "c"]]);
        assert_eq!(p("\"line1\nline2\",x"), vec![vec!["line1\nline2", "x"]]);
        assert_eq!(p("\"she said \"\"hi\"\"\",y"), vec![vec!["she said \"hi\"", "y"]]);
        // empty and quoted-empty fields
        assert_eq!(p("a,,c"), vec![vec!["a", "", "c"]]);
        assert_eq!(p("\"\",x"), vec![vec!["", "x"]]);
    }

    #[test]
    fn blank_line_is_one_empty_field() {
        assert_eq!(p("a\n\nb"), vec![vec!["a"], vec![""], vec!["b"]]);
    }

    /// A coverage-closing verification (production-hardening PR-it667; no
    /// bug found -- checked here AND in `cgen.rs`'s native mirror after a
    /// direct line-by-line read of both implementations found no
    /// divergence). An UNTERMINATED quoted field (a `"` opened but never
    /// closed before the input ends) had ZERO test coverage: the quote-
    /// consuming inner loop's `if i >= n { break; }` just stops at EOF with
    /// whatever was accumulated -- no panic, no error, matching RFC 4180's
    /// lack of any error-recovery grammar for a genuinely malformed CSV
    /// document (this parser has no error channel at all, so "read to EOF"
    /// is the only sane behavior).
    #[test]
    fn unterminated_quoted_field_reads_to_end_of_input() {
        assert_eq!(p("a,\"unterminated"), vec![vec!["a", "unterminated"]]);
    }

    /// A REAL silent-data-loss bug (PR-it678, resolving the DESIGN question
    /// it667 flagged but left open): a lone, PROPERLY-CLOSED quoted-empty
    /// field (`""`) with no trailing comma/newline delimiter used to be
    /// silently DROPPED entirely -- the end-of-parse flush condition
    /// (`!field.is_empty() || !row.is_empty()`) can't distinguish "no field
    /// content at all" (input ended right after a newline, the genuinely-
    /// desired skip) from "a real, deliberately-empty quoted field that's
    /// its row's only field" -- both leave `field`/`row` empty. This wasn't
    /// just a whole-document-is-`""` curiosity: it dropped the ENTIRE LAST
    /// ROW of any multi-row CSV whenever that row was a lone `""`, a
    /// realistic shape (a trailing blank-but-quoted last column value with
    /// no final newline). Fixed by tracking whether the current field was
    /// closed via `"..."` (even if empty) and treating that as real content
    /// for flush purposes, in both `csv.rs` and `cgen.rs`'s `k_csv_parse`.
    #[test]
    fn lone_closed_empty_quoted_field_is_not_silently_dropped() {
        // the exact case it667 characterized as "not a bug" -- now fixed.
        assert_eq!(p("\"\""), vec![vec![""]]);
        // the SAME bug, but as the last row of a multi-row document -- the
        // shape that makes this a real, reachable data-loss bug, not just a
        // whole-document curiosity.
        assert_eq!(p("a\n\"\""), vec![vec!["a"], vec![""]]);
        // a non-empty quoted lone field was NEVER affected (field.is_empty()
        // was already false) -- confirm it still round-trips correctly.
        assert_eq!(p("\"x\""), vec![vec!["x"]]);
    }

    #[test]
    fn stringify_quotes_when_needed() {
        assert_eq!(stringify(&[vec!["a".into(), "b".into()]]), "a,b");
        assert_eq!(
            stringify(&[vec!["a,b".into(), "c".into()]]),
            "\"a,b\",c"
        );
        assert_eq!(
            stringify(&[vec!["he said \"hi\"".into()]]),
            "\"he said \"\"hi\"\"\""
        );
        assert_eq!(stringify(&[vec!["x\ny".into()]]), "\"x\ny\"");
    }

    #[test]
    fn roundtrips() {
        for src in [
            "a,b,c\n1,2,3",
            "\"a,b\",c\nd,\"e\ne\"",
            "\"she said \"\"hi\"\"\",y",
            "single",
        ] {
            let parsed = parse(src);
            let back = stringify(&parsed);
            assert_eq!(parse(&back), parsed, "round-trip differs for {src:?}");
        }
    }
}
