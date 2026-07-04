# KUPL Gap Audit & Enrichment Roadmap

Audited 2026-07-04 against: `docs/design/LANGUAGE.md` (incl. §12 open
questions), the `[design]` markers in `docs/reference/LANGUAGE-REFERENCE.md`,
and known limitations called out in commit messages. Checked off as landed.

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

Ordered by the 2026-07-04 comparison audit ([`COMPARISON.md`](COMPARISON.md)):
the lowest-scoring, most load-bearing gaps vs Python/Go/TS/Java/Rust/Haskell/
C++/Swift/Kotlin. Concurrency is the #1 gap for the "universal, any software"
claim (the runtime is single-threaded today; Go/Rust/Kotlin/Swift all win).

- [◐] **Concurrency / parallelism** (audit #1) — **`par { … }` fork-join
      (it11) + parallel iteration `par_map`/`par_filter`/`par_each` (it13)**:
      fixed-branch and dynamic-collection parallel work, both deterministic and
      byte-identical on all engines (incl. native). The language seam where a
      real scheduler plugs in is now complete. Still open: a multi-threaded/
      work-stealing scheduler (execute on OS threads — needs Arc-based values),
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
      Interp + KVM + `.kx` + bundle byte-identical; native reports a clear
      "not yet supported" error (recursive ctor construction in C deferred).
      Confirmed recursive ADTs work end-to-end. (`examples/json.kupl`)
- [x] **Environment & process** (it16) — `args()` (command-line arguments),
      `env_var(name) -> Option[Str]`, `eprint` (stderr), `exit(code)`. With file
      I/O + JSON, KUPL can now write real CLI tools. `args`/`env_var` carry the
      `io.env` effect. All engines incl. native (argv, getenv, exit).
      (`examples/cli.kupl`)
- [ ] **Native components + KIR** (audit #2) — typed SSA IR; components compile
      to native (per-component GC), so real apps run at native, not VM, speed
- [ ] KIR `kernel fun` + `at(gpu)` placement; Metal lowering first
- [ ] Sized numerics (i8…u64, f32), Byte/Char, BigInt/Decimal (audit #3)
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
