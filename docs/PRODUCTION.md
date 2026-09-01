# Running KUPL in Production

An honest, code-verified account of KUPL's security posture, resource limits, and
operational behavior — and, just as importantly, of what is **not** yet production-
grade. Every claim here is checked against the implementation; where something is an
alpha-stage gap, this document says so plainly.

KUPL is **1.0-alpha**. It is feature-complete and internally consistent (four
execution engines held byte-identical, verified on every build); real, opt-in
multi-threaded concurrency (`concurrent component`), retry-hardened real-provider AI
calls (interp/vm and native alike), a self-hostable package registry (`kupl pkg
publish`), an opt-in memory cap that covers `kupl native` output too, and a
persistent build cache for `build`/`bundle`/`native` are all real, not just
designed. It has not been battle-tested at scale, has no THIRD-PARTY package
ecosystem yet (self-hosting your own registry works today), and its real-provider AI
path, while retry-hardened, is still only lightly tested against live providers.
Read the [Known Limitations](#known-limitations) section before depending on it.

---

## Security model

### What is bounded

KUPL enforces a small set of hard resource limits so that a malformed input or a
runaway program fails cleanly instead of taking down the host. Each is enforced in
**every** engine that can hit it.

| Limit | Value | Where enforced |
|---|---|---|
| Recursion / call depth | `10_000` frames | interpreter (`interp.rs` `MAX_CALL_DEPTH`), KVM (`vm.rs`, `frames.len() >= 10_000`), native (`cgen.rs`, thread-local `k_depth`) |
| Tensor length | `100_000_000` elements | interpreter (`interp.rs` `MAX_TENSOR_LEN`), native (`cgen.rs` `K_MAX_TENSOR_LEN`) — `zeros`/`arange` reject oversized requests |
| JSON nesting depth | `500` levels | JSON parser (`json.rs` `MAX_JSON_DEPTH`), native (`cgen.rs` `K_MAX_JSON_DEPTH`) |
| Expression / type nesting depth | `128` levels | parser (`parser.rs` `MAX_EXPR_DEPTH`) — deeply nested source (`[[[…]]]` or `List[List[…]]`), AND a long flat chain of the same operator (`1 + 1 + 1 + …`, `x.f().f().f()…`, `a \| b \| c \| …`, `------x`, etc. — these build an AST just as deep without ever writing a bracket) is a clean `K0121` instead of a stack-overflowing crash in every downstream recursive consumer (fmt, check, interp, compiler) |
| Component messages per settle | `1_000_000` | interpreter/KVM/native (`interp.rs` `MAX_COMPONENT_MESSAGES`) — a `wire` cycle panics cleanly rather than draining forever |
| Component message payload size | `10_000_000` bytes | interpreter/KVM/native (`interp.rs` `MAX_COMPONENT_MESSAGE_BYTES`) — a single oversized `emit` payload panics cleanly ("unbounded growth in a `wire` cycle?"), distinct from the message-count cap above |
| Timer fires per `advance` call | `10_000_000` | interpreter (`interp.rs` `MAX_ADVANCE_FIRES`) — a runaway timer configuration in an `example`/test panics cleanly rather than firing forever |
| Regex backtracking | `10_000_000` steps | shared matcher (`regex.rs` `MATCH_BUDGET`) + native (`cgen.rs` `kre_steps`) — a catastrophic-backtracking (ReDoS) pattern errors cleanly instead of hanging |
| BigInt / Rational magnitude | `20_000` limbs (~180,000 decimal digits) | arbitrary-precision arithmetic (`bigint.rs` `MAX_BIGINT_LIMBS`, native `K_MAX_BIGINT_LIMBS`) — an operation that would exceed the cap (e.g. `big(2).pow(1_000_000)`) panics cleanly instead of exhausting memory |
| Rational GCD-reduction input | `100` limbs | exact-fraction construction (`rational.rs` `MAX_GCD_INPUT_LIMBS`, native `K_MAX_GCD_INPUT_LIMBS`) — an oversized numerator/denominator errors before an expensive GCD computation runs |
| Registry response size | `10 MiB` | package fetch (`registry.rs` `MAX_REGISTRY_RESPONSE_SIZE`, via curl `--max-filesize`) — a misbehaving or malicious registry can't force an unbounded response to be buffered into memory |
| AI provider request | `120s` per attempt, `10 MiB` response, `3` retries | `ai.rs` `http_post` (shared by `anthropic`/`openai`/`ollama`) — retries transient failures (network error, HTTP `429`/`500`/`502`/`503`/`504`) with exponential backoff; a real 4xx fails immediately, no retry |
| `.kx` / bundle module length | validated ≤ remaining bytes | loader (`kx.rs`) — a tampered/corrupt count or trailer length is rejected, never over-allocated or sliced out of bounds (no OOM / panic) |
| LSP message size | `64 MiB` | language server frame reader (`lsp.rs` `MAX_MESSAGE_LEN`) — refuses an oversized `Content-Length` before allocating |
| LSP workspace file scan | `5_000` files | language server (`lsp.rs` `MAX_WORKSPACE_FILES`) — caps how many files a single workspace scan tracks |
| `http_serve` request head / body | `64 KiB` head, `10 MiB` body | interpreter (`interp.rs` — head-read loop, `MAX_BODY_SIZE`), native (`cgen.rs` — the same 64KB head cap, `K_MAX_HTTP_BODY`) — a request head or `Content-Length` body larger than the cap is truncated rather than fully buffered |
| String contents | no NUL bytes | lexer rejects `\0` and raw NUL (diagnostic `K0008`) — keeps strings safe across the native C runtime, which is NUL-terminated |
| Wall-clock execution time (`kupl run`/`--vm`) | opt-in, `--timeout=<seconds>`, off by default | CLI watchdog thread (`main.rs`/`timeout.rs`) — a hard process kill with a clean `K0901` diagnostic and exit code `124` after the deadline; not enabled unless requested |
| Total memory allocation (`kupl run`/`--vm`) | opt-in, `--max-memory=<MB>`, off by default | a custom `#[global_allocator]` (`main.rs`/`memcap.rs`) caps the interpreter/KVM/`.kx` engines uniformly (they share one process); a `K0902` diagnostic prints before the process aborts on the first over-cap allocation |
| Total memory allocation (`kupl native` output) | opt-in, `KUPL_MAX_MEMORY_MB=<N>` env var, off by default | `cgen.rs` `k_alloc` — the single choke point every native allocation goes through — checks a cumulative byte counter against the cap; same `K0902` diagnostic, then a clean `exit(101)`. Separate from `--max-memory` above because a compiled native binary is a standalone process with no `kupl` wrapper to parse a CLI flag through to |

### Crash safety

A top-level panic hook (`main.rs`, `std::panic::set_hook`) converts any internal
panic into a single clean line — `kupl: internal compiler error … — this is a bug in
KUPL, not your program` — and exits `101`. You should never see a Rust backtrace or a
raw abort. The interpreter runs on a 2 GiB stack so the depth guard is reached before
the native stack is exhausted. The CLI subcommands (`run`, `check`, `fmt`, `build`,
`native`, `dis`, `diff`, `manifest`, `context`, `new`, `lsp`, …) have been crash-
fuzzed over hundreds of malformed inputs; they emit diagnostics, never panics.

### Effects

KUPL has a **static** effect discipline. A function that performs side effects must
declare them with `uses`, and the checker propagates the declaration requirement
through ordinary function calls (private helpers, top-level `pub` functions, and a
component's own construction). The two effects are:

- **`io`** — any interaction with the outside world through the standard builtins
  (`print`, `eprint`, reading args, `exec`, `now`, file/stdin/HTTP operations).
- **`ai`** — calling an `ai fun` (the `ai` keyword is itself the boundary
  declaration; a `pub fun` that calls one must declare `uses ai`).

**Known gap (narrowed):** propagation through a *component instance's* own exposed
method (`s.method()`) is precise for both of the two syntactically-provable cases —
`let s = SomeComponent()` immediately followed by using `s` in the same function, and a
component-typed function parameter (`fun helper(s: SomeComponent) { s.method() }`) —
but does **not** follow the call when `s` arrives more indirectly: stored in a record
field or component prop, returned from another function, wrapped in a generic
(`Option[SomeComponent]`, a match-arm's own pattern-bound name, etc.), or reassigned
after its initial `let`. In those remaining cases the checker has no type information to
resolve which component's method is being called, so a `pub fun` whose only use of an
effect flows through one of them needs no `uses` declaration at all, and a caller
further up the chain that correctly declares the effect anyway will see a misleading
"declared but unused" warning. This is a deliberate tradeoff (see `effects.rs`'s own
top-of-file doc comment and production-hardening PR-it707/PR-it1124/PR-it1129/PR-it1130),
not an oversight: naively flagging every unresolved method call would force ordinary
component-constructing code to over-declare effects it may not have. It does **not**
weaken the threat model below — the effect system was never a runtime sandbox — but it
does mean an *absence* of a `uses` declaration is not proof a function performs no side
effects when component instances are involved in one of these remaining indirect ways.

### Threat model — read this before running untrusted code

**KUPL is not a sandbox.** The effect system is a *compile-time* discipline for
reasoning about and documenting side effects — it is **not** a runtime confinement
mechanism:

- A program that declares `uses io` can do arbitrary I/O, including `exec` (spawning
  subprocesses) and network access. There is no syscall filtering, no filesystem
  jail, and no capability revocation at runtime.
- The resource limits above bound **recursion, tensor allocation, JSON nesting, and
  LSP frame size**. `kupl run`/`--vm` can be given an opt-in `--timeout=<seconds>`
  wall-clock limit and an opt-in `--max-memory=<MB>` allocation cap (both off by
  default). Neither bounds file-descriptor count or output volume, and
  `--max-memory` has no effect at all on `kupl native`'s generated executable (a
  separate standalone process — see Known limitations for what to use instead). A
  program can still allocate until the OS kills it, or loop forever, if these
  flags aren't set.

**Do not run untrusted KUPL as a way to sandbox it.** If you need to execute
untrusted code, run KUPL inside an OS-level sandbox (container, VM, seccomp, cgroup
memory/CPU limits) — the same as you would for any other general-purpose language.

---

## Operations

### The four engines

KUPL runs the same program four ways, all byte-identical (this equivalence is the
project's core invariant, checked on every build):

| Engine | Command | Use when |
|---|---|---|
| Tree-walking interpreter | `kupl run file.kupl` | development, the reference semantics |
| KVM register bytecode VM | `kupl run --vm file.kupl` | faster execution of the same program |
| `.kx` compiled module | `kupl build file.kupl` then run | precompiled distribution |
| Native machine code | `kupl native file.kupl -o bin` | fastest; emits C, compiles with the system `cc` |

`kupl bundle` produces a self-contained executable from a multi-file program.

### Exit codes

- `0` — success.
- `1` — a diagnostic error (parse/type/effect error), a failed run, or a load error.
- `101` — an internal compiler error caught by the panic hook (please report it).

The exit code of `kupl run` on a program that calls `exit`/returns a code reflects
that program's own status.

### Environment variables

AI functions select a provider at call time via environment variables (this is what
makes `ai fun`s testable without a network):

| Variable | Effect |
|---|---|
| `KUPL_AI_PROVIDER` | `anthropic` (default), `openai`, `ollama`, `echo` (returns the composed prompt), or `mock` |
| `KUPL_AI_MOCK` / `KUPL_AI_MOCK_<FUN>` | canned response for the mock provider; if set, the mock is used regardless of provider. `<FUN>` is the upper-cased function name |
| `ANTHROPIC_API_KEY` | credential for the `anthropic` provider |
| `KUPL_AI_BASE_URL` | override the provider base URL (e.g. an OpenAI-compatible endpoint) |
| `KUPL_AI_MODEL` | override the model id |
| `KUPL_AI_MAX_RETRIES` | extra retry attempts (beyond the first) for a transient real-provider failure — network error or HTTP `429`/`500`/`502`/`503`/`504`; default `3`, exponential backoff |

If a mock variable is set, an `ai fun` returns the canned response with no network
call — the recommended way to make AI-using programs deterministic in tests and CI.

### Determinism notes

Valid programs produce **byte-identical** output on all four engines. Two narrow
categories are intentionally engine-dependent, and only ever on **error paths** —
they never affect the value a correct program computes:

- **Malformed-input error *message text*** for JSON parsing and `ai fun` response
  conversion may differ between the native engine and the interpreter (the native C
  runtime produces a more generic message). The accept/reject *decision* and the
  resulting value are identical — match on `Ok`/`Err` structurally, not on the string.
- **Case conversion** (`to_upper`/`to_lower`) is **ASCII-only** by definition, so it
  is identical across engines (the native runtime cannot replicate Rust's full
  Unicode casing, so the common ASCII subset is the contract).

### Performance characteristics

Output is byte-identical across engines, but *time/space complexity is not part of that
contract* — pick the engine and idiom that fit the workload:

- **In-loop accumulation.** `Str` and `List` are immutable values, so `s = s + x` or
  `xs = xs.push(x)` conceptually builds a new value each step. The **interpreter and
  KVM** detect the common self-append shape and mutate in place when the value is
  uniquely owned (no other binding aliases it), so a build loop is **O(n)**. The
  **native** backend has no ownership tracking (its C runtime copies on every append),
  so the same loop is **O(n²)** — e.g. pushing 100 000 elements one at a time takes
  milliseconds on `run`/`--vm` but seconds compiled. A value shared by another binding
  falls back to copying on every engine (value semantics are always preserved).
- **`Map`/`Set` in-loop `.insert()` is a narrower case — it stays O(n²) even on the
  interpreter and KVM.** `m = m.insert(k, v)` / `s = s.insert(v)` have the same
  uniquely-owned in-place fast path as `Str`/`List` above, but — unlike append/push —
  `.insert()` must also check whether the key/value is already present, and that
  duplicate check is an O(n) linear scan of the map/set on **every** call. The fast
  path removes the per-call clone (so allocation cost is amortized O(1), not O(n)) but
  **not** the scan, so an n-iteration build loop remains O(n²) **time** on every
  engine, not just native — e.g. inserting into a `Map` one entry at a time took
  0.4s/6.4s for n=5,000/20,000 (~15.5x time for 4x size) on `run`, matching O(n²)
  almost exactly. `Set` has an escape hatch — `Set(list)` bulk-constructs in genuine
  O(n log n) — but `Map` currently has no analogous bulk constructor, so there is no
  way to build a large `Map` other than accepting O(n²).
- **Guidance.** For large `Str`/`List` accumulation on the native backend, or any
  large `Map`/`Set` accumulation on every backend, prefer a single bulk pass — `.map` /
  `.filter` / `.fold` / `.flat_map` over a source collection, or `.join` to assemble a
  string, or `Set(list)` to bulk-build a Set — each of which allocates (and, for
  `Set(list)`, deduplicates) once and is O(n) or O(n log n) on all four engines.
  Reserve element-at-a-time `push`/`+`/`Set.insert` loops for small n or the interp/KVM
  engines, and element-at-a-time `Map.insert` loops for small n on every engine.

---

## Known limitations

Being honest about what is not yet production-grade:

- **The real-provider AI path is now retry-hardened, but still only lightly
  battle-tested.** The `anthropic`, `openai`, and `ollama` providers share one
  `http_post` (`ai.rs`) with a 120s per-attempt timeout and a 10MiB response cap; it
  now retries transient failures (network errors and HTTP `429`/`500`/`502`/`503`/`504`)
  with exponential backoff, up to `KUPL_AI_MAX_RETRIES` extra attempts (default `3`).
  A real, non-retryable 4xx (bad request, bad API key, etc.) still fails immediately.
  This is verified against a local mock HTTP server (`ai.rs` tests), not a live
  provider — the bulk of the test suite still exercises the **mock** provider (`KUPL_AI_MOCK`).
  Treat live AI calls as experimental; pin them behind the mock in CI.
- **Mostly single-threaded execution, with an opt-in exception.**
  `par_map`/`par_filter` DO spawn real OS threads (`std::thread::spawn`)
  when the callback is a pure top-level function and the list is large
  (≥ 256 elements). A `concurrent component` (see the language
  reference's own §7.2) also runs on a real OS thread — `expose` calls
  block for a real reply, and wires can deliver messages into its inbox
  non-blocking — but it's opt-in, has a real portability restriction on
  its public surface (K0306), can't yet be a wire's source, and its
  recurring timers only fire during its initial startup burst. As of
  `docs/design/CONCURRENCY_V2.md` §4.3, a top-level `concurrent`
  instance is multiplexed onto a small, fixed pool of worker threads
  rather than getting a dedicated thread of its own (a `concurrent`
  component nested inside another actor's own subtree still gets a
  dedicated thread) — an internal scheduling detail with no effect on
  program behavior, only on how many OS threads a program with many
  actors actually costs. Everything else — the structured `par { … }`
  fork-join block itself, `par_each`, and an ORDINARY (non-`concurrent`)
  component instance's own handler dispatch — remains single-threaded (a
  general-purpose M:N scheduler for those is a later, larger step; see
  `docs/design/ASYNC.md`).

  **The precise safety guarantee, stated exactly, not overclaimed or
  underclaimed** (`docs/design/CONCURRENCY_V2.md` §4.5): a `concurrent
  component` boundary prevents DATA RACES on any value crossing it — every
  message is deep-copied (`parallel.rs::to_portable`/`from_portable`), so
  no two actors, nor an actor and its coordinator, ever hold a live,
  mutably-shared reference to the same data. It does **not** prevent
  DEADLOCK between two actors each blocked in a `Call` to the other — that
  class is *refused*, not prevented by construction: a call that would
  close a cycle back to an actor already waiting (transitively) on the
  caller is rejected immediately with a clean panic
  (`"concurrent call cycle through instance {id}"`,
  `docs/design/ASYNC.md` §8.4) rather than hung on forever. It does not
  (yet) provide Swift/Kotlin-style *structured* lifetime scoping (an
  actor's lifetime isn't tied to an enclosing scope the way a structured-
  concurrency task group's is) — KUPL's static, declarative `let x =
  Comp()` spawning is closer to "structured by construction" than an
  unstructured `go`/`Thread::spawn` call, but this is a real, named gap,
  not a claimed equivalence.
- **`kupl build`/`bundle`/`native` now have a persistent, content-addressed build
  cache — `kupl run`/`kupl run --vm` do not.** `~/.kupl/build-cache/` holds the
  final artifact bytes for a given (source content, compiler identity) pair;
  an unchanged rebuild skips `compile_module`/codegen/`cc` entirely and reuses
  the cached bytes — for `kupl native`, whose dominant cost is invoking `cc`,
  this is roughly a 50x speedup on a repeat build in local testing. The cache
  key folds in the running `kupl` binary's own content hash (not just its
  version number, which this project does not bump per internal change), so a
  `cargo build` that changes the compiler always invalidates every prior
  entry — a stale hit that silently serves output from an old compiler is not
  possible by construction. `kupl run`/`kupl run --vm` are NOT cached: both
  need the checked `Program` itself (for `ProgramDb`/`ProgramImage`, powering
  `par_map`/`par_filter`'s real-thread fast path and `concurrent component`),
  which has no stable serialized form today, so there is nothing a bytecode-
  only cache could let them skip.
- **Alpha stability.** The language and `.kx` binary format are versioned (a `.kx`
  built by a different compiler version is rejected with a clear message), but no
  long-term source or ABI stability is promised yet.
- **`weight distributed`/`kupl node` is AUTHENTICATED, NOT ENCRYPTED —
  do not run it across an untrusted network.** `docs/design/AGENTS.md`
  §4/`docs/design/DISTRIBUTION.md`: a `concurrent` actor can now be
  spawned on a separate `kupl node` process over a real TCP connection
  (`interp.rs::ActorRoute::Distributed`). A shared-secret token (constant-
  time compared, `distribution.rs::tokens_match`) gates who may connect,
  spawn actors, or exchange messages at all — closing the "any host on
  the network can silently spawn arbitrary components" failure mode a
  fully open listener would have. But the token itself, and every
  message after it (including full `PortableValue` payloads), travels in
  PLAINTEXT — there is no TLS/mTLS in this slice. Put a `kupl node`
  behind a VPN, SSH tunnel, or service mesh if the link crosses anything
  you don't already trust; hand-rolling real TLS from scratch, under
  time pressure, without expert review, was deliberately judged too
  risky to attempt alongside this feature (matching this project's own
  "never roll your own crypto for a security-critical primitive without
  expert review" discipline elsewhere). This is also a NARROW slice of
  the full `docs/design/DISTRIBUTION.md` vision: one statically-
  configured node per connection (`KUPL_DISTRIBUTED_NODE` env var), no
  `cap.Cluster` capability, no dynamic cluster membership, no deployment
  manifest, no `at node(...)` placement syntax (still K0309-rejected) —
  those remain "Phase 6+," not implemented by this slice.

For the full design-vs-implemented audit, see [`GAPS.md`](GAPS.md). For the language
itself, see [`reference/LANGUAGE-REFERENCE.md`](reference/LANGUAGE-REFERENCE.md); for
every command and flag, [`reference/CLI.md`](reference/CLI.md).
