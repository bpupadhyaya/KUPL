# KUPL Interoperability Strategy

Proposal v0.1 — 2026-07-03.
Status: PROPOSAL — position statement + mechanism design.

**Why this doc exists:** new languages die of ecosystem starvation, not bad
semantics. "Universal" is earned in practice by using the world's existing code
*before* a native ecosystem exists. KUPL's AI-first bet ("models can regenerate
libraries in KUPL cheaply") is real but **additive — it is not the plan**. The plan
is below.

---

## The sanctioned pattern: foreign code lives behind a component

All interop, at every tier, surfaces to applications the same way: a **system-tier
adapter component** that `fulfills` a KUPL contract and `requires` the capabilities
that honestly describe what the foreign code can do.

```kupl
system component LibpqDriver fulfills cap.Sql {
    intent "PostgreSQL driver wrapping libpq via C FFI."
    requires cap.unsafe, net: cap.Net

    // extern decls + unsafe marshalling live here, and only here
}
```

Consequences, by design:

- Unsafety is **quarantined**: FFI exists only in system-tier components; the rest
  of the app stays app-tier and pure. Auditing a codebase for foreign-code exposure
  = listing its system components.
- Foreign code is **contained**: an adapter can't reach anything its declared
  capabilities don't grant. A wrapped C library cannot silently exfiltrate — the
  component wrapping it never received `cap.Net` unless the composition root wired
  it in.
- Foreign code is **supervisable**: a segfaulting native call is trapped at the
  component boundary (crash isolation via subprocess sandboxing for untrusted
  libraries — see levels below); supervision restarts the adapter, not the app.

## Four interop levels (choose per library, all yield adapter components)

| Level | Mechanism | Cost/risk | Use for |
|---|---|---|---|
| **L1: In-process C ABI** | `extern "c"` (already designed, TOOLCHAIN §9) | fastest; shares address space — a crash is your crash | libc, SQLite, shaping/codec libraries, anything small and trusted |
| **L2: Sandboxed in-process** | foreign lib compiled to WASM, run in embedded WASM runtime | near-native; memory-safe by construction | untrusted or crash-prone native libraries |
| **L3: Sidecar process** | foreign runtime as subprocess; typed messages over stdio/socket (the LSP pattern) | IPC latency; total isolation, any runtime | **Python/ML** (torch, numpy), Node.js libraries, legacy services |
| **L4: Embedding KUPL** | libkrt exports C ABI; host app calls KUPL components | — | incremental adoption inside existing C/C++/Swift/Java/Rust apps |

L3 is deliberately boring and deliberately first-class: most ecosystem value
(especially Python ML) is reachable safely this way, and the actor model makes a
sidecar indistinguishable from any other component — it's a component whose mailbox
happens to cross a process boundary. (This is also the stepping stone to
DISTRIBUTION.md: a sidecar is a remote node at distance zero.)

## `kupl bridge` — AI-completed bridge generation

Bridging is exactly the work models do well — mechanical, fiddly, verifiable. The
toolchain makes it a pipeline:

```
kupl bridge c-header libpq.h      → extern decls + typed adapter skeleton + TODO intents
kupl bridge python torch          → sidecar protocol stubs + contract skeleton (L3)
```

1. The generator emits **mechanically-derivable** parts: extern signatures, marshalling
   scaffolds, an adapter component skeleton with `intent "TODO"` holes.
2. An AI agent completes the **semantic mapping** (error-code → `Result` unions,
   ownership conventions, idiomatic contract surface).
3. `law` clauses on the target contract make the bridge **testable**: the property
   tests don't care that the implementation is foreign. A bridge that passes the
   contract's laws is correct in the only sense that matters.

Priority order for official bridges: **C (v1, it's the substrate) → Python sidecar
(ML gravity) → JS/TS (web APIs, via WASM component-model alignment) → JVM (enterprise,
later)**.

## Data interchange (part of interop, often forgotten)

- std ships JSON, and a canonical KUPL binary encoding (**kser** — defined once,
  shared with DISTRIBUTION.md's wire format) with schema-evolution rules.
- `derive`-style codecs come from records' structural types — no annotation
  ceremony; a `type` that can cross a port can be serialized, mechanically.
- Protobuf/Arrow bridges as packages, not core.

## Non-goals

- No source-level compatibility with any language (VISION.md non-goal, reaffirmed).
- No blessed "unsafe everywhere" escape hatch in the app tier — if a team wants raw
  FFI in application code, that's what `system` components are for, and the
  composition root will show it.
