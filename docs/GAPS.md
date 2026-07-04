# KUPL Gap Audit & Enrichment Roadmap

Audited 2026-07-04 against: `docs/design/LANGUAGE.md` (incl. §12 open
questions), the `[design]` markers in `docs/reference/LANGUAGE-REFERENCE.md`,
and known limitations called out in commit messages. Checked off as landed.

## Enrichment campaign (it1–it50) — summary

A 50-iteration enrichment campaign took KUPL from a young four-engine toolchain
to one that closes its four largest audited gaps and compiles nearly the whole
language to native machine code. Every iteration held the **sacred invariant**:
the interpreter and the KVM stay byte-identical (differential tests in
`src/vm.rs`), and the all-examples regression (`kupl run` vs `kupl run --vm`)
stays green — verified on every commit. The arc, by phase:

- **Breadth + AI-native core + effects (early)** — file I/O, JSON, HTTP, regex,
  seeded random, CSV, URL, encoding/time stdlib; the `ai fun` typed-prompt core
  with tool use, agent components, and a deterministic mock provider; the
  hierarchical effect system.
- **Sized numerics (it27–29)** — `i8…i64`/`u8…u64` and `f32`: checked/wrapping/
  saturating arithmetic, width-aware bitwise ops, the full conversion matrix.
- **Package system (it30–32)** — `kupl.toml` local path dependencies with
  namespace isolation (name-mangling), exact version pinning, and a `kupl.lock`.
- **Real-thread concurrency (it33–35)** — `par_map`/`par_filter` over a pure
  named function execute across real OS threads, on both the interpreter and the
  KVM, deterministic and byte-identical to the sequential form.
- **Native components (it36–39)** — the whole component model compiles to machine
  code: state, handlers, children, wires, `emit`, the message-queue/drain loop,
  virtual-clock timers, `supervise` restart-on-failure, and cross-component
  `expose` calls — a C mirror of `vm.rs`.
- **Native numeric surface (it40, it42)** — sized ints (boxed `__int128`) and
  `f32` (shortest-round-trip formatter) compile natively.
- **Native stdlib (it43, it45, it46, it47)** — JSON, CSV, URL/query, regex (a
  full backtracking engine), and HTTP (via system `curl`) all lower to C. The
  native backend now compiles the **entire language except `ai fun`**.
- **LSP (it44, it49)** — hover, go-to-definition, completion, find-references, and
  rename on top of diagnostics — the everyday IDE feature set.
- **Flagship examples (it41, it48)** — `native-showcase.kupl` (sized ints +
  `par_map` + exposes + wires) and `analytics.kupl` (CSV + regex + grouping +
  JSON), each byte-identical on interpreter, KVM, and native.

**Honest remaining gaps:** `ai fun` on the native backend; a hosted package
registry and third-party ecosystem; general async/await + coroutines; the
GPU/kernel and systems/ownership tiers; and the optional KValue-unboxing perf IR
(KIR). These are documented, not hidden.

## Final stretch — prioritized shortlist (it42–50)

The four big arcs (sized numerics, packages, real-thread concurrency, native
components) are complete as of it40. Remaining work, ranked by value ÷ effort:

1. ~~**Native `f32`**~~ — DONE (it42). K_F32 KValue + shortest-round-trip
   display via `strtof`; examples/sized.kupl is fully native. Native numeric
   surface complete (only ai/JSON/CSV/HTTP builtins defer).
2. ~~**Native JSON**~~ — DONE (it43). json_parse + json_stringify ported to the
   C runtime, byte-identical to src/json.rs; examples/json.kupl is fully native.
3. ~~**LSP hover / completion / go-to-definition**~~ — DONE (it44). The language
   server now serves hover (signatures), go-to-definition, and completion on top
   of diagnostics. Remaining IDE polish: rename, find-references, semantic tokens.
4. **Flagship "any software" example(s)** — a non-trivial end-to-end program
   (e.g. a small HTTP/JSON service, or a data pipeline) proving breadth, doubling
   as documentation and a regression.
5. ~~**Native regex**~~ — DONE (it46). src/regex.rs's backtracking engine ported
   to C (parser + greedy/backtrack matcher + all 4 re_ builtins), byte-identical;
   examples/showcase.kupl (regex+JSON+file I/O+par_map) is fully native. Only
   **native HTTP** (system curl) remains among the builtins — then only ai fun.
6. **WASM target** / **stdlib breadth** / **KIR unboxing (perf)** — larger or
   lower-marginal-value; revisit if the above land with iterations to spare.

