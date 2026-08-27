# Real concurrency between component instances — design sketch

v0.3 (it121, determinism strategy decided it129, concrete implementation
plan written it132) — a bounded design
deliverable, not an implementation, mirroring
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

- ~~Which of §3.4's (a)/(b)/(c) determinism strategies is right~~ —
  **RESOLVED at it129, see §7**: strategy (b), value-level determinism
  preserved with documented timing-observable nondeterminism, opt-in
  only (today's default synchronous behavior is unchanged for any
  program that doesn't explicitly opt in to real concurrency).
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
- **(it122, answered by §6 below at it123)** What a wire connecting two
  different per-thread `Interp`s looks like — `emit`'s target resolution
  and `self.current` both assume ONE shared `Interp` today (§4's it122
  update). §6 inventories the real scope and reaches a decisive
  conclusion: it's bigger than "just wires," and a blocking-only version
  of it delivers no practical benefit on its own.

## 6. The cross-Interp wire question, answered (it123)

The it122 update above found that `emit` (not just `expose`) needs to
cross a thread boundary. This section answers the follow-up question it
raised — "what does that actually require" — by inventorying the real
call sites rather than reasoning about `emit` in isolation.

### 6.1 Instance addressing is flat and globally shared, not tree-scoped

`instantiate` (interp.rs) assigns every component instance — root,
child, grandchild, however deeply nested — a plain `usize` id via
`let id = self.instances.len(); self.instances.push(...)`: ONE flat
`Vec<Instance>` for the entire running program. Wires are registered by
raw index on the SOURCE instance's own `wires: HashMap<String,
Vec<(usize, String)>>` (`self.instances[src].wires[from_port].push((dst,
to_port))`). There is no tree/hierarchy structure at runtime — a
"parent" and its "children" are related only by which instance holds a
`Value::Component(id)` for the other in its own `env`, and by which
instance's `wires` map references which other id. This means "thread off
one child" is not a locally-scoped operation on a hierarchy edge — it is
carving one entry out of a flat, globally-addressed table that every
other instance in the program can reference by raw index from anywhere.

### 6.2 The real surface: 14 functions in interp.rs, 44 touches in repl.rs

Grepping every `self.instances[id]` site (not just `emit`/`eval_method`)
finds **14 distinct functions** touching it directly: `instantiate`,
`instantiate_child` (construction + supervise-policy wiring), `emit`
(wire delivery), `drain` (dispatch loop — reads `.comp.handlers` per
queued message), `run_handler` (creates the handler's child `env`, sets/
restores `self.current`), `arm_timers` / `advance` (the virtual-clock
timer-fire loop — reads and MUTATES `.timers` in place), `restart` /
`reset_instance_state` (supervision — reads `.max_restarts`/
`.restart_history`, mutates `.env` state fields in place), `run_lifecycle`
(`on start`/`on stop`), `eval_ident` / `eval_call` / `eval_method`
(`expose` calls and ordinary method dispatch through `Value::Bound`),
`resolve_par_branches` (the `par{}` purity gate needs to read `.env` to
decide if a branch qualifies for the real-thread fast path), and
`forall_case` (property-test instance isolation). Separately, `repl.rs`'s
`:upgrade` machinery touches `.instances` **44 times** — migrating state/
props/children/wires by name is built entirely on direct, synchronous
access to the live `Instance` struct's fields.

None of these are exotic edge cases — `arm_timers`/`advance` and
`restart` in particular are core, load-bearing, exercised by ordinary
`on every`/`on after` programs and by `supervise child restart
on_failure`, not obscure paths. **A threaded instance would need EVERY
one of these 14 functions (plus `:upgrade`) to branch on "is this id
local or remote" — not just the two (`emit`, `eval_method`) the original
first-slice framing focused on.** This is a materially larger surface
than either the it121 sketch or the it122 finding had scoped, confirming
(rather than merely suspecting) that this is not a small sub-problem.

### 6.3 Even the simplest possible design (fully blocking, zero overlap) doesn't shrink this

The obvious way to keep things simple is: make EVERY cross-thread
interaction (not just `expose`, ALL of it — wire delivery, timer
firing, restart, `:upgrade`) a blocking round-trip, so the threaded
child never does anything except in direct, synchronous response to a
request from whichever thread currently "owns" execution — i.e.,
preserve today's exact FIFO, run-to-completion queue semantics, just
now spanning a thread hop. This is the SIMPLEST possible design
(no new ordering questions, byte-identical output essentially by
construction) — but it does NOT reduce the §6.2 surface at all. Every
one of those 14 functions/44 touches still needs to know how to reach
the remote instance instead of indexing `self.instances[id]` directly;
"blocking" only decides HOW they wait for the answer, not whether they
need to change. **And a fully-blocking design delivers ZERO practical
concurrency benefit even once built**: if nothing is ever allowed to
overlap in wall-clock time, the child thread's only purpose is
mechanical (proving the RPC-style plumbing works), not making the
program run any part of its work in parallel — matching what §4
already said honestly ("purely a proof that the mechanism is soundly
buildable... before any REAL concurrency is attempted"), now confirmed
concretely rather than assumed.

### 6.4 Real benefit requires §3.4 (determinism) to be answered FIRST, not after

The only way this work pays for itself is if SOME cross-thread
interactions stop blocking — e.g. the parent enqueues a message to the
threaded child and keeps draining its OWN queue instead of waiting, so
an unrelated sibling instance's timer/handler can run while the child is
mid-flight (this is also the only shape where a child's slow blocking
I/O, e.g. `http_get`'s `curl` shell-out, would stop freezing the whole
program). The moment ANY interaction is allowed to be non-blocking,
today's strict single-queue FIFO ordering (§1) is no longer trivially
preserved — two threads can each be mid-cascade at once, and the
relative arrival order of the child's replies/emissions into the
parent's queue becomes genuinely a race, which is exactly §3.4's
"determinism tension," not a new question. **Conclusion: the
cross-Interp wire question and the determinism question are the SAME
question, not two separable ones — the wire mechanism's hard part IS
determinism, and its easy part (the RPC-style call/reply plumbing
itself) is already well understood from `par_map`'s existing precedent
(§3.2).** Treating them as sequential sub-problems (as the it122 NEXT-
note proposed) was a reasonable thing to try, but investigating live
shows they don't actually decompose that way.

### 6.5 A genuinely new recommendation: an indirection layer, decoupled from threading

One real, actionable idea DID fall out of §6.2's inventory, independent
of resolving §3.4: today, `self.instances[id]` is indexed directly from
14+ call sites with no abstraction boundary in between. A future
iteration — with NO threading involved at all, a pure, behavior-
preserving refactor, verifiable as byte-identical via the existing
`cargo test` + interp-vs-vm sweep — could introduce a single point of
indirection (e.g. an `InstanceRef` accessor method replacing direct
`self.instances[id]` field access) so that WHENEVER a threading design
is eventually attempted, the "is this id local or remote" branch has to
be written in ONE place instead of audited into 14+ call sites by hand.
This is lower-risk, immediately useful for code clarity regardless of
whether real concurrency ever ships, and is the kind of narrow,
verifiable slice this campaign's own discipline favors — but it does
NOT unblock real concurrency by itself, since the actual gate is §3.4,
not the accessor pattern. Not attempted this iteration (still requires
its own scoping: which fields need to move behind the accessor, whether
`&mut` access patterns like `arm_timers`' in-place timer mutation are
even expressible through a trait-object-style indirection without a
larger borrow-checker fight) — named here as a candidate, not designed.

## 7. The determinism decision (it129)

§3.4 was flagged as "the single most consequential open decision" three
iterations ago (it121) and has sat undecided since — named as a fallback
candidate across it124 through it128 without ever being picked, the same
"carried forward as noise" pattern this campaign has explicitly resolved
before for other stale candidates (the it106 precedent). This section
makes the call, rather than deferring an eighth time.

### 7.1 The decision: strategy (b), value-level determinism with documented timing nondeterminism — opt-in only

**Strategy (b)** from §3.4 is the right choice: preserve per-instance
VALUE-level determinism (given a fixed sequence of inputs, an instance's
own computed results are always the same, on every engine, every run),
while explicitly documenting that the RELATIVE TIMING of independently-
running instances' externally-observable side effects (interleaved
`print` order between two SIBLING instances, e.g.) is not guaranteed
once real concurrency is in play.

This is not a novel trade-off invented for KUPL — it is the ORDINARY,
well-understood semantics of the actor model KUPL's own component
system already borrows. `docs/design/VISION.md`'s own inspirations
table (re-read in full for this decision, not assumed) lists
**Erlang/Elixir FIRST**, crediting it specifically for "actor isolation,
supervision trees, per-actor heaps, hot code swap" — exactly the
concurrency model KUPL's components already implement today, minus real
parallel execution. Erlang itself has NEVER guaranteed a global,
cross-process total ordering of events: message delivery order is only
guaranteed WITHIN one sender→receiver pair (FIFO per mailbox edge, the
same guarantee KUPL's own single shared queue already provides today,
trivially, since there is only one possible interleaving); the relative
ordering of unrelated processes' own observable actions has never been
part of the language's own determinism contract, in 35+ years of
production use. Adopting strategy (b) is not a departure from KUPL's
own stated inspirations — it is the most FAITHFUL continuation of the
one this campaign's own docs cite first for exactly this part of the
design.

Strategies (a) (a global logical/vector clock enforcing total order)
and (c) (scoping concurrency to only provably observation-independent
subgraphs) both remain available as FUTURE refinements layered on top
of (b) — (a) as an opt-in stronger guarantee for programs that need it
(at a real performance/complexity cost §3.4 already named), (c) as a
possible default-safe subset a future compile-time analysis could
detect automatically. Neither is chosen as the FOUNDATIONAL model here,
since both are strictly harder to implement than (b) and neither is
needed to make a first real-concurrency slice sound.

### 7.2 The trade-off is OPT-IN, not a change to today's default behavior

This is the piece that makes the decision safe to commit to now,
without contradicting this campaign's own sacred byte-identical-output
invariant: real concurrency — and the timing nondeterminism that comes
with it — is not proposed to become the DEFAULT execution mode for
ordinary KUPL programs. `kupl run`/`kupl run --vm`/`kupl native`'s
existing fully-synchronous, single-threaded, strictly-deterministic
behavior stays EXACTLY as it is today for any program that does not
explicitly opt in to real concurrency (the mechanism for opting in —
e.g. a per-component marker, a CLI flag, or something else — is
UNDECIDED and deliberately out of scope for this decision; only the
*existence* of an opt-in boundary is being decided here). This mirrors
`docs/design/VISION.md`'s own "Progressive disclosure of power" pillar
exactly: the app tier's automatic memory management and the hardware
tier's `low`-block volatility are BOTH "invisible unless you ask for
it" — real concurrency's timing nondeterminism should be no different.
Concretely, this means:

- The EXISTING interp-vs-vm-vs-native byte-identical regression
  discipline this campaign has held on EVERY commit remains fully valid
  and unchanged for every program that doesn't opt in — which is every
  program in `examples/*.kupl` today, and will remain so until a program
  deliberately asks for real concurrency.
- A FUTURE real-concurrency implementation only needs its OWN, new
  verification discipline (documented nondeterminism in TIMING-observable
  output specifically, value-level determinism still verifiable and
  verified) for the opted-in subset — it does not need to solve, or even
  touch, the sacred invariant for anything else.

### 7.3 What this does and does not unblock

This resolves §3.4 as a DESIGN decision — it does not implement
anything, and does not by itself make any of §6's own findings (the
14-function/44-touch instance-access surface, the wire-crossing
question) any smaller. A future concurrency implementation still needs
its own dedicated scoping pass, likely still landing on something close
to ASYNC.md §4's own "narrow first slice" shape (one opted-in child
instance, its own thread, blocking `mpsc` calls) as the actual entry
point, now with a decided answer for what happens once that first slice
is generalized to genuinely overlapping (non-blocking) execution instead
of deferred indefinitely. Whether to pick that implementation work up is
a SEPARATE decision from this one, to be made with the same live-
investigation discipline this campaign has applied throughout — this
section only removes the "we don't know what determinism strategy to
build toward" blocker that made every prior attempt stop at the
investigation stage.

## 8. A concrete, decisive implementation plan (it132)

Every open question §5 and §7.3 left dangling is resolved below, against
live-read code (`interp.rs`'s `Instance`/`Interp` struct definitions,
`instantiate`/`instantiate_child`/`send`/`drain`/`emit`/`run_handler`/
`restart`/`arm_timers`/`advance` in full, `ast.rs`'s `ComponentDecl`, and
`parallel.rs`'s `ProgramImage`/`PortableValue`). This is a design, not
code — nothing below has been implemented. It is written to be directly
buildable by whichever iteration picks it up next, without another round
of re-derivation.

### 8.1 The opt-in mechanism: a `concurrent` component modifier, decided

§7.2 left the opt-in mechanism itself unspecified. `ast.rs::ComponentDecl`
already has `is_app: bool` — a precedent for exactly this shape: a plain
compile-time boolean set by a declaration-site keyword, not a runtime flag
or CLI switch. **Decision: add `pub concurrent: bool` to `ComponentDecl`,
set by a new `concurrent component Foo { ... }` declaration modifier**
(mirroring `app component Foo { ... }`'s own existing `is_app` syntax
exactly). This is checkable entirely statically: `check.rs` can reject
`concurrent` on the root `app` component itself (the coordinator thread
IS the app-level instance in this design — see §8.2 — so marking it
`concurrent` is meaningless) and can enforce §8.6's port-type restriction
at the same declaration site. Because `comp.children`/`comp.wires` are
fully static (verified live: `instantiate` reads `comp.children`/
`comp.wires` directly off the `Rc<ComponentDecl>`, resolved once at
parse/check time, never constructed dynamically), **which instances end
up concurrent is knowable entirely at compile time** — no runtime
decision, no ambiguity about "is this id local or remote" that isn't
already fixed at the moment `instantiate` first creates that id.

### 8.2 Core architecture: one `Interp`, unmodified, per thread — not a rewrite

The single most important design realization, missed by §3–§7's own
framing: **every one of the 14 functions §6.2 inventoried already reads
`self.instances[id]`, `self.current`, `self.now`, etc. through `&mut
Interp` — meaning the SAME code already works correctly for a "sub-Interp"
that owns only ONE instance**, as long as `self.instances` resolves the
right thing for both local and non-local ids. So the plan is **not**
"rewrite `interp.rs` to be thread-aware everywhere." It is:

```rust
enum InstanceSlot {
    Local(Instance),        // today's exact struct, unchanged
    Remote(ActorHandle),    // a lightweight, Send+Sync channel handle
}
// Interp.instances: Vec<Instance>  becomes  Vec<InstanceSlot>
```

Every one of the 14 `self.instances[id]` sites gets ONE `match` arm added:
`Local(inst) => { /* exactly today's existing code, verbatim */ }`,
`Remote(handle) => { /* new, small, message-based logic — see below */ }`.
This is precisely the §6.5 "indirection layer" it124 declined to build —
it124's reasoning (no concrete threading design existed yet to justify it,
and a bare per-id *accessor* doesn't help `advance`'s global scan or
`forall_case`'s BFS walk) was correct for what was being proposed at the
time. §8.4 and §8.7 below show the `Local`/`Remote` **split** (not a bare
accessor) resolves both of those cleanly, which is the missing piece that
makes the indirection layer worth building now.

**The coordinator IS today's `Interp`, unchanged in every way except the
`Vec<Instance>` → `Vec<InstanceSlot>` type.** For any program with zero
`concurrent` components, every slot is `Local`, every one of the 14
`match` arms takes the unchanged branch, and behavior — including the
existing byte-identical interp/vm/native regression discipline — is
provably unaffected (a pure type/match-arm restructuring with the old
behavior preserved verbatim in the `Local` arm is the kind of change this
campaign's own `git stash` + behavioral-diff verification method proves
trivially).

**Each `concurrent`-marked instance gets its own OS thread running its
own, freshly-constructed `Interp`.** That actor's `Interp.instances`
contains exactly one `InstanceSlot::Local` (itself) plus one
`InstanceSlot::Remote` for every OTHER instance id in the program
(pointing back to the coordinator — see §8.3's hub-and-spoke restriction).
This means `emit`, `eval_method`, `run_handler`, `arm_timers`, `par{}`'s
own `resolve_par_branches` — literally every function already listed in
§6.2 — run as **the exact same compiled code**, on the actor's own
thread, operating on a `Vec<InstanceSlot>` of length equal to the whole
program's instance count, where index lookups for anyone else resolve to
`Remote`. No second code path to write and maintain; one struct, one
`match`, reused everywhere.

### 8.3 Topology: hub-and-spoke, not full mesh — a stated v1 restriction

Concurrent actors do not get direct channels to each other. Every
`Remote` handle — on the coordinator's side AND on every actor's side —
routes through the coordinator's own single inbound `mpsc::Receiver`
(actors hold cloned `Sender`s into it). This is a deliberate, named
scoping choice: a full mesh (N actors each holding N-1 direct channels)
adds real complexity (dynamic channel setup as instances are created,
no natural place to enforce the ordering guarantees §8.4/§8.5 rely on)
for zero benefit in a first slice, since the coordinator already has to
mediate any interaction with a `Local` instance anyway. A later iteration
proving hub-and-spoke's coordinator-side fan-in is a real bottleneck can
revisit this; nothing about the message shapes below prevents it later.

### 8.4 Cross-thread messages: two shapes, matching §3.3's own decision exactly

- **`Deliver(port, value)`** — fire-and-forget, non-blocking send. Used
  for ordinary wire delivery (`emit` targeting a `Remote` id) and for
  timer fires (§8.5). This is where §7's opt-in timing-nondeterminism
  applies: the sender does not wait, and does not know or care when the
  receiving actor gets around to processing it relative to anything else
  in the program.
- **`Call { fn_name, args: Vec<PortableValue>, reply: oneshot::Sender<...>
  }`** — blocking request/reply, exactly §3.3's already-decided semantics
  for `expose` calls (`eval_method` on a `Remote` id sends this and blocks
  the CALLING thread on `reply.recv()`). No new decision needed here; §3.3
  already made this call correctly three sections ago.

**A genuinely new correctness hazard, absent today: cross-actor call
cycles can deadlock.** Today, a cyclic `expose` chain (A calls B calls A)
just recurses on the one shared native call stack — it works (or
overflows via the EXISTING, already-guarded `MAX_CALL_DEPTH`/stack-margin
machinery) because there is only ever one thread. Once A and B are
different actor threads, A blocking on a reply from B while B is (perhaps
transitively) blocked waiting on a reply FROM A is a real deadlock, not a
deep-recursion panic. **Decision: track, per actor thread, the chain of
`Call` ids currently awaiting a reply; if an incoming `Call` would close a
cycle back to an id already in that thread's own pending chain, refuse it
immediately with a clean panic** (`"concurrent call cycle through
instance {id}"`) **instead of blocking** — the same "clean panic over
silent hang" discipline `MAX_COMPONENT_MESSAGES`/`MAX_ADVANCE_FIRES`
already establish elsewhere in this file for unrelated runaway-growth
classes. This needs the pending-chain metadata threaded through the
`Call` message itself (a `Vec<usize>` of instance ids already waiting,
appended to at each hop) — cheap, plain data, no shared mutable state.

