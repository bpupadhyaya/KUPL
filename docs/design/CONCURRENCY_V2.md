# Concurrency V2 — a phased plan to make KUPL's concurrency genuinely
# better than Go's and Java's

v0.1 (2026-08-24) — a design sketch, not code, mirroring `ASYNC.md`'s own
precedent exactly: investigate the field honestly, write down a credible,
concretely-scoped plan, so the next iteration that picks this up starts
from a real plan instead of re-deriving one. This document is the direct
follow-on to `ASYNC.md` (which designed and, as of PR-it1213–1225 in this
project's own hardening history, shipped KUPL's *current* `concurrent
component` actor model) — read that document first if you have not. This
one does not re-derive what already exists; it starts from it.

**Explicit goal, stated by the project owner**: make KUPL's concurrency
and multi-threading model *better* than Go's and Java's. This document
takes that goal seriously and does not soften it — but it also does not
pretend a from-scratch language can match a decade-plus of dedicated
runtime engineering in one leap. What follows is a genuine, phased path
toward that goal, with each phase independently shippable, independently
valuable, and honestly scoped against what it actually costs.

## 1. What exists today (verified live, not assumed, 2026-08-24)

- **`concurrent component`**: one real OS thread per instance
  (`interp.rs::instantiate_concurrent`), a 2GB reserved stack
  (`parallel::WORKER_STACK_SIZE`, shared with `par_map`/`par_filter`
  workers — sized to guarantee `MAX_CALL_DEPTH = 10_000` never native-
  stack-overflows). Communication is exactly two message shapes, per
  `ASYNC.md` §8.4's own decision: `Deliver(port, value)` (fire-and-forget,
  non-blocking wire delivery) and `Call { fn_name, args, reply }`
  (blocking request/reply, used for `expose fun` calls). Hub-and-spoke
  topology (`ASYNC.md` §8.3) — actors do not hold direct channels to each
  other, everything routes through the coordinator.
- **Total isolation, deep-copy on cross.** Every value crossing an actor
  boundary goes through `parallel::to_portable`/`from_portable` — a full,
  unconditional structural deep copy into `PortableValue` and back. No
  shared mutable state can ever cross a `concurrent component` boundary;
  this was a deliberate, correct decision (`ASYNC.md` §3) and remains
  correct — nothing in this document proposes weakening it. What this
  document DOES propose (§4.2) is a *fast path* that skips the copy when
  it's provably unnecessary, not a weakening of the isolation guarantee.
- **An actor processes exactly one message to full completion before
  looking at the next.** `instantiate_concurrent`'s own message loop is
  `while let Ok(msg) = inbox_rx.recv() { match msg { ... } }` — there is
  no suspension point *inside* handling one message; the next message is
  never even looked at until the current one (including any blocking I/O
  it performs) fully finishes. **This is the single most important fact
  this document's own §4.3 plan depends on** — verified again, freshly,
  for this document, not merely inherited from `ASYNC.md`.
- **Supervision**: `supervise child restart on_failure [max N in
  <duration>]` — a single strategy, restarting exactly the failed child.
  No `one_for_all`/`rest_for_one`-equivalent exists.
- **Lifecycle panic propagation** (this session's own PR-it1221/1222/1223
  fixes, 2026-08-24): a `concurrent` component's `on start`/`on stop`
  panic, or a panic while handling an incoming `Deliver`, now correctly
  propagates to the top level (`error[K0900]`, exit 101) instead of
  silently exiting 0 — closing what had been, until this session, a real
  and severe gap between the *documented* and *actual* behavior of the
  lifecycle model.
