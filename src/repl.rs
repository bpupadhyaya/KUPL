//! The KUPL REPL: define functions/types/components live, evaluate expressions.

use std::io::{self, BufRead, Write};

use crate::interp::{Flow, Interp, ProgramDb};
use crate::parser;
use crate::run;
use crate::value::Value;

const BANNER: &str = "KUPL v0.1 — K Universal Programming Language
Type declarations (fun/type/component/app), statements, or expressions.
Commands: :help  :defs  :quit";

pub fn repl() -> i32 {
    println!("{BANNER}");
    let stdin = io::stdin();
    // Each entry is (the (kind, name) pairs this input declares, its source
    // text). Kept as separate units rather than one flat string so a
    // re-typed `fun`/`type`/`component`/`contract` can REPLACE its prior
    // declaration instead of appending a same-named duplicate (production-
    // hardening PR-it703): before this, only components could be
    // "redefined" in the REPL, and only because `check.rs` had no
    // duplicate-component-name check at all (a real bug, now fixed with
    // K0278) -- redefining a `fun`/`type`/`contract` already correctly
    // errored (K0203/K0201/K0260) on the accidental last-write-wins
    // concatenation this REPL used to do. Replacing by name makes
    // redefinition an intentional, consistent operation for every item
    // kind, rather than a side effect of one item kind's checker gap.
    let mut defs_items: Vec<(Vec<(&'static str, String)>, String)> = Vec::new();
    let mut interp = Interp::new(ProgramDb::build(&Default::default(), &Default::default()));

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "kupl> " } else { "  ..> " };
        print!("{prompt}");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                return 0; // EOF
            }
            Ok(_) => {}
            Err(_) => return 1,
        }

        if buffer.is_empty() {
            let cmd = line.trim();
            match cmd {
                ":quit" | ":q" | ":exit" => return 0,
                ":help" | ":h" => {
                    println!("{BANNER}");
                    continue;
                }
                ":defs" => {
                    if defs_items.is_empty() {
                        println!("(no definitions yet)");
                    } else {
                        for (_, text) in &defs_items {
                            print!("{text}");
                        }
                    }
                    continue;
                }
                "" => continue,
                // A `:`-prefixed line is a REPL command, not KUPL source — an
                // unknown one gets a helpful message instead of a cryptic
                // "expected an expression, found `:`" parse error.
                other if other.starts_with(':') => {
                    println!("unknown command `{other}` — type :help for the list");
                    continue;
                }
                _ => {}
            }
        }

        buffer.push_str(&line);
        if !braces_balanced(&buffer) {
            continue; // keep reading a multi-line form
        }
        let input = std::mem::take(&mut buffer);
        let trimmed = input.trim();

        if is_item(trimmed) {
            // This input's own top-level (kind, name) pairs, parsed in
            // isolation -- purely syntactic, so it doesn't need the rest of
            // `defs_items` to resolve. Any prior entry sharing a key gets
            // dropped before re-concatenating, so a re-typed declaration
            // REPLACES rather than duplicates it. If parsing `input` alone
            // fails, fall back to appending unchanged; `run::compile` below
            // still reports the real error either way.
            let new_keys: Vec<(&'static str, String)> = parser::parse(&input)
                .0
                .items
                .iter()
                .filter(|it| !matches!(it, crate::ast::Item::Law(_)))
                .map(|it| (crate::sdiff::kind_tag(it), crate::sdiff::item_name(it).to_string()))
                .collect();
            let entry_text = format!("{input}\n");
            let mut candidate = String::new();
            for (keys, text) in &defs_items {
                if !keys.iter().any(|k| new_keys.contains(k)) {
                    candidate.push_str(text);
                }
            }
            candidate.push_str(&entry_text);
            // Try committing the new definition against everything defined so far.
            match run::compile(&candidate) {
                Ok(compiled) => {
                    run::print_diags(&compiled.warnings, &candidate, "<repl>");
                    defs_items.retain(|(keys, _)| !keys.iter().any(|k| new_keys.contains(k)));
                    defs_items.push((new_keys, entry_text));
                    let db = ProgramDb::build(&compiled.program, &compiled.checked);
                    // Keep live values/instances; swap in the new definitions.
                    let old = std::mem::replace(&mut interp, Interp::new(db));
                    interp.instances = old.instances;
                    interp.globals = old.globals;
                    println!("defined.");
                }
                Err(errors) => {
                    run::print_diags(&errors, &candidate, "<repl>");
                }
            }
            continue;
        }

        // Statement / expression: evaluated dynamically against the live session.
        match parser::parse_stmt_fragment(trimmed) {
            Err(d) => {
                eprintln!("error[{}]: {}", d.code, d.message);
            }
            Ok(stmt) => {
                let env = interp.globals.clone();
                match interp.exec_stmt_public(&stmt, &env) {
                    Ok(Value::Unit) => {}
                    Ok(v) => println!("{v}"),
                    Err(Flow::Panic { msg, .. }) => eprintln!("panic: {msg}"),
                    Err(Flow::Return(v)) => println!("{v}"),
                    Err(_) => eprintln!("error: `break`/`continue` outside of a loop"),
                }
            }
        }
    }
}

