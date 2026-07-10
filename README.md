# KUPL
**K Universal Programming Language**

An AI-first, component-oriented, general-purpose programming language with a
complete toolchain: REPL, interpreter, register-based virtual machine, bytecode
compiler, and native machine-code compiler — designed to run efficiently across
CPU, GPU, TPU, and neural engines, with progressive low-level control down to
the register when you need it.

KUPL is open source and **free forever**.

```kupl
component Counter {
    intent "Counts clicks and publishes the current count."

    in click: Event
    out value: Int

    state count: Int = 0

    on click {
        count += 1
        emit value(count)
    }

    example {
        send click
        send click
        expect value == 2
    }
}
```

AI is part of the language, not a library. An `ai fun` is a typed prompt
function — its return type drives structured output, parsed into real values:

```kupl
type Sentiment = { label: Str, score: Float }

ai fun classify(review: Str) -> Result[Sentiment, Str] {
    intent "Classify the sentiment with a confidence between 0 and 1."
}
```

Runs against Anthropic, any OpenAI-compatible endpoint, or Ollama — selected
by environment, not code — and against a deterministic mock provider for
tests (see `docs/reference/LANGUAGE-REFERENCE.md` §6.1).

---

## Installation

### Prerequisites

| Tool | Needed for | Install |
|---|---|---|
| **Rust** (stable, 1.75+) | building the `kupl` toolchain | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` — or on macOS: `brew install rustup && rustup default stable` |
| **A C compiler** (`cc`) | `kupl native` only (machine-code output) | macOS: `xcode-select --install` · Linux: `apt install gcc` or `clang` |

Everything else is self-contained — the toolchain has **zero external
dependencies** (no crates, no runtime downloads).

### Build and install

```sh
git clone https://github.com/bpupadhyaya/KUPL.git
cd KUPL

# Option A: install into ~/.cargo/bin (on your PATH after rustup setup)
cargo install --path .

# Option B: just build; the binary lands in target/release/kupl
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

### Verify

```sh
kupl version                 # -> kupl 1.0.0-alpha
kupl run examples/counter.kupl
cargo test                   # 216 tests, includes interpreter-vs-VM differential suite
```

---

## Quick start: your first project

```sh
kupl new hello-kupl          # scaffolds main.kupl, util.kupl, kupl.toml
kupl run hello-kupl/main.kupl
# hello from hello-kupl!
```

Or start from a single file. Save this as `greet.kupl`:

```kupl
fun greeting(name: Str) -> Str {
    "hello, {name}!"
}

fun main() {
    for name in ["world", "KUPL"] {
        print(greeting(name))
    }
}
```

```sh
kupl check greet.kupl        # parse + type-check + effect-check
kupl run greet.kupl          # hello, world! / hello, KUPL!
```

Programs can span multiple files: `use util` loads `util.kupl`, `use lib.math`
loads `lib/math.kupl` (relative to the entry file). Diagnostics always point
into the right file. See `examples/multifile/`.

---

## Compiling and running

KUPL has **four execution modes** — same semantics, verified against each other
by differential tests:

| Mode | Command | When to use |
|---|---|---|
| REPL | `kupl repl` | exploring, trying expressions, defining components live |
| Interpreter | `kupl run app.kupl` | development default; runs everything (apps, contracts, tests) |
| KVM bytecode VM | `kupl run --vm app.kupl` | production interpreter; also what `.kx`/`bundle` use |
| Native machine code | `kupl native prog.kupl -o prog` | fastest binaries for `fun main` programs (via the system C compiler) |

The interpreter is the reference semantics; the KVM is checked against it by a
differential suite on every build, and `kupl native`'s output is **byte-identical
to the interpreter across the entire deterministic example suite** (certified by a
full three-engine sweep).

### Run

```sh
kupl run app.kupl            # run the `app` (components, wiring, messages)
kupl run prog.kupl           # or a `fun main()` program
kupl run --vm app.kupl       # same program on the bytecode VM
kupl repl                    # interactive session (:help, :defs, :quit)
```

### Compile & package

```sh
kupl build app.kupl -o app.kx      # ahead-of-time compile to a .kx bytecode module
kupl run app.kx                    # run a compiled module (no source needed)

kupl bundle app.kupl -o app        # ONE self-contained executable (runtime + module)
./app                              # runs anywhere the binary runs — no kupl needed

kupl native prog.kupl -o prog      # true machine code via C, incl. components + `ai fun` (real-provider network calls defer to `bundle`)
./prog                             # add --keep-c to inspect the generated C

kupl dis app.kupl                  # human-readable disassembly of the bytecode
```