- **`par_map`/`par_filter`/`par { }`** (`parallel.rs`, pre-dating
  `concurrent component`, `ASYNC.md` §1's own "already-shipped" citation):
  real-thread DATA parallelism, gated to statically-pure callbacks only,
  falls back to sequential otherwise. A separate mechanism from
  `concurrent component`; nothing in this document proposes merging them,
  though §4.2's own "movable value" idea, if built, would benefit both.

## 2. The comparative research (condensed)

Eight languages were researched in depth (each via an independent,
citation-backed pass, ~90 combined web searches): **Go, Java, Erlang/OTP,
Rust, Pony, Swift, Kotlin, Clojure**. The full research write-ups are not
reproduced here (they run to several thousand words each); this section
distills the load-bearing facts into one comparison table plus the
specific, individually-cited findings that shaped §4's synthesis.

### 2.1 Comparison table

| | Go | Java (Loom) | Erlang/OTP | Rust | Pony | Swift | Kotlin | Clojure | **KUPL (today)** |
|---|---|---|---|---|---|---|---|---|---|
| **Unit of concurrency** | goroutine | virtual thread | BEAM process | OS thread | actor | Task/actor | coroutine | thread (+ agent) | actor (`concurrent component`) |
| **Cost per unit** | ~2KB, grows | ~few KB (heap continuation) | ~2.6KB | ~1MB (OS) | JVM/native, pooled | heap continuation | heap continuation (state machine) | OS thread | **2GB reserved stack, real OS thread** |
| **Scheduling** | M:N, GMP, work-stealing | M:N onto carrier pool | 1:core schedulers, reduction-counted preemption, work-stealing | 1:1 (OS-scheduled) | fixed OS-thread pool, work-stealing | cooperative pool, continuation-based | cooperative pool (compiler CPS) | OS-scheduled | **1:1, always-resident OS thread per actor** |
| **Preemption** | async (signal-based, since 1.14) | N/A (cooperative unmount at blocking calls) | preemptive (reduction counting) | N/A (OS) | N/A (run-to-completion, no blocking primitive) | cooperative | cooperative (checkpoint-only) | N/A (OS) | N/A (run-to-completion per message) |
| **Data-race prevention** | none (dynamic `-race` only) | none | isolation (no shared memory) | ownership + Send/Sync (compile-time) | reference capabilities (compile-time) | actor isolation + Sendable (compile-time, staged) | none | immutability + STM (runtime) | **isolation (no shared memory) — deep-copy enforced** |
| **Deadlock freedom** | no | no | no (but isolation limits blast radius) | no | **yes, by construction** (no blocking primitive) | no | no | **yes for STM specifically** (age-based barging) | no (cross-`Call` cycles possible; `ASYNC.md` §8.4 tracks and rejects them, not prevents by construction) |
| **Structured lifetimes** | no (library-only, `errgroup`; a language proposal was rejected) | in progress, 7+ years, still not final | N/A (supervision trees instead) | partial (`thread::scope`, 2022) | N/A (actors are independent) | yes, day-one (2021) | **yes, day-one (2018) — first at production scale** | N/A | partial — spawning is static/declarative (`let x = Concurrent()`), but no explicit scope/join concept |
| **Distribution** | none (external only) | none (external only) | **built into the language/VM** | none (weak ecosystem) | none shipped (aspirational only) | yes (`distributed actor`, 2022, niche) | none | none | none |
| **Message cost model** | N/A (shared memory + channels) | N/A | deep-copy (except large binaries, refcounted) | N/A | **zero-copy for `iso`, by compiler proof** | N/A | N/A | N/A (in-process refs) | **deep-copy, unconditional** |

### 2.2 The specific findings that most shaped this design

1. **Rust**: `Send`/`Sync` are zero-field marker traits that extend an
   *already-existing* single-thread aliasing rule across a thread
   boundary via ordinary trait-bound checking — no new concurrency-
   specific type-system machinery. KUPL's own `PortableValue` conversion
   is architecturally the *dynamic, runtime* analogue of the same idea
   (only structurally-portable values may cross) — worth naming
   explicitly as a point of continuity, not a gap.
2. **Rust**: `Mutex<T>` *wraps* the data it protects — the only path to
   `&T`/`&mut T` is through a held guard. This structurally eliminates
   "forgot to lock before touching shared state," independent of any
   ownership system — a pure API-design lesson, directly reusable by any
   future KUPL shared-state primitive (§4.4).
3. **Pony**: reference capabilities let a *provably-unique* (`iso`) value
   cross an actor boundary via pointer-move, zero copy, regardless of
   size, because the compiler already proved no other alias exists. Full
   adoption (the six-capability lattice) is explicitly "notoriously hard
   to retrofit" and not recommended. But KUPL's existing `Value`
   representation is *already* `Rc`-based for every heap variant
   (verified live, §1) — a **dynamic** equivalent of Pony's *static*
   proof (`Rc::strong_count(&rc) == 1` at the moment of a send) is a
   direct, narrow, low-retrofit-cost analogue. See §4.2.
4. **Pony / Erlang**: neither needs green threads, coroutines, or stack-
   switching to multiplex many actors onto few OS threads — both rely on
   the fact that an actor/process runs one unit of work to completion
   before yielding, so a *fixed pool of OS worker threads pulling
   ready-actors off a queue* suffices. KUPL's own actor model **already
   has this exact property** (§1) — the only thing standing between KUPL
   and this scaling model is that today's actors hold their OS thread
   even while idle, blocked on `inbox_rx.recv()`. See §4.3 — this is the
   single highest-leverage idea in the entire study.
5. **Erlang**: reduction-counted *preemptive* fairness — the VM enforces
   yielding, not the process. Kotlin's cooperative-only cancellation (a
   CPU-bound loop with no suspension point silently ignores cancellation)
   is the documented cautionary counter-example. Directly relevant if
   KUPL ever needs to bound how long one actor can occupy a shared worker
   thread (§4.3) — flagged as an open question there, not decided here.
6. **Erlang**: `one_for_one`/`one_for_all`/`rest_for_one` supervision
   strategies. KUPL has only the `one_for_one` equivalent today. See
   §4.1 — the smallest, safest, most immediately valuable item in this
   whole document.
7. **Clojure**: STM (refs/`dosync`/`commute`, snapshot-MVCC, age-based-
   barging conflict resolution, provably non-deadlocking) as a
   *complement* to actor isolation for the minority of state that is
   genuinely shared rather than genuinely owned by one actor. A concrete
   opportunity to do better than Clojure itself: Clojure's `commute`
   trusts a programmer's "this op is commutative" claim at runtime with
   no check; KUPL, being statically typed, could plausibly *verify*
   commutativity for known-commutative built-in operations rather than
   trusting an annotation. See §4.4 (explicitly scoped to a LATER phase
   — this is the most novel, least de-risked idea here).
8. **Swift**: actor reentrancy — mutual exclusion is released at every
   suspension point *inside* an actor method, so invariants that held
   before a suspend can be silently violated by the time it resumes,
   with no compiler signal. **This is a concrete hazard to design against
   preemptively**: KUPL's actors have no suspension points inside a
   handler today (§1), so this bug class does not exist yet — but if any
   future phase of this document (or any other change) ever introduces
   one, this exact hazard becomes live and must be designed against from
   the start, not discovered after shipping the way Swift's own team
   apparently was.
9. **Swift 5→6→6.2, and Java's still-unfinished structured-concurrency
   saga**: shipping a stricter/sounder concurrency-safety mechanism
   reliably takes multiple rounds of ergonomics course-correction, even
   for teams with vastly more resources than this project has. Budget
   real iteration time for ergonomics after any new safety mechanism
   ships, in every phase below — don't expect a first draft to be right.