### 8.5 The virtual clock: coordinator stays sole owner, decided

§3.5 asked whether each instance gets its own clock or shares one; sharing
one via a lock was left as the alternative without resolving how.
**Decision: `now: i64` stays exactly where it is today — a single field
on the coordinator's `Interp`. Concurrent actors never read or advance a
clock of their own.** What changes is how `advance()`'s existing global
earliest-fire scan reaches a `concurrent` instance's timer state, since
that state (`Instance.timers`) now lives on the actor's own thread, not
in the coordinator's `Vec<Instance>`. **Decision: a single small shared
`Arc<Mutex<BTreeMap<usize, i64>>>` — instance id → next-fire virtual-ms —
updated by an actor whenever it (re)arms or deactivates its OWN timers
(pure `i64` metadata, trivially `Send + Sync`, not a `Value`/`Rc` in
sight, so this does not reopen the `Rc`→`Arc` blocker §3.1 correctly
ruled out).** `advance()` folds this table into its existing
`self.instances.iter()` scan exactly as it folds local instances today; a
fire whose id resolves to `Remote` sends a non-blocking
`Deliver(handler_idx)` (§8.4) instead of calling `run_handler` directly.
`arm_timers` itself needs NO cross-thread change at all — it already only
reads/writes `self.instances[id].timers` for the SINGLE id being armed,
which on an actor thread is always its own `Local` slot; it only needs to
additionally publish its result into the shared next-fire table.

