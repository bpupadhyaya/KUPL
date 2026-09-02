# KUPL Agents

Proposal v0.1 — 2026-08-26.
Status: **All three originally-scoped slices, PLUS both of §5's own
follow-on questions, are now IMPLEMENTED.** §4's weight-class slice
(`agent` + `weight lightweight/heavyweight/distributed`, the last a
real, shared-secret-authenticated — not encrypted — TCP transport via
`kupl node`) — see §4's own updated note (K0125/K0316/K0317). §3's
structural slice (`protocol` + `agent ... follows Protocol` + `forbids
<effect>`) — see §3's own updated note (K1000/K1001/K1002/K1003). §3's
behavioral slice (`guard Name: Type { .. }` + `guards Name`, desugared to
plain `expect` checks with zero new interp/vm/cgen work) — see §3's own
"Behavioral rules: `guard`/`guards`" section (K1004/K1005). §5's
"identity & memory" question — `durable` (K1006, persisted via
`agent_persist.rs`, `kupl agent inspect`/`clear` CLI). §5's
"judgment vs. determinism" question — `deterministic` (K1008/K1009).
Multi-protocol composition's one concretely dangerous shape — a
same-named `guard` on two different followed protocols — is now K1010
(compile-time error). Full normative spec for everything above:
`../reference/LANGUAGE-REFERENCE.md` §7.3. §5's "infinite agents"
bounding is DONE in the narrower sense of an actual resource cap
(`MAX_ACTOR_INSTANCES`, `MAX_NODE_CONNECTIONS`) — the ONLY remaining
fully-open item in this whole initiative's original question set is
protocol inheritance across agent-spawns-agent trees — see §7's own
updated Sequencing note. This file exists to capture the concept
precisely enough that a fresh session, on any machine, can pick up
detailed design and implementation without needing the conversation
that produced it.

**Why this doc exists:** the user's own framing (verbatim, lightly trimmed):

> add another keyword called agent, which is higher level element, compared to
> actor (lower level). implement agent functionality similar to human, agent
> should be designed and implemented to simulate/emulate/represent human so
> that you can program as if infinite human agents are available. Agents can
> use actors, which are lower level, to accomplish their task, agents can use
> components. You can add features that allow programmers specify protocol for
> agents, just like humans have rules to follow: country rules, international
> rules, heritage rules, etc... agent can make use of lightweight threads like
> Go concurrency threads, or heavy weight threads like Java threads or
> concurrency/distributed threads like Erlang... agents will make KUPL an
> AI-first language.

---

## 1. The core idea

KUPL already has three tiers of "thing that does work," from most passive to
most active:

1. **`component`** — a plain, sequential unit of structure and state. No
   concurrency, no identity beyond what the program gives it.
2. **`concurrent component`** (an "actor," in the informal sense this whole
   codebase already uses the word) — an isolated, message-passing unit of
   *execution*: its own state, its own thread (or pooled-thread slot), a
   mailbox, supervision. This is machinery — correct, fast, and (per this
   session's own concurrency work) now has selective receive, bounded
   mailboxes, call timeouts, and direct actor-to-actor channels. But it has no
   notion of *identity*, *judgment*, or *constraint* — it does exactly what its
   code says, nothing more.
3. **`agent`** (NEW, this proposal) — a higher-level unit that represents a
   *worker*, not a *process*. An agent has:
   - a **persona/role** (what it's for, in the same `intent "..."` idiom every
     other KUPL declaration already uses),
   - **judgment**, via `ai fun` (already a first-class KUPL construct — an
     LLM-backed typed function) — an agent is the natural, intended consumer
     of `ai fun`, not a bolt-on,
   - **tools**, via ordinary `component`s and `concurrent component`s it owns
     or can reach — an agent does not reimplement execution, it *orchestrates*
     existing lower-level primitives,
   - **governance**, via a `protocol` (§3) it must operate within — the
     agent-level analogue of the capability system's `cap_net_root()`-style
     "you may not just seed this yourself" discipline, but for BEHAVIORAL
     rules instead of resource access.

The pitch, stated plainly: an `agent` block should read like a job
description, not a thread-spawn call. "You can program as if infinite human
agents are available" — the language handles concurrency mechanics (weight
class, §4), mailbox semantics, and rule-following (§3) so the PROGRAMMER's own
code reads like assigning work to a team, not managing a thread pool.

## 2. Relationship to existing primitives (not a replacement for any of them)

An `agent` is a NEW top-level declaration, syntactically parallel to
`component`/`concurrent component`, not a modifier on either. It:

- **uses actors and components as tools**, the same way a human worker uses
  software tools — an agent's own body can construct/hold `concurrent
  component` and `component` instances exactly like any other component can
  today (`let q = TicketQueue()`), and call their exposed functions.
- **is itself addressable and composable** the way a component is — an agent
  can presumably be a child of another agent, or of an `app`, though whether
  agent-to-agent composition needs its OWN semantics (distinct from
  component-to-component `wire`/`supervise`) is an open question (§6).
- **is NOT a rename of `concurrent component`.** The weight-class idea (§4)
  means an agent might be backed by a lightweight pooled actor, a dedicated
  OS thread, OR (per `docs/design/DISTRIBUTION.md`'s own `at node(...)`
  placement syntax, already scaffolded this session) a REMOTE node — the
  agent abstraction is deliberately decoupled from any one of those, the same
  way a job posting doesn't specify which specific employee will fill it.

## 3. Protocols: rules an agent must follow

Humans operate under layered rule sets they didn't individually choose:
national law, international treaties/norms, organizational policy, family/
cultural norms. The proposal: a new `protocol` declaration, analogous to
`contract` (which already declares a shape components can `fulfill`), except
a protocol declares BEHAVIORAL rules an agent commits to `follow`s, not a
function-signature shape.

Illustrative sketch of the FULL proposal, including the still-deferred
`rule "..."` behavioral form (§6/§7 — NOT implemented; see the "Implemented"
note below for exactly what the shipped `forbids <effect>` structural form
looks like instead):

```
protocol CompanyPolicy {
    intent "internal company rules for customer-facing agents"
    rule "no financial commitment over $10,000 without human approval"
    rule "must log every customer-data access"
}

protocol GDPR {
    intent "EU data protection baseline"
    rule "personal data may not leave the EU without explicit consent"
}

agent SupportRep follows CompanyPolicy, GDPR {
    intent "handles customer support tickets like a human support rep"
    weight lightweight

    let queue = TicketQueue()          // a concurrent component (actor)
    let mailer = EmailSender()          // a plain component

    ai fun triage(ticket: Ticket) -> Priority {
        intent "decide how urgent this ticket is"
    }

    expose fun handle(ticket: Ticket) -> Response {
        let priority = triage(ticket)
        // ...
    }
}
```

**Two enforcement models, both worth designing for, not necessarily
either/or:**

- **Structural / statically checkable rules** — a rule that constrains WHAT
  an agent's own code may reference or do (e.g. "must not call
  `cap_net_root()` directly," "must not construct a `PaymentProcessor`
  without also calling `requireApproval`") is exactly the kind of thing
  KUPL's existing effect system (`uses`/`add uses`) and capability
  restrictions (K0304 and friends) already do — a `protocol` could compile
  down to additional checker obligations, the same family of mechanism, not
  a new one. **IMPLEMENTED** (KUPL commit history, 2026-08-26) — see below.
- **Behavioral / runtime-checked rules** — a rule that constrains a VALUE at
  a decision point (e.g. "no financial commitment over $10,000") can't be
  fully verified statically in general — this needs a runtime guard,
  conceptually similar to `expect`/`law` (already-executable assertions) but
  attached to the PROTOCOL rather than a single call site, and evaluated
  automatically at whatever points the protocol declares matter (every
  `expose fun` entry? every `ai fun` result? both, configurable?). **NOT
  implemented** — remains the deferred third slice (§7), but its own
  previously-blocking DESIGN QUESTION is now RESOLVED (2026-08-26) — see
  "Behavioral rules: the design that unblocks them" below.

### Behavioral rules: `guard`/`guards` (IMPLEMENTED, 2026-08-27)

The blocker, stated precisely: KUPL is statically typed, and a behavioral
rule needs to reference the VALUE it's constraining (e.g. `result <
10000`) — but a `protocol` is declared independently of any specific
`agent`, so if a rule could apply to "whatever exposed fun an agent
attaches it to," `result`'s type would vary per following agent's own
signature, unresolvable at the protocol's own declaration site.

**The resolution: reuse `contract`'s own `law` pattern, already proven to
solve exactly this class of problem.** `check_contract` (`check.rs`) type-
checks a `law`'s body against the CONTRACT's own `sigs` — a contract
declares `expose fun get(k: Str) -> Int`, and every `law` referencing
`get(k)` type-checks against THAT fixed signature, entirely independent
of what any particular fulfilling component's own other methods look
like. A protocol can do the identical thing: declare its OWN named,
concretely-typed operation, and let agents opt in per-fun.

```
protocol SpendingLimit {
    intent "no single response commits more than $10,000"
    guard CommitAmount: Int {
        expect result < 10000
    }
}

