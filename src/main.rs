use std::process::ExitCode;

use kupl::{repl, run};

const USAGE: &str = "KUPL — K Universal Programming Language (v0.2)

Usage:
  kupl run <file.kupl> [--vm]       Run the app / `fun main` (--vm: on the KVM bytecode VM)
  kupl run <file.kx>                Run a compiled .kx module on the KVM
  kupl build <file.kupl> [-o f.kx]  Compile to a .kx bytecode module
  kupl bundle <file.kupl> [-o app]  Produce a self-contained executable (VM + module)
  kupl native <file.kupl> [-o app]  Compile to machine code via C (fun main; --keep-c)
  kupl dis <file.kupl>              Disassemble the compiled KVM bytecode
  kupl diff <old.kupl> <new.kupl>   Semantic diff (interface vs implementation)
  kupl new <name>                   Scaffold a new KUPL project
  kupl test <file.kupl>             Run `example` blocks + contract laws as tests
  kupl check <file.kupl> [--json]   Parse, type-check, and effect-check
  kupl fmt <file.kupl> [--write]    Print (or rewrite to) canonical form
  kupl context <file.kupl> <name>   Emit an item + its direct deps (LLM context)
  kupl manifest <file.kupl>         Emit component manifests as JSON (visual tools)
  kupl repl                         Start an interactive session
  kupl lsp                          Start the Language Server (stdio, for editors)
  kupl version                      Print version
";

fn main() -> ExitCode {
    // Run the whole CLI on a worker thread with a large stack. The tree-walking
    // interpreter recurses on the native stack (one KUPL call = several Rust
    // frames), so deeply-recursive programs (e.g. a backtracking solver) need
    // more than the default 8 MiB — and this keeps the interpreter's recursion
    // depth on par with the KVM's heap-allocated frame stack.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run_cli)
        .expect("spawn main thread")
        .join()
        .unwrap_or(ExitCode::from(101))
}

fn run_cli() -> ExitCode {
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
        Some("run") if vm => with_path(&args, run::run_program_vm),
        Some("run") => with_path(&args, run::run_program),
        Some("dis") => with_path(&args, run::disassemble),
        Some("native") => with_path(&args, |path| run::native(path, &args)),
        Some("diff") => match (args.get(1), args.get(2)) {
            (Some(old), Some(new)) => kupl::sdiff::semantic_diff(old, new),
            _ => {
                eprintln!("usage: kupl diff <old.kupl> <new.kupl>");
                2
            }
        },
        Some("new") => match args.get(1) {
            Some(name) => scaffold_project(name),
            None => {
                eprintln!("usage: kupl new <project-name>");
                2
            }
        },
        Some("manifest") => with_path(&args, run::emit_manifest),
        Some("pkg") => match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("tree"), Some(p)) => run::pkg_tree(p),
            (Some("lock"), Some(p)) => run::pkg_lock(p),
            _ => {
                eprintln!("usage: kupl pkg <tree|lock> <file.kupl>");
                2
            }
        },
        Some("build") => with_file(&args, |src, file| {
            build_module(&args, src, file, false)
        }),
        Some("bundle") => with_file(&args, |src, file| {
            build_module(&args, src, file, true)
        }),
        Some("test") => with_path(&args, run::run_tests),
        Some("check") => with_path(&args, |path| run::check_cmd(path, json)),
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
            (Some(path), Some(name)) => run::emit_context(path, name),
            _ => {
                eprintln!("usage: kupl context <file.kupl> <item-name>");
                2
            }
        },
        Some("repl") => repl::repl(),
        Some("lsp") => kupl::lsp::serve(),
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

/// `kupl new`: scaffold a project directory.
fn scaffold_project(name: &str) -> i32 {
    let root = std::path::Path::new(name);
    if root.exists() {
        eprintln!("error: {name} already exists");
        return 1;
    }
    let main_src = format!(
        "use util\n\napp Main {{\n    intent \"{name}: describe what this app is for.\"\n\n    let greeter = Greeter()\n    let starter = Starter()\n\n    wire starter.go -> greeter.hello\n}}\n\ncomponent Starter {{\n    intent \"Kicks things off at startup.\"\n\n    out go: Str\n\n    on start {{\n        emit go(\"{name}\")\n    }}\n}}\n\ncomponent Greeter {{\n    intent \"Greets whatever arrives.\"\n\n    in hello: Str\n\n    on hello(who) {{\n        print(greeting(who))\n    }}\n}}\n"
    );
    let util_src = "pub fun greeting(name: Str) -> Str {\n    \"hello from {name}!\"\n}\n";
    let toml = format!(
        "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.kupl\"\n"
    );
    let write = |p: &std::path::Path, c: &str| -> bool {
        if let Err(e) = std::fs::write(p, c) {
            eprintln!("error: cannot write {}: {e}", p.display());
            return false;
        }
        true
    };
    if std::fs::create_dir_all(root).is_err() {
        eprintln!("error: cannot create {name}/");
        return 1;
    }
    if !write(&root.join("main.kupl"), &main_src)
        || !write(&root.join("util.kupl"), util_src)
        || !write(&root.join("kupl.toml"), &toml)
    {
        return 1;
    }
    println!("created {name}/ (main.kupl, util.kupl, kupl.toml)");
    println!("  kupl run {name}/main.kupl");
    0
}

fn with_path(args: &[String], f: impl Fn(&str) -> i32) -> i32 {
    let Some(path) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
        eprintln!("error: missing <file.kupl> argument");
        return 2;
    };
    f(path)
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
