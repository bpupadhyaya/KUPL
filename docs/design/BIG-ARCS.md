# KUPL big-arc roadmap

_The remaining "as good or better than the 9 languages" work, designed in parallel
(2026-07-04) and sequenced for implementation. Each arc is a multi-iteration effort
implemented one-by-one in the /loop (they all touch the same core files, so parallel
implementation would conflict — design was parallel, implementation is sequential)._

## Recommended sequence
1. **Sized numerics i8..u64, f32 (type-system depth)**
2. **Package / dependency system (ecosystem)**
3. **Real-thread concurrency for par { } and par_map/par_filter**
4. **Native components + KIR (audit #2, performance)**

## Rationale

Verified against source, all four designs are honest and shippable, but they differ sharply in risk-to-invariant and in how they interact. I sequenced by three axes: (1) risk to interp==KVM, (2) whether an arc de-risks or churns the arcs after it, (3) how tractable a clean first slice is.

SIZED NUMERICS FIRST. Decisive reason: it is the only arc that MUTATES the `Value` and `Ty` enums, and two later arcs mirror `Value` variant-for-variant — the concurrency arc's `PortableValue` and (already today) `cgen.rs`'s C `KValue`. Landing `SizedInt(i128,IntW)`/`F32(f32)` into `Value`/`Ty` first means those arcs are built once against the final variant set instead of being reopened later to add cases (Rust's exhaustiveness will otherwise force back-edits into finished work). Its byte-identity story is also the cleanest of the four and thus the best way to open a higher-risk campaign phase: I confirmed `vm.rs:11` imports `raw_binary_op`/`shared_method` and calls them at 401/716, so all arithmetic/method semantics live in shared functions and interp==KVM is free by construction; i128 storage fits inside the existing 24-byte `Ctor` variant so `size_of::<Value>()` does not grow; native defers with a clear error exactly like the existing ai/json precedent I verified at `cgen.rs:21`. Impact-wise it closes a real "as good as the 9 languages" gap (Go/Rust/C/Zig all have sized ints + f32 for binary formats/interop; KUPL has none).

PACKAGE SYSTEM SECOND. It is the single lowest-risk arc — "irrelevant by construction" to the invariant because every change lives above the engines (I confirmed `kupl.toml` is read nowhere but `main.rs`, and `loader.rs` `use` resolution is pure filesystem producing one merged `Program` all four engines already consume). It is fully independent of enum churn, so its slot is free; placing it second lets the campaign bank a safe ecosystem win (multi-package, the missing table-stakes feature) right after the enum foundation is set, before the two harder engine-touching arcs. Honest caveat: iter 2's name-mangling pass must walk every AST reference site, which is the fiddly part — but misses are loud unresolved-name errors, not silent divergence.

CONCURRENCY THIRD. Real value (first true OS-thread parallelism) and low invariant risk because it is a gated, additive fast-path that falls through to today's sequential `shared_method` whenever any precondition fails. I verified its two load-bearing facts: `ast.rs` has 0 `Rc`/`RefCell` (so the program image is Send+Sync) and `effects.rs:85` already computes the purity map it needs and merely discards it. It consumes the now-stabilized `Value` (its `PortableValue` mirror covers `SizedInt`/`F32` from day one). Honest limit: v1 only accelerates `xs.par_map(pure_named_fun)` over large plain-data lists; the concurrent-AI payoff case stays sequential. That capped upside is why it sits third, not first.

NATIVE COMPONENTS LAST. Highest risk (its own author marked it risk:high) and the biggest arc: it must reproduce the entire deterministic scheduler in C — creation-order instance ids, 0..ninsts @start order, FIFO queue, first-match handler scan, push-order wire fan-out, (time,instance,decl) timer tie-break, run_timers(100) bound, print_unwired format, [supervise] text, plus setjmp/longjmp supervision. Every one of those is a fresh divergence source for the sacred pair. Doing it last means Value is already stable (its iter-4 native-sized work and this arc both touch `cgen.rs`, so coordination is cheapest when sized-native is already settled) and the team has three arcs of differential-harness experience. It is also the biggest differentiator (compiling an actor-model component system to native), so it earns being the capstone rather than the opener. Correctly, its design defers KIR entirely — KIR is an optional post-profiling performance layer, never a prerequisite.

Alternative I weighed and rejected: opening with Package as the absolute-safest warm-up. Package is genuinely the lowest-risk arc, but it de-risks nothing downstream, whereas Sized-first stabilizes the shared enum that two other arcs mirror — a compounding benefit that outweighs Package's marginally safer opening.

## Per-arc designs

- [1 · Sized numerics](bigarcs/1-sized-numerics.md)
- [2 · Package system](bigarcs/2-package-system.md)
- [3 · Real concurrency](bigarcs/3-real-concurrency.md)
- [4 · Native components](bigarcs/4-native-components.md)

## Consolidated roadmap (from synthesis)

## KUPL Big-Arc Roadmap (from HEAD ab93e66, 20,368 LOC, 129 tests)

Honest framing: these four arcs are each ~5 iterations and materially larger and higher-risk than the breadth slices shipped so far. Three of the four touch an execution engine; one reproduces the entire deterministic scheduler in C. Sequenced below by risk-to-invariant, downstream de-risking, and tractability. Verified against source before sequencing.

---

### 1. Sized numerics i8..u64, f32 — FIRST (effort ~5, risk MEDIUM)
**Why first:** the only arc that mutates the `Value`/`Ty` enums, which the concurrency arc's `PortableValue` mirror and `cgen.rs`'s C `KValue` mirror both track — landing the variants first stops those arcs being reopened later. Byte-identity is nearly free: `vm.rs:11` imports `raw_binary_op`/`shared_method` (called 401/716), so all semantics live in shared functions. i128 storage fits inside the existing 24-byte `Ctor` variant (no enum growth). Closes a genuine table-stakes gap vs the 9 languages.
- **Iter 1:** integer-width skeleton end-to-end (value/type/lexer/parser/check/interp/KVM/kx) + native clear-error defer. *(the shippable first slice — see first_iteration_prompt)*
- **Iter 2:** wrapping/saturating/bitwise methods, sized↔sized + Int↔sized conversions, width-aware to_hex/to_binary.
- **Iter 3:** f32 (`Value::F32`, `Ty::Float32`, `1.5f32`, f32 arithmetic/Display, Float↔f32). Shares enum churn with iter 1 — do soon after.
- **Iter 4:** native codegen — remove the cgen defer, C `intN_t`/`__int128` range checks + f32 printf parity; extend differential harness to native (fun-main). *Coordinate cgen.rs with the native-components arc.*
- **Risk:** broad-but-shallow exhaustive-match fan-out; Rust catches misses at compile time (loud). u64→Int edge must panic (documented).

### 2. Package / dependency system — SECOND (effort ~5, risk MEDIUM, invariant-risk ~ZERO)
**Why second:** lowest risk to the sacred pair — every change is above the engines (verified: `kupl.toml` read only in `main.rs`; `loader.rs` `use` resolution is pure filesystem producing one merged `Program`). Independent of enum churn, so its slot is free; banks the missing ecosystem table-stakes right after the enum foundation.
- **Iter 1:** `src/manifest.rs` (zero-dep TOML subset reader) + local `path` dependency resolution in the loader; flat merge kept; improved collision diagnostic. Full backward compat (no-`[dependencies]` programs load byte-identically).
- **Iter 2:** `src/resolve.rs` package-boundary name-mangling pass (the real namespacing fix) — lifts the flat-namespace collision constraint.
- **Iter 3:** `kupl.lock` + version assertions, reusing `encoding::hash_fnv`.
- **Iter 4:** `kupl pkg` add/fetch/vendor over system curl (reusing the `ai.rs` curl pattern); registry just populates a local path.
- **Risk:** iter 2's mangling must walk every AST reference site; misses are loud unresolved-name errors, and mangled names must be de-mangled for display.

### 3. Real-thread concurrency (par_map/par_filter) — THIRD (effort ~5, risk MEDIUM)
**Why third:** first true OS-thread parallelism, low invariant risk (gated additive fast-path; falls through to today's sequential `shared_method` on any doubt). Verified enablers: `ast.rs` has 0 `Rc`/`RefCell` (program image is Send+Sync); `effects.rs:85` already computes the purity map it needs and discards it. Consumes the now-stable `Value` (its `PortableValue` covers `SizedInt`/`F32` from day one). Do NOT pursue blanket Rc→Arc (RefCell isn't Sync; whole-codebase rewrite).
- **Iter 1:** expose the effects purity map; `PortableValue` Send mirror; `Arc<ProgramImage>`; `try_par_map` gated on pure named-fun callback + all-portable elements + length threshold; wire at interp.rs:2499 and vm.rs:715; gather by index; lowest-index panic. *(shippable slice)*
- **Iter 2:** par_filter (same machinery). **Iter 3:** portable pure closures. **Iter 4:** `par { }` fork-join, all-pure case. **Iter 5:** thread-pool + per-thread image cache, threshold tuning.
- **Honest limit:** v1 accelerates only `xs.par_map(pure_named_fun)` over large plain-data lists; concurrent-AI/HTTP is out of scope. Must NOT interleave with any Value/Env-representation arc.

### 4. Native components (+ deferred KIR) — LAST (effort ~5, risk HIGH)
**Why last:** highest risk and biggest arc — reproduces the whole deterministic scheduler in C (creation-order instance ids, 0..ninsts @start order, FIFO queue, first-match handler scan, push-order wire fan-out, (time,instance,decl) timer tie-break, run_timers(100) bound, print_unwired format, [supervise] text, setjmp/longjmp supervision). Verified: all six component ops stub to `k_panic` at `cgen.rs:266-268`; the fix is a C-side runtime, not a new IR. Do it when `Value` is stable, sized-native cgen work is settled (both touch cgen.rs), and the team has three arcs of differential-harness experience. Biggest differentiator, so it earns the capstone slot. **KIR is explicitly deferred** — an optional post-profiling perf layer, never a prerequisite.
- **Iter 1:** single-component apps (instance array, global `k_cur_inst` save/restore, `COMPS[]`, `k_instantiate`, StateGet/StateSet, @start, app-entry driver); children/wires/emit/timers defer with clear errors.
- **Iter 2:** multi-component (MakeInstance/WireOp/EmitOp, FIFO queue, drain, print_unwired). **Iter 3:** virtual-clock timers. **Iter 4:** supervision (setjmp/longjmp). **Iter 5+ (optional, gated on profiling):** KIR typed-SSA unboxing; native json/regex/csv C-mirrors; native GC.
- **Risk:** every scheduler-order rule is a fresh divergence source; validate with a cc-guarded inline differential test (native stdout == `kupl run`) plus the all-examples regression. Arena memory (never-free) is retained — safer for byte-identity given bounded execution.

---

**Cross-arc coordination note:** the only real conflicts are (a) sized-numerics iter 4 and native-components both edit `cgen.rs` — sequencing native last makes this cheapest; (b) the package arc and any future stdlib-import arc both touch `use` resolution in `loader.rs`. All four arcs are otherwise self-contained, and each keeps interp==KVM byte-identical either by construction (package: above engines), by shared-function delegation (sized), by gated additive fallback (concurrency), or by verbatim scheduler-order mirroring + cc-guarded differential tests (native).
