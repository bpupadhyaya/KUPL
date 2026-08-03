# Capabilities as attenuable values — design sketch + Cap.Net first slice

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

**UPDATE (it116): the recommended Cap.Net-only first slice (§4 below) is now
IMPLEMENTED** — `CapNet` (a flat type name, NOT the dotted `Cap.Net` this
sketch originally used; see the it116 correction in §5), `.limited_to(host)`,
and `http_get_with(cap, url)`, wired across all three engines
(interp/KVM/native), with real tests.

**UPDATE (it117): root-seeding is now ENFORCED.** `cap_net_root()` is
restricted (K0304) to a direct call inside the top-level `fun main`'s own
body only — not a top-level helper, not a component method/handler, and not
a closure literal even when textually written inside `fun main` (a closure
could be stored/passed elsewhere and called later, outside the composition-
root moment). This closes the LAST deliberate gap it116 left open:
`CapNet` is now a genuine "no ambient authority" security boundary for its
one shipped kind, not just tested engine plumbing. See §3.2/§5 below for
the mechanism.

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
- **CORRECTION (it113): `requires` DOES parse — it112's claim above was
  wrong, caught by live-testing rather than trusting a `grep` survey.**
  `src/parser.rs::parse_component_member`'s `Tok::KwProp | Tok::KwRequires
  =>` arm treats `requires` as a byte-for-byte syntactic ALIAS for `prop`:
  the same comma-separated `name: ty (= default)?` list, pushed into the
  exact same `ComponentDecl.props: Vec<PropDecl>` field, with nothing
  anywhere recording which keyword spelled a given prop. This has been true
  since the project's very first commit (`git log -S"KwRequires"` →
  `9729904`), not a later, half-finished feature. Live-confirmed: a
  `component` with `requires db: Int, tag: Str` compiles and runs
  identically to the equivalent `prop` declaration. The ONE real restriction
  found is unrelated to `requires` itself — `app` blocks (unlike plain
  `component` blocks) reject ANY props at all ("v0.1 apps must be
  self-contained"), which is why LANGUAGE.md §1's own example wraps its
  `requires` clause in an `app`; the same clause in a `component` works
  today. `requires` has ZERO distinct semantics from `prop` anywhere in the
  compiler (parser, `check.rs`, interp/vm/cgen) — pure alternate spelling,
  not a capability-aware keyword. This doesn't change the actual gap (§2 below
  still holds: no `cap` namespace/type/runtime value exists), but it
  STRENGTHENS §3.3's own scope-reduction finding — the `requires` syntax
  doesn't need to be treated as a stand-in for `prop`, it already IS exactly
  `prop`, today, with no grammar work required at all.
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

**Correction (it113):** the original draft of this section assumed a root
capability could be an ordinary `prop`/`requires` field on the top-level
`app`, supplied like any constructor argument. Live-tested and found FALSE:
`kupl run`/`kupl run --vm`/`kupl native` all refuse to construct a top-level
`app` that declares ANY props at all — `error: app \`X\` requires props
(...) — v0.1 apps must be self-contained` (`src/run.rs`/`src/vm.rs`/
`src/cgen.rs`, each independently enforcing the same rule at the "how do we
invoke the CLI entry point with zero args" step). This is a RUNTIME/
invocation-time restriction, not a parser/compile-time ban — an app CAN
declare props and compiles fine either way — but it means a real root
capability can never arrive as an ordinary CLI-supplied constructor
argument, since `kupl run file.kupl` has no mechanism to pass one in.

So root-seeding cannot use the `app`-prop path this sketch originally
assumed; it needs the OTHER option already named above: an implicit
binding the runtime injects directly into `fun main`'s own scope (or into
the top-level `app`'s own construction internally, NOT via its declared
prop list) — conceptually a prelude-like value the runtime constructs once
per process and hands to the entry point, the same way `env_args()`-style
runtime-provided values would need to work if KUPL ever grows a CLI-args
builtin. This still preserves the "purely lexical, audit-by-reading-the-
props" property for every component BELOW the entry point — only `fun
main`/the top-level app's own body is special, everything it hands
downward is an ordinary prop from there on.

**IMPLEMENTED (it117):** rather than an implicit extra parameter (which
would need touching `fun main`'s own call convention across all 3 engines'
entry-point dispatch), `cap_net_root()` stays an ORDINARY builtin call, but
`check.rs` now statically restricts WHERE it may be called from: a new
`Ctx::in_main_top_level: bool` field is threaded through the existing
per-function body-checking recursion (set `true` only when checking the
top-level `fun main`'s own `Ctx`, `false` everywhere else — including a
FRESH `false` for any closure literal's own body, the SAME save-fresh/
restore pattern `loop_depth`/`in_handler` already use for their own
per-closure scoping, PR-it948's precedent). A call to `cap_net_root()` with
`in_main_top_level == false` is rejected with `K0304`. This is exactly the
"purely lexical, audit-by-reading-the-props" property described above,
now actually enforced: no function anywhere except `fun main`'s own direct
body can ever independently obtain a capability — every other component
only ever RECEIVES one through an ordinary prop.

### 3.3 Attenuation is ordinary method calls, no new syntax

```kupl
app TodoApp {
    intent "..."
    // `net` is NOT a declared prop (see 3.2's correction — a top-level
    // app can't take CLI-supplied props) -- it's the runtime-injected
    // root capability, implicitly in scope in the app's own body.

    let store = TodoStore(net.limited_to("api.example.com"))
}