**Recommended for it42+:** native `f32` (1) first — small, finishes the numeric
story, and makes `sized.kupl` fully native — then native JSON (2), then LSP
completion (3). Everything stays byte-identical across engines.

## Tier 1 — language ergonomics (active)

- [x] **Record update `with`** — `user with age: 36` (design §10 uses it; today K0223)
- [x] **Std lib depth** — List: fold/any/all/sort/take/drop/get/index_of;
      Str: ends_with/replace/chars/repeat/parse_int/parse_float;
      Int: min/max; Float: floor/ceil/round/min/max/pow
- [x] **Component-private functions callable** from handlers/exposes (declared
      but unreachable today)
- [x] **User-code generics** — `fun sort_by[T](xs: List[T], key: fn(T) -> Int)`
      (checker-level instantiation; engines are ready)
- [x] **Map[K, V] and Set[T]** collections (design §3)

## Tier 1.5 — AI-native core (active)

- [x] **`ai fun` typed prompt functions** — intent-bodied functions whose
      return type drives structured output (JSON Schema derived from the
      type); `Result[T, Str]` captures failures; implicit `ai` effect;
      provider-agnostic runtime (anthropic / openai-compatible / ollama /
      deterministic mock via `KUPL_AI_MOCK*`); interpreter + KVM + `.kx`
      (native rejects with a clear error)
- [x] **Tool use** — `ai fun … tools [f, g]` exposes top-level KUPL functions
      to the model; the runtime drives the model↔tool loop (JSON ↔ typed
      values), bounded, scriptable via the mock provider for tests. Real
      providers use native tool calling (Anthropic tool_use, OpenAI tool_calls)
- [x] **Agent components** — conversation state persisted in component state
      across turns; exposes/handlers call tool-using ai funs. Plus **intent
      interpolation**: the `ai fun` intent is an interpolated string evaluated
      in the parameter scope (`intent "Reply to {msg}"`). `echo` debug provider.
      (Known limitation: effects don't propagate across expose/method calls —
      candidate for a future type-aware effect pass.)
- [ ] **Prompt-context builders** — `kupl context` output as a first-class
      value; embeddings + similarity as stdlib
- [ ] **`ai fun` on the native backend** (libcurl or platform HTTP)

## Tier 2 — component model completion

- [x] **Contract-typed requires** — `prop repo: KeyStore` accepts any
      fulfilling component; calls dispatch dynamically through the contract's
      exposes (interpreter + KVM identical). Contract names are types on props,
      params, and `let`/`var`; non-fulfilling injection is K0200. Also fixed a
      pre-existing gap: props are now type-checked when constructing from a
      top-level `fun`. (`examples/di.kupl`)
- [x] **`forall` in laws** — property-based testing: `forall x: Int { … }`
      generates 100 deterministic cases, shrinks failures to a minimal
      counterexample. Generators for Int/Bool/Float/Str/List/Option/records.
      Plus top-level `law "…" { … }` free-standing tests. Runs under
      `kupl test` on the interpreter (KVM rejects with K0804). (`examples/properties.kupl`)
- [x] **Timers** — `on every 5s` (recurring), `on after 2s` (one-shot) timer
      handlers on a virtual clock advanced explicitly (`advance 5s` example
      step; `kupl run` auto-advances bounded). Deterministic, byte-identical on
      interpreter + KVM. Durations `ms`/`s`/`m`/`h`. (`examples/timers.kupl`)
- [ ] **Hot-swap state migration** (design open Q4; Builder live-editing hook)

## Tier 3 — audit-driven priorities (next arc)

Ordered by the comparison audit ([`COMPARISON.md`](COMPARISON.md), refreshed
after it20):
the lowest-scoring, most load-bearing gaps vs Python/Go/TS/Java/Rust/Haskell/
C++/Swift/Kotlin. Concurrency is the #1 gap for the "universal, any software"
claim (the runtime is single-threaded today; Go/Rust/Kotlin/Swift all win).

