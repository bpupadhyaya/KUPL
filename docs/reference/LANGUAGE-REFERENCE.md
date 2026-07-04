# KUPL Language Reference Manual

**Version:** 1.0-alpha · **Status:** normative for the implemented language

This manual describes the KUPL language *as implemented* by the reference
toolchain in this repository. Features that exist only in the design proposal
(kernels/`at()`, capabilities-as-values, `par`, timers, generics with bounds,
the system/hardware tiers) are covered in [`../design/LANGUAGE.md`](../design/LANGUAGE.md)
and are marked **[design]** where mentioned. Everything else on this page runs
today, identically, on all four execution engines.

Companion documents:
[Standard Library](STDLIB.md) · [CLI](CLI.md) · [Diagnostics index](DIAGNOSTICS.md)

---

## 1. Source structure

- Files use the `.kupl` extension and are UTF-8.
- A file contains **items**: `fun`, `type`, `component`, `app`, `contract`,
  plus `use` and `module` declarations at any top-level position.
- **Statements end at a newline.** A statement continues onto the next line
  when the line ends with an operator, comma, dot, or open bracket, and
  newlines inside `( … )` and `[ … ]` are never significant.
  Semicolons are not part of the language.
- Comments: `// line` and `/* block */` (block comments nest).
- Canonical formatting is defined by `kupl fmt` (4-space indent, fixed member
  order inside components). Any two programs with the same AST format
  identically.

### Multi-file programs

```kupl
use util          // loads util.kupl        (same directory as the entry file)
use lib.stats     // loads lib/stats.kupl
```

`use` merges the target file's items into the program (one flat namespace in
v1.0-alpha — duplicate item names across files are an error). Loading is
recursive, cycle-safe, and deduplicated. `module` headers are accepted and
currently derive identity from the file path.

## 2. Lexical structure

### Keywords (reserved)

```
app async await break component contract continue else emit example expect
expose false fn for fun if in intent let match module new on out par prop pub
requires return send start state stop supervise test true type use uses var
while wire
```

### Contextual keywords

Valid identifiers everywhere except in their clause:
`fulfills` `law` `restart` `on_failure` `never` `forall` `every` `after`
`advance` `ai` `tools` `model`

### Identifiers

Start with a letter or `_`, continue with letters, digits, `_`. Non-ASCII
letters are permitted. `_` alone is the wildcard pattern.

### Literals

| Kind | Examples | Notes |
|---|---|---|
| Int | `42`, `1_000_000`, `-7` | 64-bit signed; overflow **panics** (never wraps) |
| Float | `3.14`, `1.5e3` | IEEE-754 f64; `1..5` is a range, `1.5` a float |
| Bool | `true`, `false` | |
| Str | `"hi"`, `"n = {x + 1}"` | UTF-8, immutable; `{expr}` interpolates (any type, rendered with Display); escapes: `\n \t \r \\ \" \{ \} \0` |
| Unit | `()` | the no-value value |
| List | `[1, 2, 3]` | homogeneous |
| Range | `0..10`, `0..=10` | Int bounds; exclusive / inclusive |

### Operators and precedence (loosest → tightest)

| Level | Operators | Notes |
|---|---|---|
| 1 | `\|>` | pipeline: `x \|> f` ≡ `f(x)`; `x \|> f(a)` ≡ `f(x, a)` (canonicalized at parse time) |
| 2 | `\|\|` | short-circuit |
| 3 | `&&` | short-circuit |
| 4 | `==` `!=` | structural equality, any type |
| 5 | `<` `<=` `>` `>=` | Int, Float, Str |
| 6 | `..` `..=` | ranges |
| 7 | `+` `-` | `+` also concatenates Str; both elementwise on Tensor |
| 8 | `*` `/` `%` | `*` `/` elementwise on Tensor; `/ 0` and `% 0` panic on Int |
| 9 | unary `-` `!` `await` | |
| 10 | postfix `f(args)` `.field` `.method(args)` `?` | |

