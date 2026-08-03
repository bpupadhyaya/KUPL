# Real concurrency between component instances — design sketch

v0.1 (it121) — a bounded design deliverable, not an implementation, mirroring
`CAPABILITIES.md`'s own it112 precedent exactly: investigate live, write down
what the real blockers are and what a credible design would need to resolve,
so the NEXT iteration that picks this up (if any) starts from a real plan
instead of re-deriving one. This document exists because `docs/design/
LANGUAGE.md` §4's own vision text ("no bare threads in the app tier; the
runtime multiplexes components on an M:N work-stealing scheduler") and
`await`'s own documented semantics ("explicit asynchrony; handlers are
implicitly async") were never implemented — confirmed live at it120's fresh
`docs/GAPS.md` survey, which named this "the #1 gap for the universal, any
software claim" this campaign's own docs have repeatedly, honestly named but
never scoped into an actual plan.

## 1. What exists today (verified live, not assumed)

Read in full for this sketch: `src/interp.rs`'s `Interp` struct and its
`send`/`drain`/`run_lifecycle`/`call_value`/`eval_method` functions, and
`docs/design/bigarcs/3-real-concurrency.md` in full (the design doc for the
ALREADY-shipped `par{}`/`par_map`/`par_filter` real-thread work, it33-it101).

- **`await` is a literal no-op.** `interp.rs`'s own `ExprKind::Await(inner)`
  arm is `self.eval(inner, env)` — byte-for-byte identical to evaluating the
  inner expression directly with no `await` at all. There is no suspension,
  no scheduler hook, nothing. Confirmed via `grep`, not assumed.
- **One `Interp`, one thread, one message queue.** `Interp` (`interp.rs:132`)
  holds EVERY component instance in a single `instances: Vec<Instance>` and
  a single shared `queue: VecDeque<(usize, String, Value)>`. `send()` pushes
  a message and immediately calls `drain()`, which pops messages FIFO and
  runs each one's handler to completion, synchronously, on the SAME call
  stack, before returning. There is no concept of "this instance is busy,
  let another instance make progress" — the whole runtime processes exactly
  one message at a time, always.
- **Cross-component `expose` calls are ALSO fully synchronous, and bypass the
  queue entirely.** `store.get(id)`-style calls resolve through
  `Value::Bound(id, name)` → `eval_method(Value::Component(id), name, args,
  span)` — a direct, reentrant Rust function call into the target instance's
  own state, borrowing the SAME `&mut Interp`. This is not a message send at
  all; it is ordinary synchronous function-call semantics wearing a
  component-method syntax. `await store.get(id)` today is therefore doubly
  synchronous: the `await` itself is a no-op, AND the call it wraps was
  never anything but a direct function call to begin with.
- **Real concurrency exists ONLY as a narrow, opt-in, PURE-only fast path**
  (`par{}`/`par_map`/`par_filter`, it33-it101, `src/parallel.rs`): a
  `PortableValue` clone-across-the-boundary mirror, an `Arc<ProgramImage>`
  snapshot of the whole program (safe to share because the AST itself is
  plain owned data — zero `Rc`/`RefCell`, confirmed via `grep -c "Rc\|
  RefCell" src/ast.rs` = 0, cited directly from `3-real-concurrency.md`),
  and `std::thread::scope` workers that each build their OWN fresh,
  thread-local `Interp` from the shared image and evaluate independently.
  This is gated to STATICALLY PURE, effect-free callbacks only — a worker
  NEVER touches `instances`, `queue`, `now`, or any other live runtime
  state. It proves real OS-thread parallelism is achievable in this
  codebase, but only for the narrow case where nothing shared or mutable
  ever needs to cross a thread boundary.

## 2. The actual gap