fn is_item(src: &str) -> bool {
    let first = src.split_whitespace().next().unwrap_or("");
    matches!(first, "fun" | "type" | "component" | "app" | "pub" | "async" | "contract" | "use" | "module")
}

/// A REAL bug found+fixed (production-hardening PR-it768): this used to be
/// completely unaware of `//` line comments and `/* */` block comments --
/// unlike the real lexer (`lexer.rs:90-123`), which supports both (block
/// comments even NESTABLE, mirrored below). Any bracket-class character
/// typed inside what the user intends as a comment (e.g. a `:(` sad-face
/// emoticon in `// ugh this crashed :(`) was counted as genuine unclosed
/// syntax, permanently WEDGING the REPL: `buffer` never balances again, so
/// every subsequent line -- including a bare `:quit` -- gets silently
/// APPENDED to the same dead buffer instead of executing/being recognized
/// as a command (the `:`-command dispatch above only fires when `buffer.
/// is_empty()`), and on EOF the entire unsubmitted buffer is silently
/// discarded with zero diagnostic that anything was lost. Live-confirmed
/// BEFORE this fix via a piped `kupl repl` session: `// ugh this crashed
/// :(` followed by `print("hi")` followed by `:quit` never printed `hi`,
/// never processed `:quit`, and exited cleanly on EOF as if nothing went
/// wrong.
fn braces_balanced(src: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    // Tracked ACROSS the whole scan (not just within one `/* .. */` span) so
    // a block comment left open at the end of the buffer -- e.g. the user
    // just typed `/* start of a` on its own line, intending to close it on a
    // LATER line -- correctly signals "keep reading" (a `..>` continuation),
    // matching how an open `{`/`(`/`[` already does, rather than prematurely
    // submitting a truncated comment as if it were a complete top-level form.
    let mut comment_depth: u32 = 0;
    let mut prev = '\0';
    let mut chars = src.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_str {
            if ch == '"' && prev != '\\' {
                in_str = false;
            }
            prev = ch;
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            // line comment: skip to end of line (or end of input).
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            prev = '\0';
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            // block comment: NESTABLE, matching `lexer.rs`'s own algorithm
            // exactly (a `/*` inside an already-open block comment opens
            // ANOTHER level, requiring a matching extra `*/` to close).
            chars.next(); // consume the '*'
            comment_depth += 1;
            while comment_depth > 0 {
                match chars.next() {
                    None => break, // buffer ends mid-comment -- reported unbalanced below
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        comment_depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        comment_depth -= 1;
                    }
                    _ => {}
                }
            }
            prev = '\0';
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
        prev = ch;
    }
    depth <= 0 && comment_depth == 0
}

#[cfg(test)]
mod tests {
    use super::{braces_balanced, is_item};