### 8.6 `PortableValue`: not widened for v1 — a checked restriction instead

§3.6 left open whether `PortableValue` needs widening to carry a `Bound`/
`Component` reference across a thread, and whether that's even
semantically sound. **Decision: don't answer that question for v1 — avoid
it entirely.** `check.rs` requires every port on a `concurrent` component
to have a type representable by the EXISTING `PortableValue` enum (no
function types, no component references) — a compile-time diagnostic,
not a runtime restriction, mirroring `par{}`'s own purity gate (restrict
to the provably-safe subset first, widen later only if a real need
appears). `Deliver`/`Call` payloads are ordinary `to_portable`/
`from_portable` conversions, already-proven code, reused verbatim.

### 8.7 Two features explicitly out of scope for v1, not silently ignored

- **`concurrent` components inside `forall`/law/property-test bodies.**
  `forall_case`'s own BFS reachability walk (§6.2) assumes every instance
  is a synchronous, directly-readable `self.instances[id]` — making that
  span a thread boundary is real, unscoped work with no forcing need yet
  (property tests are about VALUE correctness, not concurrency behavior).
  **Decision: `check.rs` rejects instantiating a `concurrent` component
  anywhere reachable from an `example`/`law`/`forall` body.**
- **`:upgrade` (REPL hot-swap) targeting a `concurrent` instance.**
  `repl.rs`'s 44 direct `.instances` touches assume live, synchronously-
  readable state for migration. **Decision: `repl.rs` rejects `:upgrade`
  if the target (or anything reachable from it) is `concurrent`,** with a
  clear message, rather than attempting to migrate actor-thread state.