## 3. Types

| Type | Values | Notes |
|---|---|---|
| `Int` | 64-bit signed integers | all arithmetic is overflow-checked |
| `Float` | IEEE-754 f64 | |
| `Bool` | `true`/`false` | conditions must be Bool (no truthiness) |
| `Str` | immutable UTF-8 | `.len()` counts characters |
| `Unit` | `()` | return type of value-less functions |
| `Event` | payload-less port messages | only meaningful as a port type |
| `List[T]` | immutable lists | methods return new lists |
| `Option[T]` | `Some(v)` / `None` | there is no null |
| `Result[T, E]` | `Ok(v)` / `Err(e)` | errors are values; `?` propagates |
| `Map[K, V]` | insertion-ordered immutable maps | order-insensitive equality |
| `Set[T]` | insertion-ordered immutable sets | order-insensitive equality |
| `Tensor` | rank-1 f64 tensors | native numeric kernels; dtype/shape params **[design]** |
| `Range` | `a..b` | iterable in `for` |
| `fn(T1, …) -> R` | functions and lambdas | |
| *user types* | see below | nominal |
| *component names* | instance references | received via props, called via exposes |

### User type declarations

```kupl
type Shape = Circle(r: Float) | Rect(w: Float, h: Float)   // union (ADT)
type User  = { name: Str, age: Int }                        // record
type UserId = new Str                                       // newtype
```

- A **record** is a single-variant type constructed with named or positional
  arguments: `User(name: "Ada", age: 36)`. Its fields are read with `.name`.
- A **union** is matched with `match`; field access requires `match` (K0231).
- A **newtype** wraps one value (field `value`); it is a distinct type — an
  intentional bulwark against ID-mixup bugs.
- No inheritance. No implicit conversions anywhere (use `.to_float()` etc.).
- **Recursive** ADTs are supported: a variant may carry its own type
  (`type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)`), directly or through
  `List`/`Map`.

### Prelude

One ADT is provided without an import — the built-in **`Json`** type, for
structured data (see STDLIB.md → Json):

```kupl
type Json = JNull | JBool(b: Bool) | JNum(n: Float) | JStr(s: Str)
          | JArr(items: List[Json]) | JObj(fields: Map[Str, Json])
```

Use it with `json_parse` / `json_stringify` and ordinary `match`.

### Inference rules

Types are inferred inside bodies (Hindley-Milner-style unification, checked
bidirectionally so lambda parameters take their types from context). **All
public boundaries must be written out**: function parameters and returns,
ports, props, expose signatures.

### Generic functions

```kupl
fun identity[T](x: T) -> T { x }
fun apply2[T, U](f: fn(T) -> U, a: T, b: T) -> List[U] { [f(a), f(b)] }
```

Type parameters in `[...]` are universally quantified and instantiated fresh
at every call site — `identity(42)`, `identity("s")`, and `identity(true)` in
one program all check. Bounds (`[T: Ord]`) are **[design]**. Type parameters
on `type` declarations are **[design]**.

## 4. Expressions

```kupl
if cond { a } else { b }            // expression; both arms must agree in type
match v {                           // expression; must be exhaustive
    Circle(r) => 3.14 * r * r
    Rect(w, h) => w * h
}
fn x { x * 2 }                      // lambda; parameter types from context
fn (x: Int, y: Int) { x + y }       // or annotated
xs.map(fn x { x + 1 })              // method call
user.name                           // field access (records)
half(n)?                            // Result propagation (functions only)
user with age: 37, name: "Ada L."   // record update: new value, fields replaced
"total: {a + b}"                    // interpolation
{ let t = a; t * t }                // block expression (value = last expr)
par { f(a)  g(b)  h(c) }            // structured fork-join → List of results
```

- `match` exhaustiveness is compile-checked: all variants of a union,
  `Some`/`None`, `Ok`/`Err`, `true`/`false` — or a catch-all `_`/binding arm.
  Unbounded scrutinees (Int, Str) require a catch-all.