    #[test]
    fn braces_balanced_drives_multiline_reads() {
        // balanced forms are ready to evaluate
        assert!(braces_balanced("fun f() -> Int { 1 }"));
        assert!(braces_balanced("2 + 3"));
        assert!(braces_balanced("[1, 2, 3].sum()"));
        // an unclosed brace/paren keeps the REPL reading (a `..>` continuation)
        assert!(!braces_balanced("fun f() -> Int {"));
        assert!(!braces_balanced("foo("));
        // braces INSIDE a string literal (incl. `{x}` interpolation) don't count —
        // otherwise the REPL would hang waiting for a matching `}` that is text.
        assert!(braces_balanced("print(\"a { b\")"));
        assert!(braces_balanced("print(\"val {x}\")"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it768): `braces_balanced`
    /// used to be completely unaware of `//` line comments and `/* */` block
    /// comments -- any bracket-class character typed inside a comment was
    /// counted as genuine unclosed syntax, permanently wedging the REPL. Live-
    /// confirmed BEFORE this fix via a real piped `kupl repl` session (see the
    /// subprocess test below for the full end-to-end repro).
    #[test]
    fn braces_balanced_ignores_brackets_inside_comments() {
        // a line comment containing bracket-class characters must not be
        // mistaken for unclosed syntax.
        assert!(braces_balanced("// look at this { unmatched"));
        assert!(braces_balanced("// ugh this crashed :("));
        assert!(braces_balanced("print(1) // trailing { comment"));
        // a block comment, including one spanning what LOOKS like a
        // multi-line unclosed form, is still recognized as fully consumed.
        assert!(braces_balanced("/* { ( [ all unmatched */"));
        assert!(braces_balanced("fun f() -> Int { /* comment { */ 1 }"));
        // NESTED block comments, mirroring `lexer.rs`'s own nestable algorithm.
        assert!(braces_balanced("/* outer /* inner { */ still outer */"));
        // a genuinely UNCLOSED block comment (no closing `*/` at all) must
        // still correctly signal "keep reading" -- otherwise a multi-line
        // comment split across several `read_line` calls (e.g. `/* start`
        // on one line, `continues */` on the next) would be prematurely
        // submitted after just the FIRST line, treating the comment's own
        // closing line as an unrelated new top-level statement instead.
        assert!(!braces_balanced("/* never closed { ["));
        // a REAL unclosed brace OUTSIDE any comment must still correctly
        // signal "keep reading" -- this fix must not over-correct into
        // treating everything after a `/` as inert.
        assert!(!braces_balanced("fun f() -> Int { // trailing comment on an open line"));
        assert!(!braces_balanced("foo(1, 2"));
    }

    #[test]
    fn is_item_classifies_declarations_vs_expressions() {
        assert!(is_item("fun f() -> Int { 1 }"));
        assert!(is_item("type P = Pt(x: Int)"));
        assert!(is_item("pub fun g() {}"));
        assert!(is_item("component C {}"));
        // statements and expressions are not items (they run against current state)
        assert!(!is_item("let x = 1"));
        assert!(!is_item("2 + 3"));
        assert!(!is_item("x.to_upper()"));
    }

    /// End-to-end companion to `braces_balanced_ignores_brackets_inside_comments`
    /// above: spawns the REAL `kupl repl` process (this codebase's established
    /// subprocess-test pattern, e.g. `main.rs::wait_with_timeout`) to confirm the
    /// full wedge is fixed, not just the underlying pure function. Live-confirmed
    /// BEFORE this fix: a `// ugh this crashed :(` comment permanently wedged the
    /// session -- `print("hi")` never ran and `:quit` never processed as a
    /// command, with the process only exiting via silent EOF.
    #[test]
    fn a_bracket_character_inside_a_repl_comment_does_not_wedge_the_session() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "// ugh this crashed :(\nprint(\"hi\")\n:quit\nprint(\"should not run\")\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung on a bracket character inside a comment").expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("hi"),
            "print(\"hi\") must actually run -- the comment must not wedge the REPL: {stdout}"
        );
        assert!(
            !stdout.contains("should not run"),
            ":quit must genuinely terminate the session, not get silently appended to a dead buffer: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }
}
