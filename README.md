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
cargo test                   # 156 tests, includes interpreter-vs-VM differential suite
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

kupl native prog.kupl -o prog      # true machine code via C (fun main programs)
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

All examples run identically on the interpreter, the VM, and (for `fun main`
programs) native — try `diff <(kupl run f.kupl) <(kupl run --vm f.kupl)`.

## What works today

Components as isolated actors (typed ports, `wire`, state, `on start/stop`,
supervision with `restart on_failure`, and timers on a deterministic virtual clock); contracts with laws that run as tests, contract types for dependency injection (dynamic dispatch through an interface), and property-based testing (`forall` with deterministic generation and shrinking);
pure functions with **inferred + enforced effects** (`pub`/`expose` must declare
`uses io` etc.); ADTs with exhaustive `match`, records, newtypes,
`Option`/`Result` + `?`, lambdas, string interpolation; checked 64-bit integers
(overflow panics, never wraps); first-class tensors with native numeric kernels;
multi-file modules; `par` structured fork-join concurrency; `ai fun` typed prompt functions with structured output and a provider-agnostic runtime (Anthropic, OpenAI-compatible, Ollama, deterministic mock), tool use (model calls KUPL functions), interpolated intents, agent components (stateful, multi-turn); the canonical formatter; semantic diff; JSON diagnostics;
component manifests; an LSP server; and four verified execution modes.

## Reference documentation

- [`docs/reference/LANGUAGE-REFERENCE.md`](docs/reference/LANGUAGE-REFERENCE.md) — the language reference manual (as implemented): lexical structure, types, expressions, statements, functions & effects, components, contracts, supervision, semantics
- [`docs/reference/STDLIB.md`](docs/reference/STDLIB.md) — built-in functions, constructors, and every method on List/Str/Int/Float/Option/Result/Tensor
- [`docs/reference/CLI.md`](docs/reference/CLI.md) — every `kupl` command, flags, exit codes, artifact formats
- [`docs/reference/DIAGNOSTICS.md`](docs/reference/DIAGNOSTICS.md) — the complete K-code index (104 diagnostics, grouped by phase)
- [`docs/COMPARISON.md`](docs/COMPARISON.md) — an honest audit of KUPL vs Python, Go, TypeScript, Java, Rust, Haskell, C++, Swift, and Kotlin (as-implemented vs designed)

## Design documents

- [`docs/design/VISION.md`](docs/design/VISION.md) — vision, the seven pillars, inspirations, non-goals
- [`docs/design/LANGUAGE.md`](docs/design/LANGUAGE.md) — component model, type system, effects & capabilities, keywords, grammar, semantics
- [`docs/design/TOOLCHAIN.md`](docs/design/TOOLCHAIN.md) — every compiler phase, VM design, runtime, roadmap
- Further design: `PLATFORMS.md`, `INTEROP.md`, `DISTRIBUTION.md`, `UI.md`, `VISUAL-TOOLS-CONTRACT.md`

## Status & roadmap

**v1.0-alpha** (2026-07): the founding vision is implemented end to end —
~21,600 lines of dependency-free Rust, 156 tests, all engines differentially
verified. Next arc (per `docs/design/TOOLCHAIN.md`): KIR (typed SSA) with GPU
lowering (Metal first), components + per-component GC in the native backend,
timers (`on every`), the package registry, LSP hover/completion, and
self-hosting. The pre-2026 Scala/Java scaffold lives in `attic/`.

## License

MIT — free forever. Fork it, ship it, build on it.
