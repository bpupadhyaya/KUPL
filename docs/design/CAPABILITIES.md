# Capabilities as attenuable values — design sketch

v0.1 (it112) — a bounded design deliverable, not an implementation. Addresses
`docs/design/LANGUAGE.md` §2's own long-standing claim ("effects are backed by
capabilities... capabilities are attenuable: `cap.Sql.read_only()`,
`cap.Http.limited_to("api.example.com")`") and the matching `docs/GAPS.md`
Tier 3 item (`cap.Http.limited_to(...)`). This document exists because that
claim was never actually implemented, and after three consecutive campaign
iterations (it109–it111) deferring the question in favor of more concretely
scoped work, this iteration commits to writing down what implementing it
would actually require — so the NEXT iteration that picks this up (if any)
starts from a real plan instead of re-deriving one.

## 1. What exists today (verified live, not assumed)

Read in full for this sketch: `src/effects.rs` (2538 lines) and the relevant
parts of `src/check.rs`'s contract-fulfillment checking.

- **Effects are a flat, hierarchical STRING namespace** — `io` (with
  sub-effects `io.fs`/`io.net`/`io.env`/`io.proc`/`io.time`) plus `ai`. There
  is no `db`/`net`/`gpu`/`unsafe` effect family as LANGUAGE.md's own examples
  show — those were the ORIGINAL vision naming, since narrowed and shipped as
  the `io.*` hierarchy (`docs/design/LANGUAGE.md` §12 Q3, **RESOLVED**).
- **Enforcement is 100% static and syntactic.** `effects.rs` builds a
  call-graph (`direct`/`edges` maps) and infers, via fixpoint, which
  builtin-carried effects (`builtin_effects`, e.g. `print` → `io`) each
  function's body can reach. `pub`/`expose` functions must declare every
  effect they use (`uses io`); private functions/handlers may stay implicit.
  There is no runtime token, object, or value involved anywhere in this
  process — it is purely "does this function's call graph reach an
  effectful builtin," checked once at compile time.
- **`requires` is a reserved word with NO grammar production.**
  `src/parser.rs`'s own reserved-identifier list includes `requires`
  (confirmed live via `grep`), but there is no parser rule that consumes it —
  the `app TodoApp { requires db: cap.Sql, http: cap.HttpServer ... }` syntax
  in LANGUAGE.md §1's own example does not parse today. It is aspirational
  vision text, not a designed-and-deferred feature.
- **There is no `cap` namespace, no capability type, and no runtime
  authority-passing value anywhere in the codebase** (confirmed via `grep`
  across `src/*.rs` at it109, re-confirmed here).
- **The closest EXISTING analog is contract-typed props + the K0264 effect
  budget**, both already fully shipped:
  - Dependency injection today is `prop store: KeyStore` where `KeyStore` is
    a `contract` — any component that `fulfills` it can be passed in
    (`examples/di.kupl`). A component gets exactly what's explicitly passed
    as a constructor prop; there is no ambient global lookup.
  - A `contract`'s own method signature can declare an effect ceiling:
    `expose fun get(id: Id) -> Option[T] uses io`. `check.rs`'s K0264 check
    then verifies every FULFILLING component's own exposed method doesn't
    exceed that ceiling (`src/check.rs`, the `covers_effect` check right
    after the K0263 signature match). This is already, in miniature, a
    STATIC capability ceiling: "any implementation of this contract method
    may use AT MOST this set of effects" — just expressed as effect names
    on a contract signature, not as a first-class runtime value a caller can
    narrow further.

## 2. The actual gap

Effects today answer "can this function's call graph reach a `print`/
`http_get` call at all" — a yes/no, all-or-nothing question per effect NAME.
They cannot answer "may this SPECIFIC call only reach `api.example.com`" or
"may this SPECIFIC call only READ, never WRITE." `uses io.net` grants
ambient, unqualified access to every `io.net`-tagged builtin the moment it's
declared; there is no way to hand a callee a NARROWED slice of that
authority. This is precisely the gap LANGUAGE.md's own vision names
(`cap.Http.limited_to(...)`) and precisely what a purely syntactic,
call-graph-based effect system cannot express by construction — attenuation
is fundamentally a VALUE-level concept (a specific object naming a specific
allowed scope), not a name-level one.

