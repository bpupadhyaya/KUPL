use std::process::ExitCode;

use kupl::{repl, run};

const USAGE: &str = "KUPL — K Universal Programming Language (v0.2)

Usage:
  kupl run <file.kupl> [--vm]       Run the app / `fun main` (--vm: on the KVM bytecode VM)
  kupl dis <file.kupl>              Disassemble the compiled KVM bytecode
  kupl test <file.kupl>             Run `example` blocks + contract laws as tests
  kupl check <file.kupl> [--json]   Parse, type-check, and effect-check
  kupl fmt <file.kupl> [--write]    Print (or rewrite to) canonical form
  kupl context <file.kupl> <name>   Emit an item + its direct deps (LLM context)
  kupl repl                         Start an interactive session
  kupl version                      Print version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let vm = args.iter().any(|a| a == "--vm");
    let code = match args.first().map(String::as_str) {
        Some("run") if vm => with_file(&args, run::run_program_vm),
        Some("run") => with_file(&args, run::run_program),
        Some("dis") => with_file(&args, run::disassemble),
        Some("test") => with_file(&args, run::run_tests),
        Some("check") => with_file(&args, |src, file| match run::compile(src) {
            Ok(c) => {
                if json {
                    println!("{}", kupl::diag::to_json(&c.warnings, src, file));
                } else {
                    run::print_diags(&c.warnings, src, file);
                    println!("ok: {file}");
                }
                0
            }
            Err(errors) => {
                if json {
                    println!("{}", kupl::diag::to_json(&errors, src, file));
                } else {
                    run::print_diags(&errors, src, file);
                }
                1
            }
        }),
        Some("fmt") => with_file(&args, |src, file| {
            let (program, diags) = kupl::parser::parse(src);
            let errors: Vec<_> = diags
                .into_iter()
                .filter(|d| d.severity == kupl::diag::Severity::Error)
                .collect();
            if !errors.is_empty() {
                run::print_diags(&errors, src, file);
                return 1;
            }
            let formatted = kupl::fmt::format_program(&program);
            if args.iter().any(|a| a == "--write") {
                if let Err(e) = std::fs::write(file, &formatted) {
                    eprintln!("error: cannot write {file}: {e}");
                    return 1;
                }
                println!("formatted: {file}");
            } else {
                print!("{formatted}");
            }
            0
        }),
        Some("context") => match (args.get(1), args.get(2)) {
            (Some(path), Some(name)) => match std::fs::read_to_string(path) {
                Ok(src) => run::emit_context(&src, path, name),
                Err(e) => {
                    eprintln!("error: cannot read {path}: {e}");
                    2
                }
            },
            _ => {
                eprintln!("usage: kupl context <file.kupl> <item-name>");
                2
            }
        },
        Some("repl") => repl::repl(),
        Some("version") | Some("--version") | Some("-V") => {
            println!("kupl {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            print!("{USAGE}");
            2
        }
    };
    ExitCode::from(code as u8)
}

fn with_file(args: &[String], f: impl Fn(&str, &str) -> i32) -> i32 {
    let Some(path) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
        eprintln!("error: missing <file.kupl> argument");
        return 2;
    };
    match std::fs::read_to_string(path) {
        Ok(src) => f(&src, path),
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            2
        }
    }
}