agent Rep follows SpendingLimit {
    intent "..."
    expose fun commit(amount: Int) -> Int guards CommitAmount {
        // ... decide the amount ...
        amount
    }
}
```

- **Grammar**: `guard Name: Type { <block> }` inside `protocol` (parser.rs::
  parse_protocol, mirroring `law "..." { <block> }` inside `contract`
  exactly except the name is a bare identifier, referenceable from
  `guards`). `guards Name1, Name2` — a soft-keyword clause on ANY `fun`
  (parsed in `parse_fun`, right before the body, the same position as
  `ai fun`'s own `tools [..]` clause) — parser-accepts-broadly, mirroring
  `weight`/`follows`'s own precedent.
- **Checker/resolution (`src/guards.rs`, a NEW pre-check pass)**:
  `desugar_guards` runs from the SAME pipeline position as `callargs::
  resolve_call_args` (`run::compile`, the loader-check path, and
  `loader.rs`'s own multi-file merge — all three call sites), BEFORE
  `check::check`. It resolves each `guards Name` reference against the
  union of every guard declared by the agent's own `follows`ed protocols:
  K1004 if `guards` appears anywhere other than an `agent`'s own `expose
  fun` (a top-level `fun`, a component's private `fun`, or a plain
  component's `expose fun`), K1005 if the name doesn't resolve (with a
  `did you mean` suggestion, mirroring K1001's precedent). A genuine
  return-type mismatch is deliberately NOT its own diagnostic code — it
  surfaces as an ordinary K0200 pointing at the rewritten code's own
  explicit `let result: Type = ...` annotation (see below), since the
  rewrite makes the fun's actual return type and the guard's declared
  type meet at a concrete, ordinarily-type-checked expression.
- **Enforcement, avoiding new per-engine runtime work entirely**: rather
  than a new interp.rs/vm.rs/cgen.rs hook (which would need each engine to
  intercept a function's own return value, correctly handling early
  `return`), `desugar_guards` rewrites a `guards`-bearing fun's body ONCE,
  at the AST level, before type-checking: every syntactic exit point
  (the implicit tail expression, and every `return`, walked recursively
  through `if`/`match`/`while`/`for`/`receive`/nested blocks — NOT
  through `Lambda` bodies, which have their own separate return-catching
  boundary, `interp.rs::call_value`'s `Closure` branch, confirmed by
  reading it directly) becomes `{ let __guard_result = <exit value>; let
  result: Type = __guard_result; <guard's own body statements, spliced in
  verbatim>; __guard_result }` (chained once per applicable guard, for
  `guards A, B`). `result` binding via an ordinary `let` (not textual
  renaming) means the guard's own body needs no AST rewriting of its own
  — it's spliced in unchanged. Because this happens BEFORE any engine
  sees the program, and `expect` already compiles identically on all four
  engines (confirmed by reading `compile.rs`'s shared `Op::JumpIfTrue`+
  `Op::Panic` lowering directly), this needed ZERO new runtime engine
  code — `kupl fmt`/the LSP never see the desugared form at all, since
  the pass runs only from the compile/check pipeline, never from bare
  `parser::parse`.
- **A REAL, pre-existing, UNRELATED bug found+fixed along the way**: the
  exhaustive control-flow test sweep this feature's own correctness
  demanded (every exit-point shape needs live verification, not just
  "looks right") surfaced a genuine pre-existing checker bug, present
  since long before this feature: a function/closure whose body's own
  LAST statement was `return EXPR` was spuriously rejected with K0200
  ("expected T, found Unit"). Root cause: `Stmt::Return` correctly checks
  its own value's type, but reports `Ty::Unit` as ITS OWN contribution to
  the enclosing BLOCK's tail-type inference (correct in isolation — a
  `return` has no "block value" of its own) — but every caller comparing
  a block's own reported type against an expectation (a fun/closure's own
  declared return type, two `match` arms required to agree) blindly used
  that `Ty::Unit`, with no awareness the block actually DIVERGES via the
  return and never falls through. Fixed centrally in `check_block`
  itself: a block ending in `Stmt::Return` now reports a fresh,
  freely-unifying type variable instead of `Ty::Unit`, so every caller's
  unification against it succeeds trivially — the closest approximation
  this checker has to a real "never" type. `fun probe() -> Int { return
  50000 }` (previously rejected) now checks clean; `fun probe() -> Int {
  return "wrong" }` is still correctly rejected (that check is
  `Stmt::Return`'s own, unaffected by this fix).
- **Verified**: `src/guards.rs`'s own test module (~15 unit tests) drives
  `apply_guards` directly against real interpreter execution (the
  oracle), covering: plain tail value (respecting/violating), the
  correctness-critical early-`return` case, `return` inside `while`/`for`/
  `match` arms, `return` nested several levels deep, an `if`-expression
  used as the fun's own tail (checked once, not per-branch), a `return`
  inside a `Lambda` correctly NOT touched by the enclosing fun's own
  guard, a body with no trailing expression (implicit `Unit`), a bare
  `return` (also `Unit`), a trailing `return` as the body's own last
  statement (wrapped exactly once, not double-wrapped), and multiple
  guards on one fun each running independently. A `main.rs` end-to-end
  test proves both the tail-value AND early-return violations panic at
  real `kupl run` (with `kupl check` passing clean, since a `guard` is a
  runtime check, not statically provable) and a guard-respecting agent
  runs cleanly. `fmt.rs` confirms `guard`/`guards` round-trip losslessly
  and are NEVER shown in desugared form. Full `cargo test --bin kupl`
  (92/92) and `cargo test --lib` (1817/1817, green twice — plus a single,
  confirmed-environmental unrelated flake on an unrelated native-process
  I/O test, reproduced as passing cleanly in isolation). Revert-and-
  verify: `apply_guards` temporarily made a no-op, confirmed a genuine
  guard violation silently succeeds instead of panicking (live-verified
  directly against the built binary), restored, `cmp` byte-identical.

**The STRUCTURAL slice (`forbids`), for reference — implemented earlier,
before `guard`/`guards` above:** narrower than the illustrative sketch at
the top of this section — one rule shape, `forbids <effect>` (a dotted
effect name, the SAME vocabulary `uses`/`add uses` already use: `io`,
`io.net`, `io.fs`, `ai`, …), not a value-level check.

```
protocol NoNetwork {
    intent "no network access"
    forbids io.net
}

