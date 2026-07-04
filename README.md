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
kupl test examples/counter.kupl   # run `example` blocks as tests
kupl check examples/todo.kupl     # parse + type-check only
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

What v0.1 already gives you: components as isolated actors with typed ports and
`wire`-based composition; pure functions, ADTs + exhaustive `match`, records,
newtypes, `Option`/`Result` with `?`, lambdas, string interpolation, list/string
methods; checked 64-bit integers (overflow panics, never wraps); `intent` and
executable `example` blocks as syntax; precise diagnostics with stable codes.

Next phases (see `docs/design/TOOLCHAIN.md`): effects/capabilities enforcement,
contracts & laws, the canonical formatter, KIR, the KVM bytecode VM, native
compilation, and the tensor/kernel hardware story.

Status: v0.1 interpreter + REPL working end-to-end (2026-07-03); design proposal
in `docs/design/`. The pre-2026 Scala/Java scaffold lives in `attic/`.
