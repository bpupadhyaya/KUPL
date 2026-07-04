# KUPL Toolchain & Implementation Architecture

Proposal v0.1 — 2026-07-03. Companion to `LANGUAGE.md`.

One compiler front end feeds four execution modes:

```
                        ┌──────────────► Tree-walk interpreter ── REPL / scripts
source (.kupl)          │
  │  lex ─ parse ─ canonicalize ─ resolve ─ typecheck ─ contracts
  ▼                                                        │
 CST ──► AST ──► TAST (typed AST) ─────────────────────────┤
                                                           ▼
                                              KIR (typed SSA IR, dialects)
                                                           │
                    ┌──────────────────────────────────────┼─────────────────────┐
                    ▼                                      ▼                     ▼
             KVM bytecode (.kx)                   native code (CPU)        device code
             register VM + JIT                 via LLVM (or Cranelift)   PTX / SPIR-V / Metal
```

Implementation language for the toolchain: **Rust** (memory-safe systems language,
first-class LLVM/Cranelift/MLIR bindings, single static binary — `kupl` installs as
one file). The 2015 Scala/Java sbt scaffold currently in `KUPL/` should be archived
(e.g. moved to `attic/`) — it predates this design.

---

## 1. Phase: Lexer

- Hand-written scanner (not generated): best error recovery and speed.
- Input UTF-8; identifiers Unicode XID; strings with `{expr}` interpolation lexed
  as nested token streams.
- Token categories: keyword, contextual-keyword, ident, int/float/str/char literal,
  operator, delimiter, NEWLINE, EOF. Comments (`//`, `/* */`) and doc text are
  **trivia** attached to tokens (needed for lossless canonical formatting).
- Newline-as-terminator rules resolved here: NEWLINE is suppressed after operators,
  commas, and open brackets (continuation), and inside `( … )`.
- Every token carries a byte span; all downstream diagnostics use spans.
- Deliverable: `kupl-lex` crate + fuzz target (lexer must never panic on any bytes).

## 2. Phase: Parser → CST → AST

- Hand-written recursive descent with Pratt expression parsing; grammar kept LL(2).
- Produces a **lossless CST** (every byte of input represented, à la rust-analyzer's
  rowan) — required for the formatter, IDE features, and semantic diff.
- AST is a typed projection of the CST with stable **node IDs** (hash of
  module path + item name + disambiguator) — these IDs are what `kupl diff`/`patch`
  and visual tools use to track components across edits.
- **Error recovery is a feature:** the parser always produces a tree; unparseable
  regions become `Error` nodes with spans. An AI agent gets *all* the errors in one
  pass, not the first one.
- CI invariant: `parse(format(ast)) == ast` (round-trip property, fuzzed).

## 3. Phase: Canonicalizer / Formatter (`kupl fmt`)

- Normative, zero-config. Fixed member order inside components (intent → fulfills →
  requires → ports → props → state → handlers → expose → private funs → examples),
  fixed spacing/wrapping (target width 100).
- Runs as an AST→text printer; the compiler warns (`--deny-format` in CI: errors)
  when input isn't canonical.
- This is an AI-first load-bearing wall: deterministic output ⇒ models reproduce
  byte-identical code for identical ASTs ⇒ diffs are pure semantics.

## 4. Phase: Name resolution & module graph

- One module per file; module path mirrors directory path from the package root.
- Explicit `use` only; no glob imports (`use std.http.*` is not in v1) — keeps
  `kupl context` closures minimal and generation unambiguous.
- No cyclic module imports (hard error); component *wiring* cycles are allowed.
- Output: fully resolved AST + package-wide symbol table + dependency graph
  (drives incremental compilation: dirty-file → affected-symbol invalidation).

## 5. Phase: Type & effect checking → TAST

- Bidirectional type checking with local (Hindley–Milner-style) inference inside
  bodies; **no inference across public boundaries** (annotations mandatory there —
  enforced in this phase).
- Effect rows unified alongside types; a call's effects must be ⊆ the caller's
  declared/inferred effects; `pub`/`expose` effects must be written explicitly.
- Capability check: each effect use is traced to a `requires` capability in scope;
  attenuated capabilities narrow the allowed effect set.
- Tensor shape checking: symbolic dimension algebra (`n`, `n*2`, `n+1`); static
  mismatch = compile error, unresolved symbols = checked dispatch with a
  compiler note.
- Exhaustiveness for `match`; ownership/borrow checking runs here for `system`
  items (an NLL-style borrow checker, only on the system-tier subset).
- Output TAST: every node typed + effect-annotated. Diagnostics carry stable codes
  (`K0001`…), explanations, and machine-applicable `fixes[]` (see §13).

## 6. Phase: Contract & example processing