Which to pick: `bundle` for component apps (full runtime, single file),
`native` for compute-heavy `fun main` programs (fastest, smallest), `.kx` when
you control the machines and want tiny artifacts (a todo app is ~1.5 KB).

### Test & quality

```sh
kupl test app.kupl           # runs `example` blocks AND contract `law`s as tests
kupl check app.kupl          # parse + type-check + effect-check (exit 0/1)
kupl check --json app.kupl   # machine-readable diagnostics (stable K-codes, spans)
kupl fmt app.kupl --write    # THE canonical format — zero config, idempotent
kupl diff old.kupl new.kupl  # semantic diff: interface-breaking vs implementation-only
```

### AI & editor tooling

```sh
kupl context app.kupl TodoStore   # one item + its direct deps — minimal LLM context
kupl manifest app.kupl            # JSON component manifests (ports/props/exposes) for visual tools
kupl lsp                          # Language Server (stdio): live diagnostics in any LSP editor
```

To use the LSP in your editor, register `kupl lsp` as the language server for
`.kupl` files. Example for Neovim:

```lua
vim.api.nvim_create_autocmd("FileType", {
  pattern = "kupl",
  callback = function()
    vim.lsp.start({ name = "kupl", cmd = { "kupl", "lsp" } })
  end,
})
vim.filetype.add({ extension = { kupl = "kupl" } })
```

---

## Examples