Both restrictions are narrow, checkable, and honestly documented — not
gaps discovered later by a confused user.

### 8.8 Engine coverage: VM/native need no special-casing at all

§3.6 assumed VM/native would need to "stay sequential" as a deliberate
fallback. Re-examined against §7's own decided contract: real concurrency
only affects the RELATIVE TIMING of independently-running instances'
side effects, never per-instance computed VALUES. A `concurrent` component
executed fully sequentially — today's exact KVM/native behavior, zero
changes needed to `vm.rs`/`cgen.rs` — is simply the single most
deterministic point in the timing-nondeterminism space §7.1 already
declared acceptable, not a fallback needing special justification. The
existing interp-vs-vm-vs-native byte-identical harness therefore continues
to apply UNCHANGED and needs no `concurrent`-aware exception: `kupl run
--vm` and `kupl native` simply never build the `Remote`/actor-thread
machinery at all (`concurrent` is parsed and checked identically on every
engine, but only the interpreter's `Interp` ever constructs an
`InstanceSlot::Remote`). A concurrency-specific new verification
discipline (§8.9) is needed only for interp-vs-itself-across-multiple-runs,
not interp-vs-vm-vs-native.

### 8.9 New verification discipline needed (v1, interp-only)

The existing byte-identical regression suite is UNCHANGED and continues
to gate every commit exactly as today (§7.2, §8.8). A NEW, additional
category is needed only for programs that use `concurrent`: run such a
program N times (interp only), assert every individual instance's own
sequence of COMPUTED VALUES (state after each handler, return values of
`expose` calls) is identical run-to-run, while explicitly NOT asserting
the interleaved ORDER of side effects (`print`/`emit` timing) across
different concurrent instances is identical — codifying §7.1's decision
as an actual, automated test shape rather than leaving it as prose.

