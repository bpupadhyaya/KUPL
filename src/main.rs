use std::process::ExitCode;

use kupl::{repl, run};

const USAGE: &str = "KUPL — K Universal Programming Language (v0.2)

Usage:
  kupl run <file.kupl> [--vm]       Run the app / `fun main` (--vm: on the KVM bytecode VM)
  kupl run <file.kx>                Run a compiled .kx module on the KVM
  kupl build <file.kupl> [-o f.kx]  Compile to a .kx bytecode module
  kupl bundle <file.kupl> [-o app]  Produce a self-contained native executable
  kupl dis <file.kupl>              Disassemble the compiled KVM bytecode
  kupl test <file.kupl>             Run `example` blocks + contract laws as tests
  kupl check <file.kupl> [--json]   Parse, type-check, and effect-check
  kupl fmt <file.kupl> [--write]    Print (or rewrite to) canonical form
  kupl context <file.kupl> <name>   Emit an item + its direct deps (LLM context)
  kupl repl                         Start an interactive session
  kupl version                      Print version
";

fn main() -> ExitCode {
    // A bundled executable carries its module in a trailer — run it directly.
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(bytes) = std::fs::read(&exe) {
            if let Some(result) = kupl::kx::read_bundle(&bytes) {
                return match result {
                    Ok(module) => ExitCode::from(run::run_module(&module, "bundled module") as u8),
                    Err(e) => {
                        eprintln!("error: corrupt bundle: {e}");
                        ExitCode::from(1)
                    }
                };
            }
        }
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let vm = args.iter().any(|a| a == "--vm");
    let kx_path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .filter(|p| p.ends_with(".kx"))
        .cloned();
    let code = match (args.first().map(String::as_str), kx_path) {
        (Some("run"), Some(path)) => match std::fs::read(&path) {
            Ok(bytes) => match kupl::kx::decode(&bytes) {
                Ok(module) => run::run_module(&module, &path),
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                2
            }
        },
        (cmd, _) => match cmd {
        Some("run") if vm => with_file(&args, run::run_program_vm),
        Some("run") => with_file(&args, run::run_program),
        Some("dis") => with_file(&args, run::disassemble),
        Some("build") => with_file(&args, |src, file| {
            build_module(&args, src, file, false)
        }),
        Some("bundle") => with_file(&args, |src, file| {
            build_module(&args, src, file, true)
        }),
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
        },
    };
    ExitCode::from(code as u8)
}

/// `kupl build` / `kupl bundle`: compile to a .kx module; bundle additionally
/// wraps it in a copy of this executable for a self-contained program.
fn build_module(args: &[String], src: &str, file: &str, bundle: bool) -> i32 {
    let compiled = match run::compile(src) {
        Ok(c) => c,
        Err(errors) => {
            run::print_diags(&errors, src, file);
            return 1;
        }
    };
    run::print_diags(&compiled.warnings, src, file);
    let module = match kupl::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => m,
        Err(diags) => {
            run::print_diags(&diags, src, file);
            return 1;
        }
    };
    let out = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| {
            let stem = file.trim_end_matches(".kupl");
            if bundle {
                stem.to_string()
            } else {
                format!("{stem}.kx")
            }
        });
    let bytes = if bundle {
        let exe = match std::env::current_exe().and_then(std::fs::read) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error: cannot read own executable: {e}");
                return 1;
            }
        };
        kupl::kx::write_bundle(&exe, &module)
    } else {
        kupl::kx::encode(&module)
    };
    if let Err(e) = std::fs::write(&out, &bytes) {
        eprintln!("error: cannot write {out}: {e}");
        return 1;
    }
    #[cfg(unix)]
    if bundle {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
    }
    println!(
        "{}: {out} ({} bytes)",
        if bundle { "bundled executable" } else { "compiled module" },
        bytes.len()
    );
    0
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