| File | Shows |
|---|---|
| `examples/counter.kupl` | components, typed ports, wiring, `example` tests |
| `examples/ai.kupl` | `ai fun` typed prompt functions: text, structured records, lists, `Result` capture |
| `examples/agent.kupl` | agentic `ai fun`: the model calls KUPL functions as tools (`tools [add, weather]`) |
| `examples/agent_component.kupl` | agent component: conversation state persisted across turns, interpolated intent, tool use |
| `examples/shapes.kupl` | functional core: ADTs, `match`, records, `Option`/`Result` + `?`, lambdas |
| `examples/todo.kupl` | a small app: store + reporter, expose functions, message flow |
| `examples/contracts.kupl` | contracts with executable `law`s (`kupl test` runs them) |
| `examples/di.kupl` | contract-typed props: dependency injection with dynamic dispatch through an interface |
| `examples/properties.kupl` | property-based testing: `forall` over generated values, top-level `law` tests |
| `examples/supervise.kupl` | fault tolerance: panics restart the component, the app survives |
| `examples/timers.kupl` | timers: `on every`/`on after` on a deterministic virtual clock (`advance`) |
| `examples/tensors.kupl` | first-class tensors, elementwise ops, dot products |
| `examples/parallel.kupl` | concurrency: `par` fork-join + `par_map`/`par_filter` iteration, incl. parallel AI fan-out |
| `examples/files.kupl` | file I/O: `read_file`/`write_file`/`append_file`/`delete_file` (`io.fs` effect) |
| `examples/json.kupl` | JSON: built-in `Json` type, `json_parse`/`json_stringify`, round-trips |
| `examples/cli.kupl` | a CLI tool: `args()`, `env_var`, `eprint`, `exit` (`io.env` effect) |
| `examples/bitflags.kupl` | bit manipulation: hex/binary literals, `.band`/`.bor`/`.shl`/`.ushr` |
| `examples/random.kupl` | seeded random: `random_ints`/`random_floats`/`shuffle` (deterministic) |
| `examples/http.kupl` | HTTP client: `http_get` → `json_parse` → summarize (`io.net` effect) |
| `examples/regex.kupl` | regex: `re_match`/`re_find_all`/`re_replace` — validate, extract, redact |
| `examples/showcase.kupl` | capstone: JSON + file I/O + regex + parallel `par_map` in one pipeline |
| `examples/time.kupl` | time/date: `now`, `format_time`, calendar fields (`io.time` effect) |
| `examples/encoding.kupl` | base64/hex encode+decode, FNV hash — auth header, sharding |
| `examples/csv.kupl` | CSV parse/stringify (RFC 4180) and a CSV→JSON converter |
| `examples/url.kupl` | URL encode/decode + query strings — build & parse request URLs |
| `examples/sized.kupl` | sized numbers `i8`…`u64` + `f32` — literal suffixes, checked arithmetic, conversions |
| `examples/multifile/` | `use`-based multi-file programs |
| `examples/pkg/` | two local packages: an app depending on `greet` via `kupl.toml` |
| `examples/native-showcase.kupl` | sized ints + parallel `par_map` + component exposes + wires — identical on interp, KVM, and **native** |
| `examples/analytics.kupl` | access-log analytics CLI: CSV parse + regex validation + status-class bucketing + latency stats + JSON summary — a real tool, identical on interp/KVM/native |
| `examples/datetime.kupl` | deterministic UTC date/time: `date_make`/`date_iso`/`parse_iso`/`*_of` over epoch seconds — pure civil-calendar math, identical on interp/KVM/native |
| `examples/match.kupl` | pattern matching: guards (`if COND`) and or-patterns (`A | B`), with sound exhaustiveness |
| `examples/ufcs.kupl` | uniform function call syntax: `x.f(args)` resolves to `f(x, args)`, so free functions chain as methods |
| `examples/iflet.kupl` | `if let` / `while let` — ergonomic Option/Result unwrapping (desugars to `match`) |
| `examples/stdin.kupl` | reading stdin: a `wc`-style Unix filter with `read_line`/`read_all` (EOF-safe) |
| `examples/exec.kupl` | subprocess: `exec(program, args)` runs external commands (argv, no shell) and captures output |
| `examples/paths.kupl` | file/path toolkit: `path_join`/`base`/`dir`/`ext` + `list_dir` (sorted) + `make_dir`/`remove_dir` |
| `examples/defaults.kupl` | default parameter values + named arguments (Python/Swift/Kotlin-style calls) |
| `examples/ssg.kupl` | a mini static site generator: markdown→HTML using the file/path toolkit + string processing (with a `law`) |
| `examples/bigint.kupl` | arbitrary-precision integers: exact `50!`, `fib(100)`, `2^256`, division/modulo/power — identical on all engines |
| `examples/rational.kupl` | exact rational numbers: `rat(n,d)` reduced fractions, `+ - * /`, `H(10)=7381/2520`, `.to_float`/`.recip` (with a `law`) |
| `examples/operators.kupl` | operator overloading: define `add`/`sub`/`mul`/`lt`… for a user `Vec2` type, use `+ - * < >` (with a `law`) |
| `examples/calc.kupl` | a mini expression-language interpreter: tokenizer → recursive-descent parser → evaluator with variables, written in KUPL (with a `law`) |
| `examples/vm.kupl` | a bytecode compiler + stack VM: source → AST → bytecode → stack machine, written in KUPL (with a `law`) |
| `examples/sets.kupl` | set algebra (`union`/`intersect`/`difference`/`symmetric_difference`/`is_subset`) + keyed `min_by`/`max_by` (with a `law`) |
| `examples/life.kupl` | Conway's Game of Life (cellular automaton): B3/S23 rule on an immutable grid, ASCII render (with a `law`) |
| `examples/stats.kupl` | descriptive statistics (mean/variance/stddev) + least-squares linear regression (with a `law`) |
| `examples/adventure.kupl` | a text-adventure engine: rooms/exits/items as a `Map` of records, immutable state threaded through commands (with a `law`) |
| `examples/diff.kupl` | a line diff via longest-common-subsequence (dynamic programming + backtrack), written in KUPL (with a `law`) |
| `examples/ledger.kupl` | a bank-ledger component: typed state, message handlers, an overdraft guard, a transaction log, and a report — the component model + records + collections together (with an `example` block) |
| `examples/maps.kupl` | Map transformations: `.filter(fn(k,v))` and `.fold(init, fn(acc,k,v))` over entries (with a `law`) |
| `examples/listops.kupl` | `List.zip_with` (element-wise combine) + `Str.trim_start`/`trim_end` (with a `law`) |
| `examples/sortgroup.kupl` | `List.sort_by` (stable) + `List.group_by` (bucket into a `Map`) (with a `law`) |
| `examples/listmore.kupl` | `List.take_while`/`drop_while` (prefix by predicate) + `flat_map`/`flatten` (with a `law`) |
| `examples/format.kupl` | number formatting: `Float.fmt(decimals)` fixed-point + `Int.to_hex`/`to_binary`/`to_radix` (with a `law`) |
| `examples/jq.kupl` | a jq-like JSON query tool: path expressions (`.a.b`, `[0]`, `[]`) over the built-in `Json` type, written in KUPL (with a `law`) |
| `examples/braces.kupl` | literal-brace escaping: `{{`/`}}` in interpolated strings for JSON/CSS/`{…}` templates (with a `law`) |
| `examples/combinators.kupl` | Option/Result combinators: `.map`/`.and_then`/`.filter`/`.ok_or`/`.map_err`/`.ok` pipelines (with a `law`) |
| `examples/sudoku.kupl` | a backtracking Sudoku solver (MRV heuristic) written in KUPL — recursion + `Option` + immutable-list updates (with a `law`) |
| `examples/generic.kupl` | generic ADTs: `type Box[T]`/`Pair[A,B]`/`Tree[T]`, sound at multiple instantiations (with a `law`) |
| `examples/collections.kupl` | a generic collections library: `Stack[T]`, `Queue[T]`, and a BST with an explicit compare fn (with a `law`) |

