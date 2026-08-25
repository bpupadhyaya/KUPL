# Non-blocking I/O for `concurrent component` handlers — design doc

**Status: design only, not yet implemented.** Written under the
Concurrency V2 initiative's own standing maximum-risk mandate, as the
authorized, SAFE alternative to full stack-switching (see
`CONCURRENCY_V2.md` §5's own PR-cv2-7 entry for why hand-rolled
stack-switching was investigated and deliberately not rushed — three
specific, unresolved memory-safety hazards). This document exists
because §5 itself says this item "should get its OWN dedicated design
document when/if it's actually pursued" — this is that document.

## 1. The actual problem, precisely stated

`docs/design/CONCURRENCY_V2.md` §4.3 already documents this honestly:
"a blocking I/O call inside a handler still occupies its worker thread
for the call's duration." Today, when a `concurrent component`'s handler
calls `http_get`/`http_post`/`http_get_with`/`read_file_with`, the
`ActorPool` worker thread processing that message blocks on the real
syscall for the whole round trip — during which that worker cannot
service ANY other actor assigned to it, even though every other actor's
own work is completely unrelated and ready to run.

This is a real, narrow gap — NOT the actor-count-scaling problem
PR-cv2-5/6/8 already solved (100,000 actors now spawn in ~1.5-1.7s; that
problem is closed). It only matters for programs that (a) use
`concurrent component` AND (b) call one of KUPL's 4 blocking builtins
from inside a handler AND (c) have enough OTHER actors on the SAME
worker that losing its capacity during the I/O call actually costs
something observable. No real KUPL program has hit this yet — it is
being addressed now because the user's own standing mandate asked for
the closest safe approximation to Go's netpoller/Java Loom-style
unmount-on-block, not because a concrete failure was reported.

## 2. What exists today (verified live, not assumed)

- **Exactly 4 blocking builtins exist in the whole language**, confirmed
  via `grep '"http_get"\|"http_post"\|"http_get_with"\|"read_file_with"'
  src/interp.rs`: `http_get`, `http_post` (dispatched together, `interp.rs`
  ~line 3323), `http_get_with` (~line 3330), `read_file_with` (~line
  3339). There is NO real-wall-clock `sleep` builtin in KUPL — the
  language's own timer model (`on every`/`on after`/`advance`) is a
  virtual clock, never a real blocking wait.
- **None of these 4 functions touch `Interp`/`Rc`-based state at all** —
  confirmed by reading `http_get_with`/`read_file_with`'s own bodies
  (`interp.rs` ~line 6939/6973): each is a plain `fn(&Value, &str) ->
  Result<Value, String>` (or similar), operating only on its own
  arguments, doing the network/file call, and returning an owned
  `Result`. This matters: the ACTUAL blocking syscall can safely run on
  a thread that has never touched an `Interp`, with zero `Send`
  concerns, since these functions are already `Send`-safe on their own
  (their arguments/return values are plain `Value`s — portable data by
  construction, not arbitrary `Rc` graphs, given they're only ever
  called with literal/portable arguments in practice).
- **Every one of the 4 call sites is reached from deep inside
  `Interp::eval`'s own recursive expression evaluator** (`interp.rs`
  ~line 3280-3346, one match arm among many inside `eval`'s own
  call-expression dispatch). `eval` recurses for every nested
  expression — a call to `http_get(url)` can appear ANYWHERE an
  expression is legal: nested inside a binary operator, a method chain,
  a loop body, a conditional branch, an argument to ANOTHER function
  call, arbitrarily deep. This is the crux of why "just suspend at these
  4 call sites" is not actually narrow in IMPLEMENTATION effort — the
  set of Rust stack frames between "top of the handler's own
  `run_handler` call" and "the blocking call site" is exactly as deep
  and varied as ordinary KUPL expression nesting allows, which is
  unbounded in principle.

## 3. Why full CPS / stack-switching stays out of scope for THIS item

