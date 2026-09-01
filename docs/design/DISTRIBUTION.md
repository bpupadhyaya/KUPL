# KUPL Distributed Components

Proposal v0.1 — 2026-07-03.
Status: Spec v1.0's placement-syntax slice is **implemented** (`at node(...)`
parses; the checker rejects it with K0309 on every runtime — see
`docs/reference/DIAGNOSTICS.md`).

**UPDATE, 2026-09-01: real remote wiring now EXISTS, via a DIFFERENT
mechanism than this doc originally sequenced.** `docs/design/AGENTS.md`
§4's `weight distributed` (not `at node(...)` — that placement syntax
remains unimplemented, still K0309) now genuinely connects a `concurrent`
actor to a separate `kupl node` process over a real TCP transport:
`interp.rs::ActorRoute::Distributed`, a hand-rolled binary wire encoding
for `PortableValue` (`src/kser.rs`, this doc's own "kser" name below,
built ahead of schedule specifically to back this), and a small
request/reply protocol (`src/distribution.rs::DistMsg` — Auth/Spawn/
Deliver/Call, all just tagged `kser`-encoded values, no second wire
format needed). This is deliberately a NARROWER slice than the full
vision below: ONE statically-configured node per connection (the
`KUPL_DISTRIBUTED_NODE` env var, `<token>@<host:port>`), no `cap.Cluster`
capability, no dynamic membership, no deployment manifest. **Security
posture, stated plainly:** the connection is SHARED-SECRET
AUTHENTICATED (a token, constant-time compared, gates who may spawn
actors or send messages at all) but NOT ENCRYPTED — every message,
including the token itself, travels in plaintext. Do not run a
`kupl node` across any network you don't already trust; put it behind a
VPN/SSH tunnel/service mesh. Hand-rolling real TLS from scratch, under
time pressure, without expert review, is exactly the class of risk this
doc's own ORIGINAL sequencing reasoning (below) was written to avoid —
this update does not attempt it either; see `docs/PRODUCTION.md`'s
Known Limitations for the same caveat in the production-readiness
context.

Everything past THIS slice — `cap.Cluster`, dynamic membership,
deployment manifests, port references, the full "what may cross a
network port" portability rules, and real transport-layer encryption —
remains **design now, implement later** (toolchain Phase 6+, per this
doc's own original sequencing below, still accurate for everything
except the one slice just described). The wire format and "what may
cross a network port" rules must be fixed in spec v1.0, because they
constrain the type system and cannot be retrofitted.

**Why this doc exists:** "any software application" includes the most common shape
of all — client + server + database across a network. KUPL's component model
(typed ports, no shared state, supervision) is precisely the shape that made
Erlang's distribution story great; leaving distribution undesigned would waste the
model's best structural advantage.

---

## Principles

1. **Location transparency of semantics, never of cost or failure.** A wire to a
   remote component is the same `wire` statement with the same typed-port semantics —
   but crossing a machine boundary is always *visible* in the code (placement is
   explicit, and remote sends carry the `net` effect). No silent remoting: latency
   and partition are architectural facts, not deployment details to hide.
2. **Capabilities do not travel ambiently.** Distribution does not create ambient
   authority; a remote node can only do what the deployment explicitly granted it.
3. **Partition is failure, and failure is supervision.** No new error model —
   unreachable node ⇒ supervised-child failure, handled by the policies that
   already exist.

## Surface

```kupl
app ShopSystem {
    intent "Storefront: browser UI, API server, worker pool."
    requires cluster: cap.Cluster

    let api    = ApiServer(...)    at node("api.shop.internal")
    let worker = ImageResizer(...) at node("gpu-pool")           // pool = any member
    let ui     = StoreFront(...)   at node(local)

    wire ui.order      -> api.orders        // remote wire: same syntax,
    wire api.thumbnail -> worker.resize     // `net` effect, kser-encoded

    supervise worker restart on_failure max 5 in 1m   // partition == failure
}
```

- `at node(...)` reuses the placement form the language already has for hardware
  (`at(gpu)`) — placement is one concept, whether the target is a device or a machine.
