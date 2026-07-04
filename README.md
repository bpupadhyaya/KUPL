# KUPL
K - Universal Programming Language

An AI-first, component-oriented, general-purpose programming language with a
complete toolchain: REPL, interpreter, virtual machine, bytecode compiler, and
native machine-code compiler — designed to run efficiently across CPU, GPU, TPU,
and neural engines, with progressive low-level control down to the register when
you need it.

KUPL is open source and **free forever**.

## Design documents

- [`docs/design/VISION.md`](docs/design/VISION.md) — vision, the seven pillars, inspirations, non-goals
- [`docs/design/LANGUAGE.md`](docs/design/LANGUAGE.md) — the language: component model, type system, effects & capabilities, keywords, grammar, semantics, worked examples
- [`docs/design/TOOLCHAIN.md`](docs/design/TOOLCHAIN.md) — every compiler phase (lexer → parser → type/effect checker → KIR → KVM bytecode / native code), VM design, runtime, REPL, CLI/LSP/packages, roadmap

## Getting started (v0.1 toolchain)

The v0.1 toolchain (lexer → parser → type/effect checker → tree-walking
interpreter with a deterministic component runtime, REPL, example-test runner)
is implemented in Rust in `src/`.

```sh
cargo build --release          # produces target/release/kupl

kupl run examples/counter.kupl    # run an app (components, wiring, messages)
kupl run examples/shapes.kupl     # run a pure functional program (fun main)
kupl run --vm examples/shapes.kupl  # same program on the KVM bytecode VM
kupl build examples/todo.kupl     # compile to a .kx bytecode module
kupl run examples/todo.kx         # run the compiled module directly
kupl bundle examples/counter.kupl -o counter-app   # self-contained executable
kupl dis examples/shapes.kupl     # disassemble the compiled bytecode
kupl test examples/counter.kupl   # run `example` blocks as tests
kupl check examples/todo.kupl     # parse + type-check + effect-check
kupl check --json broken.kupl     # machine-readable diagnostics (for AI/editors)
kupl fmt file.kupl [--write]      # THE canonical form (zero config, idempotent)
kupl context file.kupl TodoStore  # item + direct deps — minimal LLM context
kupl repl                         # interactive session
```

A taste of KUPL:

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

What works today: components as isolated actors with typed ports and
`wire`-based composition; pure functions, ADTs + exhaustive `match`, records,
newtypes, `Option`/`Result` with `?`, lambdas, string interpolation, list/string
methods; checked 64-bit integers (overflow panics, never wraps); `intent` and
executable `example` blocks as syntax; **effect inference + boundary enforcement**
(`pub`/`expose` functions must declare `uses io` etc. — inferred transitively
through private helpers); **the normative formatter** (`kupl fmt`, idempotent by
construction); **JSON diagnostics** with stable codes; **`kupl context`** for
minimal LLM prompts.

Next phases (see `docs/design/TOOLCHAIN.md`): contracts & laws, KIR, the KVM
bytecode VM, native compilation, and the tensor/kernel hardware story.

Status: v0.6 — interpreter, REPL, formatter, effects, contracts+laws, KVM bytecode VM (functional core + full component apps, differentially tested), .kx modules, self-contained executables (kupl bundle)
(2026-07-03); design proposal in `docs/design/`. The pre-2026 Scala/Java
scaffold lives in `attic/`.
