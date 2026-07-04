# KUPL — Vision & Design Principles

**KUPL (K Universal Programming Language)** is an AI-first, component-oriented,
general-purpose programming language with a complete toolchain: REPL, interpreter,
virtual machine, bytecode compiler, and native machine-code compiler.

KUPL is **open source and free forever**. Anybody can join, fork, modify, and
distribute it. Visual, AI-collaborative development environments can be built
on top of KUPL, but the language itself never depends on any of them.

---

## Why a new language

Every piece of software we build is made of components: a web app has an
authentication component, header, footer, list, search; an architecture has a
database component, connection component, logic component. Hardware is even more
component-oriented. Yet mainstream languages make the *function* or the *class*
the unit of composition and leave "components" to frameworks (React, Spring,
actors, microservices) — each with its own incompatible notion.

Meanwhile, most code today is written *with* AI, and increasingly *by* AI. No
mainstream language was designed for that. Languages optimized for terse human
cleverness (implicit conversions, ambient globals, many ways to write the same
thing) are exactly what makes LLM-generated code hard to verify and hard for
humans to repair.

KUPL makes the **component** the language-level unit of everything, and makes
**machine generability + human repairability** a first-class design constraint.

## The seven pillars

### 1. Components all the way down
The `component` is the universal unit — for UI, services, drivers, and logic.
A component has explicit typed **ports** (inputs/outputs), private **state**,
event **handlers**, an exposed call interface, and declared **capabilities**.
Components compose by **wiring**; they nest hierarchically; every component
instance is an isolated, supervisable actor.

### 2. AI-first, human-repairable
- **One canonical form.** The formatter is part of the language spec; there is
  exactly one way any program is laid out. Diffs are semantic, generation is
  deterministic, style debates don't exist.
- **Local reasoning.** A component plus the contracts it imports is a complete
  unit of understanding. `kupl context <component>` emits the minimal context
  an LLM needs — no whole-repo dumps.
- **Spec travels with code.** `intent` (natural language purpose), `example`
  (executable examples that double as tests), and contract clauses are syntax,
  not comments. AI can regenerate a component from its intent + contract;
  humans can check the code against the stated intent.
- **Explicit everything at boundaries.** Public interfaces are always fully
  typed and effect-annotated. No ambient authority: a component can only touch
  the outside world through capabilities it declares (`requires`).
- **Structured diagnostics.** Every compiler error has a stable code, a precise
  span, an explanation, and machine-readable fix suggestions (JSON mode) —
  errors are an API for both editors and models.

### 3. Full toolchain, not a toy
REPL → interpreter → register-based VM (KVM) with bytecode compiler → native
compiler (via a typed SSA IR, KIR). One language, four execution modes, chosen
per situation: exploration, scripting, deployment, maximum performance.

### 4. Visual layer ready
The compiler emits a **component manifest** (ports, props, contracts, intent,
examples) for every component. Visual tools render manifests as a palette and
canvas; wiring drawn visually is exactly `wire` statements in source. Code is
always the single source of truth — visual editing is just another editor.

### 5. Universal hardware (CPU / GPU / TPU / NPU / future)
Tensors and data-parallel `kernel` functions are in the language, not a
library bolt-on. Kernels are a restricted, analyzable subset that lowers
through KIR to CPU vector code, GPU (PTX/SPIR-V/Metal), and accelerator
backends. `at(gpu) f(x)` is placement, not a rewrite.

### 6. Progressive disclosure of power (three tiers)
- **App tier** (default): automatic memory management, capability security,
  no pointers. What application developers and AI generate day to day.
- **System tier** (`system` components): ownership/borrowing, explicit layout,
  pointers — Rust-class control, gated by `cap.unsafe`.
- **Hardware tier** (`low` blocks): volatile loads/stores, inline `asm`,
  register hints — C-class control, invisible unless you ask for it.

An application developer can spend a career in the app tier and never see a
pointer; a driver author can reach the metal without leaving the language.