- `cap.Cluster` is the capability for membership, discovery, and remote spawn;
  without it, `at node(...)` is a compile error. Transport security (mTLS,
  node identity) lives in the `cap.Cluster` provider, not in application code.
- A deployment manifest (in `kupl.toml` or a `deploy` block) can override placement
  without code edits; the code names *logical* nodes.

## What may cross a network port (spec v1.0 rules)

A type is **portable** iff it is transitively: primitives, records, unions,
collections, tensors — i.e., immutable value data. Enforced by the type checker on
any wire that may be remote.

Explicitly **not portable**:

- **Closures / functions** — no code mobility in v1 (versioning + security tarpit).
  Send data; the behavior already lives on the other side.
- **Capabilities** — never serialized. Cross-node authority is granted by the
  deployment (brokered attenuation), not mailed in messages.
- **Component references** — replaced by **port references**: a serializable,
  unforgeable handle to a specific port of a specific remote instance (this is what
  makes `reply`/request-response work remotely). Port refs are attenuated
  capabilities in spirit: holding one lets you send to that port, nothing else.

## Wire format: kser (shared with INTEROP.md)

- Canonical binary encoding of portable KUPL values; schema derived from the
  structural type, carried by content hash.
- **Evolution rules** (the part that must be right early): adding an optional/
  defaulted record field is compatible; removing or retyping is a major version;
  union variants may be added if the receiver declares a default arm. `kupl pkg`'s
  API-diff machinery (TOOLCHAIN §11) enforces this at publish time — the same
  mechanism, pointed at wire types.

## Delivery semantics (normative once spec'd)

- Per-sender-per-port **FIFO, at-most-once** — the local guarantee, verbatim; a
  remote wire adds "or the link fails," which supervision already models.
- No distributed exactly-once pretense. Idempotency is application semantics —
  express it as contract `law`s (`put(id,v); put(id,v)` ⇒ same state), which
  property tests then enforce.
- `await` on a remote expose-call gets a deadline from the wire's policy
  (`wire a.x -> b.y timeout 2s`); timeout ⇒ `Result` error, not a hang.

## Non-goals (v1 of distribution)

- No distributed shared state, no distributed transactions, no consensus in core —
  those are components/packages (a `Raft` component is a fine thing for the
  ecosystem to build *in* KUPL).
- No code mobility / remote class loading.
- No transparent global namespace of actors; discovery is explicit via `cap.Cluster`.

## Sequencing

- **Spec v1.0 (now):** portability rules, kser format, port-reference concept —
  designed, not yet implemented. Placement syntax IS implemented: a child's
  `at <expr>` clause (`let w = Worker() at node("gpu-pool")`) parses via a soft
  `at` keyword (mirroring `concurrent`/`ai`/`law`); the checker rejects every
  use with K0309 since no runtime executes it yet, and `kupl fmt` round-trips
  it losslessly. This is deliberately a syntax-and-diagnostic-only slice —
  hand-rolling the network transport (mTLS, cluster membership security) under
  time pressure and without expert review would be a more serious class of
  risk than anything else in this codebase, so it is intentionally deferred to
  Phase 6+ rather than rushed.
- **Phase 6+:** `cap.Cluster` reference provider (static member list first; dynamic
  membership later), remote wiring in KVM/native runtimes, deployment manifests.
- Visual tools benefit immediately at spec level: an architecture canvas can show
  node boundaries as real, typed facts of the program rather than documentation.
- **DONE, 2026-09-01 (a NARROWER slice, out of sequence relative to the
  above):** `weight distributed` (`docs/design/AGENTS.md` §4) shipped
  real remote wiring WITHOUT waiting for `cap.Cluster`/deployment
  manifests/`at node(...)` — a static, single-node-per-connection,
  shared-secret-authenticated (not encrypted) TCP transport, `kupl node`.
  See this doc's own top-of-file status update for the full writeup and
  the explicit security caveat. This does NOT retire the Phase 6+ items
  above — they cover genuinely different ground (cluster membership,
  deployment-manifest-driven placement, `at node(...)` syntax, real
  transport encryption) this slice deliberately left alone.