component TodoStore {
    intent "..."
    requires net: Cap.Http                 // narrower than the caller's own `net`

    expose fun sync() uses io.net -> Result[Unit, Str] {
        http_get_with(net, "https://api.example.com/todos")   // see 3.4
    }
}
```

`cap.limited_to(host: Str) -> Cap.Http` / `cap.read_only() -> Cap.Sql` are
plain methods on the capability's own type, returning a NEW capability value
of the SAME underlying kind carrying a narrower scope. **Correction (it113):**
the previous draft claimed `requires` had no grammar production and framed
"no new `requires` keyword needed" as this sketch's scope-reduction finding
— live-testing found `requires` already parses today as a full alias for
`prop` (see §1's own correction above), so the ACTUAL finding is stronger
than originally stated: every non-root component in the graph (like
`TodoStore` above) needs literally ZERO new syntax, today's `requires`/`prop`
already express "this component needs one of these, passed in explicitly."
The only genuinely new piece is the root-seeding mechanism at the entry
point itself (§3.2), which is runtime injection, not a prop at all.

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

## 4. Recommended first slice, if picked up — **IMPLEMENTED (it116)**

Ship exactly ONE capability kind first — `CapNet`/`http_get_with`/
`limited_to(host)` — rather than the full `Cap.Sql`/`Cap.Fs`/`Cap.Http`
family LANGUAGE.md's own examples show. Network access is the highest-value,
most security-relevant case for AI-generated code specifically (arbitrary
outbound requests are the most immediately dangerous failure mode), and a
single kind fully proves the pattern (opaque type, root seeding, attenuation
method, `_with` builtin variant, runtime scope check) before committing to
the shape for every other kind. This mirrors `par{}`'s own it99→it101
staged rollout and `sha256`/`hmac_sha256`'s own "ship the smallest complete
slice, generalize later" precedent.

**What shipped:** `Value::CapNet`/`Ty::CapNet` across all 6 files
(`bytecode.rs`/`compile.rs`/`check.rs`/`interp.rs`/`vm.rs`/`cgen.rs`),
`.limited_to(host: Str) -> CapNet` (via the SHARED `shared_method`
dispatch, so interp/KVM get it from ONE implementation; `cgen.rs`'s
`k_method` mirrors it), `http_get_with(cap, url) -> Result[Str, Str]`
(host-checked via a `url_host` helper that MUST stay byte-identical
between `interp.rs` and `cgen.rs`'s own `k_url_host` C mirror — both
simple string slicing, no full URL parser), and `cap_net_root() -> CapNet`
(the unrestricted root — **call-site-restricted since it117**, see §3.2).
Unlike `Char`/`Decimal` (each staged native into a
FOLLOW-UP iteration), native support shipped in the SAME iteration —
`http_get`/`http_post` already shell out to the system `curl` binary on
every engine (confirmed live, not the hand-rolled raw-socket client this
sketch's own §3.6 might have implied), so the host-check needed only
string-level URL parsing, no new C networking code.

A REAL cross-engine bug caught before it shipped: `types.rs`'s own
`Unifier::unify` had NO arm for `(Ty::CapNet, Ty::CapNet)`, silently
falling through to its catch-all mismatch case — `Ty::CapNet` and a
hypothetical `Ty::Named("CapNet", [])` would have printed IDENTICALLY in
a `K0200` diagnostic ("expected CapNet, found CapNet"), which is exactly
what surfaced it: unifying two values that were BOTH genuinely
`Ty::CapNet` still failed, live-caught via a debug print showing the
`Debug`-formatted (not just `Display`-formatted) type before fixing it.
The exact same class of bug as PR-it1180 (`value.rs`'s own `Value::Fun`
equality gap, documented in that file) — a new variant added to a type
needs EVERY relevant match updated, not just the ones a compiler error
happens to force.

## 5. Open questions this sketch does NOT resolve

- ~~Exact type namespace/naming~~ **RESOLVED (it116): a FLAT name, `CapNet`,
  not `Cap.Net`.** it114 discovered a real blocker beyond what this sketch
  anticipated: `parser.rs::parse_ty_inner` only accepts a plain `Ident` for a
  type reference — there is NO dotted-type-path grammar anywhere in KUPL
  today, and it115 confirmed the module/`use` system has no independent need
  for one either (it flattens every imported file's declarations into one
  global namespace). Rather than build dotted-type-path parsing with no
  consumer besides this one feature, `CapNet` follows `BigInt`/`Decimal`'s
  own existing flat-name precedent — zero grammar work needed.
- ~~Whether `Cap.*` should be prelude-injected~~ **RESOLVED as moot by the
  flat-name decision above**: `CapNet` is a builtin type name recognized
  directly by `check.rs::resolve_ty` (like `Int`/`Str`/`Decimal`), needing no
  `use` and no prelude-injection mechanism at all.
- ~~Whether attenuation should be allowed to WIDEN~~ **RESOLVED (it116): no.**
  `.limited_to(host)` on an unrestricted (root) capability narrows to
  `Some(host)`; called again with the SAME host it's a no-op success;
  called with a DIFFERENT host on an already-limited capability it panics
  ("cannot widen a capability already limited to `X` to a different host
  `Y`") — enforced at the point of use (`interp.rs`'s `shared_method`,
  `cgen.rs`'s `k_method`), not just a naming convention.
- Whether a contract's own effect budget (K0264) should be extended to also
  express a capability-scope budget (e.g. "any fulfilling implementation's
  `net` prop must be `limited_to` this host or narrower") — a natural
  follow-on question once a first slice exists, not a v1 requirement.
  STILL OPEN as of it116.
- How this interacts with `ai fun tools [...]` — an AI-selected tool
  function that itself requires a capability-typed prop would need that
  capability already bound at DEFINITION time (props are supplied at
  construction, not at call time), which should already just work given
  the existing prop model, but wasn't verified live as part of this sketch.
  STILL OPEN as of it116.
- ~~Root-seeding enforcement~~ **RESOLVED (it117): see §3.2.**
  `cap_net_root()` is now restricted to `fun main`'s own top-level body via
  a new `K0304` static check — no runtime code changed, only `check.rs`.