### 8.10 Recommended build order (staged, mirroring `par{}`'s own history)

1. `ComponentDecl.concurrent` + parser/check.rs support, `PortableValue`
   port-type restriction (§8.6), the two exclusions (§8.7) — no runtime
   change yet; a `concurrent` component simply behaves identically to
   today (parses, checks, runs sequentially) until step 2 exists.
2. `InstanceSlot` split (§8.2) with EVERY slot `Local` and zero actor
   threads spawned yet — a pure refactor, verify byte-identical via the
   existing suite before adding any threading at all.
3. Spawn one real actor thread for a `concurrent` instance with NO wires
   and NO `expose` calls reaching it (the narrowest possible case,
   mirroring §4's own original staging idea) — proves the actor's own
   `Interp` executes its lifecycle/timers correctly in isolation.
4. Add `Deliver` (wire emit + timer fire, §8.4/§8.5) — proves cross-thread
   message delivery and the shared next-fire table.
5. Add `Call` (blocking `expose`, §8.4) plus the cycle-detection guard.
6. Write §8.9's new verification harness against a real example program
   exercising all of the above together.

VM/native are untouched throughout (§8.8) — this entire plan is
interp-only, exactly as `par{}` itself started at it99.

## 9. Selective receive and bounded mailboxes (DONE, interp-only)

### 9.1 Selective receive

`receive { port1(pat1) [if guard1] => { .. }  port2(pat2) => { .. }  ... }`
is a new expression, valid only inside a `concurrent component`'s own
`on <port>` handler or exposed-fun body, as the ENTIRE right-hand side of
a top-level `let` (K0310 — never a bare statement, never nested inside a
private `fun`/`if`/loop, never inside `on start`/`on stop`/`on every`/
`on after`). Each arm names one of the component's own declared `in`
ports and a pattern (reusing the same `Pattern`/`match_pattern` machinery
`match` already has) to destructure that port's payload.

Semantics, matching Erlang's own core `receive` guarantee: waits for the
next message addressed to ANY of the named ports whose payload matches
that arm's pattern (and guard, if present) — SKIPPING OVER, not
discarding, any earlier-arrived non-matching message, which remains in
the mailbox in its original relative order for a LATER handler/`receive`
to consume. A port claimed by any `receive` arm may not also have a
top-level `on <port>` handler (K0311 — the two would race for the same
messages). `receive` is `concurrent`-only (K0312) and interp-only —
neither the KVM nor native has an actor mailbox to receive from, so both
reject it at compile time (K0809) rather than silently misbehaving.

Implementation: `receive` suspends via the SAME `Flow::Suspend`/
`SuspendChain` mechanism §8's blocking builtins already use (a new
`SuspendChain.receive_arms` field distinguishes the two), reusing
`WorkerCmd::IoComplete`'s existing frame/reply/drain resume logic
end-to-end rather than a second, parallel implementation. A message
arriving for a port ANY `receive` arm in the component names is routed
into the actor's own `pending` mailbox unconditionally (not just while
already suspended) — otherwise a message sent before the actor's first
`receive` call would be silently dispatched-and-dropped by the ordinary
no-handler-found path, a real divergence from Erlang's own guarantee
that every sent message waits in the mailbox regardless of receiver
readiness.

