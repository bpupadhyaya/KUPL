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

## Universal-language enrichment campaign (it99–) — concurrency-first

KUPL's own name is "K Universal Programming Language" — this campaign takes that
literally: borrowing and improving on the best features from across the language
landscape, starting with concurrency, held to the same byte-identical-across-
engines discipline as every prior campaign.

- **`par { }` real-thread fast path (it99)** — the fourth and final step of
  `docs/design/bigarcs/3-real-concurrency.md`'s own "Dependencies & ordering"
  plan, previously designed but never implemented: `par { }` fork-join branches
  now run on genuine OS threads when every branch is a plain call to a
  statically pure, top-level named function with plain-literal/identifier
  arguments (the same `pure_funs` gate `par_map`/`par_filter` already use).
  Any non-qualifying branch falls the whole block back to the exact, unchanged
  sequential loop — additive only, never a behavior change. Matches the
  sequential reference exactly, including error reporting: a panicking
  branch's REAL, original span (from deep inside the callee's own body, since
  a direct function call is never span-rewrapped, unlike `.map(f)`'s method-
  dispatch wrapper) propagates through the worker/channel boundary unchanged.
  Interpreter only for this slice; VM and native stay sequential (see the
  design doc's own slice 4 progress note). Verified live: correct results,
  genuine concurrent threading, hang-avoidance under a panic + infinite-loop
  sibling, and branch-order-correct error priority under repetition.
- **`par { }` fast-path shadowing fix (it100)** — it99's own fast path gated
  solely on `image.pure_funs.contains(name)`, never checking whether `name`
  actually resolves to the top-level function at all. Live-confirmed two
  divergences from the sequential reference: a local closure shadowing a
  pure top-level fun of the same name, and a component's own private fun of
  the same name as a pure top-level fun, both called bare inside `par { }`,
  silently dispatched to the WRONG (top-level) function. Fixed by checking a
  local binding and the current component's own funs/exposes first,
  mirroring `eval_call`'s exact precedence. Two new permanent regression
  tests.
- **`par { }` real-thread fast path on the KVM (it101)** — the SAME fast
  path from it99 now also fires on `kupl run --vm`: a new `Op::ParBlock`
  bytecode op (plus a `Chunk.par_blocks` side-table and full `.kx`
  encode/decode support), emitted by a compile-time structural gate in
  `compile.rs` mirroring interp.rs's own gate exactly, with purity checked
  at runtime in the op's own VM handler (falling back to sequential
  `call_chunk_nested` calls otherwise) — additive only, same guarantee as
  it99. `kupl native` (`cgen.rs`) needed a corresponding arm too, since it
  shares the same bytecode `Op` enum; codegen'd as a plain sequential call
  sequence (native concurrency stays deferred, per it99's own decision) —
  byte-identical to native's prior output. Found and fixed a THIRD instance
  of it100's own shadowing bug class (a top-level fun sharing a name with a
  **builtin** call form, e.g. a user's own `to_str(x)`, is still routed to
  the builtin by both engines' dispatch) via a new shared
  `BUILTIN_CALL_NAMES` list checked in both interp.rs and compile.rs, plus a
  fourth instance (component-private fun shadow) caught and fixed in
  compile.rs's own gate before shipping. Three new permanent regression
  tests, plus a dedicated VM-side hang-avoidance test for the new
  thread-spawn site.
- **Supervision restart-intensity limits (it102)** — `supervise child
  restart on_failure max N in <duration>` (BEAM/Erlang-inspired
  `max_restarts`/`max_seconds`, already specified in
  `docs/design/LANGUAGE.md`'s own original vision but never implemented
  until now): once a supervised child has restarted `N` times within the
  trailing `duration` (virtual clock, so this stays deterministic and
  reproducible, matching timers), the NEXT panic escalates instead of
  restarting again — a safety valve against an unbounded panic/restart
  crash loop. Fully opt-in and additive: omitting `max … in …` preserves
  today's exact unlimited-restart behavior. New `K0122` diagnostic
  (malformed clause) and `K0808` (restart count too large for the KVM's
  `u16` operand). Implemented across all three engines: interp.rs/vm.rs
  track a per-instance sliding-window restart history (pruned
  `VecDeque<i64>`); `cgen.rs` uses an equivalent fixed-size ring buffer
  (`KInstance`'s own doc comment) since C has no dynamic `Vec` — verified
  behaviorally equivalent to the sliding-window prune-then-check-length
  approach. `Op::MakeInstance` gained two new operands (`max_restarts`,
  `window_ms_const`) to carry this from the parent's `supervise` clause
  through to runtime, with full `.kx` encode/decode support. Seven new
  permanent regression tests (parser grammar, `kupl fmt` round-trip,
  interp/vm parity for both the escalation and sliding-window-expiry
  cases, and two native-specific tests exercising `cgen.rs`'s own
  independent C implementation directly).
- **Bounded generics: `[T: Ord]` (it103)** — closes a gap this doc has
  named explicitly since the "100-iteration enrichment campaign complete"
  summary above: `fun mymax[T: Ord](a: T, b: T) -> T { if a > b { a } else
  { b } }` now type-checks and runs correctly. `Ord` is currently the
  ONLY supported bound (an unrecognized bound name is a clean `K0123`
  parse error, not silently accepted). Purely a `check.rs` feature — KUPL's
  generics are dynamically typed at runtime on every engine (comparison
  dispatches on the actual `Value`/`KValue`'s own runtime tag, never a
  static type), confirmed live before implementing, so **zero changes were
  needed to interp.rs/vm.rs/cgen.rs's own execution** — the entire feature
  is a compile-time-only relaxation plus a NEW call-site check. An
  Ord-bounded type parameter is exempted from the existing K0281
  parametricity check specifically for comparison operators (`<`/`<=`/
  `>`/`>=`), while every other narrowing case (return value, internal
  `let`, aliasing two type parameters) is still correctly rejected. The
  call-site half closes what would otherwise be an incomplete guarantee:
  calling a bounded generic with a non-orderable concrete type (e.g. a
  user record) is now a compile-time `K0290`, not a deferred runtime
  panic (confirmed live: before this fix, the same call type-checked
  cleanly and only panicked at runtime inside the body's own comparison).
  Five new permanent regression tests (grammar/unknown-bound rejection,
  body-checking relaxation with a non-comparison-narrowing regression
  check, call-site rejection with a mixed bounded/unbounded regression
  check, `kupl fmt` round-trip, and a native-specific test proving the
  "zero runtime changes" claim live across three independently-typed call
  sites for the same generic function).
- **`kupl diff` bound-change detection (it104)** — a direct follow-up to
  it103, found by re-auditing `sdiff.rs`'s own interface fingerprint for
  the exact same "one site doesn't render a field its siblings already do"
  false-negative class this file has repeatedly closed before
  (PR-it580/643/646/864/1042/1043/1173/1187): `Item::Fun`'s fingerprint
  included `type_params` (bare names) but not the new
  `type_param_bounds`, so `kupl diff` misclassified ADDING an `Ord` bound
  to a previously-unbounded generic function as `[implementation only]` —
  confirmed live before fixing (a genuinely breaking change: a caller
  passing a non-orderable type compiled cleanly before the bound existed
  and fails to compile, K0290, after). Fixed and confirmed live in both
  directions (bound added, bound removed) plus a regression check that an
  unrelated body-only change with the SAME bound still correctly reports
  implementation-only. One new permanent regression test.
- **`Char` primitive type (it105)** — a single Unicode scalar value (`'a'`),
  closing part of the "Byte/Char … still to do" gap this doc has named since
  audit #3. New `Ty::Char`/`Value::Char(char)` (Rust's own `char` is already
  a validated, `Copy`, 4-byte Unicode scalar — a perfect fit, no allocation),
  ordered by codepoint (`<`/`<=`/`>`/`>=`, `.sort()`, `.min()`/`.max()`, and
  the `[T: Ord]` bound from it103 all widened to accept it); no `Add` (use
  `to_str(a) + to_str(b)` to build a `Str`). New lexer diagnostics K0011
  (unterminated), K0012 (unknown escape), K0013 (not exactly one character).
  Landed on the interpreter, the KVM, `.kx` build/run, and `kupl bundle`,
  all verified byte-identical live; `kupl native` cleanly, explicitly
  defers Char literals with a feature-specific message (mirroring the
  K0289 staged-rollout precedent) rather than crashing or miscompiling.
  A genuinely valuable discovery made live rather than assumed: because
  `Value::Char` slots into the EXISTING generic constant-pool mechanism
  (`Op::Const`) and the VM's comparison dispatch (`raw_binary_op`) is
  LITERALLY THE SAME function the interpreter uses, the KVM needed only one
  trivial line in `compile.rs` — a much smaller lift than it101's `par { }`
  VM-wiring. Three real bugs were found and fixed purely through live
  testing after an otherwise-clean compile (none were compiler errors,
  since each match had a silent fallback arm): a missing
  `(Ty::Char, Ty::Char)` self-unification arm in `types.rs` (every Char
  comparison failed with a confusing "expected Char, found Char"); an
  unconditional `panic!` in `kx.rs::encode_const` for any unhandled
  constant, crashing `kupl build` outright on a Char literal instead of
  erroring cleanly (fixed with proper encode/decode, since `.kx` is pure
  byte-serialization consumed by the already-working VM, unlike native);
  and a lexer error-recovery bug where `lex_char` returned early with no
  token on error, cascading a confusing secondary "expected an expression"
  parser diagnostic on top of each real one (fixed by unconditionally
  pushing a `Tok::CharLit` even on error, using `'\u{fffd}'` as a recovery
  placeholder — mirroring `lex_string`'s own established discipline). New
  permanent regression tests across the lexer, `check.rs`, `kupl fmt`
  round-trip, `cgen.rs`'s clean-rejection behavior, and an interp-vs-vm
  parity test in `main.rs`.
- **`kupl native` `Char` support (it106)** — closes the staged-rollout gap
  it105 deliberately left open: a real `K_CHAR` KValue tag in the native C
  runtime, so `Char` is now byte-identical across all FOUR engines, not
  just interp/VM/`.kx`. Pure engine-porting (Char's own semantics were
  already fully settled by it105) — a new `k_char(codepoint)` constructor
  and `k_char_utf8` encoder (the C-side counterpart to Rust's
  `char::encode_utf8`, needed only for DISPLAY; comparisons/equality
  operate on the raw codepoint directly, no UTF-8 involved), wired into
  `k_type_name`, `k_display` (bare at top level, single-quoted when
  nested — the same asymmetry `K_STR` already has, confirmed live
  including through a `Some(...)` wrapper, which correctly re-quotes a
  nested Char via the SAME generic Ctor-field-display path Str already
  uses), `k_eq`/`k_key_eq` (the latter needed no code change at all — its
  `default:` case already delegates to `k_eq` for any non-composite,
  non-float tag), and `k_cmp`/`k_list_order` (`.sort()`/`.min()`/`.max()`
  all work with zero Char-specific code, since `k_list_order` falls
  through to the generic `k_cmp`-based path for any non-float tag).
  `Value::Char`'s constant-pool emission changed from an error message to
  `k_char(<codepoint>u)`, mirroring `F32`'s own `k_f32_bits(...)` pattern
  (a raw numeric payload, not a source-text literal). New permanent
  regression test in `cgen.rs` driving the real compiled native binary
  across comparisons, `.sort()`/`.max()`, nested-Option display, and a
  non-ASCII multi-byte codepoint (`'😀'`, exercising `k_char_utf8`'s
  4-byte encoding path, not just ASCII). Full `cargo test` green twice
  sequentially (1647 lib + 59 main tests, unchanged from it105 — one
  native test removed, one added, net zero), all 363 `cgen::` tests green
  (confirms inserting `K_CHAR` into the tag enum didn't shift any other
  tag's behavior), interp-vs-vm AND interp-vs-native sweeps across all
  eligible `examples/*.kupl` clean, revert-and-verify via `git stash`
  confirmed the pre-fix binary still cleanly rejects Char in `kupl native`
  with the exact it105 error message, and the restored binary compiles
  and runs it correctly.
- **`Decimal` primitive type (it107)** — an exact base-10 arbitrary-
  precision decimal (`dec("3.14")`), closing the last item this doc's own
  "Byte/Char/BigInt/Decimal" gap line had left open besides `Byte`. Chosen
  after first investigating and formally RULING OUT `Byte` as a distinct
  type (a legitimate "investigate and rule out" outcome the it107
  NEXT-note itself anticipated): `src/encoding.rs`'s base64/hex builtins
  and every file-I/O/HTTP builtin in `docs/reference/STDLIB.md` already
  operate on `Str`, round-tripping binary-safe data through it (base64/hex
  decode validate UTF-8 and reject otherwise) — nothing in the current
  stdlib returns or consumes raw bytes in a shape a distinct `Byte` type
  would improve, so introducing one now would need inventing binary-mode
  I/O from scratch just to give it a consumer, a much larger and separate
  design question than this note originally scoped.

  `Decimal` (`src/decimal.rs`, new file) is `sig * 10^-scale` — a signed
  `BigInt` significand plus a `u32` scale — built on `BigInt` exactly the
  way `Rational` is (`src/bigint.rs`/`src/rational.rs` were read as the
  explicit templates, per the campaign's own NEXT-note). Unlike
  `Rational`, a `Decimal` is NOT auto-reduced: `dec("2.50") == dec("2.5")`
  (equality aligns scales before comparing) but each keeps its OWN stored
  scale for `Display` (`dec("2.50").to_string() == "2.50"`) — matching
  how SQL `DECIMAL`/`NUMERIC` and financial libraries preserve a value's
  own precision rather than silently trimming trailing zeros a caller
  wrote intentionally. `+`/`-`/`*` are always exact; `/` is NOT (decimal
  division doesn't generally terminate, e.g. `1/3`) and computes 34 extra
  digits of precision beyond `max(a.scale, b.scale)` — mirroring IEEE
  754-2008 `decimal128`'s own significant-digit count, a standards-
  referenced choice — rounding half-away-from-zero at the final digit.
  `%` is deliberately unsupported (a clean runtime error), mirroring
  `Rational`'s own "remainder is not supported" precedent exactly.

  A conservative `MAX_DECIMAL_SCALE` cap (1,000 digits — far below
  `BigInt`'s own ~180,000-digit cap) exists for a DIFFERENT reason than
  `BigInt`/`Rational`'s own caps: aligning two operands to a shared scale
  can force multiplying an independently-large significand by a
  `10^shift`-magnitude power, and capping `scale` directly (rather than
  needing a separate cost-estimate function like `Rational::
  cmp_would_be_too_expensive`, PR-it718) keeps that multiplication cheap
  by construction regardless of the OTHER operand's own size — documented
  at length in `decimal.rs`'s own top-of-file doc comment, with a
  dedicated test confirming this stays fast even at the cap.

  Landed on the interpreter and the KVM, `.kx` build/run, and `kupl
  bundle`, ALL verified byte-identical live with zero extra plumbing for
  `.kx`/bundle specifically — confirmed live that (unlike `Char`, a
  LITERAL that must round-trip through the constant pool) `dec(...)` is a
  plain runtime builtin call, exactly like `big`/`rat`, so it never
  touches `kx.rs`'s constant-pool encoding at all. `kupl native` cleanly,
  explicitly defers `dec(...)` with a feature-specific message, mirroring
  the `Char` it105→it106 staged-rollout precedent (a substantially bigger
  follow-up than Char's own native port, left for a future iteration:
  Decimal needs a full BigInt-backed significand+scale representation and
  rounding-aware division ported to C, not just a scalar codepoint).

  **A real, live-confirmed CROSS-ENGINE bug found+fixed along the way**,
  unrelated to Decimal itself but discovered while wiring `Decimal` into
  the exact same function: `interp.rs::list_order` (shared by the
  interpreter AND the KVM via `shared_method`) was never given a
  `Value::Char` arm when `check.rs`'s K0234 was widened to accept `Char`
  at it105 — so `['c','a','b'].sort()`/`.min()`/`.max()` type-checked
  fine and then PANICKED "min/max need Int, Float, Str, or another
  orderable type" on BOTH interp and the KVM, while `kupl native`'s own
  `k_list_order` (which falls through to the generic, type-agnostic
  `k_cmp` for any non-float tag) already handled it correctly — a genuine
  three-way divergence that slipped through it105's own testing. Fixed by
  adding the missing arm; confirmed live via all three engines before and
  after.

  New permanent regression tests: `decimal.rs`'s own unit-test module
  (parsing, scale-insensitive equality with scale-preserving display,
  exact arithmetic, rounding division, ordering, and two dedicated
  cost-safety tests for the scale-cap reasoning above), `check.rs` (a
  `is_numeric()`-everywhere-numeric-types-are test, and a wrong-arity
  regression test for the PR-it1202 "must not misreport as unknown name"
  lesson), an interp-vs-vm differential test in `vm.rs` covering
  construction/arithmetic/comparison/division-by-zero, a SEPARATE
  differential test locking in the `Char` `.sort()`/`.min()`/`.max()` fix
  above, and a `cgen.rs` clean-rejection test for native. Full `cargo
  test` green twice sequentially (1663 lib + 59 main tests, identical
  both runs), interp-vs-vm AND interp-vs-native sweeps across all
  eligible `examples/*.kupl` clean, and revert-and-verify via `git stash`
  confirmed the pre-fix binary cleanly rejects `dec(...)` as an unknown
  name while the restored binary computes it correctly.
- **`kupl native` `Decimal` support (it108)** — closes the staged-rollout
  gap it107 deliberately left open: a real `K_DECIMAL` KValue tag in the
  native C runtime, so `Decimal` is now byte-identical across all FOUR
  engines, not just interp/VM/`.kx`. Pure engine-porting, and — unlike
  Char's own native port (it106), which needed brand-new C bignum-free
  scalar code — this reuses `KRat`'s EXISTING `KBig`-based primitives
  wholesale: `k_big_mul`/`k_big_divmod`/`k_big_pow` (the last for the
  `10^shift` scale-alignment step) needed zero new C bignum algorithm
  work at all, only a new `KDec { KBig* sig; uint32_t scale; }` struct
  composing them, mirroring `KRat { KBig* num; KBig* den; }`'s own shape.
  A `k_utf8_trim_range` reuse for `dec(...)`'s own Unicode-whitespace
  trim too (the SAME primitive `k_big_from_str` already uses). Wired
  into `k_type_name`, `k_display`, `k_eq` (scale-ALIGNED comparison, not
  raw struct equality — Decimal is deliberately NOT auto-reduced like
  Rational, so `dec("2.50")`/`dec("2.5")` have different stored
  sig/scale but must compare equal), `k_cmp` (`.sort()`/`.min()`/`.max()`
  needed zero extra code, since `k_list_order` already falls through to
  `k_cmp`'s generic path), `k_add`/`k_sub`/`k_mul`/`k_div`/`k_rem`
  (rejected, mirroring `Rational`'s own precedent)/`k_neg`, and
  `.sum()`/`.product()`.

  Every native error message was verified to match `decimal.rs`'s exact
  wording, not just its behavior — confirmed live this diverged before
  fixing: a first draft's parse-error paths summarized the failure
  category ("invalid Decimal: non-digit character") where interp echoes
  the actual malformed input ("invalid Decimal: abc"), the SAME "echo,
  don't summarize" convention this codebase's other parse errors
  (`BigInt::from_str`, `parse_iso`, `query_parse`) already follow. Fixed
  by echoing the original untrimmed input string in every generic error
  path, with a dynamically-sized buffer (no length cap) to match Rust's
  own unbounded `String` formatting exactly.

  **A SEPARATE real bug found+fixed along the way**, this one NOT a
  wording mismatch but a genuine "type-checks then panics" gap in
  `Decimal`'s own it107 landing: `interp.rs::raw_unary_op` (shared by
  interp AND the KVM) had no `Value::Decimal` arm, so `-dec("3.14")`
  type-checked fine (K0236 gates on `is_numeric()`, which Decimal
  satisfies) but panicked "invalid operand type Decimal" at runtime on
  BOTH engines — found via careful re-auditing while scoping this native
  work, not a live user report. Fixed by adding the missing arm.

  New permanent regression tests: `cgen.rs` (a coverage test driving the
  compiled native binary across arithmetic/ordering/negation/`.sort()`/
  `.sum()`/scale-insensitive-equality, and a SEPARATE test asserting
  every native error path's message text matches interp's exactly,
  panic-string for panic-string), and a `vm.rs` differential test for
  the negation fix. Full `cargo test` green twice sequentially (1665 lib
  + 59 main tests, identical both runs — net delta from it107: -1
  rejection test removed, +3 coverage/regression tests added), all 365
  `cgen::` tests green (confirms inserting `K_DECIMAL` into the tag enum
  didn't shift any other tag's behavior), interp-vs-vm AND
  interp-vs-native sweeps across all eligible `examples/*.kupl` clean,
  and revert-and-verify via `git stash` confirmed the pre-fix binary
  still cleanly rejects `dec(...)` in `kupl native` (the it107 staged
  message) AND still panics on `-dec(...)` (the it107 negation bug),
  while the restored binary computes both correctly.
- **Prompt-context builders (it109)** — closes the Tier 1.5 gap this doc
  named since early in the campaign, in two independent halves.

  **`kupl context --json`** (`src/run.rs`): the same dependency-closed
  target+direct-dependency data plain-text `kupl context` already
  produced, now ALSO available as a structured `{"target":{"name",
  "source"},"dependencies":[{"name","source"},...]}` object — mirroring
  `kupl check --json`'s own established "structured output for a program
  to consume" pattern. Both error paths (missing item, ambiguous item)
  emit valid JSON under `--json` too, not a plain-text fallback a caller
  parsing stdout couldn't handle. A CLI-only change — no interp/vm/cgen
  engine changes at all, since `kupl context` has never been a language
  builtin, only a tool an `ai fun` agent can reach via `exec("kupl",
  ["context", "--json", path, name])` + `json_parse`.

  **`text_embed(s, dims) -> List[Float]` / `cosine_similarity(a, b) ->
  Float`** (`src/embed.rs`, new file): a from-scratch, zero-dependency
  **bag-of-words hash embedding** — the classic "hashing trick"
  (Weinberger et al. 2009; the same technique behind Vowpal Wabbit /
  scikit-learn's `HashingVectorizer`), deliberately NOT a neural
  embedding (no model, no weights, no network call, nothing that would
  conflict with this codebase's zero-external-dependency principle).
  Tokenizes into maximal runs of ASCII `[0-9A-Za-z]` bytes (lowercased),
  hashes each word via the SAME `hash_fnv`/`k_fnv1a` primitive the
  existing `hash_fnv` builtin already exposes and mirrors on native,
  buckets mod `dims`, accumulates raw counts, then L2-normalizes.
  Tokenization is deliberately ASCII-only (any non-ASCII byte, including
  every UTF-8 continuation/lead byte, is just a separator) — matching
  this codebase's OWN existing, documented native-backend convention for
  text processing (`kupl native`'s `to_upper`/`to_lower` and regex
  character classes are both already ASCII-oriented, per `STDLIB.md`),
  deliberately chosen over introducing a NEW, wider Unicode-vs-ASCII
  divergence class between interp/vm and native. Landed on ALL FOUR
  engines immediately (unlike `Char`/`Decimal`'s own staged native
  rollout) since reusing the existing hash primitive needed no new
  bignum-style representation work to defer.

  **A REAL, live-confirmed regression found+fixed along the way**,
  unrelated to either feature's own logic: adding two new match arms to
  `interp.rs::eval_call`'s already-large dispatch match grew that
  function's per-call stack-frame footprint in an unoptimized debug
  build just enough to tip `vm::tests::diff_ackermann_nonprimitive_
  recursion` — an ALREADY-marginal deep-recursion test whose own prior
  comment documented it as fitting the default test-thread stack by a
  thin margin — into a genuine stack overflow, aborting the whole test
  binary. Confirmed via `git stash` (passes on the pre-it109 tree,
  overflows after) before concluding this was a real regression, not a
  flake. Fixed the SAME way the neighboring `diff_mutual_recursion` test
  already handles this exact class of problem: an explicit
  `std::thread::Builder` with a 2GB stack, not a change to the
  interpreter or to Ackermann's own recursion depth.

  New permanent regression tests: `embed.rs`'s own unit-test module
  (determinism, L2-normalization, case-insensitivity, related-vs-
  unrelated ranking, zero-vector/zero-dims/mismatched-length edges),
  `run.rs`/`main.rs` (`--json` return-code coverage plus a subprocess
  test parsing the REAL binary's stdout with this codebase's own
  `json::parse` to verify shape and content, not just "did it not
  crash"), `check.rs` (type-checking + wrong-arity-is-K0242-not-K0240),
  `vm.rs` (an interp-vs-KVM differential test), and `cgen.rs` (a native
  coverage test plus a native-vs-interp exact-error-message test,
  mirroring `Decimal`'s own it108 message-matching discipline). Full
  `cargo test` green twice sequentially (1677 lib + 60 main tests,
  identical both runs), interp-vs-vm AND interp-vs-native sweeps across
  all eligible `examples/*.kupl` clean, and revert-and-verify via `git
  stash` (embed.rs is a NEW untracked file, so — per the it107 lesson —
  moved out manually before the revert build and restored after)
  confirmed the pre-fix binary rejects `--json`/`text_embed` cleanly
  while the restored binary computes both correctly.
- **`kupl patch` (it110)** — closes the Tier 4 gap this doc has named
  since early in the campaign. First investigated and RULED OUT candidate
  (a), capabilities as attenuable values (`cap.Http.limited_to(...)`):
  confirmed live that ZERO implementation exists today (no `cap`
  namespace, no capability types, nothing) — the vision doc's own framing
  ("effects are BACKED BY capabilities") means implementing this fully
  would mean connecting the EXISTING, fully-static effect checker
  (`src/effects.rs`) to a brand-new RUNTIME authority-passing mechanism,
  a genuinely deep, cross-cutting redesign rather than "add a type" —
  correctly judged too large for one iteration without a dedicated design
  pass, per this campaign's own "investigate and rule out" precedent
  (it103, it107).

  `kupl patch <target> <ItemName> <replacement> [--write]` (`src/run.rs`,
  `src/main.rs`) replaces the named item's ENTIRE source span in `target`
  with the canonical (formatted) text of the single item found in
  `replacement` — "models edit components, not line ranges"
  (`docs/design/LANGUAGE.md` §6), the semantic INVERSE of `kupl
  context`'s own item extraction. Reuses `sdiff::item_name`/`item_span`
  directly (already `pub(crate)`, already shared with `repl.rs`) rather
  than a third copy of the same match. Deliberately single-file,
  single-item, no cross-file `use` resolution: both files are parsed
  directly (`parser::parse`, mirroring `kupl fmt`'s own loading style),
  not `load_compile` (irrelevant here — patch only ever targets an item
  the target file itself declares). The replacement must contain EXACTLY
  ONE item and no `use` declarations (both clean, explicit errors, not
  silently ignored). Default mode PRINTS the patched whole-file text
  (non-mutating, safe preview); `--write` (position-independent, matching
  `context --json`'s own precedent) atomically overwrites the target.

  **Safety net, deliberately mirroring `kupl fmt --write`'s own
  PR-it837/889 discipline exactly**: recompiles the patched text and
  refuses to write if it introduces compile errors the target didn't
  already have — confirmed live end-to-end (a patch referencing an
  unknown name is refused, with the original file byte-for-byte
  untouched; an equivalent VALID patch to the same target still
  succeeds). A pure CLI feature like `kupl context`/`kupl diff` before
  it — zero interp/vm/cgen engine changes, so the interp-vs-native sweep
  doesn't apply this iteration (cgen.rs untouched).

  New permanent regression tests: `run.rs` (item replacement + `--write`
  mutation, with an UNRELATED sibling item confirmed byte-for-byte
  preserved; all three reject-with-original-untouched paths — missing
  item, multi-item replacement, a `use` in the replacement; the safety
  net's refuse-and-leave-untouched behavior, confirmed via a direct
  read-back, not just the return code) and `main.rs` (the CLI dispatch
  layer: `--write` position-independence, wrong-argument-count usage
  error, and the PR-it864-shaped "genuine extra argument, not silently
  dropped" check already applied to `diff`/`context`). `docs/GAPS.md`
  and `USAGE`/the `usage_text_mentions_every_dispatched_top_level_
  subcommand` test both updated. Full `cargo test` green twice
  sequentially (1680 lib + 61 main tests, identical both runs, no
  stack-overflow casualties this time), interp-vs-vm sweep across all
  eligible `examples/*.kupl` clean, and revert-and-verify via `git
  stash` confirmed the pre-fix binary rejects `patch` as an unrecognized
  subcommand (falling to the generic usage banner) while the restored
  binary applies the patch correctly.
- **Hot-swap state migration, first slice (it111)** — closes part of the
  Tier 2 design-open Q4 gap this doc has named since the campaign began
  (Erlang's `code_change` equivalent). Investigated candidate (a),
  capabilities as attenuable values, with a genuinely different framing
  from it110's own attempt (produce a design sketch, not implementation)
  but chose (b) after finding a much more concretely scoped path:
  `src/interp.rs`'s `Env` stores component instance state/props by NAME
  (a `Vec<(Box<str>, Value)>`, `EnvInner`), not positional index — a key
  discovery made BEFORE writing any code, not assumed, that removes the
  hardest part a naive migration design would otherwise need to solve
  (field-layout reconciliation across old/new shapes).

  `kupl repl`'s new `:upgrade <ComponentName>` command migrates every
  LIVE instance of the just-redefined component: a `state` field present
  in BOTH the old and new declaration keeps its CURRENT runtime value; a
  field only in the new declaration gets its own fresh `init` default
  (evaluated against the migrated env, so later fields can reference
  earlier ones, exactly like `instantiate`'s own left-to-right
  evaluation order); a field only in the old declaration is dropped.
  Swapping `instance.comp` to the new `Rc<ComponentDecl>` makes new/
  changed METHODS immediately callable too (methods are looked up via
  `Instance.comp` at CALL time, never cached per-instance, confirmed by
  reading `eval_method` before assuming this).

  **Deliberately narrow v1 scope, matching Erlang's own `code_change`
  itself** (which migrates a process's STATE term, not its supervision
  tree): refuses the WHOLE upgrade (no instance touched, not a partial
  migration) unless `props` and `children` are structurally unchanged by
  name. This sidesteps two real complications a fuller design would need
  to solve immediately: a newly-required prop with no default has
  nothing to migrate FROM at all, and a changed `children`/`wires` list
  would need re-spawning/re-routing, not just a `state` copy — both
  explicitly flagged as follow-up work, not silently ignored. The
  EXISTING, already-tested "frozen snapshot" default behavior (an
  instance NOT explicitly `:upgrade`d stays frozen to its original
  shape, confirmed by `repl_preserves_live_variable_and_component_
  state_across_redefinition`) is completely UNCHANGED — `:upgrade` is
  purely additive, opt-in.

  A REPL-only feature (the interpreter is the only engine `kupl repl`
  ever runs on) — zero vm.rs/cgen.rs changes, matching how `forall`
  property tests are ALSO interp-only by established precedent (K0804).

  New permanent regression tests: `repl.rs` (the core migration contract
  — matching-name state preserved, new fields defaulted, methods
  immediately callable — plus both guard-rejection paths, plus the
  zero-instances/unknown-component cases, all exercising
  `upgrade_instances` directly without the REPL I/O loop) and `main.rs`
  (a real `kupl repl` subprocess test: before `:upgrade`, the frozen-
  snapshot panic still fires exactly as it did before this change; after
  `:upgrade`, the new method works AND the migrated state continues
  accumulating from its OLD value, not a reset). Full `cargo test` green
  twice sequentially (1683 lib + 62 main tests, identical both runs, no
  stack-overflow casualties), interp-vs-vm sweep across all eligible
  `examples/*.kupl` clean (cgen.rs untouched, so no native sweep this
  iteration), and revert-and-verify via `git stash` confirmed the
  pre-fix binary reports `:upgrade` as an unknown REPL command while the
  restored binary migrates instances correctly.

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
      deterministic mock via `KUPL_AI_MOCK*`); every engine including native,
      real-provider network calls included as of it125-it127 (see below)
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
- [x] **Prompt-context builders (it109)** — `kupl context --json` emits
      the same dependency-closed target+direct-dependency data as plain-
      text `kupl context`, structured for a program to consume (mirrors
      `kupl check --json`'s own established pattern; an `ai fun` agent can
      reach it via `exec("kupl", ["context", "--json", path, name])` +
      `json_parse`). `text_embed(s, dims) -> List[Float]` /
      `cosine_similarity(a, b) -> Float`: a from-scratch, zero-dependency
      bag-of-words hash embedding (the classic "hashing trick" — no model,
      no weights, no network call), byte-identical on all four engines
      including native (see `STDLIB.md`).
- [x] **`ai fun` on the native backend** — the deterministic `KUPL_AI_MOCK*`
      path compiles natively and COMPLETELY (it51 non-tool, it52 tool use),
      byte-identical to the interpreter: structured `Result`/record/`List`
      output AND the mock tool loop (invoking compiled KUPL functions).
      examples/agent.kupl + agent_component.kupl compile native. **As of
      it125, a non-tool, `-> Str`-shaped `ai fun`'s REAL-PROVIDER network
      call also compiles natively** — `k_ai_call` ports `ai.rs`'s
      `build_prompt`/`http_post`/`openai_call`/`anthropic_call` to C,
      reusing existing native infrastructure (`k_run_curl`, `k_json_parse`,
      `k_show`, and `k_ai_convert` itself — this addition's own job ends
      the moment it produces response text, exactly like the mock path).
      Supports `anthropic` (default)/`openai`/`ollama`/`echo`; verified via
      the zero-network `echo` provider (the only one fully testable without
      a live API key). **As of it126, structured (non-`Str`) return shapes
      ALSO get a real network path** — `k_ai_schema_json`/`k_ai_wire_schema`
      port `ai.rs`'s JSON-Schema generator to C (recursive over the SAME
      `KAiShape` struct the mock path already emits per ai fun), embedded
      via `response_format`/`output_config`; the RESPONSE side needed no
      new code at all, since `k_ai_convert`/`k_ai_from_json` already
      handle shape-guided JSON parsing for every shape via the mock path.
      Verified end-to-end (not just prompt construction) against a local
      mock HTTP server (plain `std::net::TcpListener`, no live network/API
      cost) — this caught a REAL memory-safety bug (`k_ai_http_post`'s
      argv allocation was 4 slots short of what it writes, a heap buffer
      overflow) that it125's `echo`-only tests had zero coverage for,
      since `echo` never calls `k_ai_http_post` at all. **As of it127,
      tool-using ai funs ALSO get a real network path, closing the last
      remaining gap** — `k_ai_real_tool_call` ports `ai.rs`'s
      `run_tool_loop`/`tool_response`/`AnthropicProvider`/`OpenAiProvider`
      to C: a genuinely stateful multi-round conversation (each round
      POSTs the full message history, either gets a final answer or a
      request to invoke one or more tools, whose results get appended
      before the next round), with Anthropic and OpenAI/ollama needing
      their OWN imperative loop each (C has no trait objects, and the two
      providers use fully different tool-call response shapes). Verified
      via local mock servers scripting a tool-call round then a final-
      answer round, for BOTH provider shapes — `kupl bundle` is no longer
      needed for any `ai fun`, tool-using or not, any return shape.

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
- [x] **Hot-swap state migration (it111, extended it113/it114/it115/it119/
      it124/it128)** — `kupl repl`'s `:upgrade <Component>` command
      (Erlang `code_change` equivalent, design open Q4): migrates every
      LIVE instance's `state` and `props` by name (matched names keep
      their current value; new fields/props with a default get a fresh
      value; a removed field/prop is dropped) and swaps in the redefined
      component's methods immediately. `children` may GROW (it114) or
      SHRINK (it119): a genuinely new child is constructed fresh; a
      removed child is torn down (`on stop` fires via
      `Interp::run_lifecycle`, its own armed timers are cleared, and any
      wire from a still-live sibling pointing at it is pruned) —
      **recursively as of it124**: `stop_and_disarm_subtree` walks the
      removed child's own children (and theirs, and so on), parent-first,
      matching `stop_all`'s own instance-id ordering convention; no
      additional wire pruning is needed below the top level, since a wire
      can only ever connect two siblings declared on the SAME component
      (`instantiate`'s own wire-registration loop resolves both endpoints
      through one component's `child_ids` map), so a wire fully inside a
      removed subtree becomes unreachable together with its endpoints
      exactly like the top-level case. `wires` between children may be
      freely ADDED or REMOVED (it115), including RE-ROUTING an existing
      connection between two pre-existing children — a kept child's own
      live instance is never touched, only its `.wires` map is
      pruned/extended as needed. Still refuses the whole upgrade if: a
      new prop has no default; or a pre-existing child's own component
      type changed (nothing sound to migrate a live instance to a
      different type). **As of it128, the last remaining gap is closed**:
      a `migrate_<field>` component-private fun (exactly one param — the
      field's OLD value) is the user-provided hook for a field whose
      SHAPE, not just presence, changed (e.g. `Int` -> `Str`) — a naming
      CONVENTION, not new grammar (mirrors `KUPL_AI_MOCK_<NAME>`'s own
      reserved-name convention), applying to both props and state via one
      shared `apply_migration_hook` helper. A wrong-arity `migrate_*` fun
      refuses the WHOLE upgrade upfront (almost certainly a genuine
      mistake — nobody accidentally names a function exactly
      `migrate_<field>`), matching every other guard's own
      refuse-upfront-never-partially discipline.

## Tier 3 — audit-driven priorities (next arc)

Ordered by the comparison audit ([`COMPARISON.md`](COMPARISON.md), refreshed
after it129):
the lowest-scoring, most load-bearing gaps vs Python/Go/TS/Java/Rust/Haskell/
C++/Swift/Kotlin. Concurrency is the #1 gap for the "universal, any software"
claim (the runtime is single-threaded today; Go/Rust/Kotlin/Swift all win).

- [◐] **Concurrency / parallelism** (audit #1) — **`par { … }` fork-join
      (it11) + parallel iteration `par_map`/`par_filter`/`par_each` (it13), and
      as of it33-34 REAL OS-THREAD execution for `par_map` AND `par_filter` with a
      pure named callback over lists ≥ 256 elements** (`src/parallel.rs`: a `PortableValue`
      Send boundary + a Send+Sync `ProgramImage` + `std::thread::scope`; results
      placed by input index so the output is byte-identical to sequential
      `map`). Runs on BOTH the interpreter and the KVM (it35); the
      differential harness keeps a sequential-VM reference and
      absolute-value tests anchor correctness. **`par { }` fork-join
      branches ALSO now get real OS threads** (it99 interp, it101 KVM) when
      every branch is a plain call to a statically pure, top-level named
      function with plain-literal/identifier args — the same purity gate
      `par_map`/`par_filter` use; a non-qualifying branch falls the WHOLE
      block back to the unchanged sequential path (see
      `docs/design/bigarcs/3-real-concurrency.md`'s own "Progress" notes
      for the exact staging). Native/`cgen.rs` stays sequential for `par{}`
      by explicit design decision (it99), unlike `par_map`/`par_filter`
      which are interp/KVM-only real-threaded to begin with. **Genuinely
      still open** (this line was stale before it120's survey — the
      above WAS the "still open... extending real threads to `par{}`"
      item this line used to list, now landed): general **async I/O and
      `await` actually suspending** (today `await` evaluates
      synchronously — confirmed live, no scheduler exists anywhere in the
      runtime) and the **M:N work-stealing scheduler** `docs/design/
      LANGUAGE.md` §4's own vision text describes ("no bare threads in the
      app tier; the runtime multiplexes components on an M:N work-stealing
      scheduler") — neither has any implementation today, and this is
      explicitly named as the campaign's OWN "#1 gap for the universal,
      any software claim" (see this Tier's own intro line below). A
      substantial undertaking on the scale of capabilities' own
      it112-it118 arc — **design sketch written (it121,
      `docs/design/ASYNC.md`)**: the SAME `Rc`/`RefCell`-not-`Send` blocker
      `3-real-concurrency.md` already found for `par{}` applies with full
      force to per-instance concurrency too; the sketch's own recommended
      direction generalizes `PortableValue`'s existing clone-across-the-
      boundary pattern from a one-shot pure call to a long-lived
      per-instance actor thread, with cross-instance `expose` calls
      becoming genuine blocking cross-thread request/response (where
      `await` would finally gain real meaning) — but flags the
      determinism-under-real-concurrency question (§3.4) as the single
      hardest unresolved piece, on par with how root-seeding enforcement
      was the hardest remaining piece for capabilities. Not implemented —
      a bounded design deliverable, not code. Virtual clock (it9)
      preserved for deterministic tests. **it122**: attempted `ASYNC.md`
      §4's own narrow first-slice proof of concept (one child instance on
      its own thread, blocking `mpsc` `expose` calls) and found it
      undersells its own real scope — live-reading `interp.rs`'s `emit`
      confirmed wire-based port delivery, not just `expose` calls, ALSO
      shares the one global `self.instances`/`self.queue`/`self.current`
      that a threaded child would need to cross, since `emit`'s wire-
      target resolution and `self.current` both assume a single shared
      `Interp`. Documented this as an amendment to `ASYNC.md` §4/§5 rather
      than forcing an undersized attempt, and fell back to the note's own
      candidate (b) instead this iteration (see the stack-margin audit
      entry below). **it123**: answered the follow-up question it122
      raised — "what does a wire connecting two different per-thread
      `Interp`s look like" — with a new `ASYNC.md` §6, and it decomposes
      differently than expected. Inventorying every `self.instances[id]`
      access (not just `emit`/`expose`) finds **14 distinct functions**
      in `interp.rs` touching instance state directly (construction,
      wire delivery, the dispatch loop, timer arming/firing, supervision
      restart, lifecycle, `expose`/method dispatch, the `par{}` purity
      gate, property-test isolation) plus **44 touches** in `repl.rs`'s
      `:upgrade` machinery — confirming the real surface is far larger
      than "wires plus expose calls." Reached a decisive, non-obvious
      conclusion: even the SIMPLEST possible design (make every cross-
      thread interaction blocking, preserving today's exact FIFO
      ordering) does not shrink that surface at all, AND delivers zero
      practical concurrency benefit once built (nothing ever overlaps in
      wall-clock time) — it would be pure plumbing-validation. Real
      benefit requires SOME interactions to become non-blocking, which
      immediately reopens §3.4's determinism question — so the
      "cross-Interp wire" question and the "determinism" question are
      the SAME question, not sequential sub-problems as the it122 note
      had proposed. One new, genuinely actionable idea fell out of the
      inventory: a future (untried) refactor could introduce a single
      instance-access indirection layer, decoupled from threading
      entirely and verifiable as byte-identical, so a future threading
      attempt only needs to modify ONE place instead of auditing 14+
      call sites by hand — named as a candidate, not designed. Doc-only
      iteration (no `interp.rs` changes), matching it112/it121/it122's
      own precedent for design-sketch iterations. **it129: the §3.4
      determinism question is DECIDED** (`docs/design/ASYNC.md` §7, after
      sitting unpicked across it124-it128's own fallback mentions) —
      strategy (b): per-instance VALUE-level determinism preserved,
      TIMING-observable nondeterminism between independently-running
      instances documented as inherent to real concurrency, OPT-IN only
      (today's default synchronous behavior is unchanged for any program
      that doesn't explicitly ask for real concurrency, so the campaign's
      own sacred byte-identical-output regression discipline stays fully
      valid and untouched for every existing program). Grounded in
      `docs/design/VISION.md`'s own inspirations table, which lists
      Erlang/Elixir FIRST for exactly this part of the design — Erlang
      itself has never guaranteed global cross-process event ordering,
      only per-mailbox FIFO (the guarantee KUPL's own single shared queue
      already provides trivially today), so this is the most faithful
      continuation of the model this campaign's own docs already cite,
      not a departure from it. A design DECISION, not an implementation —
      a future concurrency attempt still needs its own scoping pass on
      top of this (§6's own 14-function instance-access surface, the
      wire-crossing question), but the "which determinism strategy" question
      that stalled every prior attempt at the investigation stage is
      resolved. **it132: a concrete, decisive implementation plan
      written** (`docs/design/ASYNC.md` §8) — resolves every remaining
      open question left dangling above rather than adding another
      sketch. Key decisions: (1) opt-in via a `concurrent component`
      declaration modifier, mirroring `ComponentDecl.is_app`'s own
      existing precedent, resolvable entirely at compile time since
      children/wires are already fully static; (2) the 14-function
      surface (§6.2) is resolved by an `InstanceSlot::Local(Instance) |
      Remote(ActorHandle)` split — NOT a bare per-id accessor (which
      it124 correctly declined, since no threading design existed yet to
      justify it) but a genuine split where every `concurrent` instance
      runs its OWN, otherwise-completely-unmodified `Interp` on its own
      thread, reusing the SAME 14 functions verbatim; (3) hub-and-spoke
      topology (all cross-actor traffic routes through the coordinator,
      no actor-to-actor channels) as a named v1 restriction; (4) two
      message shapes matching §3.3's already-decided semantics exactly
      (non-blocking `Deliver` for wire emit/timer fire, blocking `Call`
      for `expose`); (5) a NEW correctness hazard named and resolved —
      cross-actor `expose` call cycles can now deadlock (impossible
      today, since everything shares one call stack) — mitigated with
      per-thread pending-call-chain cycle detection and a clean panic,
      matching this file's own established "clean panic over silent
      hang" discipline; (6) the virtual clock stays coordinator-owned,
      with a small `Arc<Mutex<BTreeMap<usize,i64>>>` next-fire table (plain
      `i64` metadata, not `Value`/`Rc`, so this does NOT reopen the
      `Rc`→`Arc` blocker) letting `advance()` keep one global earliest-fire
      scan without round-tripping every tick; (7) `PortableValue` is NOT
      widened for v1 — `concurrent` component ports are restricted by
      `check.rs` to already-portable types, sidestepping the open
      Bound/Component-across-threads question entirely; (8) `concurrent`
      inside property-test bodies and `:upgrade` hot-swap of a concurrent
      instance are explicitly out of scope for v1 (checked rejections,
      not silent gaps); (9) VM/native need ZERO special-casing — a
      `concurrent` component simply never builds `Remote` slots on those
      engines, so the existing byte-identical harness applies completely
      unchanged; only a NEW interp-only, multi-run value-determinism
      check is needed, layered on top of (not replacing) today's suite;
      (10) a six-step staged build order mirroring `par{}`'s own it33-it101
      history. Still a design, not code — no `interp.rs` changes this
      iteration — but the next iteration that picks this up has an
      actual buildable plan, not another open question. **it133:
      implementation begins — §8.10 step 1 landed** (`concurrent
      component`/`concurrent app` syntax, a soft keyword matching `ai`/
      `state`'s own contextual-keyword precedent rather than a globally
      reserved word). `ComponentDecl.concurrent: bool`; two new checks
      (K0305: `concurrent` + root `app` is mutually exclusive; K0306:
      every prop/port/exposed-fun param and return type on a `concurrent`
      component must be representable by `parallel.rs::PortableValue`,
      generalizing §8.6 from "ports only" — props are needed to construct
      an actor on its own thread, exposed-fun signatures for the future
      blocking `Call` message; conservatively rejects user ADTs
      (`Ty::Named`) for v1 even where structurally portable, deferring
      arbitrary recursive-type-walk to later); `kupl fmt` round-trip.
      **Deliberately no runtime change** — interp.rs already works
      directly off the raw `ast::ComponentDecl` (confirmed live:
      `Interp::instantiate` reads `self.db.components` as `Rc<ComponentDecl>`
      directly, not a separately-compiled representation), so a
      `concurrent` component today parses, checks, and runs exactly like
      an ordinary one — zero behavior change, zero `compile.rs`/
      `bytecode.rs`/`kx.rs`/`cgen.rs` touches needed, confirming §8.8's
      own "VM/native need zero special-casing" claim already holds even
      at this first step. Full verification: `cargo build` clean;
      `cargo test` green **twice** (1711/1711 lib, excluding 4-5
      perf_guard timing tests independently confirmed flaky on a clean
      `git stash`ed baseline under this session's own concurrent test
      load, not caused by this change); `cargo test --bins` 62/62;
      `cargo test --doc` 0/0; interp-vs-vm sweep clean across all 56
      `examples/*.kupl` files with `main()`; 3 new unit tests
      (`concurrent_component_parses_checks_and_round_trips`,
      `concurrent_app_is_k0305`,
      `concurrent_component_non_portable_port_is_k0306`). Deliberately
      NOT yet documented in `LANGUAGE-REFERENCE.md` (the normative public
      spec) — the keyword has no runtime effect until step 2+ lands;
      documenting it now would describe a feature that doesn't work yet,
      the exact class of staleness this campaign's own docs discipline
      exists to prevent. Next: §8.10 step 2, the `InstanceSlot`
      `Local`/`Remote` split (pure refactor, ~32 `self.instances[id]`
      call sites across the 14 functions, zero `Remote` slots constructed
      yet — verify byte-identical before any actual threading).
- [x] **Stack-margin audit pass (it122)** — the recurring "adding a new
      `eval_call` match arm silently grows its debug-build per-call stack
      frame enough to tip an already-marginal `diff_*` recursion test into
      a genuine stack overflow" regression class (hit reactively at it109,
      it116, it118 — each time discovered only AFTER a real, unrelated
      change tipped a test over) got a proactive, EMPIRICAL sweep instead
      of another name-based guess. Method: temporarily pad `eval_call`'s
      own stack frame by 4KB (`std::hint::black_box([0u8; 4096])`,
      simulating a plausible future few-builtin-arms addition), run all
      526 `diff_*` tests under that simulated growth, iteratively `--skip`
      each stack-overflow casualty to surface every one at that pad size
      (a single overflow SIGABRTs the whole test binary, so casualties are
      found one at a time), then revert the probe. Found exactly 4 tests
      with thin-enough margin to overflow — `diff_amicable_pair`,
      `diff_fast_doubling_fibonacci`, `diff_luhn_checksum`,
      `diff_mobius_divisor_sum_identity` — NONE of which a keyword search
      over test names (`recur|fib|deep|nested|...`) would have flagged
      with confidence; `diff_mobius_divisor_sum_identity` in particular
      had an it497 comment explicitly claiming it was "verified
      empirically" safe, which was true at the time but had since eroded
      as `eval_call` grew. Wrapped all 4 in the established `std::thread::
      Builder::new().stack_size(2 * 1024 * 1024 * 1024)` pattern; re-ran
      the padded-probe sweep to confirm all 526 tests pass under the
      simulated growth before reverting the probe itself (net diff:
      `vm.rs` only, `interp.rs` untouched after revert). This is a testing-
      infrastructure hardening pass, not a language-level gap — listed
      here because it was the fallback candidate this iteration's own
      NEXT-note named, not because concurrency needed it.
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
      timers, `supervise` restart-on-failure (a C mirror of vm.rs, incl. a
      setjmp/longjmp panic landing pad), and cross-component `expose` calls
      (confirmed live: a parent calling a child's exposed method compiles and
      runs byte-identical to `kupl run`). counter/todo/timers/native-counter
      native stdout == `kupl run`. The only remaining piece is the OPTIONAL
      typed SSA IR / KIR (KValue unboxing for raw-register numeric loops — a
      performance, not correctness, arc; deliberately deferred). Effectful
      builtins (ai/json/sized/f32) inside native components defer as they do
      for `fun main`.
- [ ] KIR `kernel fun` + `at(gpu)` placement; Metal lowering first
- [◐] Sized numerics (i8…u64, f32), Byte/Char, BigInt/Decimal (audit #3) —
      sized ints i8…u64 fully landed across ALL engines: checked/wrapping/
      saturating arithmetic, width-aware bitwise, full conversion matrix
      (it27-29), and native codegen via a boxed __int128 KValue (it40). f32 runs
      on ALL engines incl. native (it28/it42, shortest-round-trip display). Bitwise Int methods + literals (it17); numeric formatting +
      math (it24). **BigInt (`big(...)`) and Rational (`rat(...)`) have since
      shipped too** — arbitrary-precision arithmetic and exact fractions, both
      byte-identical on every engine including native (see `STDLIB.md`).
      **`Char` has since landed too (it105/it106)** — a single Unicode
      scalar value (`'a'`), byte-identical on ALL FOUR engines including
      native (`kupl native` closed its staged-rollout gap at it106, via a
      real `K_CHAR` KValue tag in the C runtime), ordered by codepoint.
      **`Decimal` (`dec(...)`) has since landed too (it107/it108)** — an
      exact base-10 arbitrary-precision decimal (`sig * 10^-scale`, built
      on `BigInt` exactly like `Rational` is), byte-identical on ALL FOUR
      engines including native (`kupl native` closed its staged-rollout
      gap at it108, via a `KBig`-based `K_DECIMAL` KValue reusing the SAME
      `k_big_mul`/`k_big_divmod`/`k_big_pow` primitives `KRat` already
      has). Byte (as a type
      distinct from `u8`, investigated and left genuinely open — no
      current builtin returns/consumes raw bytes in a way a distinct type
      would improve, see the campaign log below) is the one remaining
      item here.
- [x] Broader standard library (audit #3, it12) — ~40 methods across all core
      types, all engines byte-identical incl. native. List (is_empty/concat/
      unique/init/tail/product/min/max/flatten/count/flat_map/window/chunk); Str
      (is_empty/reverse/lines/index_of/count/slice/pad_left/pad_right); Int (pow/
      gcd/clamp/sign/is_even/is_odd); Float (log/log10/exp/sin/cos/tan/sign/
      clamp/is_nan/is_infinite); Map (is_empty/get_or/merge/map_values); Set
      (is_empty/is_subset)
- [ ] System tier: ownership, `low`/`asm` (design §6; audit #4)
- [◐] **Capabilities as attenuable values (`CapNet`/`CapFs.limited_to(…)`)**
      — design sketch (it112, `docs/design/CAPABILITIES.md`; corrected
      it113/it114/it115) implemented as a real, ENFORCED, GENERALIZED
      pattern across TWO capability kinds (it116/it117/it118): `CapNet`
      (network, `http_get_with`) and `CapFs` (filesystem, `read_file_with`)
      — both flat builtin type names (no dotted-type-path grammar exists
      in KUPL, and it115 confirmed the module system doesn't need one
      either), each with `.limited_to(scope: Str)` (narrows only —
      widening an already-limited capability panics) and a `_with`-
      suffixed builtin (scope-checked BEFORE any network/disk I/O;
      existing `http_get`/`read_file` unchanged), and a root builtin
      (`cap_net_root()`/`cap_fs_root()`) — the ONLY way to obtain a
      capability at all — sharing ONE `check.rs` helper
      (`check_capability_root_call_site`) that restricts every kind's own
      root builtin to a direct call inside `fun main`'s own top-level
      body via a single `K0304` diagnostic. Both kinds are wired across
      interp/KVM/native (`examples/capabilities.kupl`) and are genuine
      "no ambient authority" security boundaries, closing the
      `docs/design/LANGUAGE.md` §2 claim. `CapFs` (it118) took
      substantially less effort than `CapNet` (it116/it117) since the
      whole pattern, including root-seeding enforcement, was already
      proven — confirms the pattern genuinely generalizes.
      **Remaining work is further generalizing to more capability kinds**
      (`CapSql`, ...) if a concrete need arises, not fixing either shipped
      kind.

## Tier 4 — ecosystem

- [ ] Package registry + `kupl pkg publish` with enforced API compat (client
      side — `kupl pkg tree`/`lock`/`fetch`, hash-verified atomic fetches, local
      path dependencies — already landed; no live server is hosted yet)
- [x] LSP: hover, completion, go-to-definition — DONE (it44; see the
      "Final stretch" entry above, which this line used to stale-duplicate).
- [x] `kupl patch` (it110) — `kupl patch <file> <ItemName> <replacement>
      [--write]` replaces one item's entire source span with a replacement
      file's own single item, the semantic inverse of `kupl context`'s item
      extraction ("models edit components, not line ranges",
      `docs/design/LANGUAGE.md` §6). Safety-checked like `kupl fmt --write`
      (refuses to write a patch that introduces new compile errors). No
      conformance suite numbering yet.
- [ ] WASM target; cross-compilation story

## Resolved design open questions (LANGUAGE.md §12)

1. UI trees → `docs/design/UI.md` (render = component construction). **Designed.**
2. Int default → **decided & shipped:** i64 checked, overflow panics.
3. Effect granularity → shipped hierarchical effects (`io` covers `io.fs`/`io.net`/`io.env`/`io.proc`/`io.time`; plus `ai`).
4. Hot-swap state migration → **FULLY RESOLVED (it128)**. Supervision
   restart hook; automatic by-name `state`/`props` migration; wire
   re-routing; RECURSIVE child removal (`on stop` + timer disarm, walking
   the removed child's own descendants; no extra wire pruning needed
   below the top level); and a user-provided `migrate_<field>` hook for a
   field whose SHAPE, not just name, changed — all shipped (it111, it113,
   it115, it119, it124, it128, `kupl repl :upgrade`). No remaining gaps.
5. Package identity → `kupl.toml` shipped; registry governance TBD.