- `expr?` requires `expr : Result[T, E]` and an enclosing function returning
  `Result[_, E]`; on `Err(e)` the function returns early with that error. Not
  allowed in handlers (K0237) — handle the Result with `match` there.
- `await expr` is accepted and currently evaluates `expr` directly (expose
  calls are synchronous in v1.0-alpha; true asynchrony is **[design]**).

### 4.1 Concurrency — `par` (structured fork-join)

`par { branch1  branch2  … }` evaluates a set of **independent** branches and
joins their results into a `List[T]` (all branches the same type `T`, one per
line or comma-separated):

```kupl
let sizes = par {
    measure(a)
    measure(b)
    measure(c)
}                                    // List of three results, in branch order
```

- Branches are **independent** — no value flows between them (each is a
  self-contained expression evaluated from the same enclosing scope), so the
  result never depends on evaluation order. Because KUPL values are immutable
  and component state is actor-isolated, `par` cannot introduce a data race
  by construction.
- Branch types must agree (K0200 otherwise); the result is `List[T]` in branch
  order.
- **Execution is deterministic** in v1.0-alpha: branches run in order, so
  `example`/`law`/`kupl test` results are fully reproducible. A real
  multi-threaded scheduler is the next step and is designed to be
  **semantics-preserving** — it will not change results, only run the branches
  on separate threads. (Async I/O and a preemptive scheduler are **[design]**.)
- The motivating use is **fanning out independent work** — most compellingly a
  batch of independent `ai fun` calls: `par { classify(x)  classify(y) }` runs
  the LLM calls as one parallel batch. Runs identically on the interpreter and
  the KVM. See `examples/parallel.kupl`.

**Parallel iteration.** `par { … }` handles a *fixed* set of branches; for a
*dynamic* collection, use the parallel List methods `par_map` / `par_filter` /
`par_each`:

```kupl
let scored = reviews.par_map(fn r { classify(r) })   // fan out over any list
let good   = items.par_filter(fn x { keep(x) })
```

They carry the same guarantees as `par`: each element is processed
independently, and execution is deterministic (results in input order), so
tests stay reproducible. `par_map` is semantically identical to `map` today —
the `par_` prefix marks the work as parallelizable for a future scheduler,
exactly as with `par`. `par_each` applies the function for its effects and
returns `()`. All three run identically on the interpreter, KVM, and native
backends.

### Patterns

`_` wildcard · `name` binding · Int/Bool/Str literals ·
`Ctor(p1, p2, …)` with nested patterns · nullary `Ctor`.

## 5. Statements

```kupl
let x = expr            // immutable binding (type annotation optional)
var n: Int = 0          // mutable binding
n = expr                // assignment; also += -= *= /=
expr                    // expression statement (block value if last)
return expr             // early return
if / match              // as statements
while cond { … }        // break / continue supported
for i in 0..10 { … }    // over Range or List
expect cond             // runtime assertion; panics if not true
emit port(value)        // components only: publish on an out port
```

## 6. Functions, purity, and effects

```kupl
fun helper(msg: Str) {              // private: effects inferred
    print(msg)
}

pub fun broadcast(msg: Str) uses io {   // public: effects MUST be declared
    helper(msg)
}
```

- Functions are **pure by default**. Effects are inferred transitively over
  the call graph (fixpoint, so recursion converges).
- **Boundary explicitness rule:** every `pub` function and every `expose`
  must declare all effects it uses (K0301). A declared-but-unused effect
  warns (K0302).
- Effect names are hierarchical: declaring `db` covers `db.read`; declaring
  `db.read` does not cover `db.write`. Built-in effectful operations in
  v1.0-alpha: `print` / `eprint` (`io`); the file builtins `read_file` /
  `write_file` / `append_file` / `delete_file` / `file_exists` (`io.fs`); and
  `args` / `env_var` (`io.env`). The sub-effects mean `uses io` covers all of
  them, while `uses io.fs` / `uses io.env` are the precise capabilities.
  Capability *values* — attenuable, passable file/network handles — are
  **[design]**.