Nothing today lets two DIFFERENT, LIVE, STATEFUL component instances make
progress concurrently, or lets one instance's slow operation (a `curl`-backed
`http_get`, a blocking `read_file`, or simply a long computation) avoid
blocking every OTHER instance in the same program. `await` promises
asynchrony the runtime has never delivered. This is a different, harder
problem than `par{}`'s own: that arc parallelizes PURE COMPUTATION over data;
this gap is about REAL ACTORS — stateful, effectful, message-driven — making
independent forward progress, which is exactly the concurrency model
`LANGUAGE.md` §4 already claims KUPL has ("isolated actors... no locks, no
shared memory, no data races by construction") but which the CURRENT
implementation only delivers as a LOGICAL isolation (separate state per
instance), never a PHYSICAL one (one thread, strictly serial).

## 3. Design sketch

### 3.1 The core blocker applies here with FULL force, not just for `par{}`

`3-real-concurrency.md`'s own "core blocker, assessed honestly" section
already did the hard analysis this sketch would otherwise have to redo:
`Value` wraps everything in `Rc`; `Closure`/`Env` are `Rc<RefCell<EnvInner>>`.
`Rc` is never `Send`. Blanket `Rc`→`Arc` does NOT fix it, because `RefCell`
is `Send` but not `Sync` — an `Arc<RefCell<EnvInner>>` still isn't `Send`;
reaching `Send` needs `Arc<Mutex<…>>`/`RwLock`, i.e. a lock on EVERY variable
read/write (`Env::get` is the hottest path in the interpreter), a
whole-codebase rewrite with a severe single-threaded perf cost, and it STILL
leaves `Interp`'s own `instances`/`queue`/`current`/`now` nowhere near
thread-safe. That doc's own verdict — "general, arbitrary-closure real-thread
parallelism is low feasibility under the current design" — was scoped to
`par{}`'s own arbitrary-closure case, but the SAME reasoning applies with
EQUAL or GREATER force to "let every component instance run on its own
thread": component state (`Instance.env`) is exactly the same `Rc<RefCell<…>>`
shape `par{}`'s own blocker analysis already ruled unfit for cross-thread
sharing. **Do not pursue `Rc`→`Arc`/locking here either — it was correctly
rejected once already, for the same underlying reason.**

### 3.2 The most promising direction: generalize `PortableValue`'s OWN pattern from "one-shot pure call" to "long-lived actor inbox"

The `par{}` arc's own recommended approach — never SHARE an `Rc` across a
thread boundary, only ever CLONE an owned, portable copy across it — is not
inherently limited to pure, one-shot computations. It is exactly the actor
model's own message-passing discipline: an actor never shares mutable state
with another actor, it only ever sends OWNED copies of data. The concrete
generalization this sketch proposes:

- Each component instance gets its OWN dedicated OS thread (or a thread from
  a bounded pool, for the M:N scheduler `LANGUAGE.md` §4's vision text
  names — start with 1:1 threads as the simplest correct baseline, treat
  M:N pooling as a LATER perf refinement, mirroring how `par_map` itself
  started with a simple threshold-gated `thread::scope` before any pooling
  question arose).
- Each instance's own `Env`/state NEVER leaves its own thread. The
  EXISTING `Rc<RefCell<EnvInner>>` representation stays completely
  unchanged FOR VALUES THAT LIVE INSIDE ONE INSTANCE — this is the key
  insight `par{}`'s own "Key enabling fact" section already established:
  `Rc` is perfectly fine WITHIN one thread; the rule is never sharing one
  `Rc` allocation BETWEEN threads.
- Cross-instance communication (today's `queue`/`send`/`wire`/`emit`
  mechanism) becomes a genuine cross-THREAD channel (e.g.
  `std::sync::mpsc`, std-only, no new dependency): a message crossing
  between two instances is converted to a `PortableValue` (or a widened
  version of it — see §3.6) at the SEND side and rebuilt into a
  thread-local `Value` at the RECEIVE side, exactly mirroring
  `to_portable`/`from_portable`'s own existing conversion, just reused for
  a LONG-LIVED inbox instead of a one-shot worker call.

### 3.3 What happens to `expose` calls — this is where `await` would gain real meaning

Today's `store.get(id)` is a direct, synchronous, reentrant Rust call
because both instances live in the SAME `Interp`, on the SAME thread. Once
instances live on separate threads, an expose call can no longer be a direct
function call — it MUST become a genuine request/response over the
cross-thread channel: send a "call `get(id)` on you, reply on this
one-shot channel" message, then the CALLER either blocks its OWN thread
waiting for the reply (the simplest correct semantics, and arguably a
faithful reading of `LANGUAGE.md`'s own "the caller `await`s the reply"
line), or — if `await` is meant to be genuinely non-blocking for the
calling instance's OWN OTHER work — the calling instance's execution would
need to suspend and let its OWN queue keep draining other messages while
the reply is pending. The BLOCKING version is dramatically simpler (no
interpreter rearchitecture needed — one thread waiting on a channel recv is
ordinary Rust, buildable directly on `mpsc`) and should be the FIRST-SLICE
target if this is ever picked up; the SUSPENDING version needs either
stackful coroutines (green threads with their own stack — a real,
from-scratch engineering undertaking for a zero-dependency codebase, no
crate like `corosensei`/`generator` to reach for) or a full
continuation-passing rewrite of `eval`'s own recursive tree-walking
structure (`eval` currently uses the NATIVE RUST CALL STACK for KUPL's own
recursion — pausing mid-evaluation and resuming later is not something the
current architecture supports at all without one of those two much larger
changes). **This sketch recommends the blocking-call-across-threads
semantics as the ONLY tractable first slice** — `await` becomes real (a
cross-thread wait, not a no-op) without needing coroutines.

### 3.4 The determinism tension — the single hardest open question

Every iteration of this campaign has held one sacred invariant: byte-identical
output across all engines, verified on every commit. Today's single-threaded,
strict-FIFO message queue makes output trivially deterministic — there is
only ever one possible interleaving. Real concurrent execution across
instance threads introduces genuine interleaving nondeterminism: which
instance's message gets processed first when two fire "simultaneously" is no
longer a well-defined question the way it is today. `par{}`'s own precedent
sidesteps this ENTIRELY by construction — results are gathered BY INDEX
regardless of which worker finishes first, so parallelism is invisible in
the output, only visible in wall-clock time. That trick does NOT generalize
to stateful actors: TWO different instances producing observably-ordered
side effects (e.g. both `print`ing, or both writing to the same file) have
no "gather by index" fallback — the ORDER of interleaved output becomes a
genuine race unless something enforces it. A credible design needs one of:
(a) a global logical/vector clock enforcing a deterministic total order over
cross-instance events regardless of real thread-scheduling order (expensive,
and arguably defeats much of the purpose of real concurrency if strictly
enforced everywhere); (b) explicitly documenting that async introduces
run-to-run nondeterminism for TIMING-observable behavior specifically (e.g.
interleaved `print` order) while keeping per-instance VALUE-level results
deterministic given a fixed set of inputs — a real, honest trade-off KUPL
has never had to make before, and a departure from this campaign's own
absolute historical discipline on determinism; or (c) scoping real
concurrency to ONLY instances that are provably observation-independent
(no shared wires, no shared external resource), an even narrower
"provably-safe-to-parallelize" gate in the spirit of `par{}`'s own
purity gate, generalized from "pure function" to "causally-independent
actor subgraph" — likely the hardest of the three to implement correctly,
but the only one that fully preserves today's determinism guarantee.
**This sketch does not resolve which of (a)/(b)/(c) is right** — it is the
single most consequential open decision, and deserves its own focused
follow-up investigation before any code is written.

### 3.5 Timers and the virtual clock

`now: i64` (`Interp`'s own virtual clock, advanced explicitly, never
wall-clock — the mechanism that keeps `on every`/`on after` deterministic
and reproducible today) is currently a SINGLE field on the SINGLE `Interp`.
Per-instance threads would need EITHER a shared, synchronized clock (another
cross-thread coordination point, another place determinism could leak) or
each instance keeping its own — and if each instance's timers fire against
its OWN independently-advancing clock, virtual-time-based tests
(`advance 5s` in an `example` block) lose their current, simple, whole-
program meaning. Not resolved here; flagged as a real, concrete consequence
of §3.2's own per-instance-thread proposal that a fuller design must address,
not an afterthought.

### 3.6 Engine coverage

Given the size of even the narrowest first slice (§3.3's blocking-call
version), and this campaign's own established precedent of shipping a
FEATURE on the interpreter first, then following up per-engine — a credible
plan would almost certainly need to stay interp-only for a first slice
(mirroring `par{}`'s own it99 precedent: "interpreter only for this slice;
VM and native stay sequential"), with the KVM's own analogous port a
SEPARATE, later iteration (needing its own answer to the same `Rc`/`Send`
question against `vm.rs`'s own register/stack representation, which is a
DIFFERENT shape than `interp.rs`'s tree-walking `Env`, not a mechanical
copy of whatever interp.rs ends up doing), and native (`cgen.rs`) staying
sequential/deferred for a long time after that, exactly as `par{}` itself
still does for native today. `PortableValue` itself would likely need
widening (it currently explicitly excludes `Closure`-with-env, `Component`,
`Bound`, `Fun`, `VmClosure` — some of THESE are exactly the things a
cross-instance message might need to carry, e.g. passing a `Bound` reference
to another instance as a message payload; whether that's even semantically
sound for a message crossing a real thread boundary is itself an open
question, not just an implementation gap).

## 4. Recommended first slice, if picked up

Given §3's own findings, the smallest genuinely tractable proof of concept
is narrower than "implement async": pick ONE pair of components with an
EXISTING `expose`-call relationship in an example program, give ONLY that
child instance its own thread (opt-in, not universal), route its expose
calls through an `mpsc` channel with blocking wait-for-reply semantics
(§3.3), and confirm the SAME program still produces byte-identical output
to today's sequential version (since a single child on its own thread,
called synchronously via blocking wait, should be OBSERVATIONALLY identical
to today's direct call — genuinely no behavior change from the KUPL
programmer's own point of view, only an internal execution-mechanism
change) before ever touching the determinism question in §3.4 for real
concurrent (non-blocking) execution. This mirrors `par{}`'s OWN staged
history exactly: prove the mechanism is soundly buildable in the NARROWEST
possible case where nothing about the hard question (§3.4) is even exercised
yet, before generalizing.

**it122 update — this first slice is narrower on paper than in practice.**
Live investigation (reading `interp.rs`'s `send`/`drain`/`emit` in full) found
that `expose` calls are NOT the only path that would need to cross a thread
boundary. `emit` (interp.rs, the handler for a component's own `out` port
writes) resolves its wire targets via `self.instances[id].wires` and pushes
directly onto the ONE shared `self.queue` — the exact same `instances`/
`queue` fields `expose` calls read via `Value::Bound`. And `self.current`
(the "instance currently executing a handler, target of `emit`") is a
single global field on `Interp`, not per-thread state. So a threaded child
is not just "its `expose` calls become blocking, everything else stays
in-process" — the moment that child's OWN handler calls `emit` to send a
value out a wired port, THAT ALSO needs to reach across the thread boundary
back into the parent's `instances`/`queue`, because the wire's target
instance lives in the parent's `Vec<Instance>`, not the child's own (would-be
separate) `Interp`. A genuinely separate per-thread `Interp` (mirroring
`par_map`'s own fresh-thread-local-`Interp`-per-worker construction, per
§3.2) would need EVERY wire crossing the parent/child boundary — not just
`expose` calls — rewritten as a blocking round-trip, plus a resolution for
`self.current` no longer being meaningfully single-valued once two
`Interp`s can each have their own notion of "currently executing instance."
This is a real scope increase over the plan as originally written above,
discovered specifically by reading the implementation rather than assuming
the sketch's own framing was complete. **Conclusion: the first slice, done
honestly (not by quietly narrowing scope to dodge the wire-emit case), is
bigger than one bounded campaign iteration — it needs `emit`/wire-delivery
crossing threads designed BEFORE `expose`, not after, since `expose` calls
don't happen in isolation from a component's own port traffic in any
realistic example.** Recommended follow-up shape: a dedicated sketch for
"what does a wire connecting two different `Interp`s (on two different
threads) look like" as ITS OWN first design question, before attempting any
running code.

## 5. Open questions this sketch does NOT resolve

- Which of §3.4's (a)/(b)/(c) determinism strategies is right — the single
  most consequential unresolved question, deserving its own dedicated
  follow-up investigation.
- Whether `await` should ever become genuinely NON-blocking (suspending the
  calling instance's own execution to let its queue keep draining) — this
  sketch recommends starting with blocking cross-thread calls specifically
  BECAUSE the non-blocking version needs coroutines or a CPS rewrite this
  sketch has not scoped at all.
- What an M:N (pooled) scheduler looks like once 1:1 per-instance threads
  are proven — likely a much later refinement, not a v1 concern.
- How `PortableValue` should be widened for message payloads that need to
  carry a `Bound`/`Component` reference (a message referring to ANOTHER
  instance, not just plain data) — not investigated live as part of this
  sketch.
- The virtual clock question (§3.5) — flagged, not resolved.
- Whether this interacts with `ai fun`'s own real-provider network calls
  (currently synchronous, `curl`-backed like `http_get`) — a natural
  candidate for genuinely benefiting from real concurrency (multiple
  in-flight model calls), not verified live as part of this sketch.
- **(it122)** What a wire connecting two different per-thread `Interp`s
  looks like — `emit`'s target resolution and `self.current` both assume
  ONE shared `Interp` today (§4's it122 update); this needs its own design
  pass before any threaded-child proof of concept is attempted, since real
  example programs exercise wire traffic, not just isolated `expose` calls.
