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

Status: design phase (proposal v0.1, 2026-07-03).
