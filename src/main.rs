use std::process::ExitCode;

use kupl::{repl, run};

const USAGE: &str = "KUPL — K Universal Programming Language (v0.1)

Usage:
  kupl run <file.kupl>     Run the app (or `fun main()`) in a file
  kupl test <file.kupl>    Run all `example` blocks as tests
  kupl check <file.kupl>   Parse and type-check only
  kupl repl                Start an interactive session
  kupl version             Print version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => with_file(&args, run::run_program),
        Some("test") => with_file(&args, run::run_tests),
        Some("check") => with_file(&args, |src, file| match run::compile(src) {
            Ok(c) => {
                run::print_diags(&c.warnings, src, file);
                println!("ok: {file}");
                0
            }
            Err(errors) => {
                run::print_diags(&errors, src, file);
                1
            }
        }),
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
    let Some(path) = args.get(1) else {
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
