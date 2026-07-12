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
  kupl pkg tree <file.kupl>         Print the resolved dependency tree
  kupl pkg lock <file.kupl>         Write a lockfile pinning resolved dependency versions
  kupl pkg fetch <file.kupl>        Download registry dependencies into the local cache
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
                // Sibling-consistency fix (PR-it594): `kupl dis missing.kx` (run.rs's
                // `disassemble`) already reports the IDENTICAL "missing .kx file"
                // condition as exit 1 -- this direct `kupl run <file.kx>` path used to
                // report exit 2 for the same failure on the same file extension.
                eprintln!("error: cannot read {path}: {e}");
                1
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
            (Some("fetch"), Some(p)) => run::pkg_fetch(p),
            _ => {
                eprintln!("usage: kupl pkg <tree|lock|fetch> <file.kupl>");
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
            // A REAL sibling-consistency bug (PR-it594): every OTHER subcommand
            // (run/check/native/test/build/bundle/dis/manifest/context, all routed
            // through `load_compile`/`loader::load`) reports an unreadable entry file
            // as a K0400 LOAD failure (exit 1) -- `kupl fmt`, the one subcommand still
            // using this standalone helper, used to report the IDENTICAL condition as
            // exit 2 (grouped with the unrelated "no argument at all" usage error
            // above), so a script checking `$?` got a different signal for the same
            // underlying problem depending on which subcommand it ran. Wording unified
            // to match K0400's "module file" phrasing too.
            eprintln!("error: cannot read module file {path}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_module, valid_project_name, with_file, USAGE};

    /// A REAL discoverability gap (PR-it669): `kupl pkg tree|lock|fetch
    /// <file.kupl>` is a fully implemented, tested subcommand (dispatched in
    /// `run_cli`'s match, with its own usage error and `run.rs` tests), but it
    /// was entirely absent from the top-level `USAGE` string -- so `kupl` with
    /// no args, an unknown subcommand, or `--help`-style confusion never told
    /// the user `pkg` exists at all. Every OTHER dispatched top-level
    /// subcommand is one line in `USAGE`; this locks in that `pkg` (and any
    /// future addition) can't silently drop out of the help text again. This
    /// is a manual list, not derived from the match arms -- so a newly added
    /// dispatched subcommand must be added HERE too, not just to `run_cli`.
    #[test]
    fn usage_text_mentions_every_dispatched_top_level_subcommand() {
        for cmd in [
            "run", "build", "bundle", "native", "dis", "diff", "new", "pkg", "test", "check", "fmt", "context",
            "manifest", "repl", "lsp", "version",
        ] {
            assert!(
                USAGE.contains(&format!("kupl {cmd}")),
                "USAGE is missing a line for the `{cmd}` subcommand, which run_cli actually dispatches"
            );
        }
    }

    #[test]
    fn with_file_reports_missing_entry_as_a_load_failure_not_a_bare_usage_error() {
        // A REAL sibling-consistency bug (PR-it594): `kupl fmt` (the one subcommand
        // still using `with_file`) used to report an unreadable entry file as exit 2,
        // the SAME code as its own "no argument at all" usage error a few lines above
        // -- while every other subcommand (run/check/native/test/build/bundle/dis/
        // manifest/context, routed through `load_compile`/`loader::load`) reports the
        // identical "can't read this entry file" condition as a K0400 LOAD failure,
        // exit 1. Fixed by giving `with_file`'s read-error branch its own exit 1.
        let args = vec!["fmt".to_string(), "/definitely/does/not/exist/kupl-it594.kupl".to_string()];
        let code = with_file(&args, |_src, _file| 0);
        assert_eq!(code, 1, "an unreadable entry file must be a load failure (exit 1), not a usage error (exit 2)");

        // the genuinely-missing-ARGUMENT case is a DIFFERENT situation and must stay exit 2.
        let no_arg = vec!["fmt".to_string()];
        assert_eq!(with_file(&no_arg, |_src, _file| 0), 2);
    }

    #[test]
    fn missing_kx_file_reports_the_same_exit_code_whether_run_or_disassembled() {
        // A REAL sibling-consistency bug (PR-it594): `kupl dis missing.kx` (routed
        // through `run::disassemble`'s own `.kx` branch) already reported a missing
        // `.kx` file as exit 1 -- `kupl run missing.kx` (a SEPARATE direct-decode path
        // in `main()`, for the same file extension) reported exit 2 for the identical
        // condition. Fixed by matching `run`'s exit code to `dis`'s.
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet (e.g. a lib-only build) -- nothing to test
        }
        let missing = "/definitely/does/not/exist/kupl-it594.kx";
        let run_code = std::process::Command::new(&bin)
            .args(["run", missing])
            .output()
            .expect("kupl run runs")
            .status
            .code();
        let dis_code = std::process::Command::new(&bin)
            .args(["dis", missing])
            .output()
            .expect("kupl dis runs")
            .status
            .code();
        assert_eq!(run_code, Some(1), "kupl run on a missing .kx file must be exit 1");
        assert_eq!(run_code, dis_code, "run and dis must agree on a missing .kx file's exit code");
    }

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

    /// A robustness-audit finding (production-hardening PR-it619): `repl.rs`
    /// had never had a process-level fuzz test at all -- only two pure-
    /// function unit tests for `braces_balanced`/`is_item` in isolation,
    /// unlike the CLI's own top-level dispatch (it578/it594's non-UTF8-argv
    /// and exit-code fixes) and `.kx` deserialization (`corrupt_kx_is_rejected_not_a_crash`'s
    /// exhaustive truncation + single-byte-flip fuzzing), both already
    /// thoroughly hardened. Pipes a battery of adversarial multi-line inputs
    /// through the REAL `kupl repl` process's stdin -- unterminated strings,
    /// deeply unbalanced braces, a 1-million-character line, multibyte
    /// UTF-8, empty/unknown REPL commands, and a mid-signature EOF with no
    /// trailing newline at all -- and asserts the process never panics
    /// (no "internal compiler error"/"panicked at" in its output) and never
    /// hangs. No bug found -- `braces_balanced`/`is_item` only ever iterate
    /// `chars()` (never byte-index or slice), so they're panic-safe by
    /// construction; `read_line`'s `Err` case already returns cleanly.
    /// Locking this in as a permanent regression test rather than leaving
    /// the REPL as the one interactive surface with no fuzz coverage at all.
    #[test]
    fn repl_survives_adversarial_piped_input_without_panicking_or_hanging() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let inputs: Vec<String> = vec![
            "".to_string(),
            "\n".to_string(),
            ":quit\n".to_string(),
            ":unknown-command\n".to_string(),
            "fun f() -> Int {\n".repeat(50) + "1\n" + &"}\n".repeat(50),
            "print(\"unterminated\n".to_string(),
            "print(\"a { b\n".to_string(),
            "fun f() { (((((((((((\n".to_string(),
            "}}}}}}}}}}}}}}}}\n".to_string(),
            "2 + 3\n".repeat(1000),
            "x".repeat(1_000_000) + "\n",
            "print(\"日本語 🎉🎉🎉\")\n".to_string(),
            "fun f(".to_string(), // truncated mid-signature, no trailing newline, then EOF
            ":defs\n:help\n:quit\n".to_string(),
            "let café = \"日本\"\ncafé\n:quit\n".to_string(),
        ];
        for input in inputs {
            let mut child = std::process::Command::new(&bin)
                .arg("repl")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("kupl repl spawns");
            // Write stdin on a background thread rather than blocking here: if
            // the child fills its stdout/stderr pipe buffer before finishing a
            // large write (the 1-million-character-line case), a synchronous
            // write_all on the main thread could deadlock against the child's
            // own blocked write -- the classic pipe-deadlock shape `subprocess`
            // avoids internally via `communicate()`. Writing concurrently with
            // the wait/read below sidesteps it regardless of buffer sizes.
            let mut stdin = child.stdin.take().unwrap();
            let input_bytes = input.clone().into_bytes();
            let writer = std::thread::spawn(move || {
                use std::io::Write as _;
                let _ = stdin.write_all(&input_bytes);
                // drop(stdin) here closes the pipe -> EOF, so the REPL's
                // `read_line` sees Ok(0) and exits cleanly once caught up.
            });
            let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
            let _ = writer.join();
            let preview: String = input.chars().take(60).collect();
            let out = out.unwrap_or_else(|| panic!("kupl repl hung on input starting {preview:?}"));
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !combined.contains("internal compiler error") && !combined.contains("panicked at"),
                "kupl repl panicked on input starting {preview:?}: {combined}"
            );
        }
    }

    /// A coverage-closing test, per production-hardening PR-it651 (no bug
    /// found -- `repl.rs`'s own core state-management claim, "Keep live
    /// values/instances; swap in the new definitions" (its redefinition-
    /// handling code comment), had ZERO test coverage of whether that claim
    /// is actually TRUE: the sibling fuzz test above only asserts the REPL
    /// never panics/hangs on adversarial input, never that a variable's
    /// VALUE or a component instance's live STATE is genuinely preserved
    /// across an interleaved, unrelated redefinition. Verified live via the
    /// real binary before writing this test (not assumed): a plain `let`
    /// binding survives an unrelated `fun` redefinition; a live component
    /// instance's mutated state survives too, continuing to accumulate
    /// correctly; and -- the trickiest case -- redefining a component
    /// ADDING a new field/method leaves an ALREADY-INSTANTIATED old instance
    /// correctly frozen to its ORIGINAL shape (a clean "does not expose"
    /// panic on the new method, not silent corruption or a crash), while a
    /// FRESH instantiation after the redefinition correctly uses the new
    /// shape -- confirming `Instance.comp` is genuinely a frozen snapshot
    /// per instance, not a live reference to mutable component metadata.
    #[test]
    fn repl_preserves_live_variable_and_component_state_across_redefinition() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "let x = 42\n\
                      fun unrelated1() -> Int { 1 }\n\
                      x\n\
                      component Counter {\n    intent \"c\"\n    state n: Int = 0\n    \
                      expose fun bump(v: Int) -> Int {\n        n = n + v\n        n\n    }\n}\n\
                      let c = Counter()\n\
                      c.bump(5)\n\
                      fun unrelated2() -> Int { 2 }\n\
                      c.bump(3)\n\
                      component Counter {\n    intent \"c\"\n    state n: Int = 0\n    \
                      expose fun bump(v: Int) -> Int {\n        n = n + v\n        n\n    }\n    \
                      expose fun readNew() -> Str {\n        \"new-shape\"\n    }\n}\n\
                      c.readNew()\n\
                      let c2 = Counter()\n\
                      c2.readNew()\n\
                      :quit\n";
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
        let out = wait_with_timeout(child, std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!("{stdout}{stderr}");
        assert!(!combined.contains("panicked at"), "kupl repl panicked: {combined}");
        // the plain `let x = 42` value survives an unrelated `fun` redefinition
        assert!(stdout.contains("42"), "x's value must survive redefinition: {stdout}");
        // the live component instance's state accumulates correctly (5, then 8)
        // across an unrelated redefinition sitting in between
        assert!(stdout.contains("\n5\n") || stdout.contains(" 5\n"), "c.bump(5): {stdout}");
        assert!(stdout.contains("\n8\n") || stdout.contains(" 8\n"), "c.bump(3) -> 8: {stdout}");
        // the OLD instance `c` stays frozen to its original shape -- a clean
        // panic naming the missing method, not a crash or silent misbehavior
        assert!(
            stderr.contains("does not expose") && stderr.contains("readNew"),
            "the pre-existing instance must not gain the new component's method: {stderr}"
        );
        // a FRESH instance created after the redefinition uses the new shape
        assert!(stdout.contains("new-shape"), "a new instance must see the new method: {stdout}");
    }

    /// Waits for `child` to exit, but gives up after `timeout` rather than
    /// blocking forever -- `std::process::Child::wait_with_output` has no
    /// built-in deadline (and this repo's own convention is that macOS has
    /// no `timeout` command to shell out to). `wait_with_output` itself is
    /// used here, not a hand-rolled `try_wait` polling loop: a large adversarial
    /// input can make the child echo well over the OS pipe buffer size (64KB)
    /// into stderr (e.g. a malformed-token error message quoting a
    /// million-character identifier verbatim) -- a polling loop that only
    /// calls `try_wait` without draining stdout/stderr concurrently would
    /// deadlock the CHILD (blocked writing to a full, unread pipe) against
    /// the TEST (blocked waiting for an exit that can't happen). Running
    /// `wait_with_output` (which drains both pipes concurrently via its own
    /// internal threads) on a background thread and racing it against the
    /// timeout via a channel gets both properties: no deadlock, AND a bound
    /// on genuine hangs.
    fn wait_with_timeout(
        child: std::process::Child,
        timeout: std::time::Duration,
    ) -> Option<std::process::Output> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        rx.recv_timeout(timeout).ok().and_then(Result::ok)
    }
}
