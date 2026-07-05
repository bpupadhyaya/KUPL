# KUPL Documentation

Welcome! This is the documentation for **KUPL** — the K Universal Programming
Language, an AI-first, component-oriented, general-purpose language with a
zero-dependency toolchain (REPL, tree-walking interpreter, register VM, bytecode
compiler, and native machine-code compiler), all held byte-identical by
differential tests.

KUPL is open source and **free forever**.

> **New to KUPL?** Start with **[Getting Started](guide/getting-started.md)**,
> then work through **[The KUPL Tutorial](guide/tutorial.md)**.

---

## Parts of the documentation

| | |
|---|---|
| **[Getting Started](guide/getting-started.md)** | Install/build the toolchain, run the REPL, and write your first program. Start here. |
| **[The KUPL Tutorial](guide/tutorial.md)** | A hands-on, progressive walkthrough of the whole language — values, functions, types, pattern matching, collections, error handling, effects, components, the AI-native core, packages, concurrency, and native compilation. The best way to learn KUPL. |
| **[Language Reference](reference/LANGUAGE-REFERENCE.md)** | The normative description of the language *as implemented*: lexical structure, types, expressions, statements, declarations, components, effects, and semantics. |
| **[Standard Library](reference/STDLIB.md)** | Every built-in type and method — `Int`, `Float`, sized integers, `Str`, `List`, `Map`, `Set`, `Option`, `Result`, `Json`, `Tensor` — plus the free functions (file I/O, JSON, HTTP, regex, CSV, URL, time, encoding, random). |
| **[Command-Line Interface](reference/CLI.md)** | Every `kupl` subcommand: `run`, `build`, `bundle`, `native`, `test`, `check`, `fmt`, `diff`, `repl`, `lsp`, `pkg`, and more. |
| **[Diagnostics Index](reference/DIAGNOSTICS.md)** | The `K####` error and warning codes, with what triggers each. |
| **[KUPL vs. the Field](COMPARISON.md)** | An honest audit comparing KUPL against Python, Go, TypeScript, Java, Rust, Haskell, C++, Swift, and Kotlin — where it matches or beats them, and where it still trails. |
| **[Gap Audit & Roadmap](GAPS.md)** | What has shipped, what is designed but not yet built, and the enrichment-campaign history. |

---

## What makes KUPL different

- **Four engines, one language.** The same program runs on a REPL, a
  tree-walking interpreter, a register-based bytecode VM, and a native
  machine-code compiler — and produces **byte-identical output** on each, enforced
  by differential tests. `kupl native` compiles the whole language (bar `ai fun`)
  to machine code.
- **Components as the unit.** A `component` is an isolated actor with typed ports,
  private state, message handlers, timers, supervision, and `example` blocks that
  are tests you write inline. Contracts give interfaces with dynamic dispatch.
- **AI as a language feature.** `ai fun` declares a typed prompt function whose
  return type drives structured output; tool use and agent components are
  first-class; a deterministic mock provider makes AI-driven code unit-testable.
- **An effect system.** Functions are pure by default; side effects are declared
  (`uses io`, `uses io.fs`, …) and checked at boundaries.
- **Batteries included, zero dependencies.** File I/O, JSON, HTTP, regex, CSV,
  URL, time, encoding, seeded random, and rich collections ship in the box — the
  whole toolchain has no external dependencies.

---

## A taste

```kupl
type Shape = Circle(r: Float) | Rect(w: Float, h: Float)

fun area(s: Shape) -> Float {
    match s {
        Circle(r) => 3.14159 * r * r
        Rect(w, h) => w * h
    }
}

fun main() uses io {
    let shapes = [Circle(2.0), Rect(3.0, 4.0)]
    let total = shapes.map(fn s { area(s) }).fold(0.0, fn acc, a { acc + a })
    print("total area = {total.format(2)}")
}
```

```text
$ kupl run shapes.kupl
total area = 24.57
```

---

*This documentation describes KUPL **as implemented** by the reference toolchain
in this repository. Features that exist only in the design proposal are covered
in [`design/LANGUAGE.md`](design/LANGUAGE.md) and marked **[design]**.*