10. **Every language that added structured concurrency as a retrofit
    (Go, Java) paid a durable tax**; every language that had it from
    inception (Kotlin, Swift) didn't. KUPL's own `concurrent component`
    spawning is *already* static/declarative
    (`let child = Concurrent()`, resolved entirely at compile time per
    `ASYNC.md` §8.1) — closer to "structured by construction" than Go's
    unstructured `go f()` ever was. §4.5 makes this explicit rather than
    leaving it implicit.

## 3. What this document deliberately does NOT propose

Named explicitly, mirroring `ASYNC.md` §5's own "open questions this
sketch does not resolve" discipline — these were seriously considered and
set aside, not overlooked:

- **A CPS rewrite or unsafe stackful coroutines for the tree-walking
  interpreter, to get arbitrary suspend/resume.** This remains the
  correct, load-bearing reason a *general* M:N scheduler (matching Go's
  or Java's own scope) is not proposed here. §4.3's worker-pool idea
  gets most of the *scaling* benefit without this cost, precisely because
  KUPL actors don't need to suspend mid-handler today (§1). If a future
  need for genuine non-blocking I/O *inside* a handler ever arises, that
  is a narrower, separately-scoped problem — see §5 (deferred).
- **A full reference-capability type system (Pony's six-capability
  lattice).** Confirmed by the research itself (§2.2.3) as high-risk to
  retrofit. §4.2 takes the narrow, dynamically-checked subset instead.
- **Built-in distribution (Erlang/Swift-style).** No language studied
  besides Erlang treats this as anything but bolted-on, and even Swift's
  version (2022, real, shipped) is described by its own research pass as
  "niche, adoption/tooling maturity lags far behind local actors." Out of
  scope for this document; worth its own dedicated design pass later if
  ever pursued, not a `CONCURRENCY_V2` line item.
- **Weakening the deep-copy isolation guarantee generally.** §4.2 adds a
  *fast path*, never a way to share a live mutable reference across an
  actor boundary undetected.

## 4. The phased roadmap

Each phase is independently shippable and independently valuable — this
is not "do all of §4 or nothing." Each item below should, when actually
implemented, follow the exact discipline already established in this
codebase's own hardening history: live verification before code, a real
regression test, a revert-and-verify cycle, `cargo test` green twice, and
— for anything touching `interp.rs` upstream of an engine boundary — the
SACRED interp/VM/native cross-engine sweep (though note: `concurrent
component` is, and after this document remains, interp-ONLY — VM and
native have no support for it, confirmed via `grep` during this session's
own PR-it1221 work — so most items below need NO cross-engine sweep,
exactly like the recent PR-it1221-1225 lifecycle fixes needed none).

### 4.1 v1a — Supervision strategies (smallest, safest, ship first)

Add `one_for_all` and `rest_for_one` restart strategies to `supervise`,
alongside the existing (unnamed, implicitly `one_for_one`) default —
Erlang's own three-strategy vocabulary (§2.2.6). Concretely:
- New syntax: `supervise child restart on_failure one_for_all` /
  `... rest_for_one`, defaulting to today's exact behavior
  (`one_for_one`) when no strategy keyword is given, so every existing
  program's behavior is provably unchanged (a pure additive parse/check/
  interp change, same category as this session's own `--timeout`-style
  additive CLI fixes).
- `one_for_all`: when a child fails, every OTHER child under the SAME
  parent that also declares `restart on_failure` (of any strategy) is
  torn down and restarted too, not just the failed one.
- `rest_for_one`: every child declared AFTER the failed one, in source
  order (`comp.children`'s own declaration order — already static,
  `ASYNC.md` §8.1), is restarted too; children declared before it are
  untouched.
- This is a pure extension of `interp.rs::restart`'s existing per-
  instance logic — no new cross-thread messaging, no interaction with
  `concurrent component` at all (a plain `Local`-instance supervision
  concern, unrelated to the actor-thread machinery). Lowest-risk item in
  this whole document; a good first PR-cv2 entry.

### 4.2 v1b — A movable/owned value fast path for cross-actor sends

**The idea** (from Pony, §2.2.3, adapted to KUPL's existing `Rc`-based
`Value` representation, §1): at the exact moment a value is about to
cross a `concurrent component` boundary (a `Deliver`/`Call` send), check
whether its outermost `Rc` (for the heap-allocating variants — `List`,
`Str`, `Map`, `Set`, `Tensor`, `BigInt`, `Rational`, `Decimal`, per §1's
live-verified variant list) has `Rc::strong_count(&rc) == 1`. If so, no
other reference to this data exists ANYWHERE in the sending actor's own
program state — an actor's own `env` holds the only path to it, and that
env is about to release it via the send. In that case, the value's inner
data can be MOVED (via `Rc::try_unwrap` or `Rc::get_mut`, mirroring the
existing pattern already used at `vm.rs`'s own `Rc::get_mut(rc)`-based
string-mutation fast path — precedent already exists in this codebase)
into the receiving actor's own newly-allocated `Rc`, instead of running
the full recursive `to_portable`/`from_portable` structural walk.