- `where` clauses on record fields → constructor-time checks (elided when provable).
- `law`/`example`/`test` blocks are type-checked against the real interfaces
  (an example that drifts from the API is a compile error — executable docs
  can't rot), then compiled into the package's test binary, not the release
  artifact.
- `intent` strings are carried into TAST, KIR metadata, and manifests.
- Manifest emission (`.kman.json` per component): name, intent, ports (name/type/
  direction), props, requires, fulfills, examples (source text), doc trivia, node ID.
  This is the palette/canvas API for visual tools and `kupl context`'s index.

## 7. KIR — the typed SSA intermediate representation

MLIR-inspired: one IR, multiple **dialects**, progressive lowering.

- **Form:** SSA with basic blocks + region-bearing ops (loops/if are regions until
  late lowering — keeps optimization structural, not CFG-archaeological).
- **Dialects:**
  - `comp` — component model ops: `comp.def`, `comp.spawn`, `comp.wire`,
    `comp.emit`, `comp.send`, `comp.call` (expose invocation), `comp.state.get/set`,
    `comp.supervise`. Supervision & mailbox semantics live here.
  - `core` — functional core: arithmetic, ADT construct/match, closures,
    collections, string ops, `Result`/`?` desugar.
  - `tensor` — tensor algebra ops with shape attributes; `tensor.par`,
    `tensor.reduce`, `tensor.map`; fusion happens at this level.
  - `sys` — pointers, volatile, atomics, `asm`, explicit layout.
- **Metadata:** every op keeps source spans, node IDs, and (on component/fun defs)
  intent strings — so *diagnostics from any phase, even codegen, map back to
  source*, and visual tools can highlight the running op's component.
- **Passes (target-independent):** inlining, DCE, const-fold/prop, escape analysis
  (decides message move vs copy; stack-promotes non-escaping data), handler
  devirtualization (wire targets are usually statically known), tensor fusion,
  bounds-check elision via `where`-clause facts.
- KIR has a stable text format (`.kir`) — printable, parseable, diffable; the
  compiler-explorer story and the debugging story for compiler devs and for AI
  agents inspecting codegen.

## 8. KVM — the KUPL Virtual Machine

**Register-based** bytecode VM (Lua-lineage: fewer dispatches than a stack machine,
simpler JIT mapping later).

- **Module format `.kx`:** header (magic `KUPL`, version, flags) · constant pool
  (interned strs, numerics, type descriptors) · component descriptors (ports,
  state layout, handler table, manifest offset) · function bodies (bytecode) ·
  debug table (bytecode offset → span) · signature.
- **Instruction shape:** 32-bit fixed width, `op a b c` / `op a imm16`; ~90 opcodes
  in v1. Families: moves/consts · arith/compare (typed: `add.i64`, `add.f64`) ·
  ADT make/tag-test/field · collection ops (persistent-structure aware) · call/ret/
  tailcall · closure make/upval · **actor family** (`spawn`, `send`, `emit`,
  `recv_dispatch`, `reply`) · `await`/suspend · tensor handle ops (dispatch to
  device runtime) · guard/panic · safepoint.
- **Execution engine v1:** threaded interpreter (computed goto). Safepoints at
  calls/loop back-edges for GC and preemption. Design leaves room for a template
  JIT (v3+) — bytecode is JIT-friendly by construction (typed ops, no dynamic
  arity, register file per frame).
- **Scheduler:** M:N — N worker threads, work-stealing deques of component
  *activations* (instance + pending message). A handler runs to completion or to
  an `await`/mailbox suspension; budget-based preemption at safepoints prevents a
  hot component from starving others.
- **Memory:** per-component-instance heap (bump-allocated nursery + mark-compact
  tenured; sizes are small because heaps are per-instance). Message send: escape
  analysis proved-unique values are **moved** (pointer transfer), otherwise deep-
  copied (cheap for persistent structures — structural sharing survives copy via
  a shared immutable region for interned/frozen data). **No global pauses ever.**
- **Supervision runtime:** panic → unwind instance → notify supervisor per policy →
  fresh instance (optionally `on start` receives last-good snapshot for state
  migration — the hot-swap hook visual live-editing builds on).
- **Device runtime:** unified stream abstraction over CUDA/Metal/Vulkan-compute
  (and future TPU/NPU plugins): device discovery, memory pools, async copies,
  kernel launch, events. `at(target)` compiles to a dispatch through this layer;
  kernels are provided as pre-lowered fatbinaries (native mode) or JIT-lowered
  from KIR `tensor`/`core` dialects (VM mode).

## 9. Native compiler (`kupl build --native`)

- KIR (post-optimization) lowers: `comp` dialect → runtime calls into **libkrt**
  (the same scheduler/GC/mailbox/device runtime as KVM, factored as a C-ABI Rust
  library) · `core`+`sys` → LLVM IR · `tensor` → target kernels (LLVM vector CPU;
  PTX via NVPTX; SPIR-V; Metal via AIR) embedded as fatbinary sections.
- Two CPU backends: **Cranelift** for `-O0` dev builds (compile speed ≈ VM-level
  iteration) and **LLVM** for release.
- Output: single static executable embedding libkrt; cross-compilation first-class
  (Rust-style target triples); WASM (+WASI) is a supported target for
  browser/edge — and how visual tools can preview components client-side.
- FFI: `extern "c"` declarations (system tier only) both directions; exported
  C ABI for embedding KUPL in existing apps.

## 10. Interpreter & REPL (`kupl repl`)

- The tree-walk interpreter executes TAST directly — zero lowering latency,
  maximal introspection; it is also the reference semantics for differential
  testing against KVM and native (three engines, one behavior, fuzzed).
- REPL abilities: define/redefine components live · `spawn` instances ·
  `send counter.click` · `watch counter.value` (live port tap) · `:type expr`,
  `:effects expr`, `:wire`, `:supervisors` introspection · session save/replay
  (`.kupl-session` files are just source — a REPL session is a reproducible
  script).
- Hot-swap: redefining a component migrates existing instances via the state-
  migration hook; this same machinery powers live visual canvases.

## 11. CLI, LSP, packages

- **`kupl`** single binary: `new build run test fmt doc repl context diff patch
  pkg lsp`. `kupl test` runs unit `test`s + `example` blocks + contract `law`
  property tests. `kupl doc` renders manifests + intents + examples into docs.
- **LSP server** built on the same incremental front end (CST/AST/TAST are the
  IDE data structures — one implementation, no drift).
- **Packages:** `kupl.toml` manifest; lockfile; content-addressed, signed archives;
  registry protocol is a static-file CDN (fork-friendly, vendor-neutral — anyone
  can host a mirror with a web server). SemVer with **enforced** API compatibility:
  `kupl pkg publish` diffs the public surface against the previous version and
  refuses a minor bump on a breaking change (manifests make this cheap).

## 12. Testing the toolchain itself

- Conformance suite (`spec-tests/`): every normative sentence in LANGUAGE.md gets
  numbered executable tests — forks prove compatibility by passing it.
- Differential execution: interpreter vs KVM vs native on the same corpus.
- Round-trip properties: `parse∘format = id`, `kx encode∘decode = id`.
- Fuzzing: lexer, parser, bytecode verifier (`.kx` from untrusted sources must be
  safe to *load*), GC (allocation-heavy stress with shrinking).

## 13. Diagnostics contract (AI-facing, normative)

```json
{
  "code": "K0312",
  "severity": "error",
  "span": {"file": "todo/list.kupl", "start": 812, "end": 847},
  "message": "port `filter` expects Str, wire provides Int",
  "explanation": "wire search.count -> list.filter connects an out port of type Int …",
  "fixes": [
    {"title": "map the value", "edits": [{"span": …, "insert": "search.count |> to_str"}]},
    {"title": "change port type", "edits": [{"span": …, "replace": "in filter: Int"}]}
  ],
  "related": [{"span": …, "note": "port declared here"}]
}
```

Every phase emits this shape (`--json`). Stable codes are documented in
`diagnostics.md` with worked examples — that document is deliberately written to
be excellent LLM context.

## 14. Roadmap / bootstrap plan

| Phase | Deliverable | Proves |
|---|---|---|
| 0 | Spec v0.1 (these docs) + 20 canonical example programs (`examples/`) | design reads well to humans *and* models — test: can an LLM write correct KUPL from LANGUAGE.md alone? |
| 1 | Rust workspace: lexer, parser, CST/AST, `kupl fmt`, resolver, type/effect checker, tree-walk interpreter, REPL | the language exists; iterate on syntax while it's cheap |
| 2 | Contracts/examples/laws in `kupl test`; manifests; `kupl context`; JSON diagnostics | the AI-first tooling story end-to-end |
| 3 | KIR + KVM (bytecode compiler, register VM, scheduler, per-component GC, supervision) | production interpreter performance; visual tools can target this |
| 4 | Native backend (Cranelift dev / LLVM release), libkrt, WASM target | deployment story |
| 5 | `tensor` dialect + device runtime (Metal first, then PTX/SPIR-V) | heterogeneous-hardware story |
| 6 | LSP, `kupl diff/patch`, package registry; visual-tool integration hardening | ecosystem |
| 7+ | Template JIT for KVM; self-hosting front end in KUPL (system tier proves itself) | maturity |

Guiding rule for sequencing: **every phase ships something a user (or a visual
tool, or an AI agent) can actually run.** No multi-year dark tunnels.