This matters most for the language's own stated purpose: verifying that
AI-generated code cannot exceed a declared effect is useful, but "this
function may use `io.net`" and "this function may only talk to
`api.example.com`" are very different guarantees for code nobody
necessarily read line-by-line before running.

## 3. Design sketch

### 3.1 Capabilities are opaque runtime values, never user-constructible

A new family of intrinsic runtime types — sketched here as `Cap.Net`,
`Cap.Fs`, `Cap.Env` (naming TBD; could also be `net.Cap`/`Net.Cap` depending
on how the type namespace is organized) — each an OPAQUE value with no
KUPL-visible constructor. This is the single most important invariant: if
user code could construct a capability from nothing (`Cap.Net.new()`), the
entire "no ambient authority" guarantee collapses immediately. Contrast with
`Json`, the ONE existing prelude ADT with public constructors
(`JObj`/`JArr`/...) — capabilities must NOT follow that precedent.

### 3.2 Root capabilities are seeded at exactly one place

The runtime seeds a fixed set of ROOT capability values only at the
composition root — the top-level `app`'s own construction, or an implicit
binding available to `fun main`. Every other component in the instance
graph only ever RECEIVES a capability (or an attenuated derivative of one)
through an ordinary constructor prop, exactly like a contract-typed
dependency does today. No component can reach outside its own prop list to
find one — this is what makes "capability in scope" a purely lexical,
audit-by-reading-the-props property, matching the vision text's "no ambient
authority" claim literally.

### 3.3 Attenuation is ordinary method calls, no new syntax

```kupl
app TodoApp {
    intent "..."
    prop net: Cap.Http                     // an ordinary prop, capability-typed

    let store = TodoStore(net.limited_to("api.example.com"))
}

component TodoStore {
    intent "..."
    prop net: Cap.Http                     // narrower than the caller's own `net`

    expose fun sync() uses io.net -> Result[Unit, Str] {
        http_get_with(net, "https://api.example.com/todos")   // see 3.4
    }
}
```

`cap.limited_to(host: Str) -> Cap.Http` / `cap.read_only() -> Cap.Sql` are
plain methods on the capability's own type, returning a NEW capability value
of the SAME underlying kind carrying a narrower scope. No `requires` keyword
needed at all — `prop` (already fully implemented, including contract-typed
props) already expresses "this component needs one of these, passed in
explicitly," which is exactly what a capability also needs. This is the
sketch's main scope-reduction finding: the vision text's own `requires`
clause syntax is not a prerequisite for capabilities-as-values; it could be
an independent, later syntactic-sugar layer over what `prop` already does.

### 3.4 Enforcement: additive, not a replacement for effects — pick ONE

Two shapes were considered for how `uses io.net` relates to capability
VALUES; only one is recommended.

**Option A — effects become sugar for implicit capability parameters.**
`uses io.net` on a signature would mean "an implicit `Cap.Net`-shaped
parameter is required and threaded through this call," collapsing effects
and capabilities into one mechanism, matching "effects are backed by
capabilities" literally. Rejected for now: this would require every one of
the ~40+ effectful builtins (`print`, `http_get`, `read_file`, ...) to
thread an implicit capability argument through the ENTIRE existing call
graph, and `effects.rs` is a 2538-line, extensively fuzzed and
production-hardened module — its own doc comment lists multiple "REAL bug
found+fixed" cases for the CURRENT purely-syntactic design alone. Adding
capability-threading multiplies that surface area (every call site, every
closure-capture edge case, every native/VM mirror) for a rewrite of an
already-correct, heavily-tested system, with a real risk of reintroducing
exactly the class of soundness gaps (PR-it706 in `check.rs`, cited above)
this campaign has spent significant effort closing.