### 9.2 Bounded mailboxes (overflow protection, not sender back-pressure)

`PooledActor.pending` is capped at `MAILBOX_CAP` (100,000 messages,
`interp.rs`) — a backstop against a genuine bug (a `receive`-only port
with no consumer, or an actor stuck suspended a very long time) growing
this queue without bound, not a normal-operation limit. Exceeding it is
a clean, diagnosed panic that kills the actor (fail fast), not silent
unbounded growth and not a hang.

This is v1: overflow protection, NOT true sender-side back-pressure (the
SENDER blocking/retrying until the receiver's mailbox has room, the way
a bounded Go channel does). Real back-pressure would need a new
cross-thread signaling protocol between the sending and receiving
actors' own worker threads — a substantially larger change, deliberately
left for a future increment once (or if) a real KUPL program demonstrates
the fail-fast behavior here isn't sufficient.

**A specific reason blocking sends are NOT a safe default for `Pooled`
actors**: `ActorPool` multiplexes many actors onto a small, shared set of
worker threads (Go-goroutine-style). If a `Pooled` actor's own worker
thread BLOCKED while sending to a full peer, it would stall EVERY other
actor time-sliced onto that same worker thread too, not just the sender —
a worse failure mode than today's fail-fast panic (one actor dies) since
it would silently freeze unrelated actors that happen to share a pool
thread with the blocked sender. Any future back-pressure design needs to
either scope blocking to `Dedicated` actors only (one OS thread per actor,
so blocking only affects that one actor — see below) or give `Pooled`
workers their own cooperative/async scheduler first (a materially bigger
change, arguably KIR/coroutine territory).