agent Rep follows NoNetwork {
    intent "a rep that shouldn't reach the network"
    expose fun greet(name: Str) -> Str { "hello, {name}" }
}
```

- **Grammar**: `protocol Name { intent "..." forbids <effect>... }` is a new
  top-level `Item::Protocol` (`token.rs::KwProtocol`, `parser.rs::
  parse_protocol`, mirroring `parse_contract`'s own shape). `agent Foo
  follows Protocol1, Protocol2 { .. }` — `follows` parses right after the
  component/agent NAME, before the `{`, the exact same position and shape as
  the existing `fulfills <contract>` clause (NOT inside the body, unlike
  `weight`) — parser-accepts-broadly on any `component`/`agent`, checker-
  narrowed to `agent`-only (K1003, mirroring K0317's own precedent exactly).
- **Checker**: K1000 (duplicate protocol name), K1001 (`follows` names an
  unknown protocol, with a `did you mean` suggestion), K1003 (`follows` on a
  non-agent) all live in `check.rs`, mirroring `check_fulfills`'s own
  unknown-contract-name precedent. K1002 (an agent's own EXPOSED fun
  actually performs an effect a followed protocol forbids) lives in
  `effects.rs::check_protocols`, reusing `infer_effects`'s already-built
  transitive fixpoint (the SAME computation K0301/K0302's boundary-
  explicitness enforcement is built on) and the same hierarchical `covers`
  semantics (`forbids io` also forbids `io.net`) — called from inside
  `check_effects` itself so none of that function's ~15 existing call sites
  needed to change.
- **VM/native/fmt/resolve/lsp/sdiff**: `protocol`/`follows` are purely
  static/checker-time constructs (no runtime data needed), so `interp.rs`'s
  `ProgramDb`, `compile.rs`, `vm.rs`, and `cgen.rs` all treat `Item::
  Protocol` as a no-op — confirmed live (`kupl run --vm`/`kupl native` both
  execute a `follows`-respecting agent exactly like an ordinary one, and a
  program that fails K1002 fails identically under `kupl check`/`kupl run`,
  since the interpreter's own entry point runs the checker first).
  `fmt.rs` round-trips both losslessly; `sdiff.rs`'s `interface_of` treats a
  protocol's own `forbids` list as its whole public interface (widening or
  narrowing it is `[INTERFACE — breaking]`, matching the precedent
  `contract`'s own effect-budget fingerprint already established).

**UPDATE, 2026-09-02 — the concrete part of this is now RESOLVED (K1010).**
How multiple protocols compose when an agent `follows` more than one had
three candidate answers (strictest-wins? explicit priority? a conflict is
a compile-time error?). `forbids` never actually needed one (multiple
`forbids` lists just union — no two forbidden-effect lists can
meaningfully "conflict," they only ever narrow further). `guard`
composition DID have one real, dangerous shape: two DIFFERENT followed
protocols declaring a guard with the SAME NAME (e.g. both `protocol A`
and `protocol B` declare `guard Approve: Int { .. }` with different
bodies) used to resolve silently to whichever protocol was encountered
last in `follows` — a genuine footgun (a later, unrelated protocol
addition could silently change what an existing `guards Approve` clause
enforces, with zero diagnostic). Picked the third candidate answer:
**a same-name collision across two different followed protocols is now
a compile-time error, K1010** (`guards.rs::desugar_guards`), independent
of whether any exposed fun actually references the colliding name —
rename one of the guards to disambiguate. Still open, genuinely: whether
a rule can be partially statically checked and partially runtime-checked
(moot today — `forbids` is 100% static, `guard` 100% runtime, nothing
straddles both yet); whether protocols are inherited (an agent spawned BY
another agent automatically follows its parent's protocols, mirroring how
a child process inherits capabilities in most sandboxing models) or must
be declared explicitly every time — this remains unstarted, and is a
materially different question (agent-spawns-agent trees) from the
guard-naming question just closed above.

## 4. Weight class: let the agent pick its own concurrency implementation

The user's own framing: an agent should be able to use "lightweight threads
like Go," "heavy weight threads like Java," or "concurrency/distributed
threads like Erlang" — not because KUPL needs three separate concurrency
models, but because **KUPL, per this session's own concurrency-v2/v3/v4
work, already has the building blocks for exactly these three tiers**, and
`agent` is the right place to expose a friendly, opinionated choice over
them rather than making every agent author reason about `ActorPool` sizing
directly:

| `weight` value | Backed by (already built) | Go/Java/Erlang analogue |
|---|---|---|
| `lightweight` (default) | `ActorPool`-multiplexed pooled actor (`interp.rs`'s own `PooledActor`/`ActorRoute::Pooled`) — many agents share few OS threads | Go goroutines |
| `heavyweight` | Dedicated OS thread (`ActorRoute::Dedicated`) — one real thread, always resident | Java platform threads |
| `distributed` | **UPDATE, 2026-09-01 — IMPLEMENTED**, via a real TCP transport (`ActorRoute::Distributed`, `kupl node`, `docs/design/DISTRIBUTION.md`'s own "wire format: kser" section) — NOT `at node(...)` placement (that remains a SEPARATE, still-unimplemented syntax, K0309, its own future increment) | Erlang distributed processes |

This table is the single most important insight this proposal makes: **an
`agent` is not a fourth concurrency primitive that needs its own runtime.**
It is a NAMING and DEFAULTS layer over the three concurrency tiers this
codebase already has. The prediction held: implementing this slice was
almost entirely parse-the-keyword-and-route, confirmed by the actual
implementation (KUPL commit history, 2026-08-26):

- **Grammar**: `agent Foo { .. }` is a new hard keyword (`token.rs::
  KwAgent`, parsed by the SAME `parse_component` function `component`/
  `app` already share, extended with an `is_agent: bool` parameter) —
  reusing `ast::ComponentDecl` wholesale (`is_agent: bool` + `weight:
  Option<AgentWeight>` fields added, mirroring the EXISTING `is_app: bool`
  precedent) rather than a new AST node, so an agent gets ports/state/
  handlers/exposed-funs/children/supervision for free, identical to a
  `concurrent component`. `is_agent` always forces `concurrent: true` at
  parse time (an agent IS inherently its own actor). A `weight <value>`
  clause is a new contextual keyword inside the body (`parser.rs`, same
  style as `state`), agent-only (K0317). `distributed` originally parsed
  but was checker-rejected (K0316, mirroring K0309's own precedent) since
  real transport didn't exist yet — **UPDATE, 2026-09-01: K0316 is now
  RETIRED, `distributed` is genuinely implemented.** See §7's own updated
  entry for the full writeup.
- **Runtime**: `interp.rs::instantiate_concurrent` gained TWO new
  conditions — `weight heavyweight` forces the dedicated-thread path
  (`ActorRoute::Dedicated`) even at the TOP level (not just the pre-
  existing nested-spawn case); `weight distributed` (checked FIRST,
  entirely separate from the Pooled-vs-Dedicated choice) connects out to
  a `kupl node` over TCP instead. `lightweight`/unset are a complete
  no-op, identical to `concurrent component`'s existing pooled-by-default
  behavior. A new THIRD `ActorRoute::Distributed` variant backs
  `distributed`; `heavyweight`/`lightweight` needed no new dispatch
  machinery beyond the existing `Pooled`/`Dedicated` split.
- **VM/native**: needed ZERO changes — `compile.rs`/`cgen.rs` never read
  `is_agent`/`weight` at all, confirmed live (`kupl run --vm`/`kupl
  native` both execute an `agent` exactly like an ordinary `concurrent
  component`, byte-identical output).

Real new engineering work remains concentrated in §3 (protocols, NOT YET
implemented) and the agent-level orchestration surface (§5, largely
unaddressed) — not in concurrency mechanics, exactly as predicted.

## 5. What "simulate/emulate/represent a human" should mean, concretely

This is the vaguest part of the request and the part most likely to be
argued about — worth being explicit that this proposal does NOT commit to a
specific answer yet, only to naming the axes:

- **Identity & memory**: does an agent have persistent state ACROSS restarts
  (a "who this agent is" that survives a crash/redeploy), the way a human
  employee's knowledge persists even if they take a day off? KUPL's existing
  `state` fields are per-instance and reset on `supervise restart` — an
  agent's own persistent identity may need something closer to durable
  storage, out of scope for THIS language-level proposal but worth flagging
  as a real gap (KUPL has no persistence/database layer today, per
  `docs/GAPS.md`'s own existing assessment). **UPDATE, 2026-09-01 — the
  narrow, honest v1 slice of this is now DONE**: `durable` (a new
  agent-only contextual keyword — `agent Foo { durable ... }`) persists an
  agent's own `state` fields to disk (`src/agent_persist.rs`, reusing
  `kser`'s existing binary encoding entirely — a `PortableValue::Map`
  keyed by field name, no new serialization code) across SEPARATE `kupl
  run` invocations of the same program, not just an in-process supervised
  restart (which already preserved agent state before this, see the
  `restart()` note just above this list). K1006 rejects `durable` on a
  non-agent. Works with EVERY `weight` value, including `distributed` —
  `Interp::serve_distributed_connection` (the `kupl node` server side)
  calls the SAME `instantiate_local`/`stop_all` functions the load/save
  hooks already live inside, so no special-casing was needed. (A K1007
  restriction — "`durable` + `weight distributed` not yet supported" —
  was added and then retired the SAME day, once live testing proved the
  original "the server doesn't share the save hook" assumption wrong; a
  `durable agent { weight distributed }` genuinely persists correctly,
  on the NODE's own filesystem, verified across 3 separate client `kupl
  run` invocations against one long-lived `kupl node`.) Deliberately
  does NOT cover: crash-consistency (persistence
  only happens on a SUCCESSFUL `kupl run`, inheriting `run.rs::
  run_program`'s own pre-existing "no graceful shutdown on panic"
  asymmetry), concurrent-writer safety across processes, schema migration
  for a `state` field whose TYPE changes between versions (an added/
  removed field degrades gracefully; a changed type does not), or
  per-instance identity (one state file per agent DECLARATION name, not
  per dynamically-spawned instance) — all explicitly named, not silently
  glossed over, matching this whole session's "authenticated but not
  encrypted"-style honesty discipline. Verified: `agent_persist.rs`'s own
  unit tests (round-trip, corrupt-file tolerance, non-portable-field
  handling); a genuine end-to-end test running the real `kupl` binary FOUR
  separate times in a row and confirming a counter keeps incrementing
  (`1, 2, 3, 4`), plus a negative control proving a non-`durable` agent
  never accumulates (`1, 1, 1`); full `cargo test --lib`/`cargo test --bin
  kupl` green.
- **Judgment vs. determinism — PARTIALLY ANSWERED by existing machinery,
  2026-08-27**: `ai fun` is already non-deterministic (a live LLM call) —
  an agent leaning on it inherits that. The distinction this axis asks
  for ("mark parts of an agent as required-deterministic vs. judgment-
  based") turns out to ALREADY EXIST, for free, in KUPL's own effect
  system — confirmed by reading `effects.rs` directly: an `ai fun`
  performs the `ai` EFFECT (`info.decl.ai.is_some()` inserts `"ai"` into
  its own direct effect set), which propagates transitively through the
  SAME `infer_effects` fixpoint every other effect uses, and is enforced
  by the SAME K0301 boundary-explicitness rule (`must_declare` funs —
  `pub`/`expose` — are REJECTED if they transitively reach an effect they
  don't declare). Consequence: an agent's own `expose fun` that does NOT
  declare `uses ai` is ALREADY statically GUARANTEED, by machinery this
  whole initiative never had to build, to be free of any AI judgment call
  anywhere in its transitive call graph — auditable/replayable by
  construction, no new language feature needed. The per-function
  distinction is real and unaffected by the update below: an agent can
  still freely mix `uses ai` and non-`uses ai` exposed funs, exactly as
  §3's own `SupportRep` sketch already does.

  **UPDATE, 2026-09-02 — the coarser, agent-level declaration is now
  DONE too.** `agent Foo { deterministic }` (a new bare contextual
  keyword, same shape as `durable`) is a checker-enforced,
  compliance-strength guarantee that EVERY one of an agent's own exposed
  funs — not just the ones an author remembered to leave `ai`-free — has
  a transitive effect set that never includes `ai`. Mechanism: K1008
  (`check.rs`) rejects `deterministic` on a non-`agent`; the actual
  enforcement is K1009 (`effects.rs::check_deterministic_agents`),
  structurally IDENTICAL to how a followed `protocol`'s own `forbids`
  list is already enforced (K1002) — `deterministic` is exactly
  "implicitly follows a protocol that forbids `ai`, without declaring
  one," reusing the SAME `infer_effects` fixpoint, not a new mechanism.
  `kupl fmt` round-trips `deterministic` losslessly. Verified: 6 new
  tests (agent-only rejection, a well-formed clean case, a violation
  caught transitively through a private helper — mirroring K1002's own
  transitivity test exactly — a genuine non-violating case, and
  confirming an agent WITHOUT `deterministic` is completely unaffected);
  full `cargo test --lib` green twice (1866/1866); `cargo test --bin
  kupl` green (99/99, unchanged — this is a checker-only feature with
  no runtime/CLI surface).
- **"Infinite agents"**: does this mean elastic, demand-driven spawning (an
  agent POOL that grows/shrinks, closer to how `lightweight` already
  multiplexes), or literally unbounded concurrent instances (which the
  bounded-mailbox work earlier this session suggests should have SOME cap,
  even if generous, for the same DoS-safety reasons)? Leaning toward: the
  SAME `MAILBOX_CAP`-style "generous but bounded, clean panic on genuine
  runaway growth" philosophy this whole session has applied consistently,
  not a literal unbounded claim. **UPDATE, 2026-08-28 — the bounding half
  of this is now DONE**: `interp.rs::MAX_ACTOR_INSTANCES` (1,000,000)
  caps the TOTAL number of `concurrent component`/`agent` instances one
  program may spawn over its whole run, enforced in
  `instantiate_concurrent` via `reserve_actor_slot` — a runaway/recursive
  spawn loop now fails with a clean, diagnosed panic instead of unbounded
  thread/memory growth, exactly the leaning stated above. This is a
  monotonic total-spawned-this-run counter, NOT a live-instance gauge
  (unlike `MAILBOX_CAP`, which shrinks as messages are consumed) — no
  cross-thread "actor died, free its slot" signal exists yet to support
  true liveness tracking, so a program that spawns-and-lets-die
  1,000,001 SHORT-LIVED actors over a long run would still hit this cap
  even though far fewer than that are ever alive at once. The "elastic,
  demand-driven POOL" framing above remains genuinely unresolved — this
  update only closes the "is spawning literally unbounded" half of the
  question, not the pool-sizing/backpressure design.

## 6. Explicitly open questions (do not resolve in this pass)

- Final `protocol`/`agent`/`weight`/`follows` keyword choices and grammar —
  the sketches in §3/§4 are illustrative, not committed syntax. Whether
  `protocol` is closer to `contract` (a checker-level shape) or to a new,
  distinct construct needs a real design pass against `check.rs`'s existing
  effect-system machinery before any parser work starts.
- Agent-to-agent composition and supervision — does `supervise` (already
  built for `concurrent component` children) extend to agents unchanged, or
  does an agent's own "worker went off the rails" failure mode need a
  DIFFERENT recovery model (e.g. "reassign the task to another agent"
  instead of "restart the same one")?
- How a `protocol`'s runtime-checked rules integrate with `ai fun`'s own
  existing tool-calling loop (`docs/design/` — `ai fun` can already call
  other functions as tools; should protocol rules apply to those inner tool
  calls too, or only the agent's own top-level entry points?).
- Whether this needs its OWN diagnostic code range (a new `K10xx`-style
  block) once real implementation starts, matching this project's own
  existing per-subsystem numbering convention.

## 7. Sequencing

- **DONE:** `agent` + `weight lightweight/heavyweight/distributed` dispatch
  onto the three existing spawn paths (§4's own updated section has the
  full mechanism). Verified: `kupl check`/`kupl run`/`kupl run --vm`/
  `kupl native` all agree; a real end-to-end test proves BOTH
  `lightweight` and `heavyweight` produce correct results on every
  engine; K0125 (malformed `weight` value)/K0316 (`distributed` not
  implemented, since RETIRED — see below)/K0317 (`weight` is agent-only)
  diagnostics; `kupl fmt` round-trips `agent`/`weight` losslessly; full
  `cargo test --bin kupl` (90/90) and `cargo test --lib` (1790/1790,
  green twice); revert-and-verify on both the checker restriction and
  the interp.rs dispatch.
- **DONE, 2026-09-01:** `weight distributed` itself — real, node-to-node
  actor transport (`interp.rs::ActorRoute::Distributed`, `kupl node`
  server subcommand, `src/kser.rs` hand-rolled binary wire encoding for
  `PortableValue`, `src/distribution.rs`'s `DistMsg` protocol + shared-
  secret-authenticated TCP). K0316 retired. Security posture stated
  plainly: AUTHENTICATED (a shared-secret token, constant-time compared)
  but NOT ENCRYPTED — run behind a VPN/SSH tunnel for anything crossing
  an untrusted network; see `docs/design/DISTRIBUTION.md`'s own updated
  status and `docs/PRODUCTION.md`'s Known Limitations for the full
  writeup. Verified: `kser`/`distribution` unit tests (binary round-trip
  including NaN bit-pattern preservation, frame boundaries, auth/spawn/
  deliver/call protocol shapes, all against real TCP loopback mock
  servers); a genuine end-to-end test spawning a REAL second `kupl node`
  OS process and round-tripping a message through it; full `cargo test
  --lib` green twice and `cargo test --bin kupl` green; revert-and-verify
  on the `ActorRoute` wiring. NOT implemented: `at node(...)` placement
  syntax (a separate, still-unimplemented mechanism, K0309) and the
  broader `cap.Cluster`/dynamic-membership/mTLS vision `DISTRIBUTION.md`
  itself scopes as "Phase 6+" — this ships the single-static-node,
  authenticated-but-plaintext slice specifically, not the full spec.
- **DONE:** §3's structural slice — `protocol Name { intent "..." forbids
  <effect>... }` + `agent Foo follows Protocol1, Protocol2 { .. }`,
  checker-enforced via K1000/K1001/K1002/K1003 (§3's own updated section
  has the full mechanism). Verified: `kupl check`/`kupl run`/`kupl run
  --vm` all agree that a protocol-violating agent is rejected (K1002) and
  a protocol-respecting one runs cleanly; `kupl fmt` round-trips
  `protocol`/`follows` losslessly; `kupl diff` treats a protocol's
  `forbids` list as public interface; full `cargo test --bin kupl`
  (91/91) and `cargo test --lib` (1799/1799, green twice); revert-and-
  verify on both `effects.rs::check_protocols` (K1002) and `sdiff.rs`'s
  `interface_of` Protocol arm.
- **DONE:** §3's behavioral/runtime-checked enforcement model —
  `guard Name: Type { .. }` on `protocol` + explicit opt-in `guards Name`
  on an agent's own exposed fun, desugared to plain `expect` checks at
  every syntactic exit point BEFORE type-checking (§3's own updated
  "Behavioral rules: `guard`/`guards`" section has the full mechanism).
  Verified: `src/guards.rs`'s own ~15-test control-flow sweep (tail
  value, early `return`, nested `if`/`match`/`while`/`for`, `Lambda`
  correctly excluded, `Unit`/bare-return, multiple guards); a real
  end-to-end test proves both a tail-value AND an early-return violation
  panic at `kupl run` (with `kupl check` passing clean, since a `guard`
  is a runtime check); `kupl fmt` round-trips `guard`/`guards` losslessly
  and never shows the desugared form; full `cargo test --bin kupl`
  (92/92) and `cargo test --lib` (1817/1817, green twice); revert-and-
  verify on `apply_guards`. Along the way, found+fixed a genuine
  pre-existing, unrelated checker bug (`check_block` treating a
  block ending in `return` as `Ty::Unit` for tail-type-unification
  purposes, spuriously rejecting valid `return`-terminated fun/closure
  bodies and mismatched `match` arms) — see §3's own writeup.
- **DONE, 2026-09-01:** `weight distributed` itself, real node-to-node
  actor transport (`interp.rs::ActorRoute::Distributed`, `kupl node`,
  `src/kser.rs`, `src/distribution.rs`) — see §4's own updated table.
  Shared-secret authenticated, NOT encrypted; a static single-node
  slice, not the full `cap.Cluster` vision. K0316 retired.
- **DONE, 2026-09-01:** §5's "identity & memory" axis, the narrow
  in-process-crash-boundary slice — `durable agent` (`src/agent_persist.rs`,
  see §5's own updated entry above for the full writeup). NOT the full
  vision (crash-consistency, cross-process concurrent writers, schema
  migration, per-instance identity all remain open, explicitly named
  rather than silently deferred).
- **DONE, 2026-09-02:** §5's "judgment vs. determinism" axis, the
  coarser agent-level declaration — `agent Foo { deterministic }`,
  K1008/K1009. See §5's own updated entry above for the full mechanism.
- **DONE, 2026-09-02:** multi-protocol composition/conflict resolution
  (§3), for its one concretely dangerous shape — a same-named `guard`
  declared by two DIFFERENT followed protocols is now K1010 (a
  compile-time error), instead of silently resolving to whichever
  protocol is encountered last in `follows`. `forbids` never needed a
  resolution (union only narrows, never conflicts). See §3's own updated
  entry above.
- **Recommended next slice:** the ONLY remaining named-but-unstarted item
  from this initiative's own original question set is protocol
  inheritance across agent-spawns-agent trees (§3's own updated entry,
  last paragraph) — a materially different, bigger question than the
  guard-naming collision just closed, since it needs actual spawn-tree
  bookkeeping that doesn't exist yet, not just a static name check.
