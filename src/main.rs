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
    // Production safety net: a bug in the compiler should never dump a raw Rust
    // panic + backtrace at the user. Convert any panic into one concise,
    // reportable line. The worker-thread join below turns it into exit code 101.
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!(" [{}:{}]", l.file(), l.line()))
            .unwrap_or_default();
        eprintln!(
            "kupl: internal compiler error{loc} — this is a bug in KUPL, not your program. \
             Please report it with the input that triggered it."
        );
    }));
    // Run the whole CLI on a worker thread with a large stack. The tree-walking
    // interpreter recurses on the native stack (one KUPL call = several Rust
    // frames), so deeply-recursive programs (e.g. a backtracking solver) need
    // more than the default 8 MiB — and this keeps the interpreter's recursion
    // depth on par with the KVM's heap-allocated frame stack.
    std::thread::Builder::new()
        // Sized so the interpreter can reach its MAX_CALL_DEPTH (10 000) recursion
        // guard before exhausting the native stack — the guard then yields a clean
        // `stack overflow` panic (matching the KVM) instead of a fatal abort. The
        // reservation is virtual; only touched pages commit.
        .stack_size(2 * 1024 * 1024 * 1024)
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

    // `std::env::args()` PANICS on any argument that isn't valid Unicode. A raw,
    // non-UTF8 argv element is rare but real (e.g. a filename-derived argument
    // forwarded by another tool) — this is the CLI's own top-level arg parsing,
    // reached before user code ever runs, so a bare Rust panic here previously
    // crashed the whole `kupl` invocation as a misleading "internal compiler
    // error" no matter what subcommand was requested. `args_os()` never panics;
    // an unrepresentable argument becomes a placeholder instead (matches
    // interp::program_args's identical fix for a user program's OWN `args()`
    // builtin, PR-it578).
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_str().map(str::to_string).unwrap_or_else(|| "\u{FFFD}".to_string()))
        .collect();
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
        Some("build") => with_path(&args, |file| build_module(&args, file, false)),
        Some("bundle") => with_path(&args, |file| build_module(&args, file, true)),
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
            // The formatter renders from the AST, which has no comments — warn so a
            // format-on-save / `--write` never silently deletes them.
            if kupl::fmt::source_has_comments(src) {
                eprintln!(
                    "note: `kupl fmt` does not yet preserve comments — they are dropped from the formatted output"
                );
            }
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
///
/// Uses `load_compile` (the multi-file-aware loader, same as `kupl run`/`kupl
/// check`) rather than a raw single-file read -- before PR-it507, `build`/
/// `bundle` read the entry file directly and never resolved `use` imports, so
/// a valid multi-file program that `kupl run`/`kupl check` accepted failed to
/// even compile to a `.kx` module or bundled executable ("unknown name"
/// errors for every cross-module function).
fn build_module(args: &[String], file: &str, bundle: bool) -> i32 {
    let (compiled, map) = match run::load_compile(file) {
        Ok(ok) => ok,
        Err(code) => return code,
    };
    let module = match kupl::compile::compile_module(&compiled.program, &compiled.checked) {
        Ok(m) => m,
        Err(diags) => {
            run::print_diags_map(&diags, &map);
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
/// A project name must be a plain, filesystem- and manifest-safe token: it becomes
/// a directory AND is embedded verbatim in the generated `kupl.toml`/source. This
/// rejects path traversal (`../evil`, `/abs`, `a/b`), the `.`/`..` specials, an
/// empty name (which would scatter files into the cwd), and any character that
/// would break the manifest string (quotes, backslashes, controls).
fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn scaffold_project(name: &str) -> i32 {
    if !valid_project_name(name) {
        eprintln!(
            "error: invalid project name `{name}` — use letters, digits, `-` or `_` \
             (must start with a letter or digit; no path separators or `..`)"
        );
        return 1;
    }
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

#[cfg(test)]
mod tests {
    use super::{build_module, valid_project_name};

    #[test]
    fn build_resolves_multi_file_use_imports() {
        // A real capability bug (PR-it507): `kupl build`/`kupl bundle` read the entry file
        // directly (`with_file` -> raw `std::fs::read_to_string` + single-file `run::compile`)
        // and never resolved `use` imports -- while `kupl run`/`kupl check` route through the
        // multi-file-aware `run::load_compile` (which calls the loader). So a valid multi-file
        // program (examples/multifile/main.kupl, which does `use util` / `use lib.stats`) that
        // `kupl run`/`kupl check` accepted FAILED to even compile to a `.kx` module: "unknown
        // name `mean`" / "unknown name `label`" for the cross-module functions. Fixed by
        // switching `build`/`bundle` to `with_path` + `run::load_compile` (same loader as run/
        // check), so `build_module` now sees the resolved multi-file program.
        let entry = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/multifile/main.kupl");
        let out = std::env::temp_dir().join(format!("kupl_it507_test_{}.kx", std::process::id()));
        let args = vec![
            "build".to_string(),
            entry.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
        ];
        let code = build_module(&args, entry.to_str().unwrap(), false);
        assert_eq!(code, 0, "build_module must succeed on a valid multi-file program");
        let bytes = std::fs::read(&out).expect("build_module must have written the .kx file");
        let module = kupl::kx::decode(&bytes).expect("compiled module must decode");
        // The cross-module functions (from util.kupl and lib/stats.kupl, pulled in via `use`)
        // must actually be present in the compiled module -- not just "didn't crash".
        assert!(module.funs.contains_key("mean"), "compiled module must resolve `use lib.stats`'s `mean`");
        assert!(module.funs.contains_key("label"), "compiled module must resolve `use util`'s `label`");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn project_name_rejects_traversal_injection_and_empty() {
        // safe names accepted
        for ok in ["myapp", "my-app", "my_app", "app2", "A1"] {
            assert!(valid_project_name(ok), "should accept `{ok}`");
        }
        // path traversal / separators / absolute / specials / injection / empty
        for bad in ["../evil", "..", ".", "/abs", "a/b", "a\\b", "x\"evil", "", "-leading", "a b", "na\nme", "app;rm"] {
            assert!(!valid_project_name(bad), "should reject `{bad:?}`");
        }
        // over-long names are rejected (keeps paths + manifests sane)
        assert!(!valid_project_name(&"a".repeat(65)));
    }
}
