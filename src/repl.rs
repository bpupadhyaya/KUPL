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
    let mut defs = String::new();
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
                    if defs.is_empty() {
                        println!("(no definitions yet)");
                    } else {
                        print!("{defs}");
                    }
                    continue;
                }
                "" => continue,
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
            // Try committing the new definition against everything defined so far.
            let candidate = format!("{defs}{input}\n");
            match run::compile(&candidate) {
                Ok(compiled) => {
                    run::print_diags(&compiled.warnings, &candidate, "<repl>");
                    defs = candidate;
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

fn braces_balanced(src: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    let mut prev = '\0';
    for ch in src.chars() {
        if in_str {
            if ch == '"' && prev != '\\' {
                in_str = false;
            }
        } else {
            match ch {
                '"' => in_str = true,
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        prev = ch;
    }
    depth <= 0
}
