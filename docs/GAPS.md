# KUPL Gap Audit & Enrichment Roadmap

Audited 2026-07-04 against: `docs/design/LANGUAGE.md` (incl. §12 open
questions), the `[design]` markers in `docs/reference/LANGUAGE-REFERENCE.md`,
and known limitations called out in commit messages. Checked off as landed.

## Enrichment campaign complete — 100 iterations

The 100-iteration enrichment campaign is **complete**. It took KUPL from an early
language to a complete, honestly-documented one, held to a strict invariant
throughout: every engine produces byte-identical output, verified on every build.

**Final certified state:**

- **Four engines, byte-identical** — the interpreter (reference semantics), the KVM
  register bytecode VM (checked against the interpreter by a per-build differential
  suite, byte-identical across all 63 examples), `.kx` compiled modules, and a
  **native** machine-code backend (via generated C) whose output is **byte-identical
  to the interpreter across all 55 sweepable examples** (0 divergences; the eight
  skips are stdin/network/subprocess/thread-order programs, multi-component apps that
  use `kupl bundle`, and one cosmetic error-string case — see the it98 note below).
- **A comprehensive, zero-dependency standard library** — `List`/`Map`/`Set`/`Str`
  with the full functional toolkit, the exact numeric tower `Int → BigInt →
  Rational`, sized numerics, and JSON/CSV/URL/regex/HTTP/time/encoding/random
  batteries — all in the box, no external crates.
- **A modern type system + syntax** — generics over functions *and* types, operator
  overloading, `Option`/`Result` combinators, exhaustive `match`, an effect system,
  and no null; all four syntactic papercuts the flagship demos surfaced are fixed.
- **The distinctive core** — components as isolated actors with typed ports, private
  state, supervision, and inline `example`/`law` tests; `ai fun` as a typed,
  mockable language feature.
- **~29k lines of dependency-free Rust, 216 tests, `cargo build` warning-clean**, and
  flagship programs across 11+ domains (web backends, data tools, algorithms,
  language implementation — an interpreter *and* a compiler/VM, data structures,
  simulation, numerical computing, interactive fiction, diffing, and a
  component-based application).

**Honest remaining gaps (unchanged, explicitly deferred):** a hosted package
registry + third-party ecosystem; general async/await + coroutines; bounded generics
/ typeclasses (`[T: Ord]` — ordered generic code passes an explicit compare
function today); the GPU/kernel and systems/ownership tiers; a WASM target; and the
KValue-unboxing performance IR. KUPL is **feature-complete for general-purpose,
component-oriented, AI-native programming** — what remains is maturity/ecosystem and
the explicitly-deferred hardware/performance tiers, all tracked honestly in
[`COMPARISON.md`](COMPARISON.md) and the campaign history below.

**Tooling limitation — `kupl fmt` does not preserve comments.** The formatter renders
from the AST, and the lexer discards comments, so formatting a file drops every `//`
and `/* … */` comment. The formatter is otherwise a stable, canonical fixpoint
(`fmt(fmt(x)) == fmt(x)`) and never changes a program's runtime behavior. `kupl fmt`
prints a `note:` to stderr whenever the input contains comments, so a format-on-save
or `fmt --write` never silently deletes them. Comment-preserving formatting (a
lexer/parser trivia system) is deferred.

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

**Honest remaining gaps (as of it50):** `ai fun` on the native backend; a hosted
package registry and third-party ecosystem; general async/await + coroutines; the
GPU/kernel and systems/ownership tiers; and the optional KValue-unboxing perf IR
(KIR). These are documented, not hidden.

## Enrichment campaign (it51–it66) — extension summary

The campaign was extended past it50; a further set of iterations deepened the
standard library and the language, holding the same sacred invariant (interp==KVM
byte-identical + the all-examples regression green on every commit):

- **Native completeness (it51–52)** — `ai fun` now compiles to native via its
  deterministic `KUPL_AI_MOCK*` path (non-tool and tool-use), so the native
  backend compiles the entire language (real-provider network calls aside).