**Dedicated actors are ALSO now capped** (closing a real, previously
unflagged gap: unlike `Pooled`, a `Dedicated` actor's inbox had NO bound
at all before this fix — its own `mpsc::channel()` was fully unbounded).
`ActorRoute::Dedicated`'s inbox is now a `std::sync::mpsc::SyncSender`
bounded at the SAME `MAILBOX_CAP`.

**Dedicated actors now have genuine BOUNDED sender back-pressure**
(`try_send_with_backoff`, `interp.rs`), not just overflow protection —
architecturally safe here specifically because `Dedicated` is one-OS-
thread-per-actor (see the `Pooled`-starvation reasoning above): a sender
whose target mailbox is momentarily full retries with a short
exponential backoff (starting at 50µs, capped at 5ms per attempt) for up
to `DEDICATED_BACKPRESSURE_TIMEOUT` (200ms) before giving up. This is a
deliberately SHORT, bounded wait, not unbounded blocking — `MAILBOX_CAP`
(100,000) is a "genuine bug backstop," so a mailbox that's genuinely at
that size almost certainly means the receiver is stuck, not just briefly
busy, and a long wait wouldn't help, only delay an inevitable clean
failure. The value exists to smooth over a real but transient burst (a
receiver actively draining, momentarily behind) — `std::sync::mpsc::
SyncSender` has no built-in `send_timeout` (only unbounded `send` and
non-waiting `try_send`), so this hand-rolls the bounded middle ground via
a poll-with-backoff loop rather than a true condvar-based wake-on-drain
design (a materially bigger change, left for a future increment if this
proves insufficient). Applied at both send sites (`Interp::send`, the
`emit`/wire path, and `call_remote_impl`, the blocking-`Call` path) —
overflow past the deadline is still a clean, diagnosed panic on the
SENDER's side (the receiving actor can't be reached into and killed the
way `kill_actor` does for `Pooled`, since it lives on a separate OS
thread — consistent with this section's own "no true cancellation"
limitation, §9.3).

### 9.3 Call timeout (DONE, interp-only) — timeout, not cancellation