- Recursion (incl. mutual) is fully supported. Functions are first-class:
  pass them by name or as lambdas; calls through variables are supported
  (their effects are not tracked in v1.0-alpha — documented limitation).

### 6.1 AI-native functions (`ai fun`)

An `ai fun` is a **typed prompt function**: the body is an intent, not code.
Calling it sends the intent plus the rendered arguments to an LLM provider
and converts the response to the declared return type.

```kupl
type Sentiment = { label: Str, score: Float }

ai fun haiku(topic: Str) -> Str {
    intent "Write a haiku about the topic."
}

ai fun classify(review: Str) -> Result[Sentiment, Str] {
    intent "Classify the sentiment with a confidence between 0 and 1."
    model "claude-opus-4-8"          // optional per-function override
}
```

Rules:

- The **return type drives structured output**. `-> Str` returns the model's
  text. Any other supported type is requested as JSON (with a machine-derived
  JSON Schema) and parsed into a real KUPL value. Supported: `Str`, `Int`,
  `Float`, `Bool`, `List[T]`, `Option[T]`, and record types whose fields are
  supported (K0271 otherwise; a return type is required — K0270).
- Declaring `-> Result[T, Str]` makes the call **total**: provider failures,
  refusals, and malformed responses come back as `Err(message)`. Any other
  return type panics on failure (supervision applies, §9).
- An `ai fun` performs the **`ai` effect**; the keyword itself is the
  boundary declaration. Callers are checked as usual: a `pub fun` that calls
  one must declare `uses ai`.
- The **intent is an interpolated string** evaluated in the parameter scope at
  call time: `intent "Summarize {text} in one line."` substitutes the argument.
  Arguments are also appended to the prompt as `name: value` lines (Display
  form), so an intent that mentions no parameters still receives them.
- `ai fun`s are declared at the top level (components call them freely) and
  cannot be generic. Bodies allow exactly `intent "…"` and an optional
  `model "…"` (K0119).

**Providers** are selected at run time — the program text stays portable:

| `KUPL_AI_PROVIDER` | Endpoint | Auth / model |
|---|---|---|
| `anthropic` (default) | Anthropic Messages API | `ANTHROPIC_API_KEY`; model `claude-opus-4-8` unless overridden; structured output uses native JSON-schema enforcement |
| `openai` | any OpenAI-compatible `/v1/chat/completions` (`KUPL_AI_BASE_URL`) | `OPENAI_API_KEY`; `KUPL_AI_MODEL` required |
| `ollama` | local OpenAI-compatible endpoint (default `http://localhost:11434`) | no key; `KUPL_AI_MODEL` required |
| `mock` | none — deterministic | response text from `KUPL_AI_MOCK_<FUN_NAME>` or `KUPL_AI_MOCK` |