**This must be VERIFIED LIVE, not assumed, before implementation begins**
(matching `ASYNC.md`'s own discipline exactly) — specifically:
- Does `Rc::strong_count(&rc) == 1` at the top level actually imply no
  aliasing of anything REACHABLE from it? (I.e., does KUPL's `Value` do
  any internal structural sharing — e.g. does a `List` slice/tail
  operation reuse the SAME inner `Rc<Vec<Value>>` as its parent, the way
  Clojure's persistent vectors do — or does every mutation/derivation
  always allocate a fresh `Rc`? If KUPL does NOT do structural sharing
  internally today, `strong_count == 1` at the top level is sufficient by
  itself; if it DOES, the check needs to be a recursive "every Rc in this
  subgraph has strong_count 1" walk, which is more expensive and may
  erode the benefit for anything but the outermost container. This is a
  concrete, answerable question — answer it with `grep`/direct reading of
  `value.rs`'s own construction sites before writing any Move logic, not
  by assumption.)
- What is the actual, measured win? `to_portable`'s own cost is
  proportional to value size (§1's PortableValue conversion is already a
  documented, deliberate design point — see `parallel.rs`'s own doc
  comments on why deep-copy was chosen for `par_map`). Benchmark a
  realistic large-value send (e.g., a multi-thousand-element `List`) with
  and without the fast path before committing engineering time — if the
  win is marginal for realistic KUPL program shapes, this item should be
  deprioritized in favor of §4.3, which has a clearer, larger payoff.
- Fall back to the existing deep-copy path unconditionally whenever the
  uniqueness check fails or is inconclusive — matching this module's own
  established "when identity/safety can't be proven, degrade to the
  always-safe path" precedent (`buildcache.rs`'s own `self_hash`/
  `cache_dir` `Option`-returning design, PR-it1215/1217, is the exact
  same shape of decision already made and tested in this codebase).

**LIVE-VERIFIED (PR-cv2-2 investigation, 2026-08-24) — DEPRIORITIZED, not
implemented.** Both open questions above were answered directly, and the
answer is: this section's benefit is real but narrow, and not worth the
risk at current priority. Specifics:
- **A literal "move the `Rc` across the thread boundary" is not actually
  what's needed, and isn't directly possible anyway**: `Rc<T>` is not
  `Send` (a static, compile-time property — a runtime `strong_count == 1`
  check cannot make an `Rc` itself cross an `std::sync::mpsc::Sender`
  typed for cross-thread transfer without `unsafe impl Send`, which this
  codebase has zero precedent for and which this document's own §3
  explicitly rules unsafe stack-switching/coroutines out of scope for
  similar risk reasons). `ActorMsg::Deliver`/`ActorMsg::Call` already only
  ever carry `PortableValue` (`interp.rs`'s `inbox: Option<mpsc::Sender<
  ActorMsg>>`, confirmed via `grep`) — `PortableValue` is Send precisely
  *because* it owns its data outright and contains no `Rc` at all. The
  only sound version of this idea is narrower: teach `to_portable` to use
  `Rc::try_unwrap` instead of `(**rc).clone()` at each leaf when the
  refcount allows it, still producing an ordinary `Rc`-free `PortableValue`
  — an ownership-transfer optimization *inside* the existing conversion,
  not a bypass of it. No unsafe needed either way.
- **Structural sharing**: confirmed via `value.rs` that `List`/`Map`/`Set`
  are `Rc<Vec<Value>>`/`Rc<Vec<(Value,Value)>>` with no slice/tail-sharing
  operations — but a DIFFERENT, more fundamental aliasing path exists
  regardless: any ordinary re-use of the same `Value` in two places (e.g.
  `[inner, inner]`, or storing the same List in two variables) does a
  cheap `Rc::clone` on `Value`'s own `#[derive(Clone)]`, inflating a
  *nested* value's refcount even while the *outer* container's own Rc
  stays at 1. So a shallow, top-level-only `strong_count == 1` check is
  unsound for anything with Rc-bearing children (`List`/`Map`/`Set`/`Ctor`
  of non-scalar elements) — only sound for `Str`/`BigInt`/`Rational`/
  `Decimal`/`Tensor` directly, or a container proven (recursively) unique
  all the way down. The recursive version is exactly as expensive to WALK
  as `to_portable` already is (must visit every node either way to check
  its own refcount) — it only saves the leaf-level byte-copy, not the
  traversal.
- **Measured**: a scratch benchmark (`parallel.rs`, `to_portable` called
  directly, release build, removed after gathering data — not left in the
  tree) on three shapes: a 200k-element `List<Int>` (no Rc-bearing leaves
  at all — zero possible win) cost **~3.0ms/call**; a 50k-element
  `List<Str>` (~40 bytes each — leaf-level win possible) cost
  **~1.37ms/call**; a single 200KB `Str` (the single BEST case for this
  optimization — one Rc, one leaf, zero container traversal) cost only
  **~2.4µs/call already**, on the existing clone-based path. The
  traversal/allocation cost of rebuilding the `PortableValue` tree (an
  unavoidable cost either way, since `Value` and `PortableValue` are
  different Rust types — converting between them requires visiting every
  node regardless of who owns the underlying bytes) dominates total cost
  for any container-shaped value; the byte-copy this optimization would
  skip is a small fraction of that, and even in the best-case (a bare
  large string) is already sub-3-microsecond — almost certainly smaller
  than the `mpsc` channel send + thread-wake cost the message pays
  immediately afterward.
- **Conclusion, exactly matching this section's own pre-written fallback
  instruction**: the win is marginal for realistic KUPL program shapes.
  Deprioritized in favor of §4.3. Do not re-attempt this without either a
  concrete user-reported perf complaint pointing here specifically, or a
  message shape this benchmark didn't cover (e.g. very large `BigInt`/
  `Tensor` payloads, where the leaf-copy cost could plausibly dominate
  differently than the `Str`/`List` shapes tested here).

### 4.3 v2 — A fixed worker-thread pool, replacing "1 OS thread per actor,
### always resident" (the highest-leverage item in this document)

**The core insight** (§2.2.4, §1): KUPL actors already run exactly one
message to full completion before yielding — there is no suspension point
inside a handler. This is PRECISELY the property that lets Pony and
Erlang multiplex many actors onto a small, fixed pool of real OS threads
via a work-stealing scheduler, with ZERO need for green threads,
coroutines, or stack-switching. KUPL can adopt the identical scheduling
strategy without touching `interp.rs`'s own evaluation model at all —
this is a scheduler/runtime change, not an interpreter rewrite.

