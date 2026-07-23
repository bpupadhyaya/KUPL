use std::process::ExitCode;

use kupl::{repl, run};

// SOUNDNESS FIX (production-hardening PR-it974): this banner used to hardcode
// a literal "(v0.2)" -- stale since Cargo.toml (and `kupl version`/`kupl
// --version`, which reads `env!("CARGO_PKG_VERSION")` at line ~349) moved on
// to "1.0.0-alpha" long ago, so `kupl --help`/a bare `kupl`/any usage error
// showed a DIFFERENT, wrong version than `kupl version` itself -- an internal
// inconsistency in the shipped binary's own output, not just a docs staleness
// issue. Built via `concat!` + `env!("CARGO_PKG_VERSION")` (both compile-time,
// so USAGE stays a plain `&'static str` -- no other call site needs to change)
// so this can never drift out of sync with the crate's own version again.
const USAGE: &str = concat!(
    "KUPL — K Universal Programming Language (v",
    env!("CARGO_PKG_VERSION"),
    ")

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
  kupl fmt <file.kupl> --check      Exit nonzero if the file isn't already canonical (CI gate)
  kupl context <file.kupl> <name>   Emit an item + its direct deps (LLM context)
  kupl manifest <file.kupl>         Emit component manifests as JSON (visual tools)
  kupl repl                         Start an interactive session
  kupl lsp                          Start the Language Server (stdio, for editors)
  kupl version                      Print version
"
);

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
                    Ok(module) => {
                        // A REAL, live-confirmed cross-engine divergence found+fixed
                        // (production-hardening PR-it798, found via an Explore-agent
                        // survey): this executable IS the whole running process (no
                        // `kupl run`/`--` wrapper at all), so its own args are
                        // EVERYTHING after the binary name -- `program_args()`'s
                        // `--`-separator convention (correct only for the `kupl run`
                        // CLI-wrapper shape) doesn't apply here, but `run_module`
                        // used to always go through it, so `args()` silently
                        // returned `[]` unless invoked with a bizarre spurious
                        // `./myapp -- a b c`. Matches `program_args()`'s OWN
                        // invalid-UTF8-placeholder handling for consistency.
                        let raw_args: Vec<String> = std::env::args_os()
                            .skip(1)
                            .map(|a| a.to_str().map(str::to_string).unwrap_or_else(|| "\u{FFFD}".to_string()))
                            .collect();
                        ExitCode::from(run::run_module(&module, "bundled module", Some(raw_args)) as u8)
                    }
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
    // A REAL, LIVE-CONFIRMED silent-wrong-behavior bug found+fixed
    // (production-hardening PR-it998, a close-read survey of this file's
    // CLI dispatch): this used to scan for the FIRST non-`--`-prefixed
    // token itself (`args.iter().skip(1).find(...)`), completely bypassing
    // `find_path_arg`'s hardened single-positional-argument enforcement --
    // so `kupl run report.kx realprog.kupl` silently ran `report.kx` and
    // NEVER even looked at `realprog.kupl` (no "unexpected extra argument"
    // rejection, unlike every OTHER subcommand's PR-it864/PR-it697/PR-it798
    // precedent for this exact bug class), and `kupl run -- report.kx`
    // silently ran `report.kx` too, violating the `--`-separator contract
    // ("everything after `--` belongs to the program, not kupl") that
    // `find_path_arg` and `interp::program_args` both otherwise honor.
    // Routing through `find_path_arg` (the SAME helper every other
    // file-taking subcommand already uses) fixes both: a genuine second
    // positional argument is now cleanly rejected, and a token after `--`
    // is never considered a candidate path.
    let kx_path = match args.first().map(String::as_str) {
        Some("run") => match find_path_arg(&args) {
            Ok(path) if path.ends_with(".kx") => Some(path.to_string()),
            _ => None,
        },
        _ => None,
    };
    let code = match (args.first().map(String::as_str), kx_path) {
        (Some("run"), Some(path)) => match std::fs::read(&path) {
            Ok(bytes) => match kupl::kx::decode(&bytes) {
                Ok(module) => run::run_module(&module, &path, None),
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
        // A REAL bug found+fixed (production-hardening PR-it864, an Explore
        // survey finding, independently re-verified live before
        // implementing): the SAME "unexpected extra argument silently
        // dropped" shape `find_path_arg`'s own fix (production-hardening
        // PR-it697) already closed for every OTHER file-taking subcommand --
        // `match (args.get(1), args.get(2))` never even looks at `args.get(3)`,
        // so a genuinely unexpected THIRD positional argument (a plausible
        // typo, or a leftover argument from a copy-pasted command) was
        // silently IGNORED with zero diagnostic, running the diff on just the
        // first two paths. Live-confirmed BEFORE this fix: `kupl diff
        // old.kupl new.kupl extra_typo` ran the diff cleanly, exit 1/0
        // depending on content, with `extra_typo` never examined or
        // mentioned anywhere.
        Some("diff") => match (args.get(1), args.get(2)) {
            (Some(old), Some(new)) if args.get(3).is_none() => kupl::sdiff::semantic_diff(old, new),
            (Some(_), Some(_)) => {
                eprintln!("error: unexpected extra argument `{}`", args[3]);
                2
            }
            _ => {
                eprintln!("usage: kupl diff <old.kupl> <new.kupl>");
                2
            }
        },
        // A REAL bug found+fixed (production-hardening PR-it1062, a background
        // close-read survey finding): the SAME "unexpected extra argument
        // silently dropped" shape PR-it864 already closed for `diff`/`context`
        // just above -- `new` took its single positional argument via a raw
        // `args.get(1)`, never checking `args.get(2)` at all, so a genuinely
        // unexpected SECOND positional argument (a plausible typo, or a
        // leftover argument from a copy-pasted/mis-templated command, e.g. a
        // script doing `kupl new "$name" "$stray_var"`) was silently IGNORED
        // with zero diagnostic, scaffolding the project from just the first
        // argument. Live-confirmed BEFORE this fix: `kupl new demo
        // unexpected_second_arg unexpected_third_arg` created `demo/` cleanly,
        // exit 0, with the extra arguments never examined or mentioned
        // anywhere -- a CI-invisibility risk, matching PR-it864's own framing.
        Some("new") => match (args.get(1), args.get(2)) {
            (Some(name), None) => scaffold_project(name),
            (Some(_), Some(extra)) => {
                eprintln!("error: unexpected extra argument `{extra}`");
                2
            }
            (None, _) => {
                eprintln!("usage: kupl new <project-name>");
                2
            }
        },
        Some("manifest") => with_path(&args, run::emit_manifest),
        // A REAL usability bug found+fixed (production-hardening PR-it782, an
        // Explore survey finding, independently re-verified live before
        // implementing): `pkg`'s path argument was a raw `args.get(2)`, unlike
        // EVERY other file-taking subcommand (via `with_path`/`find_path_arg`),
        // which all skip `--flag`s and an `-o <value>` pair wherever they
        // appear. A flag placed before the path (a perfectly natural ordering,
        // matching every sibling subcommand's own accepted ordering) was
        // misread AS the path itself. Confirmed live before this fix: `kupl
        // pkg tree --json simple.kupl` reported `error: entry --json: No such
        // file or directory`, never even looking at `simple.kupl`. Fixed by
        // routing through the SAME `find_path_arg` every other subcommand
        // uses -- sliced to `&args[1..]` so `find_path_arg`'s own internal
        // `args[0]`-is-the-command-word skip lands on `tree`/`lock`/`fetch`
        // (the pkg SUB-subcommand), not `pkg` itself.
        Some("pkg") => match args.get(1).map(String::as_str) {
            Some(sub @ ("tree" | "lock" | "fetch")) => match find_path_arg(&args[1..]) {
                Ok(p) => match sub {
                    "tree" => run::pkg_tree(p),
                    "lock" => run::pkg_lock(p),
                    "fetch" => run::pkg_fetch(p),
                    _ => unreachable!(),
                },
                Err(msg) => {
                    eprintln!("error: {msg}");
                    2
                }
            },
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
            let write = args.iter().any(|a| a == "--write");
            let check = args.iter().any(|a| a == "--check");
            // PRODUCTION-HARDENING (PR-it774): `kupl fmt --check` was entirely
            // missing -- a survey-flagged gap (agentId aca5b82689fe978bd),
            // reframed as a MISSING FEATURE, not a regression (`--check` was
            // never documented or implemented; USAGE only ever listed
            // `[--write]`). Any other flag, including `--check`, was silently
            // ignored and fell through to the default print-and-exit-0 path
            // regardless of whether the file was already canonical -- a CI
            // pipeline wired to gate on `kupl fmt --check` (the `rustfmt
            // --check`/`prettier --check` convention) would never fail.
            // `--write` and `--check` together is a genuinely ambiguous
            // combination (write the file, or only report on it?) -- rejected
            // as a usage error rather than silently picking one, matching
            // this file's own convention of an explicit `eprintln!` +
            // exit 2 for a malformed invocation.
            if write && check {
                eprintln!("usage: kupl fmt <file.kupl> [--write | --check] — pass at most one");
                return 2;
            }
            if check {
                // Reuses exit code 1 for "not clean" -- the SAME code `kupl
                // check`'s own `has_errors` branch already uses (run.rs's
                // `check_cmd`), so `--check` finding a diff and a genuine
                // parse/load failure (the `errors` branch above, also exit 1)
                // both signal "this file isn't in a fully clean state" with
                // ONE consistent code, matching this project's own existing
                // convention as well as `rustfmt --check`'s exit 1. Never
                // writes to the file.
                if formatted == src {
                    println!("ok: {file}");
                    return 0;
                }
                eprintln!("would reformat: {file}");
                return 1;
            }
            if write {
                // A REAL, live-confirmed DATA-LOSS bug found+fixed (production-
                // hardening PR-it837): `format_program`'s AST-to-source rendering
                // can, for at least one confirmed pathological case (a `Float`/
                // `F32` literal whose magnitude overflows to infinity -- e.g.
                // `1e400` -- which the lexer silently accepts as a valid `inf`
                // value with NO diagnostic anywhere in the pipeline), produce
                // text that does NOT compile as valid KUPL: `let x: Float =
                // 1e400` formats to `let x: Float = inf`, and the KUPL lexer has
                // no `inf`/`nan` literal syntax, so `inf` re-lexes as a bare
                // IDENTIFIER reference -- syntactically valid (the PARSER alone
                // does not catch this; `inf` parses fine as a name), but an
                // "unknown name" (K0240) once the FULL checker runs. `--write`
                // used to overwrite the file UNCONDITIONALLY with this invalid
                // text -- no backup, no validity check -- PERMANENTLY destroying
                // the original, valid source with no way to recover it. This is a
                // general safety net closing the whole BUG CLASS (any current or
                // future formatter defect that could produce broken output), not
                // just the one confirmed instance: recompile the freshly-
                // formatted text through the FULL pipeline (`run::compile`, not
                // just `parser::parse` -- a bare re-parse would have MISSED this
                // exact bug, since `inf` parses cleanly and only fails at
                // check-time) and refuse to write if it doesn't come back clean,
                // since `format_program`'s own doc comment promise ("any two
                // programs with the same AST render identically") implies
                // round-tripping, which this enforces defensively rather than
                // trusting silently.
                if let Err(_compile_errors) = kupl::run::compile(&formatted) {
                    eprintln!(
                        "error: internal formatter bug producing invalid output for {file} -- refusing to overwrite the file (original left untouched; please report this as a KUPL bug)"
                    );
                    return 1;
                }
                // Atomic write (production-hardening PR-it1103): see
                // `loader::write_atomically`'s own doc comment -- a plain
                // `std::fs::write` here could expose a torn/empty read of
                // the user's own source file to a concurrent reader (an
                // editor, an LSP, a file-watcher-triggered `kupl run`/
                // `check`), a realistic "format on save" workflow.
                if let Err(e) = kupl::loader::write_atomically(std::path::Path::new(file), &formatted) {
                    eprintln!("error: cannot write {file}: {e}");
                    return 1;
                }
                println!("formatted: {file}");
            } else {
                // A REAL, live-confirmed bug found+fixed (production-hardening
                // PR-it889, an Explore survey finding, independently re-verified
                // live before implementing): the SAME formatter-bug-producing-
                // invalid-output class PR-it837 already guards for `--write`
                // above (and `lsp.rs::resolve_formatting`) was never applied to
                // THIS plain-print path -- confirmed live before this fix, a
                // pathological string-interpolation input (a literal `{{` inside
                // an interpolated conditional expression, tripping
                // `reindent_inline`'s naive per-line brace-count heuristic into
                // its raw-multi-line-block fallback) formatted to text with
                // literal newlines spliced into what was a single-line string
                // interpolation -- syntactically invalid, 7 cascading parse
                // errors on re-check, printed at exit 0 with zero diagnostic.
                // Worse than the write-path gap PR-it837 closed: `kupl fmt
                // file.kupl > file.kupl`, a natural shell-redirection workflow,
                // would have the shell TRUNCATE the source file before this
                // command even runs, so the file-level round-trip guard on
                // `--write` never gets a chance to protect it either. Same fix:
                // recompile the freshly-formatted text through the full
                // pipeline and refuse to print it if it doesn't come back
                // clean.
                if let Err(_compile_errors) = kupl::run::compile(&formatted) {
                    eprintln!(
                        "error: internal formatter bug producing invalid output for {file} -- refusing to print it (please report this as a KUPL bug)"
                    );
                    return 1;
                }
                print!("{formatted}");
            }
            0
        }),
        // A REAL bug found+fixed (production-hardening PR-it864): the SAME
        // "unexpected extra argument silently dropped" shape as `diff`'s
        // identical fix immediately above (both share this exact match
        // pattern) -- `args.get(3)` was never examined, so a genuinely
        // unexpected THIRD positional argument was silently ignored.
        Some("context") => match (args.get(1), args.get(2)) {
            (Some(path), Some(name)) if args.get(3).is_none() => run::emit_context(path, name),
            (Some(_), Some(_)) => {
                eprintln!("error: unexpected extra argument `{}`", args[3]);
                2
            }
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
        // PRODUCTION-HARDENING (PR-it772): an EXPLICITLY requested help screen is a
        // successful invocation, not a usage error -- the universal CLI convention
        // (git, cargo, curl, npm, docker, ...) is exit 0 for `--help`/`-h`/`help`.
        // This used to fall into the catch-all `_` arm below and share exit code 2
        // with a genuinely invalid invocation, indistinguishable to a script that
        // checks `kupl --help; echo $?` to confirm the binary is sane. Deliberately
        // NARROW in scope: only these three EXPLICIT forms get 0 -- a bare `kupl`
        // (no subcommand at all) and any unrecognized subcommand are still more
        // defensibly "the user probably made a mistake," so they keep exit 2 below,
        // unchanged.
        Some("--help") | Some("-h") | Some("help") => {
            print!("{USAGE}");
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
    // See `run::native`'s identical guard (production-hardening PR-it782)
    // for the full rationale: `.kx` fed here used to walk the lexer over
    // raw bytecode byte-by-byte instead of a clean error.
    if file.ends_with(".kx") {
        let cmd = if bundle { "bundle" } else { "build" };
        eprintln!(
            "error: {file} is already compiled bytecode (.kx) -- `kupl {cmd}` needs `.kupl` \
             source, not an existing module"
        );
        return 1;
    }
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
    // A REAL bug found+fixed (production-hardening PR-it862, an Explore
    // survey finding, independently re-verified live before implementing): a
    // trailing `-o` with no following value (e.g. `kupl build foo.kupl -o`,
    // a plausible fat-fingered mistake or an empty-string shell-expansion
    // that dropped the intended value from argv) used to be treated
    // IDENTICALLY to `-o` simply being absent -- `args.get(i + 1)` returns
    // `None` either way, silently falling back to the DEFAULT output path
    // instead of erroring. Live-confirmed BEFORE this fix: a pre-existing
    // `foo.kx` at the default path was silently overwritten by `kupl build
    // foo.kupl -o`, with zero diagnostic and exit code 0, even though the
    // user explicitly asked for `-o` to control the output path.
    let o_pos = args.iter().position(|a| a == "-o");
    if let Some(i) = o_pos {
        if args.get(i + 1).is_none() {
            eprintln!("error: -o requires a value");
            return 2;
        }
        // A REAL, LIVE-CONFIRMED silent-wrong-behavior bug found+fixed
        // (production-hardening PR-it999, the SECOND finding from the same
        // main.rs CLI-dispatch survey that produced PR-it998's kx-fast-path
        // fix): `args.iter().position(...)` always returns the FIRST `-o`
        // occurrence -- a REPEATED `-o` was silently discarded with ZERO
        // diagnostic. Live-confirmed BEFORE this fix: `kupl build foo.kupl
        // -o first.kx -o second.kx` silently produced ONLY `first.kx`,
        // `second.kx` never created, exit 0, no error or warning anywhere.
        // Rejected cleanly instead, matching this file's OWN established
        // convention of rejecting ambiguous/duplicate input rather than
        // silently picking one (e.g. `fmt`'s `--write`+`--check` rejection,
        // `find_path_arg`'s single-positional-path enforcement, PR-it864's
        // extra-positional-argument rejection) -- `args[i + 2..]` (past the
        // first `-o` AND its already-consumed value) is always a valid
        // slice here since `args.get(i + 1)` was just confirmed `Some`
        // above, so `i + 2 <= args.len()`.
        if args[i + 2..].iter().any(|a| a == "-o") {
            eprintln!("error: -o specified more than once");
            return 2;
        }
    }
    let out = o_pos
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
    // A CRITICAL data-loss bug found+fixed (production-hardening PR-it781):
    // `build` always appends `.kx` to its default output path, so it can
    // never collide with the source by construction -- but `bundle`'s
    // default is the BARE stem (no suffix), a no-op if `file` doesn't
    // literally end in `.kupl`, and an explicit `-o` can name the source
    // path for either. See `run::output_would_overwrite_source`'s own doc
    // comment for the full live repro and design rationale.
    if run::output_would_overwrite_source(&out, file) {
        eprintln!(
            "error: refusing to overwrite the source file {file} -- the output path resolves to the \
             same file (use -o to choose a different output path)"
        );
        return 1;
    }
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

/// Find the `<file.kupl>` positional argument among `args[1..]`, skipping
/// long-form flags (`--foo`, e.g. `--keep-c`) and the `-o <value>` flag
/// (`build`/`bundle`/`native` accept it, and — unlike `--foo` flags — it's a
/// TWO-token pair, so it must be skipped as a unit wherever it appears, not
/// just when it trails the path). Two REAL, previously-silent bugs fixed
/// together here (production-hardening PR-it697), since fixing either alone
/// would make the other WORSE: (1) `-o` appearing BEFORE the path (a
/// perfectly natural flag ordering, e.g. `kupl build -o out.kx foo.kupl`)
/// was itself misidentified AS the path (`-o` doesn't start with `--`),
/// producing a confusing "cannot read module file -o" error instead of
/// compiling `foo.kupl` to `out.kx` — confirmed live before this fix. (2) a
/// SECOND, genuinely unexpected positional argument (a typo, a leftover
/// argument from a copy-pasted command, e.g. `kupl run foo.kupl bar.kupl`)
/// was silently DROPPED with no diagnostic at all — confirmed live running
/// `foo.kupl` and exiting 0, `bar.kupl` never even read. Returns the path,
/// or a usage-error message (missing path / an unexpected extra argument).
///
/// A THIRD REAL bug found+fixed (production-hardening PR-it798, found while
/// verifying a related fix): a bare `--` token used to be treated the SAME
/// as any OTHER long-form flag (`a.starts_with("--")` matches `"--"`
/// trivially too) — just skipped, scanning continuing past it — instead of
/// being recognized as the documented separator meaning "everything after
/// this belongs to the PROGRAM being run, stop looking for kupl's own
/// arguments" (`program_args()`'s own doc comment, and `examples/cli.kupl`'s
/// own usage instructions, both document `kupl run prog.kupl -- a b c` as
/// the correct way to pass a program its own arguments). Confirmed live
/// before this fix: `kupl run examples/cli.kupl -- Ada Grace` failed with
/// `error: unexpected extra argument \`Ada\`` (exit 2) — the EXACT
/// documented usage pattern for `args()`, broken. Now a bare `--` stops
/// scanning immediately, matching `program_args()`'s own semantics.
///
/// A FOURTH REAL bug found+fixed (production-hardening PR-it881, found while
/// re-auditing this function's own carried-forward lead): this doc comment
/// has said "`build`/`bundle`/`native` accept it" since PR-it697 — but the
/// `-o`-pair-skipping below applied UNCONDITIONALLY to every subcommand
/// routed through `with_path`/`with_file`, including `run`/`dis`/`manifest`/
/// `test`/`check`/`fmt`/`pkg tree|lock|fetch`, none of which document or
/// implement an `-o` flag at all. Confirmed live before this fix: `kupl run
/// -o foo.kupl` (foo.kupl genuinely exists) reported `error: missing
/// <file.kupl> argument` — the real path was silently swallowed as `-o`'s
/// phantom "value" — and `kupl run foo.kupl -o bogus_junk` ran `foo.kupl`
/// cleanly at exit 0, `-o bogus_junk` never examined, the EXACT "extra
/// argument silently dropped" shape PR-it697/PR-it798 already close for every
/// OTHER unrecognized token on these subcommands. `args[0]` (the subcommand
/// word itself, already skipped by `i = 1` below) is checked to gate the
/// special-case to just the three subcommands that genuinely accept `-o` —
/// on any other subcommand `-o` is now just an ordinary token, correctly
/// becoming the path or triggering "unexpected extra argument" like anything
/// else.
fn find_path_arg(args: &[String]) -> Result<&str, String> {
    let accepts_o = matches!(args.first().map(String::as_str), Some("native") | Some("build") | Some("bundle"));
    let mut path: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break; // everything after this belongs to the program, not kupl
        }
        if accepts_o && a == "-o" {
            i += 2; // the flag AND its value, as a unit
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        match path {
            None => path = Some(a),
            Some(_) => return Err(format!("unexpected extra argument `{a}`")),
        }
        i += 1;
    }
    path.ok_or_else(|| "missing <file.kupl> argument".to_string())
}

fn with_path(args: &[String], f: impl Fn(&str) -> i32) -> i32 {
    match find_path_arg(args) {
        Ok(path) => f(path),
        Err(msg) => {
            eprintln!("error: {msg}");
            2
        }
    }
}

fn with_file(args: &[String], f: impl Fn(&str, &str) -> i32) -> i32 {
    let path = match find_path_arg(args) {
        Ok(path) => path,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 2;
        }
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
    use super::{build_module, find_path_arg, valid_project_name, with_file, USAGE};

    /// A REAL bug found+fixed (production-hardening PR-it974): `USAGE`'s
    /// banner used to hardcode a literal "(v0.2)", stale since the crate's
    /// own version moved on -- `kupl --help`/a bare `kupl`/any usage error
    /// showed a DIFFERENT version than `kupl version` (which reads
    /// `env!("CARGO_PKG_VERSION")`), an internal inconsistency in the
    /// shipped binary's own output, live-confirmed before fixing. Locks in
    /// that the banner always matches the crate's actual version, built via
    /// the SAME `env!("CARGO_PKG_VERSION")` `kupl version` itself uses, so
    /// this can never silently drift out of sync again.
    #[test]
    fn usage_banner_version_matches_the_crate_version() {
        assert!(
            USAGE.contains(&format!("(v{})", env!("CARGO_PKG_VERSION"))),
            "USAGE's banner must report the crate's ACTUAL version, not a stale hardcoded one: {USAGE:?}"
        );
    }

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

    /// A REAL CLI-scripting correctness gap (PR-it772, found by an Explore
    /// survey, agentId aca5b82689fe978bd, and independently live-verified via
    /// `kupl --help; echo $?` before implementing): `kupl --help`/`-h`/`help`
    /// used to fall into the SAME catch-all `_` arm as a genuinely invalid
    /// invocation, sharing exit code 2 -- indistinguishable to a script that
    /// checks the exit code to confirm the binary is sane, and contrary to the
    /// universal CLI convention (git, cargo, curl, npm, docker, ...) that an
    /// EXPLICITLY requested help screen is a successful invocation (exit 0).
    /// Deliberately narrow in scope: a bare `kupl` (no subcommand at all) and
    /// a genuinely unrecognized subcommand are more defensibly "the user
    /// probably made a mistake," so BOTH still correctly return exit 2,
    /// unchanged -- locked in here alongside the fix so a future change can't
    /// silently widen the scope to bare/bogus invocations without noticing.
    #[test]
    fn explicit_help_exits_zero_but_a_bare_or_unrecognized_invocation_still_exits_two() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet (e.g. a lib-only build) -- nothing to test
        }
        let code = |args: &[&str]| -> Option<i32> {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs").status.code()
        };
        for flag in ["--help", "-h", "help"] {
            assert_eq!(code(&[flag]), Some(0), "kupl {flag} must exit 0 (explicit help request)");
        }
        assert_eq!(code(&[]), Some(2), "bare `kupl` with no subcommand must still exit 2");
        assert_eq!(code(&["definitely-not-a-real-subcommand"]), Some(2), "an unrecognized subcommand must still exit 2");
    }

    /// A REAL missing-feature gap found+closed (production-hardening PR-it774,
    /// an Explore survey finding, agentId aca5b82689fe978bd, reframed from the
    /// survey's own "broken CI gate" framing to "a missing feature" -- `--check`
    /// was never documented or implemented in the first place, USAGE only ever
    /// listed `[--write]`): `kupl fmt <file.kupl> --check` fell through to the
    /// SAME default print-and-exit-0 path as no flag at all, so a CI pipeline
    /// wired to gate on it (the `rustfmt --check`/`prettier --check` convention)
    /// would NEVER fail, regardless of whether the file was canonically
    /// formatted. Added a real `--check` mode: exits 0 (printing `ok: {file}`)
    /// when the file's current content already matches the formatter's own
    /// canonical output, exits 1 (printing `would reformat: {file}`, matching
    /// this project's OWN existing convention -- `run.rs::check_cmd`'s
    /// `has_errors` branch ALSO uses exit 1, so "needs reformatting" and "has a
    /// parse/load error" both consistently signal "not clean") when it doesn't
    /// -- and NEVER writes to the file either way. `--write` and `--check`
    /// together is genuinely ambiguous (write, or only report?) -- rejected as
    /// a usage error (exit 2) rather than silently picking one.
    #[test]
    fn fmt_check_exits_zero_when_canonical_one_when_not_and_never_writes_the_file() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let tmp = std::env::temp_dir().join(format!("kupl_it774_fmt_check_{}.kupl", std::process::id()));

        // a file already in the formatter's own canonical form: exit 0, `ok: ...`.
        let adventure = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/adventure.kupl");
        let canon = run(&["fmt", adventure.to_str().unwrap()]).stdout; // the formatter's own output IS canonical by definition
        std::fs::write(&tmp, &canon).unwrap();
        let ok = run(&["fmt", tmp.to_str().unwrap(), "--check"]);
        assert_eq!(ok.status.code(), Some(0), "{ok:?}");
        assert!(String::from_utf8_lossy(&ok.stdout).contains("ok: "), "{ok:?}");
        assert_eq!(std::fs::read(&tmp).unwrap(), canon, "--check must never modify the file");

        // a genuinely non-canonical file: exit 1, `would reformat: ...`, unchanged on disk.
        let dirty = b"fun   main( )  {\nprint(\"hi\")\n}\n".to_vec();
        std::fs::write(&tmp, &dirty).unwrap();
        let bad = run(&["fmt", tmp.to_str().unwrap(), "--check"]);
        assert_eq!(bad.status.code(), Some(1), "{bad:?}");
        assert!(String::from_utf8_lossy(&bad.stderr).contains("would reformat: "), "{bad:?}");
        assert_eq!(std::fs::read(&tmp).unwrap(), dirty, "--check must never modify the file, even when it disagrees");

        // --check and --write together: rejected as a usage error, file still untouched.
        let both = run(&["fmt", tmp.to_str().unwrap(), "--check", "--write"]);
        assert_eq!(both.status.code(), Some(2), "{both:?}");
        assert_eq!(std::fs::read(&tmp).unwrap(), dirty, "a rejected combination must not write either");

        let _ = std::fs::remove_file(&tmp);
    }

    /// A REAL, live-confirmed DATA-LOSS bug found+fixed (production-hardening
    /// PR-it837): a `Float`/`F32` literal whose magnitude overflows to
    /// infinity (`1e400`, `1e40f32`, ...) is silently accepted by the lexer
    /// with ZERO diagnostic -- the program runs fine, printing `inf`. But
    /// `fmt::format_program` renders the resulting non-finite value via
    /// `Display`, producing the bare text `inf` -- NOT valid KUPL syntax (no
    /// `inf`/`nan` literal form exists; it re-lexes as an ordinary
    /// identifier, syntactically fine but an "unknown name" once the checker
    /// runs, K0240). `kupl fmt --write` used to overwrite the file with this
    /// broken text UNCONDITIONALLY -- no backup, no validity check --
    /// PERMANENTLY destroying the original, valid source with no way to
    /// recover it (confirmed live before this fix: the file's content after
    /// `--write` was literally unparseable, exit-code-1-on-every-subsequent-
    /// `kupl run`). Fixed by recompiling the freshly-formatted text through
    /// the FULL pipeline (`run::compile`, not a bare re-parse -- `inf` as an
    /// identifier parses cleanly, only the checker catches it) before
    /// writing, refusing (exit 1, clear error, file left byte-for-byte
    /// untouched) if it doesn't come back clean.
    #[test]
    fn fmt_write_refuses_to_corrupt_the_file_when_formatting_an_overflowing_float_literal() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let tmp = std::env::temp_dir().join(format!("kupl_it837_fmt_write_{}.kupl", std::process::id()));
        let original = b"fun main() uses io {\n    let x: Float = 1e400\n    print(\"{x}\")\n}\n".to_vec();
        std::fs::write(&tmp, &original).unwrap();

        // sanity: the ORIGINAL file is valid and runs fine before touching `fmt` at all.
        let ran = run(&["run", tmp.to_str().unwrap()]);
        assert_eq!(ran.status.code(), Some(0), "the original 1e400 literal must run fine: {ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "inf");

        let written = run(&["fmt", tmp.to_str().unwrap(), "--write"]);
        assert_eq!(written.status.code(), Some(1), "must refuse to write, not silently corrupt: {written:?}");
        assert!(
            String::from_utf8_lossy(&written.stderr).contains("refusing to overwrite"),
            "{written:?}"
        );
        assert_eq!(std::fs::read(&tmp).unwrap(), original, "the file must be BYTE-FOR-BYTE untouched, not corrupted");

        // the F32 variant shares the identical mechanism.
        let tmp_f32 = std::env::temp_dir().join(format!("kupl_it837_fmt_write_f32_{}.kupl", std::process::id()));
        let original_f32 = b"fun main() uses io { let x = 1e40f32 ; print(\"{x}\") }\n".to_vec();
        std::fs::write(&tmp_f32, &original_f32).unwrap();
        let written_f32 = run(&["fmt", tmp_f32.to_str().unwrap(), "--write"]);
        assert_eq!(written_f32.status.code(), Some(1), "{written_f32:?}");
        assert_eq!(std::fs::read(&tmp_f32).unwrap(), original_f32, "F32 case must also be untouched");

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&tmp_f32);
    }

    /// A REAL, live-confirmed bug found+fixed (production-hardening PR-it889,
    /// an Explore survey finding, independently re-verified live before
    /// implementing): the SAME PR-it837 safety net `--write` above (and
    /// `lsp.rs::resolve_formatting`) already had was never applied to the
    /// PLAIN (no-flag) `kupl fmt` print path. A pathological string-
    /// interpolation input -- a literal `{{` inside an interpolated
    /// conditional expression, tripping `reindent_inline`'s naive per-line
    /// brace-count heuristic into its raw-multi-line-block fallback --
    /// formatted to text with literal newlines spliced into what was a
    /// single-line string interpolation, syntactically INVALID (7 cascading
    /// parse errors on re-check), printed unconditionally at exit 0 with
    /// zero diagnostic. Confirmed live before this fix: `kupl fmt` on this
    /// exact input printed the broken text at exit 0, while `kupl fmt
    /// --write` on the SAME input already correctly refused. Worse than the
    /// write-path gap PR-it837 closed: `kupl fmt file.kupl > file.kupl`, a
    /// natural shell-redirection workflow, has the shell TRUNCATE the
    /// source file before this command even runs -- the file-level
    /// round-trip guard on `--write` never gets a chance to protect that
    /// usage either. Fixed identically: recompile the freshly-formatted
    /// text through the full pipeline and refuse to print it if it doesn't
    /// come back clean.
    #[test]
    fn fmt_plain_print_refuses_to_emit_invalid_output_from_a_brace_counting_heuristic_gap() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let tmp = std::env::temp_dir().join(format!("kupl_it889_fmt_plain_{}.kupl", std::process::id()));
        let original =
            b"fun main() uses io {\n    var a = true\n    print(\"outer {if a { \"{{\" } else { \"y\" }} end\")\n}\n".to_vec();
        std::fs::write(&tmp, &original).unwrap();

        // sanity: the ORIGINAL file is valid and runs fine before touching `fmt` at all.
        let ran = run(&["run", tmp.to_str().unwrap()]);
        assert_eq!(ran.status.code(), Some(0), "the original program must run fine: {ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "outer { end");

        let printed = run(&["fmt", tmp.to_str().unwrap()]);
        assert_eq!(printed.status.code(), Some(1), "must refuse to print invalid output: {printed:?}");
        assert!(
            String::from_utf8_lossy(&printed.stderr).contains("refusing to print"),
            "{printed:?}"
        );
        assert!(printed.stdout.is_empty(), "must not print anything on stdout when refusing: {printed:?}");

        let _ = std::fs::remove_file(&tmp);
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

    /// TWO REAL, previously-silent bugs in `with_path`/`with_file`'s shared path-
    /// finding logic, fixed together (production-hardening PR-it697), across the 9
    /// subcommands routed through it (run/dis/native/manifest/build/bundle/test/
    /// check/fmt): (1) `-o` (a TWO-token flag `build`/`bundle`/`native` accept)
    /// appearing BEFORE the path -- a natural flag ordering -- was itself
    /// misidentified AS the path, since it doesn't start with `--`. (2) a genuinely
    /// unexpected SECOND positional argument (a typo, a leftover argument from a
    /// copy-pasted command) was silently DROPPED with zero diagnostic, running the
    /// FIRST file and silently ignoring the rest.
    #[test]
    fn find_path_arg_skips_the_o_flag_pair_and_rejects_a_genuine_extra_argument() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // `-o` BEFORE the path is correctly skipped as a flag+value PAIR, not
        // mistaken for the path itself.
        assert_eq!(find_path_arg(&s(&["build", "-o", "out.kx", "foo.kupl"])), Ok("foo.kupl"));
        // ...and still works in its more common position, AFTER the path.
        assert_eq!(find_path_arg(&s(&["build", "foo.kupl", "-o", "out.kx"])), Ok("foo.kupl"));
        // a `--`-prefixed boolean flag (no value) in either position is unaffected.
        assert_eq!(find_path_arg(&s(&["native", "--keep-c", "foo.kupl"])), Ok("foo.kupl"));
        assert_eq!(find_path_arg(&s(&["native", "--keep-c", "-o", "out", "foo.kupl"])), Ok("foo.kupl"));

        // a genuinely unexpected SECOND positional argument is a clean error, not
        // silently dropped -- whether it's a plausible typo or a second real path.
        assert_eq!(
            find_path_arg(&s(&["run", "foo.kupl", "extra_typo"])),
            Err("unexpected extra argument `extra_typo`".to_string())
        );
        assert_eq!(
            find_path_arg(&s(&["run", "foo.kupl", "bar.kupl"])),
            Err("unexpected extra argument `bar.kupl`".to_string())
        );
        // an ordinary single path is, of course, entirely unaffected.
        assert_eq!(find_path_arg(&s(&["run", "foo.kupl"])), Ok("foo.kupl"));
        // no path at all is the pre-existing missing-argument error, unchanged.
        assert_eq!(find_path_arg(&s(&["run"])), Err("missing <file.kupl> argument".to_string()));
    }

    /// A REAL, previously-silent bug (production-hardening PR-it881, found
    /// while re-auditing this function's own carried-forward lead): `-o`
    /// pair-skipping applied to EVERY subcommand, not just the three
    /// (`native`/`build`/`bundle`) that actually accept `-o` -- this
    /// function's own doc comment already said so, but the code never
    /// enforced it. On any subcommand that doesn't accept `-o`, the token
    /// must be treated like any other ordinary argument: the path if none is
    /// set yet, or a clean "unexpected extra argument" error otherwise.
    #[test]
    fn find_path_arg_only_special_cases_o_for_subcommands_that_accept_it() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Confirmed live before this fix: the real path was silently
        // swallowed as `-o`'s phantom "value", reporting a misleading
        // missing-argument error even though a path WAS given. Post-fix,
        // `-o` is just an ordinary token on `run` -- it becomes the (bogus)
        // path candidate itself, and the real file correctly surfaces as the
        // genuine extra argument, matching this function's existing
        // two-positional-arguments convention.
        assert_eq!(
            find_path_arg(&s(&["run", "-o", "foo.kupl"])),
            Err("unexpected extra argument `foo.kupl`".to_string())
        );
        // Confirmed live before this fix: this ran clean at exit 0, `-o
        // bogus_junk` never examined -- must now be a clean error, matching
        // every other unrecognized extra argument on this subcommand.
        assert_eq!(
            find_path_arg(&s(&["run", "foo.kupl", "-o", "bogus_junk"])),
            Err("unexpected extra argument `-o`".to_string())
        );
        // the SAME shape on `dis`/`manifest`/`test`/`check`/`fmt`/pkg's own
        // `tree`/`lock`/`fetch` sub-subcommands (all routed through this
        // function with none of them accepting `-o`).
        assert_eq!(
            find_path_arg(&s(&["dis", "foo.kupl", "-o", "bogus_junk"])),
            Err("unexpected extra argument `-o`".to_string())
        );
        assert_eq!(
            find_path_arg(&s(&["tree", "foo.kupl", "-o", "bogus_junk"])),
            Err("unexpected extra argument `-o`".to_string())
        );
        // meanwhile the three subcommands that DO accept `-o` are entirely
        // unaffected -- re-confirming the existing pair-skip behavior still
        // works post-fix.
        assert_eq!(find_path_arg(&s(&["native", "-o", "out", "foo.kupl"])), Ok("foo.kupl"));
    }

    /// A REAL, previously-silent bug (production-hardening PR-it798, found
    /// while verifying an unrelated fix): a bare `--` used to be treated
    /// like any other `--foo`-style flag -- skipped, with scanning
    /// continuing past it -- instead of the documented separator meaning
    /// "everything after this belongs to the PROGRAM, not to kupl's own
    /// CLI parsing" (`kupl run prog.kupl -- a b c`, per `program_args()`'s
    /// own doc comment and `examples/cli.kupl`'s usage instructions).
    /// Confirmed live before the fix: `kupl run examples/cli.kupl -- Ada
    /// Grace` failed with `error: unexpected extra argument \`Ada\``.
    #[test]
    fn find_path_arg_treats_a_bare_separator_as_a_hard_stop() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // args after `--` are the PROGRAM's, not extra positional arguments.
        assert_eq!(find_path_arg(&s(&["run", "foo.kupl", "--", "Ada", "Grace"])), Ok("foo.kupl"));
        // even a `--`-prefixed-looking program arg is left alone once past the separator.
        assert_eq!(find_path_arg(&s(&["run", "foo.kupl", "--", "--not-a-kupl-flag"])), Ok("foo.kupl"));
        // `--` with nothing after it is still just a normal, valid separator.
        assert_eq!(find_path_arg(&s(&["run", "foo.kupl", "--"])), Ok("foo.kupl"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it864, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// the SAME "unexpected extra argument silently dropped" shape
    /// `find_path_arg`'s own fix (production-hardening PR-it697) already
    /// closed for every OTHER file-taking subcommand -- `diff`/`context`
    /// take their two positional arguments via a raw `match (args.get(1),
    /// args.get(2))`, never even LOOKING at `args.get(3)`, so a genuinely
    /// unexpected THIRD positional argument (a plausible typo, or a leftover
    /// argument from a copy-pasted command) was silently IGNORED with zero
    /// diagnostic. Live-confirmed BEFORE this fix: `kupl diff old.kupl
    /// new.kupl extra_typo` ran the diff cleanly, `extra_typo` never
    /// examined or mentioned anywhere.
    #[test]
    fn diff_and_context_reject_a_genuine_extra_argument_instead_of_silently_dropping_it() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-diffctx-extra-arg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.kupl");
        let b = dir.join("b.kupl");
        std::fs::write(&a, "fun main() uses io {\n    print(\"hi\")\n}\n").unwrap();
        std::fs::write(&b, "fun main() uses io {\n    print(\"hi2\")\n}\n").unwrap();
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };

        let d = run(&["diff", a.to_str().unwrap(), b.to_str().unwrap(), "extra_typo"]);
        assert_eq!(d.status.code(), Some(2), "{d:?}");
        assert!(
            String::from_utf8_lossy(&d.stderr).contains("unexpected extra argument `extra_typo`"),
            "{d:?}"
        );

        let c = run(&["context", a.to_str().unwrap(), "main", "extra_typo"]);
        assert_eq!(c.status.code(), Some(2), "{c:?}");
        assert!(
            String::from_utf8_lossy(&c.stderr).contains("unexpected extra argument `extra_typo`"),
            "{c:?}"
        );

        // sanity: the ordinary two-argument form is entirely unaffected.
        let d_ok = run(&["diff", a.to_str().unwrap(), b.to_str().unwrap()]);
        assert_eq!(d_ok.status.code(), Some(1), "a real semantic change must still report exit 1: {d_ok:?}");
        let c_ok = run(&["context", a.to_str().unwrap(), "main"]);
        assert_eq!(c_ok.status.code(), Some(0), "{c_ok:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it1062, a background
    /// close-read survey finding): the SAME "unexpected extra argument
    /// silently dropped" shape the test above locks in for `diff`/`context`
    /// (PR-it864) also applied to `new`, which took its single positional
    /// argument via a raw `args.get(1)`, never checking `args.get(2)` at
    /// all. Live-confirmed BEFORE this fix: `kupl new demo extra_arg`
    /// created `demo/` cleanly, exit 0, `extra_arg` never examined.
    #[test]
    fn new_rejects_a_genuine_extra_argument_instead_of_silently_dropping_it() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-new-extra-arg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str], cwd: &std::path::Path| -> std::process::Output {
            std::process::Command::new(&bin).args(args).current_dir(cwd).output().expect("kupl runs")
        };

        let n = run(&["new", "demo", "extra_arg"], &dir);
        assert_eq!(n.status.code(), Some(2), "{n:?}");
        assert!(
            String::from_utf8_lossy(&n.stderr).contains("unexpected extra argument `extra_arg`"),
            "{n:?}"
        );
        assert!(!dir.join("demo").exists(), "a rejected `new` invocation must not create the project dir");

        // sanity: the ordinary single-argument form is entirely unaffected.
        let n_ok = run(&["new", "demo"], &dir);
        assert_eq!(n_ok.status.code(), Some(0), "{n_ok:?}");
        assert!(dir.join("demo").join("kupl.toml").exists(), "a valid `new` invocation must still scaffold the project");

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A CRITICAL data-loss bug found+fixed (production-hardening PR-it781, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `bundle`'s default output path is the source path with
    /// `.kupl` trimmed off, a no-op when the source file doesn't literally
    /// end in `.kupl` -- so the computed output SILENTLY COLLIDED WITH THE
    /// SOURCE FILE and overwrote it with a compiled executable, no warning,
    /// no confirmation, permanently. Confirmed live before this fix: `kupl
    /// bundle foo` (source file literally named `foo`) destroyed `foo`,
    /// replacing it with a Mach-O executable, exit code 0. `build` always
    /// appends `.kx` so its DEFAULT path can never collide by construction,
    /// but an explicit `-o <source-path>` hits the identical collision for
    /// either -- covers both the bundle-default and the build-explicit-`-o`
    /// shapes of the same underlying bug.
    #[test]
    fn bundle_and_build_refuse_to_overwrite_the_source_file() {
        let dir = std::env::temp_dir().join(format!("kupl-owos-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "fun main() uses io {\n    print(\"hi\")\n}\n";

        // `bundle`, extensionless source -> the computed default collides.
        let extensionless = dir.join("foo");
        std::fs::write(&extensionless, src).unwrap();
        let p = extensionless.to_str().unwrap().to_string();
        let code = build_module(&[], &p, true);
        assert_eq!(code, 1, "bundle must refuse rather than overwrite the source");
        assert_eq!(std::fs::read_to_string(&extensionless).unwrap(), src, "source must be untouched");

        // `build`, a normal `.kupl` file but an explicit `-o` naming the source itself.
        let named = dir.join("selfout.kupl");
        std::fs::write(&named, src).unwrap();
        let p2 = named.to_str().unwrap().to_string();
        let args = vec!["build".to_string(), p2.clone(), "-o".to_string(), p2.clone()];
        let code2 = build_module(&args, &p2, false);
        assert_eq!(code2, 1, "an explicit -o matching the source must also be refused");
        assert_eq!(std::fs::read_to_string(&named).unwrap(), src, "source must be untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL bug found+fixed (production-hardening PR-it862, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// a trailing `-o` with no following value (a plausible fat-fingered
    /// mistake, or an empty-string shell-expansion that dropped the intended
    /// value from argv) used to be treated IDENTICALLY to `-o` being absent
    /// entirely -- `args.get(i + 1)` returns `None` either way -- silently
    /// falling back to the DEFAULT output path instead of erroring.
    /// Live-confirmed BEFORE this fix: `kupl build foo.kupl -o` silently
    /// overwrote a pre-existing, unrelated `foo.kx` at the default path with
    /// zero diagnostic and exit code 0, even though the user explicitly
    /// asked `-o` to control the output path. The IDENTICAL shape also
    /// existed in `run::native`'s own `-o`-value extraction, fixed together.
    #[test]
    fn a_trailing_o_flag_with_no_value_is_a_clean_error_not_a_silent_default_fallback() {
        let dir = std::env::temp_dir().join(format!("kupl-oflag-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "fun main() uses io {\n    print(\"hi\")\n}\n";
        let source = dir.join("foo.kupl");
        std::fs::write(&source, src).unwrap();
        let p = source.to_str().unwrap().to_string();

        // a pre-existing file at the DEFAULT output path must survive untouched.
        let default_out = dir.join("foo.kx");
        std::fs::write(&default_out, "PRE-EXISTING-DATA").unwrap();

        let args = vec!["build".to_string(), p.clone(), "-o".to_string()];
        let code = build_module(&args, &p, false);
        assert_eq!(code, 2, "a trailing -o with no value must be a clean usage error");
        assert_eq!(
            std::fs::read_to_string(&default_out).unwrap(),
            "PRE-EXISTING-DATA",
            "the default output path must NOT be silently overwritten"
        );

        // sanity: a genuine `-o <value>` still works normally (no regression).
        let real_out = dir.join("real.kx");
        let args2 = vec!["build".to_string(), p.clone(), "-o".to_string(), real_out.to_str().unwrap().to_string()];
        let code2 = build_module(&args2, &p, false);
        assert_eq!(code2, 0, "a genuine -o value must still work");
        assert!(real_out.exists());

        // sanity: NO -o at all still falls back to the default cleanly (no regression).
        std::fs::remove_file(&default_out).unwrap();
        let code3 = build_module(&[p.clone()], &p, false);
        assert_eq!(code3, 0, "omitting -o entirely must still default normally");
        assert!(default_out.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, LIVE-CONFIRMED silent-wrong-behavior bug found+fixed
    /// (production-hardening PR-it999, the SECOND finding from the same
    /// main.rs CLI-dispatch survey that produced PR-it998's kx-fast-path
    /// fix): `args.iter().position(...)` always returns the FIRST `-o`
    /// occurrence -- a REPEATED `-o` was silently discarded with ZERO
    /// diagnostic. Live-confirmed BEFORE this fix: `kupl build foo.kupl -o
    /// first.kx -o second.kx` silently produced ONLY `first.kx`, exit 0,
    /// `second.kx` never created, no error/warning anywhere. Rejected
    /// cleanly instead, matching this file's own established convention of
    /// rejecting ambiguous/duplicate input rather than silently picking
    /// one. The IDENTICAL shape also existed in `run::native`'s own `-o`
    /// extraction, fixed together.
    #[test]
    fn a_repeated_o_flag_is_a_clean_error_not_a_silent_first_occurrence_win() {
        let dir = std::env::temp_dir().join(format!("kupl-dup-oflag-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "fun main() uses io {\n    print(\"hi\")\n}\n";
        let source = dir.join("foo.kupl");
        std::fs::write(&source, src).unwrap();
        let p = source.to_str().unwrap().to_string();

        let first = dir.join("first.kx");
        let second = dir.join("second.kx");
        let args = vec![
            "build".to_string(),
            p.clone(),
            "-o".to_string(),
            first.to_str().unwrap().to_string(),
            "-o".to_string(),
            second.to_str().unwrap().to_string(),
        ];
        let code = build_module(&args, &p, false);
        assert_eq!(code, 2, "a repeated -o must be a clean usage error");
        assert!(!first.exists(), "neither -o value must be silently used");
        assert!(!second.exists(), "neither -o value must be silently used");

        // sanity: a single `-o <value>` still works normally (no regression).
        let args2 = vec!["build".to_string(), p.clone(), "-o".to_string(), first.to_str().unwrap().to_string()];
        let code2 = build_module(&args2, &p, false);
        assert_eq!(code2, 0, "a single -o value must still work");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL usability bug found+fixed (production-hardening PR-it782, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `build`/`bundle`/`native` all tried to parse a `.kx`
    /// file as `.kupl` SOURCE, unlike `run`/`dis` which already special-case
    /// it -- the lexer walked the raw bytecode byte-by-byte, emitting one
    /// `K0001` per non-token byte. Confirmed live before this fix: `kupl
    /// native qux.kx` (a genuine compiled module) printed 1455 lines of
    /// garbage instead of one clean, actionable error. Subprocess test
    /// (matching this file's own `fmt_check_exits_zero_...` convention)
    /// since the crux of the bug is the OUTPUT VOLUME, not just the exit
    /// code -- asserts the error is a SINGLE line, not thousands.
    #[test]
    fn build_bundle_native_reject_a_kx_file_with_one_clean_line_not_thousands_of_lexer_errors() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-kx-input-guard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("qux.kupl");
        std::fs::write(&src, "fun main() uses io {\n    print(\"hi\")\n}\n").unwrap();
        let kx = dir.join("qux.kx");
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let built = run(&["build", src.to_str().unwrap(), "-o", kx.to_str().unwrap()]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");

        for cmd in ["native", "build", "bundle"] {
            let out = run(&[cmd, kx.to_str().unwrap()]);
            assert_eq!(out.status.code(), Some(1), "`{cmd}` on a .kx file: {out:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                stderr.lines().count(),
                1,
                "`{cmd}` on a .kx file must report ONE clean line, not a lexer-error dump: {stderr:?}"
            );
            assert!(stderr.contains("already compiled bytecode"), "`{cmd}`: {stderr:?}");
        }

        // `run`/`dis` must remain unaffected -- they already handle `.kx` input directly.
        let ran = run(&["run", kx.to_str().unwrap()]);
        assert_eq!(ran.status.code(), Some(0), "{ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "hi");
        let dis = run(&["dis", kx.to_str().unwrap()]);
        assert_eq!(dis.status.code(), Some(0), "{dis:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, LIVE-CONFIRMED silent-wrong-behavior bug found+fixed
    /// (production-hardening PR-it998, a close-read survey of this file's
    /// CLI dispatch): `kupl run`'s direct `.kx` fast path used to find its
    /// path argument via a bare `args.iter().skip(1).find(|a|
    /// !a.starts_with("--"))` scan, completely bypassing `find_path_arg`'s
    /// hardened single-positional-argument enforcement -- so a genuinely
    /// unexpected SECOND positional argument was silently ignored (the SAME
    /// bug class `find_path_arg`'s own PR-it697/PR-it864/PR-it798 fixes
    /// already closed for every OTHER subcommand), and a token AFTER a
    /// literal `--` separator was still treated as a candidate path,
    /// violating the documented "everything after `--` belongs to the
    /// PROGRAM" contract. Live-confirmed BEFORE this fix: `kupl run
    /// report.kx realprog.kupl` silently ran ONLY `report.kx`, never even
    /// examining `realprog.kupl`; `kupl run -- report.kx` also silently ran
    /// `report.kx`. Fixed by routing the `.kx` detection through
    /// `find_path_arg` (the SAME helper every other file-taking subcommand
    /// already uses) instead of a separate, cruder scan.
    #[test]
    fn kx_fast_path_rejects_an_extra_argument_and_honors_the_separator_like_every_sibling_subcommand() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-kx-fast-path-extra-arg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("report998.kupl");
        std::fs::write(&src, "fun main() uses io {\n    print(\"report\")\n}\n").unwrap();
        let kx = dir.join("report998.kx");
        let other = dir.join("realprog998.kupl");
        std::fs::write(&other, "fun main() uses io {\n    print(\"realprog\")\n}\n").unwrap();
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };
        let built = run(&["build", src.to_str().unwrap(), "-o", kx.to_str().unwrap()]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");

        // a genuine second positional argument must be cleanly rejected, not silently dropped.
        let extra = run(&["run", kx.to_str().unwrap(), other.to_str().unwrap()]);
        assert_eq!(extra.status.code(), Some(2), "{extra:?}");
        assert!(
            String::from_utf8_lossy(&extra.stderr).contains("unexpected extra argument"),
            "{extra:?}"
        );

        // a token after `--` belongs to the program, not to kupl -- never a candidate path.
        let sep = run(&["run", "--", kx.to_str().unwrap()]);
        assert_eq!(sep.status.code(), Some(2), "{sep:?}");
        assert!(String::from_utf8_lossy(&sep.stderr).contains("missing <file.kupl> argument"), "{sep:?}");

        // the ordinary, single-argument `.kx` fast path is unaffected.
        let ok = run(&["run", kx.to_str().unwrap()]);
        assert_eq!(ok.status.code(), Some(0), "{ok:?}");
        assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), "report");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL usability bug found+fixed (production-hardening PR-it782, an
    /// Explore survey finding, independently re-verified live before
    /// implementing): `kupl pkg tree|lock|fetch` read their path argument via
    /// raw `args.get(2)`, unlike every OTHER file-taking subcommand (which
    /// all route through `find_path_arg`, skipping `--flag`s and an `-o
    /// <value>` pair wherever they appear) -- so a flag placed BEFORE the
    /// path was misread as the path itself. Confirmed live before this fix:
    /// `kupl pkg tree --json qux.kupl` reported `error: entry --json: No
    /// such file or directory`, never even looking at `qux.kupl`.
    #[test]
    fn pkg_tree_accepts_a_flag_before_the_path_like_every_sibling_subcommand() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-pkg-flag-order-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("qux.kupl");
        std::fs::write(&src, "fun main() uses io {\n    print(\"hi\")\n}\n").unwrap();
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new(&bin).args(args).output().expect("kupl runs")
        };

        // flag BEFORE the path -- must resolve the path correctly, not misread the flag as it.
        let before = run(&["pkg", "tree", "--bogus-flag", src.to_str().unwrap()]);
        assert_eq!(before.status.code(), Some(0), "{before:?}");
        assert!(String::from_utf8_lossy(&before.stdout).contains("no dependencies"), "{before:?}");

        // flag AFTER the path -- the pre-existing ordering must keep working too.
        let after = run(&["pkg", "tree", src.to_str().unwrap(), "--bogus-flag"]);
        assert_eq!(after.status.code(), Some(0), "{after:?}");

        // no path at all -- a clean usage error, not a panic.
        let missing = run(&["pkg", "tree"]);
        assert_eq!(missing.status.code(), Some(2), "{missing:?}");

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A REAL bug found+fixed (production-hardening PR-it758): the test
    /// immediately above proves a redefined COMPONENT's own decl is safely
    /// frozen per-instance -- but a plain top-level value whose declared
    /// `type` gets redefined was never protected the same way, because bare
    /// `let`/`var` statements aren't tracked in `defs_items` and so never
    /// get swept into the re-`compile()` pass that catches the component
    /// case. `interp.rs`'s `ExprKind::Field`/`ExprKind::With` both looked up
    /// a field's POSITION in the CURRENT `self.db.ctors` table, then indexed
    /// directly into the OLD value's `fields` Vec at that position -- if a
    /// redefinition grew the field list, that index landed past the stale
    /// value's actual length, a raw Rust `Vec` index panic. Because `main.rs`
    /// runs the whole CLI (including the REPL) on a single worker thread,
    /// this ABORTED THE ENTIRE `kupl repl` PROCESS (exit 101, "internal
    /// compiler error") -- not just the one offending statement, losing the
    /// user's whole interactive session. Confirmed live BEFORE this fix,
    /// both for a field READ (`v.y`) and a field UPDATE (`v with y: 5`).
    /// Fixed by bounds-checking both indexing sites (`fields.get(i)`/
    /// `new_fields.get_mut(i)`) and reporting a clean, catchable panic
    /// instead -- the REPL's own `Err(Flow::Panic{msg,..}) => eprintln!(...)`
    /// handler already turns that into a normal per-statement error, so the
    /// session survives and keeps working afterward (verified below).
    #[test]
    fn redefining_a_type_under_a_live_value_is_a_clean_panic_not_a_process_abort() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "type T = A(x: Int)\n\
                      let v = A(1)\n\
                      type T = A(x: Int, y: Int)\n\
                      v.y\n\
                      v with y: 5\n\
                      print(\"still alive\")\n\
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
        assert!(
            !combined.contains("internal compiler error") && !combined.contains("panicked at"),
            "a redefined type under a live value must never abort the whole REPL process: {combined}"
        );
        assert!(
            stderr.matches("value's shape no longer matches its current definition").count() == 2,
            "both the field READ and the field UPDATE must report the same clean panic: {stderr}"
        );
        assert!(
            stdout.contains("still alive"),
            "the REPL session must survive the panic and keep evaluating later statements: {stdout}"
        );
        assert!(out.status.success(), "the REPL process itself must exit cleanly via :quit, not abort: {out:?}");
    }

    /// A REAL, live-confirmed silent-state-corruption bug found+fixed
    /// (production-hardening PR-it992, an Explore survey finding): see
    /// `repl.rs`'s own `defs_items` doc comment for the full root-cause
    /// analysis. `;` lexes to the same `Newline` statement-terminator token
    /// the parser uses, so `type A = X(v: Int); type B = Y(v: Int)` on ONE
    /// REPL line is legal KUPL that used to be tracked as ONE entry keyed by
    /// BOTH `A` and `B`. Later redefining `A` ALONE (with a wider shape)
    /// used to silently delete the WHOLE original entry -- including `type
    /// B`, never touched -- because the redefinition-by-name filter matched
    /// on ANY shared key. `type B`'s constructor `Y` then vanished from the
    /// session with zero error, and a later `Y(...)` panicked `unknown
    /// name`. Also confirms the ANALOGOUS fix works for redefining the
    /// MIDDLE item of a THREE-item single-line submission (`A`/`B`/`C`,
    /// redefine `B`), and that a genuinely single-item redefinition (the
    /// ORIGINAL, most common shape this whole mechanism exists for) still
    /// works unchanged.
    #[test]
    fn redefining_one_item_from_a_multi_item_repl_line_does_not_delete_its_untouched_siblings() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "type A992 = X992(v: Int); type B992 = Y992(v: Int)\n\
                      Y992(v: 5)\n\
                      type A992 = X992(v: Int, w: Int)\n\
                      Y992(v: 5)\n\
                      type P992 = Q992(v: Int); type R992 = S992(v: Int); type T992 = U992(v: Int)\n\
                      Q992(v: 1)\n\
                      S992(v: 2)\n\
                      U992(v: 3)\n\
                      type R992 = S992(v: Int, w: Int)\n\
                      Q992(v: 1)\n\
                      S992(v: 2, w: 9)\n\
                      U992(v: 3)\n\
                      fun add992(a: Int, b: Int) -> Int { a + b }\n\
                      add992(2, 3)\n\
                      fun add992(a: Int, b: Int) -> Int { a + b + 100 }\n\
                      add992(2, 3)\n\
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
        assert!(
            !stderr.contains("unknown name"),
            "an untouched sibling item must survive redefining ANOTHER item from the same original \
             multi-item submission: {stderr}"
        );
        assert!(stdout.contains("Y992(5)"), "B992's constructor must still work before A992 is redefined: {stdout}");
        assert!(
            stdout.matches("Y992(5)").count() == 2,
            "B992's constructor must STILL work identically after redefining unrelated A992: {stdout}"
        );
        assert!(
            stdout.contains("S992(2, 9)"),
            "the middle item (R992/S992) must pick up its OWN redefinition: {stdout}"
        );
        assert!(
            stdout.matches("Q992(1)").count() == 2 && stdout.matches("U992(3)").count() == 2,
            "the first and last items of a three-item submission must survive redefining the MIDDLE one: {stdout}"
        );
        assert!(stdout.contains("kupl> 5\n"), "add992(2,3) must be 5 before its own redefinition: {stdout}");
        assert!(
            stdout.contains("kupl> 105\n"),
            "add992(2,3) must be 105 after its own (single-item) redefinition: {stdout}"
        );
    }

    /// A REAL bug found+fixed (production-hardening PR-it763, the second
    /// finding from the same survey that produced PR-it762's lockfile
    /// field-escaping fix): `kupl pkg tree`'s drift check only ever asked
    /// "does THIS dependency's hash differ from what the lockfile
    /// recorded" for each dependency in the CURRENT manifest -- it never
    /// checked whether the lockfile itself contained an ORPHANED entry (a
    /// dependency removed from `[dependencies]` without re-running `kupl
    /// pkg lock`), and a brand-new never-locked dependency was
    /// indistinguishable from an already-locked, unchanged one (both fell
    /// through to the same "no marker" branch). Live-confirmed before this
    /// fix: removing a dependency silently made it vanish from `pkg tree`'s
    /// output with zero indication the lockfile was now stale; adding one
    /// produced no signal it had never been locked either. Fixed by (a)
    /// splitting `locked.get(name) == None` out from "locked and unchanged"
    /// with its own `[new, not yet locked]` marker, and (b) diffing
    /// `locked`'s OWN names against the current dependency set to report
    /// orphaned lock entries explicitly.
    #[test]
    fn pkg_tree_flags_both_a_brand_new_unlocked_dependency_and_an_orphaned_lock_entry() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let base = std::env::temp_dir().join(format!("kupl-pkgtree-lockstate-it763-{}", std::process::id()));
        let proj = base.join("proj");
        let dep_a = base.join("depA");
        let dep_b = base.join("depB");
        for d in [&proj, &dep_a, &dep_b] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(dep_a.join("kupl.toml"), "[project]\nname = \"depA\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(dep_a.join("main.kupl"), "pub fun a() -> Int {\n    1\n}\n").unwrap();
        std::fs::write(dep_b.join("kupl.toml"), "[project]\nname = \"depB\"\nentry = \"main.kupl\"\n").unwrap();
        std::fs::write(dep_b.join("main.kupl"), "pub fun b() -> Int {\n    2\n}\n").unwrap();

        let main_kupl = proj.join("main.kupl");
        let main_str = main_kupl.to_str().unwrap();

        // lock a project depending on ONLY depA -- depB has NEVER been locked.
        std::fs::write(
            proj.join("kupl.toml"),
            "[project]\nname = \"proj\"\nentry = \"main.kupl\"\n\n[dependencies]\ndepA = { path = \"../depA\" }\n",
        )
        .unwrap();
        std::fs::write(proj.join("main.kupl"), "use depA\nfun main() {}\n").unwrap();
        assert!(
            std::process::Command::new(&bin).args(["pkg", "lock", main_str]).status().unwrap().success(),
            "pkg lock must succeed"
        );

        // add depB (never locked) alongside the still-locked depA.
        std::fs::write(
            proj.join("kupl.toml"),
            "[project]\nname = \"proj\"\nentry = \"main.kupl\"\n\n\
             [dependencies]\ndepA = { path = \"../depA\" }\ndepB = { path = \"../depB\" }\n",
        )
        .unwrap();
        std::fs::write(proj.join("main.kupl"), "use depA\nuse depB\nfun main() {}\n").unwrap();
        let out = std::process::Command::new(&bin).args(["pkg", "tree", main_str]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("depB") && stdout.contains("[new, not yet locked]"),
            "a brand-new, never-locked dependency must be flagged as such, not silently treated as unchanged: {stdout}"
        );
        assert!(
            stdout.lines().any(|l| l.starts_with("depA ") && !l.contains("[drift]") && !l.contains("[new,")),
            "the already-locked, unchanged depA must show no marker at all: {stdout}"
        );

        // now lock BOTH, then remove depB from the manifest WITHOUT
        // re-locking -> depB's kupl.lock entry is now orphaned.
        assert!(
            std::process::Command::new(&bin).args(["pkg", "lock", main_str]).status().unwrap().success(),
            "pkg lock (both deps) must succeed"
        );
        std::fs::write(
            proj.join("kupl.toml"),
            "[project]\nname = \"proj\"\nentry = \"main.kupl\"\n\n[dependencies]\ndepA = { path = \"../depA\" }\n",
        )
        .unwrap();
        std::fs::write(proj.join("main.kupl"), "use depA\nfun main() {}\n").unwrap();
        let out2 = std::process::Command::new(&bin).args(["pkg", "tree", main_str]).output().unwrap();
        let stdout2 = String::from_utf8_lossy(&out2.stdout);
        assert!(
            stdout2.contains("depB") && stdout2.contains("no longer in kupl.toml"),
            "a dependency removed from the manifest but still present in kupl.lock must be flagged as an orphan: {stdout2}"
        );
        assert!(
            !stdout2.lines().any(|l| l.contains("depB") && l.contains("[drift]")),
            "an orphan must not ALSO be misreported as ordinary drift: {stdout2}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A REAL structural test-coverage gap found+closed (production-hardening
    /// PR-it785, an Explore survey finding into the differential-testing
    /// harness's OWN meta-level coverage, independently re-verified live
    /// before adding this test): `vm.rs`'s own `differential()` helper (the
    /// mechanism behind the vast majority of this project's interp-vs-KVM
    /// byte-identity tests) only ever compares a `probe()` function's
    /// RETURN VALUE -- it never captures or diffs actual printed STDOUT,
    /// since `print()` writes straight to the real process stdout in both
    /// engines with no injectable sink. `run_program_vm` (the function
    /// behind `kupl run --vm`) had, before this test, EXACTLY ONE caller in
    /// the entire codebase -- `main.rs`'s own CLI dispatch -- and was never
    /// invoked by any test at all. So an entire category of observable
    /// program behavior (printed output, its ORDER and COMPLETENESS
    /// relative to panics/`par_map` scheduling/loops) had ZERO differential
    /// coverage anywhere in the suite. INDEPENDENTLY VERIFIED clean before
    /// adding this test (no bug found -- this is a coverage gap being
    /// closed, not a divergence being fixed, per this campaign's own
    /// standing rule for coverage-gap findings): ran `kupl run` vs `kupl
    /// run --vm` on a print+loop+panic program and a `par_map`+print
    /// program (3x, since `par_map`'s real-thread scheduling is the
    /// specific risk case) and found byte-identical stdout+exit code every
    /// time. This test locks that verified-clean state in permanently.
    #[test]
    fn stdout_is_byte_identical_between_the_interpreter_and_the_vm() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-vm-stdout-parity-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run_both = |file: &std::path::Path| -> (std::process::Output, std::process::Output) {
            let interp = std::process::Command::new(&bin).args(["run", file.to_str().unwrap()]).output().unwrap();
            let vm = std::process::Command::new(&bin).args(["run", "--vm", file.to_str().unwrap()]).output().unwrap();
            (interp, vm)
        };

        // prints interleaved with a loop, then a genuine runtime panic mid-program.
        let prints = dir.join("prints.kupl");
        std::fs::write(
            &prints,
            "fun main() uses io {\n    print(\"first\")\n    let xs = [1, 2, 3]\n    \
             for x in xs {\n        print(x)\n    }\n    print(\"before panic\")\n    \
             let bad = 1 / 0\n    print(\"never\")\n}\n",
        )
        .unwrap();
        let (i1, v1) = run_both(&prints);
        assert_eq!(i1.stdout, v1.stdout, "stdout must match up to the panic point");
        assert_eq!(i1.status.code(), v1.status.code(), "the panic exit code must also match");
        assert!(String::from_utf8_lossy(&i1.stdout).contains("before panic"), "sanity: prints before the panic ran");
        assert!(!String::from_utf8_lossy(&i1.stdout).contains("never"), "sanity: the print AFTER the panic must not run");

        // `par_map`'s real-OS-thread scheduling is the specific risk case for
        // print ORDER divergence -- repeat several times, not just once.
        let parmap = dir.join("parmap.kupl");
        std::fs::write(
            &parmap,
            "fun main() uses io {\n    let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]\n    \
             let ys = xs.par_map(fn x { x * 2 })\n    for y in ys {\n        print(y)\n    }\n}\n",
        )
        .unwrap();
        for _ in 0..3 {
            let (i2, v2) = run_both(&parmap);
            assert_eq!(i2.stdout, v2.stdout, "par_map's output order must match between engines");
            assert_eq!(i2.status.code(), Some(0), "{i2:?}");
            assert_eq!(v2.status.code(), Some(0), "{v2:?}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it894,
    /// an Explore survey finding, agentId a7ba91a6862653340, independently
    /// re-verified live before implementing -- see `callargs.rs`'s own doc
    /// comment on `resolve_call_args`/`Resolver` for the full writeup). A
    /// local `let`/lambda-param binding that shadows a top-level function
    /// name resolves correctly for an ORDINARY positional call (matching
    /// `interp.rs`'s real scoping) -- but `callargs.rs`'s named-argument/
    /// trailing-default rewrite used to match a call's callee by IDENTIFIER
    /// TEXT alone, with no scope awareness, so a call to the SHADOWING local
    /// using named arguments or a trailing default was silently rewritten
    /// using the UNRELATED top-level function's own parameter names/order/
    /// defaults -- either a silent WRONG VALUE (zero diagnostics) or a
    /// spurious rejection of otherwise-valid code, identically on interp and
    /// the VM, with `kupl check` reporting nothing.
    #[test]
    fn a_local_binding_that_shadows_a_top_level_fun_name_is_never_confused_with_it_by_the_named_arg_rewrite() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return; // no debug binary built yet -- nothing to test
        }
        let dir = std::env::temp_dir().join(format!("kupl-it894-shadow-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run_both = |file: &std::path::Path| -> (std::process::Output, std::process::Output) {
            let interp = std::process::Command::new(&bin).args(["run", file.to_str().unwrap()]).output().unwrap();
            let vm = std::process::Command::new(&bin).args(["run", "--vm", file.to_str().unwrap()]).output().unwrap();
            (interp, vm)
        };

        // A named-argument call to a LOCAL closure that shadows an unrelated
        // top-level `combine(x, y)` used to silently rewrite `combine(y: 2,
        // x: 5)` into the top-level function's `(x, y)` order and hand THAT
        // to the local closure -- printing `52` (`m=5, n=2`) with exit 0 and
        // zero diagnostics. The correct behavior is the SAME clean K0241
        // ("named arguments are only allowed for constructors and props")
        // that a non-colliding local already gets -- calling a plain local
        // VALUE with named arguments is never valid, shadowing or not.
        let wrong_value = dir.join("wrong_value.kupl");
        std::fs::write(
            &wrong_value,
            "fun combine(x: Int, y: Int) -> Int {\n    x - y\n}\n\
             fun outer() -> Int {\n    let combine = fn(m, n) { m * 10 + n }\n    combine(y: 2, x: 5)\n}\n\
             fun main() uses io {\n    print(to_str(outer()))\n}\n",
        )
        .unwrap();
        let (i1, v1) = run_both(&wrong_value);
        assert_eq!(i1.stdout, v1.stdout, "interp/vm must agree");
        assert_eq!(i1.status.code(), v1.status.code(), "interp/vm exit codes must agree");
        assert_ne!(i1.status.code(), Some(0), "a named-arg call to a shadowing local must be a clean error, not silently succeed with a wrong value");
        assert!(
            String::from_utf8_lossy(&i1.stderr).contains("K0241"),
            "must be the SAME K0241 a non-colliding local gets, not silently resolved: {i1:?}"
        );
        assert!(
            !String::from_utf8_lossy(&i1.stdout).contains('5'),
            "must never print the wrong value `52` computed from the unrelated top-level function's argument order: {i1:?}"
        );

        // The mirror-image failure: omitting a trailing argument on a
        // one-parameter LOCAL closure that shadows an unrelated top-level
        // `greet(name, punctuation = "!")` used to spuriously fail with
        // K0242 ("this function takes 1 argument, 2 given"), because the
        // unrelated top-level function's own trailing default got appended
        // to an otherwise complete, ordinary call.
        let spurious_error = dir.join("spurious_error.kupl");
        std::fs::write(
            &spurious_error,
            "fun greet(name: Str, punctuation: Str = \"!\") -> Str {\n    name + punctuation\n}\n\
             fun outer() -> Str {\n    let greet = fn(n) { \"hi \" + n }\n    greet(\"world\")\n}\n\
             fun main() uses io {\n    print(outer())\n}\n",
        )
        .unwrap();
        let (i2, v2) = run_both(&spurious_error);
        assert_eq!(i2.stdout, v2.stdout, "interp/vm must agree");
        assert_eq!(i2.status.code(), Some(0), "an ordinary complete call to a shadowing local must not be rejected: {i2:?}");
        assert_eq!(v2.status.code(), Some(0), "{v2:?}");
        assert_eq!(String::from_utf8_lossy(&i2.stdout).trim(), "hi world");

        // Sanity/regression: an ORDINARY POSITIONAL call to a shadowing
        // local already resolved correctly before this fix and must be
        // completely unaffected by it (this fix only touches the named-arg/
        // trailing-default rewrite path).
        let positional_ok = dir.join("positional_ok.kupl");
        std::fs::write(
            &positional_ok,
            "fun add(x: Int, y: Int) -> Int {\n    x + y\n}\n\
             fun outer() -> Int {\n    let add = fn(m, n) { m - n }\n    add(1, 100)\n}\n\
             fun main() uses io {\n    print(to_str(outer()))\n}\n",
        )
        .unwrap();
        let (i3, v3) = run_both(&positional_ok);
        assert_eq!(i3.stdout, v3.stdout, "interp/vm must agree");
        assert_eq!(i3.status.code(), Some(0), "{i3:?}");
        assert_eq!(String::from_utf8_lossy(&i3.stdout).trim(), "-99", "an ordinary positional shadowing call must still resolve to the LOCAL closure, unaffected by this fix");

        let _ = std::fs::remove_dir_all(&dir);
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
