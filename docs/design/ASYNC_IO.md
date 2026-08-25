# Non-blocking I/O for `concurrent component` handlers — design doc

**Status: §4/§6's own RESTRICTION (the diagnostic, K0295) is
IMPLEMENTED — PR-cv2-9, KUPL commit `a065cce`. The actual §5 scheduling
logic (suspend/resume: `Flow::Suspend`/`SuspendedHandler`,
`WorkerCmd::IoComplete`, `PooledActor.suspended`/`.pending`/
`.pending_stop`) is now ALSO IMPLEMENTED — PR-cv2-10. v1 scope, stated
honestly: only `http_get`/`http_post` (both `Str`-only arguments)
genuinely suspend; `http_get_with`/`read_file_with` still satisfy
K0295's syntactic restriction but execute inline (blocking) at
runtime, because their `CapNet`/`CapFs` argument is deliberately
opaque to `parallel::to_portable` (see `interp.rs`'s own
`blocking_builtin_static_name` doc comment) and so cannot be safely
captured and handed to a spawned I/O thread. `Call`-triggered
`expose fun` execution also does not suspend in v1 (only
`Deliver`-triggered handler bodies do) — deferred because suspending a
`Call` needs its own reply channel stashed across the suspend, a more
complex feature not needed for the FIRST slice. Written under the
Concurrency V2 initiative's own standing maximum-risk
mandate, as the authorized, SAFE alternative to full stack-switching
(see `CONCURRENCY_V2.md` §5's own PR-cv2-7 entry for why hand-rolled
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

## 7. Open questions — resolved during PR-cv2-10 implementation

- Does the top-level-only restriction apply to a `concurrent
  component`'s plain (non-`expose`) private `fun`s too? STILL OPEN —
  K0295's checker restriction (PR-cv2-9) covers `c.funs` per its own
  implementation, but the runtime suspend logic added in PR-cv2-10
  (`exec_stmts_checked`) only fires when `Interp.allow_suspend` is set,
  which is only true directly around a `Deliver`-triggered handler's
  own `exec_block` call — a private `fun` called FROM a handler runs
  with whatever `allow_suspend` state its caller left it in (still
  `true`, since nothing resets it mid-handler), so in practice a
  blocking call inside a reachable private `fun` DOES suspend correctly
  today. Not deliberately designed for; not yet covered by a dedicated
  test. A real gap to close with explicit test coverage, not a
  confirmed-safe behavior.
- Cross-engine consistency: RE-CONFIRMED LIVE at implementation time —
  `concurrent component` remains interp.rs-only; PR-cv2-10 touches only
  `interp.rs` (plus this doc and `main.rs` tests), no `vm.rs`/
  `cgen.rs`/`kx.rs` changes were needed.
- Does REPL's own `:upgrade` interact safely with a MID-SUSPEND actor?
  STILL NOT INVESTIGATED — genuinely open, flagged for whoever picks
  this up next. A live check (redefine a component while one of its
  instances is suspended on `http_get`, confirm no crash/hang/silent
  corruption) has not been performed.
- A NEW finding, not anticipated in this document's original §5 sketch:
  `WorkerCmd::Stop` shares the SAME per-worker channel as `Deliver`/
  `Call`, so a `Stop` queued right behind a `Deliver` that suspends
  would be dequeued and processed WHILE the actor is still suspended —
  tearing the actor down (and acking `stop_all`'s blocking wait) before
  the spawned I/O thread's `IoComplete` ever arrives, silently
  discarding the rest of the handler's execution with no panic, no
  diagnostic, just missing output. Caught LIVE (not by static
  reasoning) via the very first end-to-end test written for this
  feature — see `PooledActor.pending_stop`'s own doc comment in
  `interp.rs` for the fix (defer the teardown until the actor is idle
  again). A reminder that "the design doc says this edge case is
  probably fine" is not the same as verifying it.

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
- A finding from actually doing this (PR-cv2-10), worth stating for
  whoever extends this next: the FIRST cut of `Flow::Suspend` held its
  `SuspendedHandler` payload inline (unboxed). That struct alone is 96
  bytes, which made `Flow`/`EvalResult` — the return type of every
  recursive `Interp::eval` call — 96 bytes too, and silently pushed an
  unrelated, already-deep-recursion differential test into a genuine
  stack overflow. `cargo build`/`cargo check` gave zero warning; only
  running the FULL suite surfaced it. Fixed by boxing the variant
  (`Flow::Suspend(Box<SuspendedHandler>)`), with a permanent
  `const _: () = assert!(size_of::<Flow>() <= 64, ...)` guard added
  right next to the `EvalResult` type alias so a future large variant
  fails to COMPILE instead of silently regressing stack headroom again.
  Any future addition to `Flow` (or anything else returned from deep
  recursive evaluator code) should check its size impact, not assume a
  new field/variant is "small enough" without measuring.

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
