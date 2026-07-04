# KUPL CLI Reference

**Version:** 1.0-alpha · one binary: `kupl`

All file-taking commands are **multi-file aware**: they load the entry file
plus everything reachable through `use`, and diagnostics point into the file
they belong to. Exit codes: `0` success · `1` compile/test/diff failure ·
`2` usage or I/O error · `101` runtime panic.

## Running

### `kupl run <file.kupl>`
Runs the first `app` in the program (instantiate → `on start` in creation
order → drain the message queue). With no `app`, runs `fun main()`. Values
emitted on unwired out ports print as `Component.port = value`.

### `kupl run --vm <file.kupl>`
Same program on the KVM register bytecode VM. Output is byte-identical to the
interpreter (enforced by differential tests).

### `kupl run <file.kx>`
Runs a compiled bytecode module directly — no source needed.

### `kupl repl`
Interactive session. Enter expressions, statements, or whole declarations
(`fun`/`type`/`component` — multi-line input is detected by bracket balance).
Commands: `:help` `:defs` `:quit`.

## Compiling & packaging

### `kupl build <file.kupl> [-o out.kx]`
Ahead-of-time compile to a `.kx` bytecode module (magic `KUPLKX01`).
Default output: `<file>.kx`. Artifacts are small (a todo app ≈ 1.5 KB).

### `kupl bundle <file.kupl> [-o app]`
Produces a **self-contained executable**: a copy of the `kupl` runtime with
the compiled module appended as a trailer. The result runs the app directly
(`./app`) on any machine of the same OS/arch, with no kupl installation.
Recommended for component apps.

### `kupl native <file.kupl> [-o prog] [--keep-c]`
Compiles to **machine code**: bytecode → generated C → `$CC` (default `cc`)
`-O2`. Requires a `fun main()` program (component apps: use `bundle`).
`--keep-c` keeps the generated `.c` beside the output for inspection.

### `kupl dis <file.kupl>`
Prints the compiled KVM bytecode: every chunk (functions, lambdas, component
init/handlers/exposes), constants, and the constructor table.

## Testing & quality

### `kupl test <file.kupl>`
Runs every component's `example` blocks **and** every contract `law` against
every fulfilling component. Output: `ok`/`FAIL` per case + summary. A failing
`expect` reports the exact source expectation.

### `kupl check <file.kupl> [--json]`
Parse + type-check + effect-check without running. `--json` emits
`{"diagnostics":[{severity, code, message, file, span:{start,end,line,col}}]}`
with per-file positions — built for editors and AI agents.

### `kupl fmt <file.kupl> [--write]`
THE canonical format — zero configuration, idempotent, fixed member order
inside components. Prints to stdout, or rewrites in place with `--write`.
Formats the named file only (per-file by design).

### `kupl diff <old.kupl> <new.kupl>`
**Semantic** comparison: items are compared by canonical form, so formatting
and comments never register. Changes are classified
`[INTERFACE — breaking]` (signatures, ports, props, exposes, fulfills) vs
`[implementation only]`, plus `added`/`removed`. Exit 0 ⇔ semantically
identical.

## AI & editor tooling

### `kupl context <file.kupl> <ItemName>`
Emits the named item's source plus the full source of everything it directly
references — the minimal dependency-closed context for an LLM prompt.

### `kupl manifest <file.kupl>`
JSON component manifests: name, kind, intent, ports (name/dir/type), props,
state, exposes (params/returns/effects), fulfills, children, wires, example
count. This is the palette/canvas API for visual tools.

### `kupl lsp`
Language Server Protocol over stdio. Capabilities: full-text document sync,
`publishDiagnostics` on open/change/save. Unsaved buffer contents override
disk; `use`-dependencies are read from disk. Register `kupl lsp` as the
server for `.kupl` files in any LSP-capable editor.

## Project scaffolding

### `kupl new <name>`
Creates `<name>/` with `main.kupl` (a wired two-component app), `util.kupl`
(demonstrating `use` + a `pub fun`), and `kupl.toml`. The project runs
immediately: `kupl run <name>/main.kupl`.

`kupl.toml` (v1.0-alpha fields):

```toml
[project]
name = "my-app"
version = "0.1.0"
entry = "main.kupl"
```

### `kupl version`
Prints the toolchain version.

## Environment

| Variable | Used by | Meaning |
|---|---|---|
| `CC` | `kupl native` | C compiler to invoke (default `cc`) |

## Artifact formats

| Format | Produced by | Notes |
|---|---|---|
| `.kx` | `build` | binary module: chunks, constants, ctor table, component metadata; magic `KUPLKX01`, little-endian |
| bundle | `bundle` | `[kupl binary][.kx][u64 length]["KUPLBNDL"]` — the runtime detects its own trailer at startup |
| `.c` / executable | `native` | generated C embeds a ~400-line runtime mirroring interpreter semantics |
| `.kman` JSON | `manifest` | printed to stdout |