All examples run identically on the interpreter, the VM, and (for `fun main`
programs) native — try `diff <(kupl run f.kupl) <(kupl run --vm f.kupl)`.

**Demos** (`examples/demos/`) are runnable programs that block (an HTTP server
serves forever), so they live outside the automated example regression but are
covered by integration tests:

| Demo | What it shows |
|---|---|
| `examples/demos/server.kupl` | a tiny HTTP server: `http_serve(port, handler)` routing method+path to a response body |
| `examples/demos/api.kupl` | a JSON REST API web backend: `http_serve` + `json_stringify` + routing (`/health`, `/add/2/3`, `/echo/x`, `/time/0`) |

## What works today

Components as isolated actors (typed ports, `wire`, state, `on start/stop`,
supervision with `restart on_failure`, and timers on a deterministic virtual clock); contracts with laws that run as tests, contract types for dependency injection (dynamic dispatch through an interface), and property-based testing (`forall` with deterministic generation and shrinking);
pure functions with **inferred + enforced effects** (`pub`/`expose` must declare
`uses io` etc.); ADTs with exhaustive `match`, records, newtypes,
`Option`/`Result` + `?`, lambdas, string interpolation; checked 64-bit integers
(overflow panics, never wraps); first-class tensors with native numeric kernels;
multi-file modules; `par` structured fork-join concurrency; `ai fun` typed prompt functions with structured output and a provider-agnostic runtime (Anthropic, OpenAI-compatible, Ollama, deterministic mock), tool use (model calls KUPL functions), interpolated intents, agent components (stateful, multi-turn); the canonical formatter; semantic diff; JSON diagnostics;
component manifests; an LSP server; and four verified execution modes.

## Documentation

Start at the **[documentation home](docs/index.md)** — a docs.python.org-style
index tying everything together. New users should read
**[Getting Started](docs/guide/getting-started.md)** then work through
**[The KUPL Tutorial](docs/guide/tutorial.md)** (a hands-on tour of the whole
language, every example verified against the toolchain).

## Reference documentation

- [`docs/reference/LANGUAGE-REFERENCE.md`](docs/reference/LANGUAGE-REFERENCE.md) — the language reference manual (as implemented): lexical structure, types, expressions, statements, functions & effects, components, contracts, supervision, semantics
- [`docs/reference/STDLIB.md`](docs/reference/STDLIB.md) — built-in functions, constructors, and every method on List/Str/Int/Float/Option/Result/Tensor
- [`docs/reference/CLI.md`](docs/reference/CLI.md) — every `kupl` command, flags, exit codes, artifact formats
- [`docs/reference/DIAGNOSTICS.md`](docs/reference/DIAGNOSTICS.md) — the complete K-code index (104 diagnostics, grouped by phase)
- [`docs/PRODUCTION.md`](docs/PRODUCTION.md) — running KUPL in production: security model, resource limits, threat model (it is **not** a sandbox), operations, and an honest list of known limitations
- [`docs/COMPARISON.md`](docs/COMPARISON.md) — an honest audit of KUPL vs Python, Go, TypeScript, Java, Rust, Haskell, C++, Swift, and Kotlin (as-implemented vs designed)

## Design documents

- [`docs/design/VISION.md`](docs/design/VISION.md) — vision, the seven pillars, inspirations, non-goals
- [`docs/design/LANGUAGE.md`](docs/design/LANGUAGE.md) — component model, type system, effects & capabilities, keywords, grammar, semantics
- [`docs/design/TOOLCHAIN.md`](docs/design/TOOLCHAIN.md) — every compiler phase, VM design, runtime, roadmap
- Further design: `PLATFORMS.md`, `INTEROP.md`, `DISTRIBUTION.md`, `UI.md`, `VISUAL-TOOLS-CONTRACT.md`

## Status & roadmap

**v1.0-alpha** (2026-07): the founding vision is implemented end to end —
~29,100 lines of dependency-free Rust, 216 tests, all engines differentially
verified. Next arc (per `docs/design/TOOLCHAIN.md`): KIR (typed SSA) with GPU
lowering (Metal first), components + per-component GC in the native backend,
timers (`on every`), the package registry, LSP hover/completion, and
self-hosting. The pre-2026 Scala/Java scaffold lives in `attic/`.

## License

MIT — free forever. Fork it, ship it, build on it.