**The problem this solves**: today, `N` live `concurrent component`
instances cost `N` real OS threads, each holding a 2GB stack reservation,
whether or not that actor currently has any work. This caps the practical
number of concurrent components in a single KUPL program far below what a
Pony/Erlang/Go-scale system could support — not because of computational
cost, but because of OS thread-creation/context-switch/address-space-
reservation overhead multiplied by actor count.

**The shape of the fix** (design-level, NOT a final implementation — the
next iteration that picks this up should verify each point live against
current `interp.rs`/`parallel.rs` code before writing anything, exactly
as `ASYNC.md` §8 did for the ORIGINAL actor model):
- Replace "each `concurrent component` spawn creates a dedicated
  `std::thread::Builder::spawn` with its own always-blocking `while let
  Ok(msg) = inbox_rx.recv()` loop" with: a **fixed pool** of `N` worker
  OS threads (default sized to `std::thread::available_parallelism()`,
  mirroring `parallel.rs::par_eval_with_stack_size`'s own existing
  sizing convention), each running a work-stealing loop over a queue of
  **ready actors** (an actor is "ready" exactly when it has ≥1 message in
  its own private queue and is not CURRENTLY being processed by another
  worker).
- Each actor keeps its own private mailbox (`VecDeque<ActorMsg>` behind a
  lock or lock-free MPSC queue — a genuinely new piece of concurrent-data-
  structure engineering this codebase hasn't needed before; this is
  where real care is needed, matching the rigor this project already
  applies to `registry.rs`'s own `CacheLock`/staleness-recovery work) —
  NOT a dedicated OS-level channel/thread per actor.
- A worker thread that picks up a ready actor processes exactly ONE
  queued message (matching today's own per-message-to-completion
  semantics EXACTLY, §1 — no interpreter change needed here at all),
  then re-checks that actor's queue: if more messages are pending,
  either continue processing the SAME actor (bounded — see the fairness
  question below) or requeue it as ready and pick up a different actor
  (fairness-preferring). Either choice preserves per-actor FIFO message
  ordering (already a property `ASYNC.md` §6.1/current implementation
  relies on) as long as only one worker ever processes a given actor's
  queue at a time — enforce this via a per-actor "currently being
  processed" flag/lock, checked before a worker claims an actor.
- **Open fairness question, explicitly deferred, not decided here**:
  should a worker keep processing the SAME actor across MANY queued
  messages before yielding to another actor (higher single-actor
  throughput, risk of starving other ready actors), or yield after every
  single message (fairer, more scheduler overhead)? Erlang's reduction-
  counting (§2.2.5) is the natural mechanism if starvation turns out to
  be a real problem in practice — but do NOT build reduction-counting
  preemptively; measure first whether naive per-message yielding is
  already good enough, and only reach for something more elaborate if a
  live-reproduced starvation scenario justifies it (matching this
  project's own "a real gap found+fixed" discipline — never a
  speculative complexity addition).
- **A blocking I/O call inside a handler still occupies its worker
  thread for the call's duration** — this is the SAME tradeoff Erlang
  documents for NIFs and Go documents for blocking syscalls (§2.2 Go
  notes: "no limit... on threads blocked in system calls... does not
  count against GOMAXPROCS"). A KUPL program with many actors ALL making
  long blocking I/O calls simultaneously could still exhaust the worker
  pool — flagged honestly as a known, inherited limitation of this phase,
  not solved here (§5 names the eventual, much harder fix).
- **Migration is additive, not a breaking rewrite**: the message-passing
  API (`Deliver`/`Call`) and the hub-and-spoke topology are UNCHANGED —
  this phase only changes how an actor's own handler code gets scheduled
  onto a real OS thread, not what it can do once running. Existing
  `concurrent component` programs should be provably unaffected in
  observable behavior (same "prove it via the old code path taking the
  unchanged branch" argument `ASYNC.md` §8.2 already used for the
  `Local`/`Remote` split).

**LIVE-IMPLEMENTED (PR-cv2-3, 2026-08-24).** A fixed `ActorPool` now
exists in `interp.rs`. Everything sketched above is built EXACTLY as
described, with one clarified detail this document's own draft left
open: `Interp` is NOT `Send` (verified live via a scratch
`assert_send::<Interp>()` test — fails on `Env`'s `Rc<RefCell<EnvInner>>`
and `ProgramDb`'s `Rc`-keyed maps), so a generic work-stealing pool (any
of N threads may run any ready actor) is not reachable without `unsafe
impl Send`. The pool instead SHARDS: each pooled actor is assigned to
exactly one worker at creation (round-robin) and stays pinned to it for
its whole life — no cross-thread `Interp` movement ever needed, and (a
nice side effect) no per-actor "currently processing" lock needed either,
since a worker's own command loop already processes one command at a
time. Only TOP-LEVEL (coordinator-spawned) actors are pooled; a
`concurrent component` spawned from code already running on a pool
worker's own thread (a nested child) still gets a dedicated thread —
checked via a thread-local flag set once per worker — so a parent
blocking a `Call` on its own child can never deadlock against a shared
worker's single-threaded command loop, at any nesting depth.

**The actual measured win, found via live benchmarking (per this
document's own "verify live" discipline for every item in this
roadmap)**: the pool alone made NO measurable difference to a synthetic
1000/3000/6000-actor benchmark. Investigating why led to a real,
unrelated finding: an O(n²) bug in `check.rs`'s children-checking loop
(rebuilt the "children in scope" portion of a fresh `Ctx` from scratch
for every child — an O(n) rescan run N times) that dominated the
benchmark's own cost regardless of engine or threading model — `kupl
check` on 4000 flat sibling children took ~6 seconds before the fix,
~20ms after, with NO code touched outside `check.rs`. This bug affected
EVERY KUPL command (`check`/`run`/`build`/...) for ANY program with many
sibling children in one component, not just `concurrent` ones — fixed
first, separately from the pool itself. With both fixes in place, the
concurrent-actor benchmark improved from 19.4s to 6.0s at 6000 actors
(interp `kupl run`) — a real win, though a smaller residual superlinear
cost remains (per-actor memory footprint stayed roughly unchanged at
~1.4MB/actor RSS even after pooling, suggesting the pool's OWN
contribution is now the SMALLER of the two effects at this scale) —
flagged honestly as a real, open question for a future iteration, not
chased further in this PR: is that residual cost algorithmic (another
per-actor operation scaling with total actor count somewhere in
`instantiate_concurrent`/`ActorPool`/`worker_loop`) or a genuine resource
constant (allocator/OS-scheduler contention under heavy simultaneous
thread pressure, inherent to spawning thousands of actors near-
simultaneously regardless of implementation)? Whoever picks this up next
should MEASURE first (matching this PR's own discipline), not assume.
Verification: the pool's deadlock-avoidance design confirmed via a live
nested-actor fixture (a pooled parent blocking a `Call` on its own
dedicated-thread child); 200-sibling-actor isolation confirmed live; a
genuine revert-and-verify for both fixes (each measurably regresses when
undone); full suite green twice.

**PR-cv2-5 (LIVE-IMPLEMENTED, 2026-08-24) — the open question above,
partially but not fully answered.** `worker_loop` was calling
`ProgramImage::actor_db` (the full-program deep-clone) on EVERY actor
spawn, even though PR-cv2-3 already made many actors share one worker —
redundant, since the clone only needs to happen ONCE per worker. Caching
it (`worker_loop`'s own `cached_db`, keyed by `Arc::ptr_eq` against the
`ProgramImage`) roughly HALVES both wall-clock time and peak memory for
many-actor programs, measured cleanly on a quiet machine: 6000 actors
went from ~6.0-6.4s/several-GB to ~3.0-3.5s, confirmed via a genuine
revert-and-verify. This is a REAL, substantial fix — but it does NOT
fully close the open question above. Re-measuring peak RSS per actor
AFTER this fix still shows GROWTH, not a flat per-actor constant: ~152KB/
actor at 1000 actors, ~707KB/actor at 6000 actors — smaller in absolute
terms than before, but still clearly superlinear, and its root cause is
NOT YET IDENTIFIED (checked and ruled out: `Interp`/`PooledActor`/
`Instance`/`ProgramDb`'s own `size_of` are all a few hundred bytes each,
nowhere near hundreds of KB; `run_timers(100)` is a no-op for a
timer-free component; `Vec` reallocation is standard amortized-O(1)).
Left as a genuinely open, NOT-yet-explained question for whoever
continues this investigation — needs proper memory profiling (this
session didn't have a heap profiler readily available and stopped
ad-hoc guessing rather than keep burning effort on trial and error).

**PR-cv2-6 (LIVE-IMPLEMENTED, 2026-08-24) — the question above, actually
answered.** Continued past the "no profiler available" stopping point by
building a minimal one: a temporary `pub fn current_allocated_bytes()`
in `memcap.rs` (exposing the EXISTING `CappedAlloc` allocator's own
net-bytes-allocated counter, already built for `--max-memory` and reused
here for free) plus scratch trace points bracketing each phase of one
actor's spawn. Found: ~100% of the per-actor cost was in the single line
`Interp::new(db)` — not `instantiate_local`/`start_all`/`run_timers` at
all. `Interp::new`'s own body calls `ProgramImage::from_db(&db)`
internally — a SECOND full deep-clone of the whole program's AST, built
again from the `db` PR-cv2-5's cache had JUST cheaply cloned, even
though a perfectly good `Arc<ProgramImage>` was already in scope at both
call sites. This bug predates the whole concurrency-v2 initiative
(present since `concurrent component` was first built) — invisible
before because dedicated-OS-thread creation overhead masked it, and
nobody had benchmarked thousands of actors until this session did.
Fixed via a new `Interp::new_with_image(db, image)` constructor that
reuses the already-built image via `Arc::clone` instead of rebuilding.
**Compounding with PR-cv2-5: 20,000 actors went from ~33.0s/~45.5GB to
~0.35s/~232MB — ~94x faster, ~196x less memory.** Verified up to 100,000
actors (1.71s, held linear scaling). All scratch instrumentation removed
after the finding was confirmed — none of it shipped. Full suite green
twice; a genuine revert-and-verify at decisive scale (20,000 actors,
confirmed to fail predictably when reverted). KUPL commit `d8fb142`.

**A refinement to the "~11KB/actor" figure above, PR-cv2-6's own follow-
up investigation (same session, same technique)**: that number was
itself inflated by measurement noise — `ALLOCATED` is a single
process-wide atomic counter, and RSS/(actor count) conflates ALL 18
worker threads' concurrent allocation activity together. Forcing the
pool to a single worker (`ActorPool::get`'s own `n`, temporarily
hardcoded to `1`, then reverted) for a clean trace found the TRUE
steady-state net cost is ~4.2KB/actor, not ~11KB — comparable to
lightweight-thread territory, not a cause for further alarm. Breakdown:
~3KB in `ProgramDb::clone()` (seven small `HashMap`/`HashSet` fields,
each paying its own minimum bucket allocation on clone regardless of
entry count), ~1.2KB in `instantiate_local` (one `Env`, one state field,
one `Instance` push), ~100 bytes in `Interp::new_with_image` itself.
This is now a genuinely small, likely-near-floor number for the CURRENT
per-actor-owns-its-own-`ProgramDb` architecture — further reduction
would need `ProgramDb`'s own collections to be shared (not cloned) across
every actor on a worker, which is architecturally sound (the AST is
immutable after checking, never mutated by a plain `kupl run` actor) but
would require changing `Interp.db`'s own TYPE from an owned `ProgramDb`
to a shared reference, touching every call site that reads it — a real,
larger refactor for a ~3KB/actor win (meaningful at huge scale, e.g.
~300MB saved at 100,000 actors, but not transformative given the
current ~232MB/20,000-actors baseline is already solid). Deprioritized
in favor of the bigger remaining lever below — flagged here as a real,
scoped, low-risk future micro-optimization if `concurrent component`
memory at extreme scale (100,000+) ever becomes the binding constraint.

### 4.4 v2/v3 (scope after §4.3 ships and is measured) — an STM-style
### `ref` primitive for genuinely shared cross-actor state

From Clojure (§2.2.7): actors for concurrent BEHAVIOR, a transactional
reference type for the minority of state that's genuinely SHARED rather
than owned by one actor (a coordination counter, a shared cache visible
to multiple components) — an escape hatch that's safer than either
routing everything through message-passing (an awkward fit for naturally-
shared data) or ad hoc unsynchronized globals (unsafe). Concretely, a
possible `kupl` primitive: a `Ref[T]`-typed value (constructible only for
portable `T`, matching the existing portability constraint already
enforced for actor boundaries) with `read`/`update`-style operations that
use snapshot-MVCC + retry-on-conflict (Clojure's own age-based "barging"
resolution is a well-specified, implementable algorithm, not a research
problem) — provably non-deadlocking BY THE SAME ARGUMENT Clojure's STM
already provides (§2.1 table: "yes for STM specifically").

**The concrete opportunity to do better than Clojure itself** (§2.2.7):
Clojure's `commute` (skip-retry for declared-commutative operations)
trusts the programmer's claim at runtime with no verification. KUPL,
being statically typed, could define a small, closed set of KNOWN-
commutative built-in operations (integer/float addition, set union, list
append-without-reordering-dependence, etc.) and let the type/effect
checker verify a `commute`-style fast path is actually being used only
for provably-commutative operations — a genuine, novel improvement on the
reference language, not just a port. This is explicitly the MOST
speculative, least de-risked item in this document — do not start it
before §4.1–§4.3 have shipped, been used in real KUPL programs, and
proven that genuinely-shared cross-actor state is a real, recurring need
(not merely a theoretically nice-to-have) — matching this project's own
"a real gap found+fixed" discipline, never a speculative feature added
because a reference language has it.

### 4.5 v1 (documentation-only) — precise, honest safety-guarantee
### documentation

**SHIPPED (PR-cv2-4, 2026-08-24)** — later than "alongside §4.1" as
originally planned, but landed once §4.3's own thread-model change made
this the natural moment to also refresh the now-slightly-stale "1
dedicated OS thread per instance" wording. See `docs/PRODUCTION.md`'s own
"Mostly single-threaded execution" bullet for the shipped text.

Update `docs/design/ASYNC.md` and/or `docs/PRODUCTION.md` with a
Rust-Book-precise statement of what KUPL's `concurrent component` model
actually guarantees, modeled directly on the precision the Rust research
pass found in the Rust Book itself (§2.2, verbatim-quoted there): **KUPL
prevents DATA RACES on values crossing a `concurrent component` boundary
(total isolation, deep-copy-enforced) — it does NOT prevent DEADLOCK
between two actors each blocked in a `Call` to the other** (already
tracked and rejected via a chain-of-pending-calls check, `ASYNC.md` §8.4
— "refused, not prevented by construction," an important and currently
under-emphasized distinction) **and does NOT (yet) provide Swift/Kotlin-
style STRUCTURED lifetime scoping**, though §2.2.10 argues KUPL's
existing static/declarative spawning is already closer to "structured by
construction" than Go's model ever was — state this precisely rather than
either overclaiming or underclaiming it. This costs nothing to ship (no
code change) and directly avoids the single most repeated lesson from the
whole research pass (§2.2.9): be precise about the guarantee's actual
boundary, every time, in public-facing documentation.

## 5. Deferred to a much later phase, explicitly out of scope for now

- **Genuine non-blocking I/O inside a `concurrent component` handler**
  (matching Go's netpoller or Java's Loom-unmount-on-block): this is the
  ONE item that would require solving some version of the suspend/resume
  problem §3 explicitly declines to solve broadly. If ever pursued, scope
  it NARROWLY — a CPS-style transform applied ONLY at the specific,
  small set of blocking builtin call sites (`http_get`, `read_file`,
  `sleep`, `advance`) rather than a whole-interpreter rewrite — this is a
  meaningfully smaller, more tractable problem than general coroutines,
  and should get its OWN dedicated design document when/if it's actually
  pursued, not be folded into this one speculatively.

  **LIVE-INVESTIGATED (2026-08-24, under the user's own standing
  maximum-risk mandate, which explicitly asked whether a full green-
  thread/stack-switching scheduler rewrite should be attempted next).
  Two findings materially change this item's own priority and framing:**

  1. **The premise that motivated investigating this — "KUPL needs
     green threads to support Go/Erlang-scale actor counts" — turned
     out to be FALSE by the time this investigation started.**
     PR-cv2-5/PR-cv2-6 (§4.3's own entry above) had ALREADY solved
     actor-count scaling through a completely different, much safer
     mechanism (fixing two redundant per-actor AST clones): 100,000
     concurrent actors now spawn in ~1.7s at ~11KB/actor with clean
     linear scaling, using the EXISTING OS-thread-pool model, no green
     threads needed. Stack-switching would NOT improve actor-count
     scaling further — that problem is closed. The ONLY remaining gap
     stack-switching would address is narrower: **a blocking I/O call
     inside one actor's handler still occupies its whole worker for the
     call's duration** (already honestly documented in §4.3's own
     "shape of the fix" section) — genuine, but a much smaller target
     than "support millions of actors," which is why this item stays
     in §5 rather than being promoted to §4.
  2. **Feasibility of the underlying mechanism, checked directly rather
     than assumed**: `ucontext.h` (`getcontext`/`makecontext`/
     `swapcontext`) — the zero-Cargo-dependency way to implement
     stackful coroutines via the SYSTEM libc, since this project's own
     zero-external-crate policy rules out `corosensei`/`generator`-style
     crates — DOES compile, link, and correctly perform a real context
     switch on this machine's current macOS/Xcode SDK, confirmed via a
     standalone C program (`getcontext`+`makecontext`+`swapcontext`,
     switching into a function and back), when `_XOPEN_SOURCE` is
     defined. It compiles with deprecation warnings ("no longer
     supported" since macOS 10.6) — functional today, but a real,
     unquantified future-portability risk on macOS specifically; Linux
     glibc's own `ucontext.h` is not deprecated and is the more stable
     target. This is a genuinely useful, load-bearing fact for whoever
     designs this next — it means the mechanism is NOT a dead end, contrary
     to what an assumption-only pass might have concluded.

  **Why this was NOT implemented in the same session that verified its
  feasibility, despite the standing maximum-risk mandate authorizing
  large architectural risk**: the specific risk here is categorically
  different from every other risk taken in this initiative so far.
  PR-cv2-1 through PR-cv2-6 all risked *architecture/performance*
  regressions, each caught by a real, fast-running test before landing.
  Hand-rolled stack-switching risks *silent memory corruption* in KUPL's
  own interpreter core, through at least three specific, unresolved
  hazards that a rushed implementation would not reliably catch even
  with careful testing: (a) Rust relies on a guard page at each native
  thread's stack boundary to turn stack overflow into a clean, caught
  panic (this project's own `MAX_CALL_DEPTH`/PR-it1213 history shows how
  seriously it treats this) — a manually-allocated coroutine stack needs
  its OWN guard page (`mprotect`) set up correctly, or overflow silently
  corrupts adjacent memory instead of panicking cleanly; (b) Rust's own
  unwinding machinery (panic propagation) is not verified-safe across a
  manually swapped stack without careful, specific handling; (c) every
  `thread_local!` this initiative already built (`POOL_WORKER_ID`) is
  keyed to the OS thread, not to a "logical" coroutine — multiplexing
  several coroutines onto one OS thread via stack-switching would share
  those thread-locals across all of them unless explicitly redesigned,
  a subtle correctness trap easy to introduce without noticing. Getting
  any of these wrong produces exactly the failure class this project has
  spent 1200+ hardening iterations eliminating (silent wrong answers,
  not clean panics) — and unlike a performance regression, a memory-
  safety bug here would not be reliably caught by "did the existing test
  suite still pass," since undefined behavior is not obligated to
  manifest deterministically. Rushing this specific piece of work would
  not be "maximum ambition" — it would be gambling the project's own
  hard-won correctness bar for a speculative, narrow, not-yet-demonstrated-
  as-needed capability (no real KUPL program has ever hit this — same
  "real gap found and verified, not speculative" bar this project already
  holds itself to elsewhere). This judgment call is explicitly permitted
  by the mandate's own text ("if genuinely blocked... use judgment") and
  does not relax the mandate for anything else in this initiative.

  **What a future, careful pass on this should do, concretely, now that
  the two findings above exist**: write a REAL dedicated design document
  (matching `ASYNC.md`'s own pre-implementation rigor) that resolves the
  three hazards above BEFORE any code — likely via `mprotect`-based guard
  pages on each coroutine stack, an explicit audit of every
  `thread_local!` a coroutine's own code path can reach, and a decision
  on whether Rust's unwinding is provably safe here or whether panics
  need to be caught and re-thrown across the switch point manually. Given
  actor-count scaling is already solved, the ACTUAL target for this
  work should be scoped down to just "don't block a worker's OTHER
  actors during an `http_get`/`read_file` call," not general green
  threads — which may make the narrower CPS-transform alternative (no
  unsafe code at all, since the transform only needs to cover the small,
  now-precisely-counted set of functions that can transitively reach one
  of KUPL's 4 blocking builtins: `http_get`, `http_post`, `http_get_with`,
  `read_file_with` — confirmed via direct grep; there is no real-
  wall-clock `sleep` builtin in the language today, only the virtual
  clock's `advance`) the SAFER first choice to design toward, even though
  it touches more of the interpreter's own code structure than
  stack-switching would.

- **Built-in distribution** (Erlang/Swift-style, §3) — its own dedicated
  design pass, if ever pursued at all.
- **A general reference-capability type system** (§3) — not retrofitted;
  if KUPL's type system ever grows an affine/ownership dimension for
  OTHER reasons (e.g. general memory-safety goals unrelated to
  concurrency), revisit whether extending it to §4.2's narrow fast path
  becomes cheaper as a byproduct — but do not build one FOR this purpose
  alone.

## 6. Provenance note

This document is Phase 4 of a 5-phase process (research across 8 languages
→ compare → synthesize → **design doc (this file)** → implement). The full
per-language research write-ups this document condenses are not
reproduced here; §2's table and cited findings are the durable, public
record of that research. Implementation progress against the roadmap in
§4 should be tracked the same way this project already tracks its other
hardening work — real commits, real tests, real regression coverage —
rather than through this document's own text, which will otherwise drift
out of date the moment implementation starts.
