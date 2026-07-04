# KUPL ↔ Visual Tools Contract

v0.1 — 2026-07-03. Formalizes VISION.md §"The visual layer" as numbered, testable
obligations. The contract is public and vendor-neutral — every visual tool gets
the same surface, and KUPL never depends on any of them.

| # | Obligation | Where designed | Status |
|---|---|---|---|
| C1 | Lossless parse ⇄ print, canonical formatter (`parse∘format = id`, fuzzed) | TOOLCHAIN §2–3 | designed ✅ |
| C2 | Stable node IDs surviving edits/reformat; used by `kupl diff`/`patch` | TOOLCHAIN §2 | designed ✅ |
| C3 | **Tool-metadata annotation blocks** on any declaration | this doc, below | **gap → proposed** |
| C4 | Component manifests (`.kman.json`: ports, props, requires, intent, examples, node ID) | TOOLCHAIN §6 | designed ✅ |
| C5 | Introspection API: enumerate a package's components/ports/types/effects | TOOLCHAIN §6 + `kupl context` | designed ✅ (manifest index) |
| C6 | Embeddable interpreter; component-granular hot swap + state migration; **message-trace hooks** | TOOLCHAIN §10; LANGUAGE open Q4; traces below | partial → **trace hooks proposed** |
| C7 | Toolchain as Rust crates (lexer/parser/checker/formatter) + LSP | TOOLCHAIN §11, front-end crates | designed ✅ |
| C8 | Incremental, error-tolerant parsing (always-a-tree, all errors in one pass) | TOOLCHAIN §2, §4 | designed ✅ |

## C3 proposal: annotation blocks

Tools need to persist non-semantic data (canvas coordinates in architecture view,
fold state, design notes) *in the source file* — sidecar files drift and break the
plain-text/git-native promise. Proposal:

```kupl
component TodoStore {
    @meta(studio.canvas: { x: 420, y: 160 }, studio.color: "amber")
    intent "…"
    …
}
```

- `@meta(...)` accepts namespaced keys (`<tool>.<key>`) with KUPL literal values.
- **Semantics-free by definition:** the compiler type-checks literals, carries them
  through CST/AST/manifest, and otherwise ignores them; `kupl fmt` places the block
  in fixed position (after the declaration header, before `intent`); `kupl diff`
  classifies `@meta`-only changes as non-semantic.
- Unknown namespaces are preserved verbatim (forward compatibility between tools).

## C6 addition: message-trace hooks

The interpreter and KVM expose a subscription API (in-process for embedders; also
`kupl repl :trace`): every delivered message yields `{seq, from: (instance, port),
to: (instance, port), payload, span, timestamp}` with node IDs. Requirements:

- Per-instance and per-port filtering; bounded ring buffer; zero cost when no
  subscriber (safepoint-checked flag).
- Payloads exposed as kser (DISTRIBUTION.md encoding) — tools decode without
  linking runtime internals.
- Replay: a recorded trace can re-drive a component in a fresh instance (same
  machinery as `example` blocks) — this is what visual time-travel debugging and
  interaction-recording build on.

## Change process

Visual-tool needs enter KUPL only as numbered contract items proposed here (C9,
C10, …) and reviewed like any language change. No tool gets — or needs — a
private fork of language behavior.