Confirmed by direct investigation (not assumed): there is no way to
truly suspend an arbitrary-depth Rust call stack and resume it later
without either (a) unsafe stackful coroutines (`ucontext.h`-based —
confirmed live to compile and work on this machine's current macOS SDK,
per `CONCURRENCY_V2.md`'s own PR-cv2-7 entry — but carrying the three
specific, unresolved memory-safety hazards that entry documents:
stack-overflow guard pages, unwinding across a manually-switched stack,
and `thread_local!` semantics under multiplexed coroutines), or (b) a
genuine CPS/state-machine transform of `Interp::eval` itself — which,
per the finding in §2 above, would need to cover essentially the WHOLE
recursive expression evaluator, not just 4 call sites, since the
"continuation after a blocking call" can be arbitrarily complex
surrounding code. Both are explicitly ruled out for THIS document's own
scope — (a) for the same memory-safety reasons PR-cv2-7 already gave,
(b) because a whole-evaluator CPS rewrite is a categorically different,
much larger undertaking than "add non-blocking I/O," with its own,
separate correctness risk (subtly mishandling some KUPL syntax
construct in the transform would be a silent WRONG-ANSWER bug, the
worst class of regression this project's own hardening history has
spent the most effort eliminating).

## 4. The proposed design: a deliberately RESTRICTED capability, not general async/await

**Core idea**: rather than support suspension at an arbitrary point in
an arbitrary expression, restrict where a blocking-builtin call may
syntactically appear, so that "the continuation" becomes simple enough
to represent WITHOUT a general CPS transform or stack-switching.

**The restriction**: a call to `http_get`/`http_post`/`http_get_with`/
`read_file_with` is only permitted as the ENTIRE right-hand side of a
`let` statement that is a DIRECT, TOP-LEVEL statement of a `concurrent
component`'s own handler body (`Handler.body: Block`, itself a flat
`Vec<Stmt>` — confirmed via `ast.rs`'s own `Block`/`Handler` definitions)
— not nested inside a loop, `if`/`match` branch, another function call's
argument list, or a non-handler `fun`/`expose fun` body. Concretely:

```kupl
concurrent component Fetcher {
    expose fun poke() -> Str {
        let body = http_get("https://example.com")   // OK: top-level let in a handler-reachable body
        body
    }
}
```

is allowed (assuming `expose fun` bodies are extended the same
top-level-only rule a handler gets — see open question in §7), while

```kupl
    expose fun poke() -> Str {
        if some_cond {
            let body = http_get(url)   // REJECTED: nested inside an `if` branch
            body
        } else { "" }
    }
```

and

```kupl
    expose fun poke() -> Str {
        let body = http_get(url).trim()   // REJECTED: not the ENTIRE right-hand side
        body
    }
```

are both rejected with a new, clean diagnostic at CHECK time (a new
K0xxx code — see §6), matching this project's own "clean panic/error
over silent wrong answer or an obscure internal-compiler-error" bar.
This is a real, honestly-scoped restriction, not a stealth limitation —
it must be documented in `docs/reference/LANGUAGE-REFERENCE.md` and
`docs/PRODUCTION.md` as precisely as `CONCURRENCY_V2.md` §4.5 already
documents the isolation/deadlock guarantees, if this ships.

**Why this restriction is sufficient to avoid CPS entirely**: with a
blocking call ONLY ever appearing as `let NAME = http_get(...)` at the
top level of a flat statement list, "the continuation" after it
completes is exactly: *the remaining statements in this same `Block`,
starting at index `i+1`, with `NAME` now bound in the environment to the
completed result.* This is representable as a plain, small, ordinary
Rust struct — no heap-allocated closure chain, no interpreter rewrite:

```rust
struct SuspendedHandler {
    /// Which statement index to resume from (the one AFTER the `let
    /// name = <blocking builtin>` that triggered the suspend).
    resume_at: usize,
    /// The env this handler's remaining statements should run in --
    /// already has every `let` binding made BEFORE the blocking call.
    env: Env,
    /// Where to bind the completed I/O result once it arrives.
    bind_name: String,
    /// The REMAINING statements to run once resumed (an owned copy or
    /// an Rc-shared slice of the handler body -- the body itself is
    /// static AST data, safe to reference cheaply).
    remaining: Rc<[Stmt]>,
}
```

## 5. Scheduler integration (extends `ActorPool`, no new primitive)

- `PooledActor` (already defined in `interp.rs`, PR-cv2-3) gains a new
  field: `suspended: Option<SuspendedHandler>`.
- When `worker_loop` evaluates a handler and hits a top-level `let name
  = <blocking builtin>(...)` statement, instead of calling the builtin
  inline, it: (a) evaluates the builtin's OWN arguments (ordinary,
  synchronous `eval` calls — arguments themselves are NOT allowed to
  contain a nested blocking call, per §4's restriction), (b) spawns a
  plain `std::thread::spawn` (a normal, `Send`-safe thread — these 4
  functions don't touch `Interp` state at all, per §2) to perform the
  ACTUAL blocking call, (c) stores a `SuspendedHandler` on the
  `PooledActor`'s own slot marking it "awaiting I/O, not ready," and (d)
  returns to its OWN `rx.recv()` loop immediately, free to process ANY
  OTHER ready actor — this is the entire payoff.
- The spawned I/O thread, on completion, sends a NEW `WorkerCmd`
  variant (e.g. `WorkerCmd::IoComplete { local_id, result }`) back
  through the SAME worker's own existing channel — reusing the
  already-built `mpsc::Sender<WorkerCmd>` infrastructure from PR-cv2-3,
  no new cross-thread primitive needed.
- When the worker later pops `WorkerCmd::IoComplete` for a given
  `local_id`, it binds `result` into the `SuspendedHandler`'s own `env`
  under `bind_name`, then resumes ordinary synchronous evaluation of
  `remaining` from `resume_at` — indistinguishable, from that point on,
  from a normal (never-suspended) handler execution.
- **Per-actor ordering is preserved automatically**: while
  `suspended.is_some()`, that actor's slot is simply never selected as
  "ready" for a NEW `Deliver`/`Call` — any message arriving during the
  suspend queues in the actor's own existing mailbox exactly as it
  already does today while a worker is busy running ANY handler
  (suspended or not), so this needs no new queuing logic at all, only a
  new "am I suspended" check at the point `worker_loop` currently
  decides whether an actor is eligible for its next command.
- **Panic during the suspended I/O itself** (e.g. the spawned thread
  panics) is treated exactly like a `Deliver`-triggered panic already
  is (PR-it1223's own established pattern) — caught, recorded via
  `shutdown_panic`, and the actor's slot marked dead, no new panic-path
  code needed beyond routing `IoComplete`'s own error case through the
  EXISTING panic-handling arm `WorkerCmd::Msg`'s `ActorMsg::Deliver`
  handler already has.

## 6. What ships in v1 vs. explicitly deferred

**v1 (this document's own proposed scope)**:
- The 4 existing blocking builtins, restricted to top-level `let`
  bindings in `concurrent component` handler/`expose fun` bodies only.
- A new diagnostic (working name **K0295**, next free code in the K02xx
  checker range per `docs/reference/DIAGNOSTICS.md`'s own numbering) —
  "a blocking builtin call (`http_get`/`http_post`/`http_get_with`/
  `read_file_with`) may only appear as the entire right-hand side of a
  top-level `let` inside a `concurrent component`'s own handler or
  exposed-function body" — rejecting every nested/non-top-level case
  from §4 at CHECK time, never silently miscompiling one.
- No change to PLAIN (non-`concurrent`) component or top-level `fun`
  code at all — those keep blocking exactly as today (there is no
  "other actor" whose capacity is being protected there, so nothing to
  gain and real complexity to avoid).

**Explicitly deferred, NOT this document's scope**:
- Blocking calls inside loops, conditionals, or nested call arguments —
  would need either unrolling into multiple suspend points (real design
  work: what does "suspend inside a loop iteration N" resume into?) or
  accepting they simply can't suspend (documented restriction stays).
- `par { }`/`par_map`/`par_filter` interaction — those already have
  their OWN real-thread fast path (`parallel.rs`); whether a suspended
  blocking call inside a `par` branch needs anything special is a real,
  unresolved question, deliberately left open rather than guessed at.
- Multiple blocking calls in sequence within one handler (`let a =
  http_get(u1)` then later `let b = http_get(u2)`) — each individually
  fits the v1 restriction, but chaining resumes needs `resume_at` to
  possibly point at ANOTHER blocking `let`, re-suspending immediately —
  this falls out naturally from the design in §5 (the resumed execution
  just hits the SAME restricted-call-site logic again) but is called
  out here as something the FIRST implementation's own tests must cover
  explicitly, not assume works by construction.

## 7. Open questions this document does NOT resolve

- Does the top-level-only restriction apply to a `concurrent
  component`'s plain (non-`expose`) private `fun`s too, or only
  handlers/`expose fun` bodies directly? Leaning toward: any function
  reachable from a handler needs the SAME restriction (a private helper
  `fun` called from a handler is still "inside" that handler's own
  execution for suspend purposes) — but this needs a real reachability
  check in the effect checker, not just handler-body-literal syntax,
  which is a real, nontrivial piece of implementation work of its own
  (mirrors the KIND of reachability analysis `check.rs`'s own effect
  system already does for `uses`-clause propagation — a real precedent
  to reuse, not build from scratch).
- Cross-engine consistency: `concurrent component` is confirmed
  interp.rs-only (VM/native both no-op it, per `ASYNC.md` §8.8) — so
  this feature, like PR-cv2-1 through PR-cv2-8, only needs to touch
  `interp.rs`/`check.rs`, not `vm.rs`/`cgen.rs`/`kx.rs`. This should be
  RE-confirmed live (not assumed from memory) at implementation time,
  matching this initiative's own now-established discipline, in case
  anything about `concurrent`'s cross-engine status changed since.
- Does REPL's own `:upgrade` interact safely with a MID-SUSPEND actor
  (a handler paused waiting on I/O when a redefinition happens)? Not
  investigated here — flag as a real edge case for the implementer to
  check live before shipping, not assume away.

## 8. Verification plan (matches this project's own established discipline)

- Live `.kupl` fixtures: a top-level-`let`-bound `http_get` inside a
  handler, confirmed to free the worker (a SECOND sibling actor's own
  message, sent while the first is still "in flight," must be observed
  to complete BEFORE the slow one does — the same kind of genuine-race
  proof `main.rs`'s own
  `concurrent_component_runs_isolated_workers_with_value_determinism_
  but_timing_races_on_interp_only` test already uses for a different
  claim).
- Each of the 4 rejected-nesting shapes from §4 gets its own K0295
  test, confirming a clean diagnostic, not a panic or miscompile.
- The "second blocking call after resume" case from §6 gets a live
  fixture proving it works, not just an assertion that the design
  "should" handle it.
- Full suite green twice; a genuine revert-and-verify for whatever the
  first real commit turns out to be, matching every PR-cv2-N before
  this one.

## 9. Effort/risk assessment, stated honestly

This is a REAL, multi-file undertaking — new AST/checker validation, a
new `PooledActor` field and `WorkerCmd` variant, new scheduler logic in
`worker_loop`, and genuine new test coverage for a genuinely new
runtime behavior (an actor that's alive but not immediately processing
its own mailbox). It is smaller and categorically SAFER than full
stack-switching (zero `unsafe` code anywhere in this design; every
concurrency-relevant piece reuses `Send`-safe primitives — plain
`Value`-in-`Value`-out builtin functions and the EXISTING `mpsc`
channel infrastructure PR-cv2-3 already built and tested), but it is
NOT a small patch — realistically comparable in scope to PR-cv2-1
(supervision strategies), not a one-sitting change. Should be
implemented incrementally, with the restriction/diagnostic (§4, §6)
landing FIRST and independently verifiable (a real, if unexciting, win
on its own — a clear error instead of silent wrong behavior for
now-invalid nesting), THEN the actual suspend/resume scheduler logic
(§5) as a separate, later commit, each with its own full
revert-and-verify cycle — not one large, unreviewable change.