- [◐] **Concurrency / parallelism** (audit #1) — **`par { … }` fork-join
      (it11) + parallel iteration `par_map`/`par_filter`/`par_each` (it13), and
      as of it33-34 REAL OS-THREAD execution for `par_map` AND `par_filter` with a
      pure named callback over lists ≥ 256 elements** (`src/parallel.rs`: a `PortableValue`
      Send boundary + a Send+Sync `ProgramImage` + `std::thread::scope`; results
      placed by input index so the output is byte-identical to sequential
      `map`). Everything else still evaluates sequentially (deterministic,
      byte-identical on all engines incl. native). Runs on BOTH the interpreter and the
      KVM (it35); the differential harness keeps a sequential-VM reference and
      absolute-value tests anchor correctness. Still open: extending real
      threads to `par{}` fixed branches, a work-stealing scheduler,
      async I/O, and `await` actually suspending (evaluates synchronously
      today). Virtual clock (it9) preserved for deterministic tests.
- [x] **File I/O** (it14) — `read_file`/`write_file`/`append_file`/`delete_file`
      (→ `Result`) + `file_exists`, gated behind the `io.fs` effect. A core "any
      software" capability (a universal language must touch the filesystem).
      Shared builtin impl (interp+KVM) + cgen.rs C runtime → all engines run real
      file I/O; interp==KVM==native on the success path (OS error *text* is
      platform-dependent). (`examples/files.kupl`)
- [x] **JSON** (it15) — built-in recursive `Json` ADT (via a prelude) +
      `json_parse` / `json_stringify` (pure). Round-trips are stable (key order
      preserved, ints without `.0`). Pairs with file I/O and the AI-native core.
      Interp + KVM + `.kx` + bundle byte-identical; **native too (it43)** — the
      parser/serializer are ported to the C runtime, so compiled binaries do
      JSON. Confirmed recursive ADTs work end-to-end. (`examples/json.kupl`)
- [x] **Environment & process** (it16) — `args()` (command-line arguments),
      `env_var(name) -> Option[Str]`, `eprint` (stderr), `exit(code)`. With file
      I/O + JSON, KUPL can now write real CLI tools. `args`/`env_var` carry the
      `io.env` effect. All engines incl. native (argv, getenv, exit).
      (`examples/cli.kupl`)
- [◐] **Native components + KIR** (audit #2) — as of it36-37, `kupl native`
      compiles COMPONENT apps to machine code: instance state, `on start`/port
      handlers, child components, `wire`s, `emit`, the message-queue/drain loop, virtual-clock
      timers, and `supervise` restart-on-failure (a C mirror of vm.rs, incl. a
      setjmp/longjmp panic landing pad). counter/todo/timers/native-counter
      native stdout == `kupl run`. Remaining: cross-component expose calls,
      The only remaining piece is the OPTIONAL typed SSA IR / KIR (KValue
      unboxing for raw-register numeric loops — a performance, not correctness,
      arc; deliberately deferred). Effectful builtins (ai/json/sized/f32) inside
      native components defer as they do for `fun main`.
- [ ] KIR `kernel fun` + `at(gpu)` placement; Metal lowering first
- [◐] Sized numerics (i8…u64, f32), Byte/Char, BigInt/Decimal (audit #3) —
      sized ints i8…u64 fully landed across ALL engines: checked/wrapping/
      saturating arithmetic, width-aware bitwise, full conversion matrix
      (it27-29), and native codegen via a boxed __int128 KValue (it40). f32 runs
      on ALL engines incl. native (it28/it42, shortest-round-trip display). Bitwise Int methods + literals (it17); numeric formatting +
      math (it24). Byte/Char, BigInt/Decimal still to do.
- [x] Broader standard library (audit #3, it12) — ~40 methods across all core
      types, all engines byte-identical incl. native. List (is_empty/concat/
      unique/init/tail/product/min/max/flatten/count/flat_map/window/chunk); Str
      (is_empty/reverse/lines/index_of/count/slice/pad_left/pad_right); Int (pow/
      gcd/clamp/sign/is_even/is_odd); Float (log/log10/exp/sin/cos/tan/sign/
      clamp/is_nan/is_infinite); Map (is_empty/get_or/merge/map_values); Set
      (is_empty/is_subset)
- [ ] System tier: ownership, `low`/`asm` (design §6; audit #4)
- [ ] Capabilities as attenuable values (`cap.Http.limited_to(…)`)

## Tier 4 — ecosystem

- [ ] Package registry + `kupl pkg publish` with enforced API compat
- [ ] LSP: hover, completion, go-to-definition (diagnostics ship today)
- [ ] `kupl patch` (component-granular edits); conformance suite numbering
- [ ] WASM target; cross-compilation story

## Resolved design open questions (LANGUAGE.md §12)

1. UI trees → `docs/design/UI.md` (render = component construction). **Designed.**
2. Int default → **decided & shipped:** i64 checked, overflow panics.
3. Effect granularity → shipped hierarchical effects (`db` covers `db.read`).
4. Hot-swap state migration → supervision restart hook shipped; migration TBD.
5. Package identity → `kupl.toml` shipped; registry governance TBD.
