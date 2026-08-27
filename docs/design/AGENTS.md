# KUPL Agents

Proposal v0.1 — 2026-08-26.
Status: **§4's own weight-class slice (`agent` + `weight lightweight/
heavyweight/distributed`) is IMPLEMENTED** — see §4's own updated note
below for the exact mechanism and diagnostics (K0125/K0316/K0317).
§3 (`protocol`/`follows`) remains PROPOSAL — designed, not yet built. This
file exists to capture the concept precisely enough that a fresh session,
on any machine, can pick up detailed design and implementation without
needing the conversation that produced it.

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

Illustrative sketch (syntax not finalized — see §6):

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

agent SupportRep {
    intent "handles customer support tickets like a human support rep"
    weight lightweight
    follows CompanyPolicy, GDPR

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
  a new one.
- **Behavioral / runtime-checked rules** — a rule that constrains a VALUE at
  a decision point (e.g. "no financial commitment over $10,000") can't be
  fully verified statically in general — this needs a runtime guard,
  conceptually similar to `expect`/`law` (already-executable assertions) but
  attached to the PROTOCOL rather than a single call site, and evaluated
  automatically at whatever points the protocol declares matter (every
  `expose fun` entry? every `ai fun` result? both, configurable?).

**Open, deliberately unresolved in this doc:** how MULTIPLE protocols
compose when an agent `follows` more than one (strictest-wins? explicit
priority? a conflict is a compile-time error?); whether a rule can be
partially statically checked and partially runtime-checked; whether
protocols are inherited (an agent spawned BY another agent automatically
follows its parent's protocols, mirroring how a child process inherits
capabilities in most sandboxing models) or must be declared explicitly every
time.

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
| `distributed` | A remote node, via `docs/design/DISTRIBUTION.md`'s own `at node(...)` placement syntax (parses today; K0309 rejects it pending real transport — see that doc's own Phase 6+ note) | Erlang distributed processes |

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
  style as `state`), agent-only (K0317), with `distributed` parsing but
  checker-rejected (K0316, mirroring K0309's own precedent exactly) since
  real transport doesn't exist yet.
- **Runtime**: `interp.rs::instantiate_concurrent` gained ONE new
  condition — `weight heavyweight` forces the dedicated-thread path
  (`ActorRoute::Dedicated`) even at the TOP level (not just the pre-
  existing nested-spawn case); `lightweight`/unset are a complete no-op,
  identical to `concurrent component`'s existing pooled-by-default
  behavior. No new `ActorRoute` variant, no new dispatch machinery.
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
  `docs/GAPS.md`'s own existing assessment).
- **Judgment vs. determinism**: `ai fun` is already non-deterministic
  (a live LLM call) — an agent leaning on it inherits that. Should `agent`
  bodies be ALLOWED to mix pure/deterministic exposed funs with `ai fun`
  judgment calls freely (as sketched in §3's `SupportRep` example), or
  should there be a way to mark parts of an agent as required-deterministic
  (auditable, replayable) vs. judgment-based (not)? Real products built on
  agents often need this distinction for compliance/debugging reasons.
- **"Infinite agents"**: does this mean elastic, demand-driven spawning (an
  agent POOL that grows/shrinks, closer to how `lightweight` already
  multiplexes), or literally unbounded concurrent instances (which the
  bounded-mailbox work earlier this session suggests should have SOME cap,
  even if generous, for the same DoS-safety reasons)? Leaning toward: the
  SAME `MAILBOX_CAP`-style "generous but bounded, clean panic on genuine
  runaway growth" philosophy this whole session has applied consistently,
  not a literal unbounded claim.

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
  implemented)/K0317 (`weight` is agent-only) diagnostics; `kupl fmt`
  round-trips `agent`/`weight` losslessly; full `cargo test --bin kupl`
  (90/90) and `cargo test --lib` (1790/1790, green twice); revert-and-
  verify on both the checker restriction and the interp.rs dispatch.
- **NOT STARTED:** §3 (`protocol`/`follows`) — the genuinely harder half
  of this proposal (rule composition/conflict, structural vs. behavioral
  enforcement, integration with the effect/capability system). §5's
  open questions (identity/persistence, judgment-vs-determinism,
  "infinite agents" bounding) also remain fully open.
- **Recommended next slice:** `protocol` as a NEW top-level declaration
  (parallel to `contract`), `follows <protocol>, ...` as an `agent`-only
  clause (mirroring `weight`'s own soft-keyword-inside-body precedent).
  Start with STRUCTURAL rules only (checker-level, extending the
  existing effect/capability system, e.g. "an agent following protocol P
  may not call capability root X directly") — defer behavioral/runtime-
  checked rules (needing a NEW execution hook, closer to `expect`/`law`
  but attached to the protocol rather than one call site) to a THIRD
  slice, matching this whole initiative's own repeated "ship the
  provably-scoped piece first" discipline.