- **Date/time (it53)** — a deterministic UTC calendar keyed on epoch seconds:
  `date_make`, `date_iso`, `parse_iso`, and the `*_of` extractors, pure integer
  civil math, byte-identical on every engine including native.
- **Stdlib depth (it54)** — `List.sort_by`/`position`/`partition`, `Str.rfind`/
  `replace_first`/`split_once`.
- **Match ergonomics (it55–56)** — guards (`if COND`), or-patterns (`A | B`),
  `@` bindings, and Int range patterns (`lo..hi`, `lo..=hi`), all lowering to
  existing branch ops (native-free).
- **UFCS (it57)** — `x.f(args)` resolves to a top-level `f(x, args…)` when there
  is no built-in method: free functions read as methods and chain.
- **`if let` / `while let` (it58)** — refutable binding that desugars to `match`.
- **Stdin (it59)** — `read_line`/`read_all` (Unix-filter programs).
- **Subprocess (it60)** — `exec(program, args)`, argv-based (no shell).
- **File/path toolkit (it61)** — `list_dir` (sorted), `make_dir`/`remove_dir`,
  and the pure `path_join`/`path_base`/`path_dir`/`path_ext` helpers.
- **Default params + named args (it62)** — resolved to positional form before
  checking, so all engines see plain positional calls.
- **Flagship app (it63)** — `examples/ssg.kupl`, a mini static-site generator
  (markdown→HTML on disk) using the file/path toolkit + string processing.
- **BigInt (it64–65)** — arbitrary-precision integers (`+ - * / %`, comparisons,
  `.pow`/`.abs`/`.sign`), a from-scratch base-1e9 bignum with a native C mirror,
  byte-identical on every engine.

**Remaining gaps (as of it66):** a hosted package registry + third-party
ecosystem; general async/await + coroutines; the GPU/kernel and systems/ownership
tiers; a WASM target; and the KValue-unboxing perf IR (KIR is design-locked-out —
"lower existing bytecode, no KIR").

## Enrichment campaign (it67–it81) — type system + web + flagships

The campaign continued past it66; these iterations deepened the type system,
added a web-server tier, and proved universality — all held byte-identical across
the interpreter, KVM, and native (except the blocking HTTP server, validated by
live socket tests rather than the stdout regression):

- **HTTP server (it67–68)** — `http_serve(port, handler)`, a real blocking server
  dispatching to a KUPL handler; interp+KVM, then native via POSIX sockets. The
  one honest regression exception (blocks) — it lives in `examples/demos/` and is
  covered by live unit tests.
- **Rational (it70)** — exact fractions over `BigInt`, completing the numeric
  tower `Int -> BigInt -> Rational`; native via a C mirror.
- **Operator overloading (it71)** — `+ - * / % < <= > >=` on user types resolve to
  `add`/`sub`/…/`lt` functions (== stays structural); a lowered call, so native is
  free.
- **Number formatting (it73)** — `Float.fmt(decimals)`, a hand-rolled round-half-
  away algorithm mirrored in C (byte-identical, no platform `%.*f`).
- **Option/Result combinators (it77)** — `.map`/`.and_then`/`.filter`/`.ok_or` /
  `.map_err`/`.ok`, variant-guarded, callbacks via the shared call path.
- **Ergonomics (it75, it76, it78)** — literal-brace escaping `{{`/`}}`, `else` on a
  new line, and multi-line method chains — three lexer/parser fixes surfaced by
  the flagship demos, each byte-identical for free.
- **Interpreter recursion depth (it79)** — the tree-walker now runs on a 512 MiB
  worker-thread stack, matching the KVM's heap frames (closes a latent deep-
  recursion divergence).
- **Generic ADTs (it80)** — `type Box[T]`, `type Pair[A, B]`, `type Tree[T]`:
  parametric user types, sound, with type parameters checked then erased at
  runtime — a parser + checker-only change, so all engines were unchanged.