`<call> timeout <duration>` (e.g. `worker.compute(x) timeout 2s`) bounds
how long the CALLER waits for a `Call` to a `concurrent component`'s own
exposed fun before giving up with a clean panic, instead of blocking
forever. `<call>` must be a method call whose receiver's static type is
a `concurrent` component (K0313 — a timeout on an ordinary same-thread
call is meaningless, since such a call always completes on the same call
stack and can never legitimately time out). `<duration>` is a plain
literal (`parser.rs`'s own `parse_duration`, the same grammar `on every`/
`on after` already use), not a general expression — a deliberately
narrower v1 than a fully dynamic timeout value. Zero/negative durations
are rejected (K0314).

**This is a TIMEOUT, not CANCELLATION.** The callee actor is never told
to stop and has no interrupt/preemption mechanism — nothing in this
codebase (or in safe Rust generally) can forcibly halt another OS
thread's in-flight computation. On timeout, the actor keeps running to
whatever completion it would have reached anyway; its eventual reply (if
any) lands on a reply channel the caller has already stopped listening
to — a silently dropped `Sender::send` with no live receiver, which is
safe (no panic, no hang), just wasted work. True cancellation would
require every execution engine to support interrupting an in-flight
actor computation, a materially larger undertaking left for a future
increment, matching this section's own "ship the honestly-scoped slice,
document the rest" pattern.

Engine coverage: unlike `receive` (K0809, genuinely impossible on VM/
native — no mailbox exists there), `timeout` compiles straight through
to the wrapped call itself on the VM/native engines (`compile.rs`),
UNCHANGED — those engines run every `concurrent` component sequentially
(§8.8), so a call there always completes on the same call stack and can
never actually block long enough to time out. The wrapper is therefore
byte-identically a no-op on those two engines, not a rejected construct
— `timeout` does not reduce engine coverage at all.

### 9.4 Actor-to-actor direct channels (DONE, interp-only, pooled-to-pooled)

§8.3's own "later iteration" caveat, now partially taken: `wire a.out ->
b.in` where BOTH `a` and `b` are `concurrent` (pooled) actors now
delivers DIRECTLY, actor-to-actor, with no coordinator round-trip — not
the full N-to-N mesh §8.3 declined to build, but a real, wire-declared
direct channel between two specific actors, genuinely closing "no
explicit actor-to-actor channels" for the common case.

Mechanism: when the PARENT component's own `instantiate_local` processes
a wire whose source is a pooled `concurrent` actor and whose destination
is ALSO a pooled `concurrent` actor, it sends the source actor a
`WorkerCmd::RegisterRemoteWire` carrying a `RemoteWireTarget` — a
cross-thread-sendable clone of the destination's own `worker_tx`/
`local_id` (deliberately NOT the full `ActorRoute`, which also owns
non-`Clone` teardown state like a `JoinHandle`). The source actor's own
`Interp` gains a `remote_wires` table (parallel to, not replacing, the
existing per-instance `Instance.wires` used for same-`Interp` targets);
its own future `emit`s on that port send an `ActorMsg::Deliver` straight
onto the destination's own worker channel.

Two things remain unsupported, both a clean, specific panic rather than
silent misbehavior: a `concurrent` source wired to a plain (non-
concurrent) destination (the ORIGINAL "cross-Interp wire" problem, §6,
still doesn't have a general solution), and either side using a
dedicated (non-pooled) actor thread (the nested-spawn fallback path,
`ActorRoute::Dedicated` — no `RemoteWireTarget` variant for it in v1).

A genuinely NEW risk this introduces, closed proactively: a cross-actor
wire cycle (`wire a.out -> b.in`, `wire b.out -> a.in`, both handlers
re-emitting) bypasses `self.queue`/`drain()` entirely (each hop sends
straight across an `mpsc::Sender` to a DIFFERENT actor's own worker
thread), so `drain()`'s own existing `MAX_COMPONENT_MESSAGES` wire-cycle
guard never sees it. A new `Interp::remote_emit_count`, capped at the
SAME `MAX_COMPONENT_MESSAGES`, closes this the same way.

A known, DETERMINISTIC (not racy) startup limitation, documented rather
than fixed: a remote wire is not yet registered during the SOURCE
actor's own `on start` handler or its first timer fires (`WorkerCmd::
Spawn` runs the actor's ENTIRE startup sequence synchronously, before
the worker loop can process any LATER command on the same channel,
including `RegisterRemoteWire`) — an `emit` from `on start` on a port
that will later be wired remotely is silently unwired at that moment.
Only affects the actor's own startup; any LATER `emit` (from an exposed
fun call, a later timer fire, or a `Deliver`-triggered handler) sees the
wire correctly registered. Fixing this properly would mean threading
wire info into `WorkerCmd::Spawn` itself, ahead of knowing the
destination's own route at spawn time — left as a documented future
increment rather than rushed into this slice.