If `KUPL_AI_MOCK`/`KUPL_AI_MOCK_<FUN_NAME>` is set, the mock provider is used
regardless of `KUPL_AI_PROVIDER` — this is how `ai fun`s are tested: examples
and differential tests run byte-identical on every engine with no network.
For structured shapes the mock text (and any provider's reply) may be either
the documented wire form `{"value": <payload>}` or the bare payload; markdown
code fences are stripped. `KUPL_AI_MODEL` overrides the default model for any
provider; a `model "…"` clause in the function wins over both.

`ai fun`s run on the interpreter, the KVM, and inside `.kx`/bundles. The
native backend rejects programs containing them with a clear error (planned).

### 6.2 Tool use (`ai fun … tools [f, g]`)

An `ai fun` can let the model **call KUPL functions** while it produces the
answer. `tools [f, g]` names top-level functions the model may invoke; the
runtime drives the model↔tool loop — converting the model's JSON arguments to
typed KUPL values, running the function, and converting the result back —
until the model returns a final answer of the declared return type.

```kupl
fun add(a: Int, b: Int) -> Int { a + b }
fun weather(city: Str) -> Str { "sunny, 21C in {city}" }

ai fun assist(question: Str) -> Str tools [add, weather] {
    intent "Answer the question. Use add for arithmetic and weather for conditions."
}
```

Rules:

- Each tool must be a **monomorphic, non-ai top-level function** whose
  parameter and return types are supported structured-output shapes
  (§6.1) — otherwise K0272. The function's signature becomes the tool's
  JSON Schema; its name and rendered signature become the tool description.
- The loop is bounded (8 rounds) so a misbehaving model cannot spin forever.
- A panic inside a tool surfaces to the ai fun as a failure: captured as
  `Err` if the ai fun returns `Result[T, Str]`, otherwise it panics.
- Tool effects are **not** statically propagated to the ai fun in
  v1.0-alpha (a documented limitation, like calls through variables).

The mock provider scripts the loop deterministically: `KUPL_AI_MOCK_<FUN>` is
a JSON **array of rounds**, each either `{"tool": name, "input": {…}}` or
`{"final": <payload>}`. This runs the full agent loop with no network, so
tool-using ai funs are differentially tested on every engine. Real providers
use native tool calling (Anthropic `tools`/`tool_use`/`tool_result`;
OpenAI-compatible `tools`/`tool_calls`/`role:"tool"`).

The `echo` provider (`KUPL_AI_PROVIDER=echo`) returns the composed prompt
verbatim without any network call — handy for seeing exactly what an ai fun,
including its resolved intent, would send.

### 6.3 Agent components

An **agent** is just a component that keeps conversation state and calls
ai funs from its handlers/exposes — no special construct. The component is the
memory: each turn appends to state, so later turns carry earlier context.

```kupl
fun add(a: Int, b: Int) -> Int { a + b }

ai fun reply(history: List[Str], msg: Str) -> Str tools [add] {
    intent "Conversation so far: {history}. Reply to: {msg}"
}

component Assistant {
    intent "A stateful chat assistant that remembers the conversation."
    state history: List[Str] = []

    expose fun ask(msg: Str) uses ai -> Str {
        let answer = reply(history, msg)
        history = history.push("user: {msg}").push("assistant: {answer}")
        answer
    }
}
```

`Assistant().ask(...)` is a synchronous request/response call; state persists
across calls on the instance. See `examples/agent_component.kupl`.

Note the effects limitation: effects do **not** propagate across an expose
call (`bot.ask(...)` is a method call on an instance), so a caller of `ask`
is not statically required to declare `ai` — the same v1.0-alpha limitation
that applies to calls through variables. The `ai` effect is still required and
checked on `ask` itself.

## 7. Components

The component is the universal unit. Every instance is an isolated actor:
private state, a mailbox, handlers that run to completion one at a time.

```kupl
component TodoStore {
    intent "Holds todos; exposes queries; accepts new items."   // 1. intent

    prop capacity: Int = 100                                    // 2. props
    in  add: Str                                                // 3. in ports
    out changed: Int                                            //    out ports
    state todos: List[Todo] = []                                // 4. state

    on start { … }                                              // 5. handlers
    on add(title) { … emit changed(todos.len()) }

    expose fun all() -> List[Todo] { todos }                    // 6. exposes

    example {                                                   // 7. examples
        send add("milk")
        expect changed == 1
    }
}
```

Member kinds (the formatter enforces this order):

| Member | Meaning |
|---|---|
| `intent "…"` | required natural-language purpose (missing → warning K0300) |
| `prop name: T [= default]` | instantiation-time configuration, immutable; `requires` is accepted as a synonym. `T` may be a contract type — see §8.2 (dependency injection) |
| `in name: T` / `out name: T` | typed ports; `Event` = no payload |
| `state name[: T] = init` | private mutable state; invisible outside |
| `let child = Comp(args)` | child instance (positional or `name:` args against props) |
| `wire a.out -> b.in` | connect children's ports (types must match) |
| `supervise child restart on_failure` | see §9 |
| `on port(payload) { … }` | handler; `on start` / `on stop` lifecycle |
| `on every <dur> { … }` / `on after <dur> { … }` | timer handlers (no payload) — see §7.1 |
| `expose fun …` | synchronous request/response interface |
| `fun …` (private) | component-local helper; sees props/state/children; callable from handlers, exposes, and other component functions |
| `example { … }` | executable spec: `send port(v)` steps + `expect` over out-port values |

Semantics:

- `emit port(v)` records the value and enqueues one message per wire; with no
  wires attached, `kupl run` prints `Comp.port = v` (observable output).
- Message delivery is FIFO and deterministic; handlers on one instance never
  interleave.
- An `app` is a component that is also the entry point. `kupl run` instantiates
  the first `app`, delivers `on start` to every instance in creation order,
  then drains the queue to quiescence.
- Expose calls (`store.all()`) are synchronous method calls on the instance.
- Component state cannot leave the component except through emitted messages
  and expose return values.

### 7.1 Timers and the virtual clock

Components do periodic and delayed work with timer handlers:

```kupl
component Ticker {
    intent "Emits a rising tick on a recurring timer."
    out tick: Int
    state n: Int = 0

    on every 5s { n += 1  emit tick(n) }    // recurring
    on after 2s { emit tick(0) }            // one-shot

    example {
        advance 12s        // fires the recurring timer at 5s and 10s
        expect tick == 2
    }
}
```

- **Durations** are an integer and a unit: `ms`, `s`, `m`, `h`
  (`100ms`, `5s`, `2m`, `1h`). Timer handlers take **no payload** (like
  `on start`); a non-positive duration is an error (K0266).
- **Time is a virtual clock, never wall-clock.** It only moves when advanced
  explicitly, which makes timer behavior fully deterministic and reproducible.
  In `example` blocks, the **`advance <dur>`** step moves the clock forward,
  firing every due timer in time order (ties broken by instance, then
  declaration order). A recurring timer fires once per interval crossed within
  a single `advance`; a one-shot fires at most once.
- Timers are armed at `on start` (relative to the current time) and re-armed on
  a supervision restart.
- `kupl run` advances the clock **automatically but bounded** (up to 100 timer
  firings) so an app with a recurring timer yields finite, deterministic output
  rather than running forever.
- Timers run identically on the interpreter and the KVM (differentially
  tested). Like all component features, they require `kupl run`/`--vm`/`bundle`
  — the native backend does not compile components. See `examples/timers.kupl`.

## 8. Contracts and laws

```kupl
contract KeyStore {
    intent "Durable keyed storage for string values."

    expose fun put(key: Str, value: Str) -> Bool
    expose fun get(key: Str) -> Option[Str]

    law "put then get returns the value" {
        put("k", "v")
        expect get("k") == Some("v")
    }
}

component MemoryStore fulfills KeyStore { … }
```

- `component X fulfills C` is verified at compile time: every contract
  signature must be exposed with the exact type (K0262/K0263) and effects
  within the contract's budget (K0264).
- Every `law` runs against every fulfilling component under `kupl test`, with
  the contract's functions bound to a live instance.

### 8.1 Property-based testing (`forall`)

A `forall` runs its body over many **generated** values instead of one
concrete scenario:

```kupl
law "reverse is its own inverse" {          // top-level, free-standing test
    forall xs: List[Int] {
        expect xs.reverse().reverse() == xs
    }
}

law "addition commutes" {
    forall a: Int, b: Int {                 // multiple binders
        expect a + b == b + a
    }
}
```

- Generation is **deterministic** (a fixed seed), so a `forall` passes or fails
  identically on every machine and run — reproducible and CI-friendly. Each
  `forall` runs 100 cases.
- On failure the runner **shrinks** the counterexample toward a minimal
  falsifying case and reports the binding, e.g.
  `property failed for n = 50`. A panic in the body (e.g. a division by zero)
  is also a falsifying case and is reported with the offending input.
- Generators cover `Int`, `Bool`, `Float`, `Str`, `List[T]`, `Option[T]`, and
  record/ADT types (fields generated recursively). Generated integers are
  bounded to ±1e6 so ordinary arithmetic in a property stays inside checked
  `Int` — test boundary/overflow behavior with explicit concrete `expect`s.
- A **top-level `law "name" { … }`** is a free-standing test (property or
  concrete) that `kupl test` runs alongside component `example` blocks and
  contract laws. `forall` may also appear inside contract laws and any block.
- `forall` runs on the interpreter under `kupl test`; it is not compiled to the
  KVM (K0804 if used in a function compiled with `--vm`/`native`). See
  `examples/properties.kupl`.

### 8.2 Contract types (dependency injection)

A **contract name is a type**. A component can declare a prop (or a function
parameter) of a contract type; **any component that fulfills the contract is
assignable to it**, and calls on the value dispatch dynamically through the
contract's exposed functions. This is dependency injection: a consumer depends
on the interface, not on a concrete implementation.

```kupl
component Cache {
    intent "Reads through to any injected KeyStore."
    prop store: KeyStore                       // any component fulfilling KeyStore

    expose fun recall(key: Str) -> Str {
        match store.get(key) { Some(v) => v, None => "<miss>" }
    }
}

let a = Cache(store: MemStore())               // inject one implementation
let b = Cache(store: LoudStore())              // …or another — same consumer
```

- A contract type is accepted anywhere a type is: props, `let`/`var`
  annotations, and function/expose parameters.
- Passing a component that does **not** fulfill the contract is a type error
  (K0200); calling a function not in the contract is K0247.
- Dispatch is dynamic and runs identically on the interpreter and the KVM (the
  value is a component instance; the method is resolved by name at the call).
  Contract types compose with the native backend's existing component
  limitation (native uses `kupl bundle`). See `examples/di.kupl`.

## 9. Failure and supervision

- `panic("msg")`, failed `expect`, integer overflow, division by zero, and
  out-of-range access all **panic**.
- An unsupervised panic in a handler terminates the program with a rendered
  diagnostic (exit code 101).
- With `supervise child restart on_failure`, a panic in that child's handlers
  instead: prints `[supervise] Comp restarted after panic: …` to stderr,
  resets the child's `state` fields to their initial values (props, children,
  and wiring are preserved), re-runs `on start`, and the app continues.