- **Flagship apps** — a JSON REST API (it69), a jq-like JSON query tool (it74), a
  language interpreter (it72), a Sudoku solver (it79), and a generic collections
  library (it81), all written in KUPL.

**Remaining gaps (unchanged, honest):** a hosted package registry + third-party
ecosystem; general async/await + coroutines; **bounded generics / typeclasses**
(`[T: Ord]` — ordered generic code passes an explicit compare function today); the
GPU/kernel and systems/ownership tiers; a WASM target; and the KValue-unboxing
perf IR (KIR is design-locked-out).

## Enrichment campaign (it82–it95) — stdlib completion + ergonomics + flagships

The campaign continued past it81. This arc finished the standard library, removed
the remaining syntactic sharp edges, and broadened the flagship set — all held
byte-identical across the interpreter, KVM, and native:

- **Set operations (it84)** — `symmetric_difference` completed the `Set` algebra
  (`union`/`intersect`/`difference` already shipped).
- **Contextual keywords (it90)** — `out`/`state`/`start`/`stop` are now contextual:
  reserved only inside a component, ordinary identifiers everywhere else. With the
  earlier `{{`/`}}`, multi-line `else`, and multi-line method-chain fixes, all four
  syntactic papercuts the flagship demos surfaced are closed — each a parse-time
  change, so every engine stayed byte-identical. (`in` stays reserved for `for … in`.)
- **Collection API completion (it89, it91, it94, it95)** — `Map.filter`/`.fold`;
  `List.zip_with` (element-wise combine), `.group_by` (bucket into a `Map`),
  `.take_while`/`.drop_while`; `Str.trim_start`/`.trim_end`. The `List`/`Map`/`Set`/
  `Str` method surface is now comprehensive; callbacks route through the shared
  method path (interp==KVM by construction) and native via `k_call`.
- **Tutorial + docs refresh (it87)** — the learning tutorial was brought current
  with generics, operator overloading, combinators, the numeric tower, and the web
  server, with every snippet verified to run.
- **Flagship apps** — the CSV/stats analytics (it82/86), Conway's Game of Life
  (it85), a text-adventure engine (it88), an LCS line-diff (it92), and a capstone
  bank-ledger component (it93) — adding the simulation, interactive-fiction,
  diffing, and real-application domains to the flagship set. Every deterministic
  example rides the interp-vs-`--vm` regression and compiles to matching native.

**Remaining gaps (unchanged, honest):** the same list as above — a hosted package
registry + third-party ecosystem; general async/await + coroutines; bounded
generics / typeclasses; the GPU/kernel and systems/ownership tiers; a WASM target;
and the KValue-unboxing perf IR. The language and standard library are otherwise
feature-complete for general-purpose, component-oriented, AI-native programming.

**Native-backend certification (it98):** a full sweep compiling every deterministic
example with `kupl native` and diffing its output (stdout and stderr separately)
against the interpreter confirms **native is byte-identical to the interpreter
across all 55 sweepable examples**. Eight are skipped for legitimate reasons, not
divergences: `stdin` (reads input), `parallel`/`parallel-bench` (thread-scheduling
order / timing), `http`/`exec` (network / subprocess — environment-dependent),
`contracts`/`properties` (multi-component programs — `kupl native` targets a single
`fun main`/one-component `app`, so these use `kupl bundle`), and `ai` (a
deliberately-malformed mock exercises the JSON-parse *error* path, where the native
C parser's error-message text is less detailed than the interpreter's Rust parser —
a cosmetic error-string difference; every successful parse and all normal output
match). No native codegen divergence was found.

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
- [◐] **`ai fun` on the native backend** — the deterministic `KUPL_AI_MOCK*`
      path compiles natively and COMPLETELY (it51 non-tool, it52 tool use),
      byte-identical to the interpreter: structured `Result`/record/`List`
      output AND the mock tool loop (invoking compiled KUPL functions).
      examples/agent.kupl + agent_component.kupl compile native. Only a
      real-provider *network* call defers at runtime (use `bundle`).

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