### 7. Open forever
Apache-2.0 (or MIT) licensed spec, compiler, VM, runtime, and standard
library. Vendor-neutral, platform-independent. Governance goal: a small spec,
a reference implementation, and a conformance test suite so forks stay
compatible by choice.

## The visual layer

KUPL is designed so that visual builders can sit on top of it: human-AI
collaborative environments where a developer can *demonstrate* what they
want by composing visual and non-visual
components when prompting falls short, and the AI fills in everything else,
with complete KUPL code generated behind the scenes, always editable by hand.
KUPL's obligations to such tools are exactly: component manifests, canonical
form, semantic diff, structured diagnostics, and hot-swappable components in
the VM. No visual tool is ever required to use KUPL.

## Inspirations (steal the best from everywhere)

| From | We take |
|---|---|
| Erlang/Elixir | actor isolation, supervision trees, per-actor heaps, hot code swap |
| Rust | ownership/borrowing (system tier), Result/Option, traits-as-contracts, cargo-quality tooling |
| Go | small language, fast builds, one obvious way, batteries-included std |
| TypeScript/React | component/props/events mental model developers already know |
| Haskell/Koka | pure-by-default functions, effect rows, inference |
| Eiffel | design-by-contract (pre/post/invariants) |
| Python | readability, REPL culture, examples-as-docs (doctest) |
| APL lineage / NumPy | first-class arrays/tensors, kernel programming |
| Lua | small register-based VM design |
| MLIR/LLVM | dialect-based lowering to heterogeneous hardware |
| Smalltalk (Squeak/Pharo) | live environment, visual composition over a real language |
| E/Pony | capability security |

## What "Universal" means (and deliberately doesn't)

KUPL claims universality on five axes, and rejects it on one:

- **Domains** — one `component` model for UI, services, drivers, ML pipelines.
- **Hardware** — CPU/GPU/TPU/NPU via kernels and placement (`at`), one semantics.
- **Altitude** — app tier to inline `asm` in one language, progressively disclosed.
- **Execution modes** — REPL, interpreter, VM, native: explore-to-ship without
  switching languages.
- **Authorship** — designed equally for human and machine writers and repairers.
- **Platforms & ecosystems** — universal *by strategy, not by wish*: see
  `PLATFORMS.md` (capability-provider profiles, honest compile-time target checks),
  `INTEROP.md` (foreign code behind adapter components), `DISTRIBUTION.md`
  (typed ports across machines), `UI.md` (one view protocol, swappable renderers).

**Not** universal in paradigm or style: no inheritance, no macros (v1), no
exceptions, one canonical form, one way to do most things. That narrowness is the
point — fewer degrees of freedom per token is what makes machine generation
reliable and human repair fast. KUPL is universal in where it runs and what it can
build, opinionated in how code is written.

## Non-goals (v1)

- Not a research playground: no dependent types, no macros in v1 (canonical
  form + codegen by AI reduces the need; revisit later).
- Not source-compatible with any existing language.
- No implicit conversions, no inheritance (composition + contracts instead),
  no exceptions for control flow (Result + `?`), no null.

## Documents

- `LANGUAGE.md` — the language: model, types, effects, keywords, grammar, semantics, examples.
- `TOOLCHAIN.md` — every phase: lexer, parser, canonicalizer, resolver, type/effect checker, KIR, optimizer, KVM bytecode + VM, native backend, runtime, REPL, CLI, LSP, package manager.
- `UI.md` — the view protocol: `render` blocks, typed layout/style, renderer adapters (wgpu reference, DOM, native).
- `INTEROP.md` — foreign code behind adapter components: C ABI, WASM sandbox, sidecars, `kupl bridge`, kser interchange.
- `DISTRIBUTION.md` — components across machines: explicit placement, portability rules, kser wire format, partition-as-supervision.
- `PLATFORMS.md` — target profiles, capability-provider adapters, lifecycle-as-ports, packaging (server/desktop/web/mobile/embedded).
- `VISUAL-TOOLS-CONTRACT.md` — numbered obligations (C1–C8+) to visual tools: canonical form, node IDs, `@meta` blocks, manifests, hot swap, trace hooks.