- `restart never` documents the escalation default explicitly.

## 10. Numerics and equality (normative)

- `Int` + `Int` overflow → panic `integer overflow in addition` (same for
  `-`, `*`, negation, `abs`, `sum`). `Int / 0` and `Int % 0` panic.
- `Float` follows IEEE-754; division by zero yields `inf`/`nan` (no panic).
- `==`/`!=` are structural for every type; `<` `<=` `>` `>=` are defined for
  Int, Float, and Str (lexicographic by bytes).
- Display of floats uses the shortest representation that round-trips
  (`3.5`, `0.30000000000000004`); whole floats show one decimal (`12.0`).
  All engines — including native machine code — format identically.

## 11. Execution modes

| Mode | Invocation | Coverage |
|---|---|---|
| REPL | `kupl repl` | expressions, definitions, live redefinition |
| Interpreter | `kupl run` | everything (reference semantics) |
| KVM bytecode VM | `kupl run --vm`, `.kx`, `bundle` | everything |
| Native (C) | `kupl native` | `fun main` programs (components and `ai fun` **[design]** for native) |

The interpreter defines the semantics; the VM and native backend are held to
it by differential tests. Known intentional VM/native limits: assignment to a
lambda-captured outer `var` is a compile error on the KVM (K0803) — captures
are by value; component state accessed from lambdas is live on all engines.

## 12. Grammar

The implemented grammar is LL(2); the authoritative EBNF sketch is in
[`../design/LANGUAGE.md` §9](../design/LANGUAGE.md). The invariant
`parse(fmt(program)) == program` is enforced by round-trip tests.