**Option B — capabilities are a separate, additive, opt-in layer (RECOMMENDED).**
`uses io.net` stays EXACTLY as it is today — the existing, already-correct
static effect check, unchanged, zero regression risk. Capabilities are a
NEW, parallel mechanism: a component that wants attenuated access declares
a capability-typed prop and calls NEW builtin variants that accept a
capability argument explicitly (`http_get_with(cap, url)`, sketched
alongside the EXISTING effect-checked-only `http_get(url)`, which keeps
working unchanged for code that doesn't need attenuation). `uses io.net` is
still required on any function calling `http_get_with` — capabilities
NARROW what a granted effect can reach, they do not replace the boundary
-explicitness effects already enforce. This is incrementally adoptable
(existing code is entirely unaffected), matches this campaign's own
established staged-rollout discipline (`Char`/`Decimal`/`par{}` all landed
narrow-then-widened, never as an all-at-once rewrite of existing,
correct machinery), and is the recommended path if this is ever picked up.

### 3.5 Where does enforcement actually happen?

Runtime, not static, for v1: `http_get_with(cap, url)` checks `url`'s host
against `cap`'s own carried scope (a `HostAllowlist(Vec<String>)`-shaped
runtime field, one variant per capability kind) and returns `Err`/panics if
it's out of scope — the SAME "runtime value carries the constraint, checked
at the point of use" shape sized-int width-checking or `Rational`'s
denominator-positivity invariant already use elsewhere in this codebase.
Static verification (proving at compile time that a specific call site's
capability argument is provably within some declared bound) is a possible
future refinement, not a v1 requirement — mirrors how `[T: Ord]` bounded
generics (it103) started as a pure runtime-dispatch-unaffected type-checker
feature before any question of deeper static guarantees arose.

### 3.6 Engine coverage

Since a capability is an ordinary runtime `Value` variant (structurally
close to a `Contract`-typed component reference, which every engine already
handles), this should need no NEW cross-engine design work beyond what this
campaign has now done twice for a genuinely new `Value` variant (`Char`
it105/it106, `Decimal` it107/it108): a new `Value::Cap`/`KValue` tag, wired
through `type_name`/`Display`/equality, plus whatever builtins are added
(`_with`-suffixed variants, attenuation methods) following the SAME 6-file
pattern (`bytecode.rs`/`compile.rs`/`check.rs`/`interp.rs`/`vm.rs`/
`cgen.rs`) every prior builtin addition in this campaign has used. No reason
to anticipate this needs staging the way `Decimal`'s native port did —
capability VALUES carry no arbitrary-precision arithmetic, just a tag plus
a small enum payload.

## 4. Recommended first slice, if picked up

Ship exactly ONE capability kind first — `Cap.Net`/`http_get_with`/
`limited_to(host)` — rather than the full `Cap.Sql`/`Cap.Fs`/`Cap.Http`
family LANGUAGE.md's own examples show. Network access is the highest-value,
most security-relevant case for AI-generated code specifically (arbitrary
outbound requests are the most immediately dangerous failure mode), and a
single kind fully proves the pattern (opaque type, root seeding, attenuation
method, `_with` builtin variant, runtime scope check) before committing to
the shape for every other kind. This mirrors `par{}`'s own it99→it101
staged rollout and `sha256`/`hmac_sha256`'s own "ship the smallest complete
slice, generalize later" precedent.

## 5. Open questions this sketch does NOT resolve

- Exact type namespace/naming (`Cap.Net` vs `net.Cap` vs something else) —
  needs to fit whatever the eventual module/namespace story looks like.
- Whether `Cap.*` should be prelude-injected like `Json` (available with no
  import) or require an explicit `use` — leaning prelude-injected, to match
  `Json`'s own precedent, but not decided.
- Whether attenuation should be allowed to WIDEN (a bug, should be
  impossible by construction) — `limited_to`/`read_only`-style methods
  should be designed so every attenuation method's own return type can only
  narrow, never widen, the scope it's called on; this needs to be an
  explicit invariant the method implementations enforce, not just a
  naming convention.
- Whether a contract's own effect budget (K0264) should be extended to also
  express a capability-scope budget (e.g. "any fulfilling implementation's
  `net` prop must be `limited_to` this host or narrower") — a natural
  follow-on question once a first slice exists, not a v1 requirement.
- How this interacts with `ai fun tools [...]` — an AI-selected tool
  function that itself requires a capability-typed prop would need that
  capability already bound at DEFINITION time (props are supplied at
  construction, not at call time), which should already just work given
  the existing prop model, but wasn't verified live as part of this sketch.
