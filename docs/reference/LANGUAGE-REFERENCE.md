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
expose false fn for fun if in intent let match module new on out prop pub
requires return send start state stop supervise test true type use uses var
while wire
```

### Contextual keywords

Valid identifiers everywhere except in their clause:
`fulfills` `law` `restart` `on_failure` `never`

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

### Inference rules

Types are inferred inside bodies (Hindley-Milner-style unification, checked
bidirectionally so lambda parameters take their types from context). **All
public boundaries must be written out**: function parameters and returns,
ports, props, expose signatures. Generics for user code are **[design]**;
`List`/`Option`/`Result` and the builtin methods are generic internally.

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
```

- `match` exhaustiveness is compile-checked: all variants of a union,
  `Some`/`None`, `Ok`/`Err`, `true`/`false` — or a catch-all `_`/binding arm.
  Unbounded scrutinees (Int, Str) require a catch-all.
- `expr?` requires `expr : Result[T, E]` and an enclosing function returning
  `Result[_, E]`; on `Err(e)` the function returns early with that error. Not
  allowed in handlers (K0237) — handle the Result with `match` there.
- `await expr` is accepted and currently evaluates `expr` directly (expose
  calls are synchronous in v1.0-alpha; true asynchrony is **[design]**).

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
  `db.read` does not cover `db.write`. The built-in effectful operation in
  v1.0-alpha is `print` (`io`); capability *values* are **[design]**.
- Recursion (incl. mutual) is fully supported. Functions are first-class:
  pass them by name or as lambdas; calls through variables are supported
  (their effects are not tracked in v1.0-alpha — documented limitation).

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
| `prop name: T [= default]` | instantiation-time configuration, immutable; `requires` is accepted as a synonym |
| `in name: T` / `out name: T` | typed ports; `Event` = no payload |
| `state name[: T] = init` | private mutable state; invisible outside |
| `let child = Comp(args)` | child instance (positional or `name:` args against props) |
| `wire a.out -> b.in` | connect children's ports (types must match) |
| `supervise child restart on_failure` | see §9 |
| `on port(payload) { … }` | handler; `on start` / `on stop` lifecycle |
| `expose fun …` | synchronous request/response interface |
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
  the contract's functions bound to a live instance. `forall` quantification
  is **[design]** — laws are concrete executable scenarios today.

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
| Native (C) | `kupl native` | `fun main` programs (components **[design]** for native) |

The interpreter defines the semantics; the VM and native backend are held to
it by differential tests. Known intentional VM/native limits: assignment to a
lambda-captured outer `var` is a compile error on the KVM (K0803) — captures
are by value; component state accessed from lambdas is live on all engines.

## 12. Grammar

The implemented grammar is LL(2); the authoritative EBNF sketch is in
[`../design/LANGUAGE.md` §9](../design/LANGUAGE.md). The invariant
`parse(fmt(program)) == program` is enforced by round-trip tests.
