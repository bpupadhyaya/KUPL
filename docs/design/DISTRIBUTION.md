# KUPL Distributed Components

Proposal v0.1 — 2026-07-03.
Status: PROPOSAL — **design now, implement later** (toolchain Phase 6+). The wire
format and "what may cross a network port" rules must be fixed in spec v1.0, because
they constrain the type system and cannot be retrofitted.

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

- **Spec v1.0 (now):** portability rules, kser format, port-reference concept,
  placement syntax reserved (`at node(...)` parses; single-node runtime rejects it
  with a clear diagnostic).
- **Phase 6+:** `cap.Cluster` reference provider (static member list first; dynamic
  membership later), remote wiring in KVM/native runtimes, deployment manifests.
- Visual tools benefit immediately at spec level: an architecture canvas can show
  node boundaries as real, typed facts of the program rather than documentation.
