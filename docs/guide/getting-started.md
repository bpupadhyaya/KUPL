# Getting Started

This guide gets you from nothing to a running KUPL program in a few minutes.

- [Building the toolchain](#building-the-toolchain)
- [Your first program](#your-first-program)
- [The REPL](#the-repl)
- [The four engines](#the-four-engines)
- [Project scaffolding](#project-scaffolding)
- [Editor support](#editor-support)
- [Where to go next](#where-to-go-next)

---

## Building the toolchain

KUPL is a single Rust binary named `kupl`, with **zero external dependencies**.
You need a Rust toolchain (`cargo`) to build it, and a C compiler (`cc`, the
system default) only if you want to use `kupl native`.

```text
$ git clone <this-repo> kupl
$ cd kupl
$ cargo build --release
$ ./target/release/kupl version
```

Put `./target/release/kupl` on your `PATH` (or use the full path). The rest of
this guide writes it simply as `kupl`.

---

## Your first program

Create a file `hello.kupl`:

```kupl
fun main() uses io {
    print("Hello, KUPL!")
}
```

Run it:

```text
$ kupl run hello.kupl
Hello, KUPL!
```

A few things to notice:

- **`fun main()`** is the entry point for a plain program (component apps use an
  `app` instead — see the [tutorial](tutorial.md#components)).
- **`uses io`** declares that `main` performs the `io` effect. `print` needs it.
  KUPL functions are **pure by default**; any side effect is declared and checked.
- **No semicolons.** Statements end at newlines; the layout is unambiguous.
- **String interpolation:** `"n = {x + 1}"` embeds an expression. (A literal
  brace is written `\{`.)

---

## The REPL

For interactive exploration:

```text
$ kupl repl
kupl> 1 + 2 * 3
7
kupl> let xs = [1, 2, 3]
kupl> xs.map(fn n { n * n })
[1, 4, 9]
kupl> fun double(n: Int) -> Int { n * 2 }
kupl> double(21)
42
kupl> :quit
```

Enter expressions, statements, or whole declarations (`fun`/`type`/`component` —
multi-line input is detected by bracket balance). Commands: `:help`, `:defs`,
`:quit`.

---

## The four engines

The same source runs on four engines that all produce **byte-identical** output
(enforced by differential tests):

```text
$ kupl run hello.kupl            # tree-walking interpreter (reference semantics)
$ kupl run --vm hello.kupl       # register bytecode VM (KVM)
$ kupl build hello.kupl -o h.kx  # compile to a .kx bytecode module
$ kupl run h.kx                  # run the compiled module
$ kupl native hello.kupl -o hi   # compile to native machine code (via C)
$ ./hi                           # a real executable
```

- **`kupl run`** — the interpreter; the reference for language semantics.
- **`kupl run --vm`** — the KVM; the same program on a register VM.
- **`kupl build`** — ahead-of-time compile to a small `.kx` module.
- **`kupl bundle`** — a self-contained executable (the runtime + your module),
  recommended for component apps.
- **`kupl native`** — true machine code via generated C, compiled with the system
  `cc -O2`. Compiles the whole language except `ai fun`.

You almost never need to think about which engine you're on — they agree by
construction.

---

## Testing, formatting, checking

```text
$ kupl test app.kupl     # run every `law` and component `example` block
$ kupl check app.kupl    # type-check + effect-check without running
$ kupl fmt app.kupl      # THE canonical format (zero config); --write to rewrite
$ kupl diff a.kupl b.kupl # semantic diff (formatting/comments never register)
```

`example` blocks and top-level `law`s are tests you write **inline** with the
code; `kupl test` runs them.

---

## Project scaffolding

Start a multi-file project:

```text
$ kupl new my-app
$ kupl run my-app/main.kupl
```

`kupl new` creates `main.kupl`, a `util.kupl` demonstrating `use` + a `pub fun`,
and a `kupl.toml` manifest. A directory with a `kupl.toml` is a **package**; its
`[dependencies]` can name other local packages by path (see the
[tutorial's packages section](tutorial.md#packages-and-modules)).

---

## Editor support

KUPL ships a Language Server:

```text
$ kupl lsp   # LSP over stdio
```

Register `kupl lsp` as the server for `.kupl` files in any LSP-capable editor.
It provides diagnostics, hover (signatures), go-to-definition, completion,
find-references, and rename. See the [CLI reference](../reference/CLI.md#kupl-lsp).

---

## Where to go next

- Work through **[The KUPL Tutorial](tutorial.md)** — the guided tour of the whole
  language.
- Keep the **[Language Reference](../reference/LANGUAGE-REFERENCE.md)** and
  **[Standard Library](../reference/STDLIB.md)** open as you write.
