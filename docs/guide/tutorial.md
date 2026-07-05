# The KUPL Tutorial

This tutorial is a hands-on tour of the whole language. It assumes you can build
and run the toolchain (see **[Getting Started](getting-started.md)**) and that
you're comfortable with another programming language.

Every code block here runs on all four engines and produces the shown output.
Save a snippet to `t.kupl` and run `kupl run t.kupl`.

**Contents**

1. [Values and types](#1-values-and-types)
2. [Variables and functions](#2-variables-and-functions)
3. [Control flow](#3-control-flow)
4. [Algebraic data types and `match`](#4-algebraic-data-types-and-match)
5. [Records and newtypes](#5-records-and-newtypes)
6. [Collections](#6-collections)
7. [Error handling: `Option`, `Result`, `?`](#7-error-handling-option-result-)
8. [Strings](#8-strings)
9. [Generics](#9-generics)
10. [Effects](#10-effects)
11. [Components](#11-components)
12. [Contracts and dependency injection](#12-contracts-and-dependency-injection)
13. [The AI-native core](#13-the-ai-native-core)
14. [Testing: `example`, `law`, `forall`](#14-testing-example-law-forall)
15. [Packages and modules](#15-packages-and-modules)
16. [Concurrency](#16-concurrency)
17. [Sized numerics](#17-sized-numerics)
18. [Compiling to native code](#18-compiling-to-native-code)
19. [Where to go from here](#19-where-to-go-from-here)

---

## 1. Values and types

KUPL has a small set of built-in types:

```kupl
fun main() uses io {
    print(42)              // Int   — 64-bit signed, overflow-checked
    print(3.14)            // Float — IEEE-754 f64
    print(true)            // Bool
    print("hello")         // Str   — immutable UTF-8
    print(())              // Unit  — the no-value value
    print([1, 2, 3])       // List[Int]
    print(1..5)            // Range (exclusive); 1..=5 is inclusive
}
```

```text
42
3.14
true
hello
()
[1, 2, 3]
1..5
```

Arithmetic on `Int` is **overflow-checked** — it panics rather than silently
wrapping. Division and remainder by zero panic. There is no implicit conversion:
use `.to_float()`, `.to_int()`, etc.

---

## 2. Variables and functions

`let` binds an immutable value; `var` a reassignable one:

```kupl
fun main() uses io {
    let x = 10          // immutable
    var total = 0       // mutable
    total = total + x
    total += 5          // compound assignment
    print(total)        // 15
}
```

Functions are declared with `fun`. **Types are inferred inside bodies**, but every
public boundary — parameters and return type — must be written out:

```kupl
fun add(a: Int, b: Int) -> Int {
    a + b               // the last expression is the return value
}

fun greet(name: Str) -> Str {
    "Hi, " + name
}

fun main() uses io {
    print(add(2, 3))            // 5
    print(greet("Ada"))         // Hi, Ada
}
```

Any function whose first parameter is some type can be called with **method
syntax** on a value of that type — `area(shape)` and `shape.area()` are the same
call (UFCS). This lets your own functions read as methods and chain, without any
`impl` blocks:

```kupl
fun scaled(s: Shape, k: Float) -> Float { area(s) * k }

Circle(2.0).area()             // == area(Circle(2.0))
Circle(2.0).scaled(10.0)       // == scaled(Circle(2.0), 10.0)
```

Built-in methods (like `List.map`) take precedence; the free-function fallback
only applies when there's no built-in method of that name.

Parameters can have **defaults** (which must be trailing), and calls can pass
**named arguments** in any order (after any positional ones):

```kupl
fun greet(name: Str, greeting: Str = "Hello") -> Str { "{greeting}, {name}" }

greet("Ada")                   // "Hello, Ada"  (default used)
greet("Ada", "Hi")             // "Hi, Ada"
greet(greeting: "Yo", name: "Ada")   // named, reordered
```

The last expression in a block is its value — no `return` needed (though `return`
exists for early exit). **Lambdas** use `fn`:

```kupl
fun main() uses io {
    let square = fn n { n * n }
    let plus = fn a, b { a + b }          // multiple params
    print(square(9))                       // 81
    print(plus(20, 22))                    // 42
}
```

---

## 3. Control flow

`if`/`else` is an **expression** — both arms must have the same type:

```kupl
fun classify(n: Int) -> Str {
    if n < 0 {
        "negative"
    } else if n == 0 {
        "zero"
    } else {
        "positive"
    }
}

fun main() uses io {
    for n in [-1, 0, 1] {
        print(classify(n))
    }
    var i = 0
    while i < 3 {
        print("i = {i}")
        i += 1
    }
}
```

```text
negative
zero
positive
i = 0
i = 1
i = 2
```

`for x in xs` iterates a `List` or a `Range`. `break` and `continue` work as
usual.

---

## 4. Algebraic data types and `match`

A `type` with `|` is a **union** (an ADT). You take it apart with `match`, which
must be **exhaustive**:

```kupl
type Shape = Circle(r: Float) | Rect(w: Float, h: Float) | Point

fun area(s: Shape) -> Float {
    match s {
        Circle(r) => 3.14159 * r * r
        Rect(w, h) => w * h
        Point => 0.0
    }
}

fun main() uses io {
    let shapes = [Circle(2.0), Rect(3.0, 4.0), Point]
    for s in shapes {
        print(area(s).format(2))
    }
}
```

```text
12.57
12.00
0.00
```

> **Note:** `match` arms are separated by newlines (or commas). Two arms on one
> line without a separator is a syntax error.

Patterns **nest**, bind variables, and match literals and wildcards (`_`). ADTs
can be **recursive** — a variant may carry its own type:

```kupl
type Tree = Leaf(v: Int) | Node(l: Tree, r: Tree)

fun sum(t: Tree) -> Int {
    match t {
        Leaf(v) => v
        Node(l, r) => sum(l) + sum(r)
    }
}

fun main() uses io {
    let t = Node(Node(Leaf(1), Leaf(2)), Leaf(3))
    print(sum(t))       // 6
}
```

Arms can carry a **guard** (`if COND`) and can share a body with an
**or-pattern** (`P1 | P2`):

```kupl
fun sign(n: Int) -> Str {
    match n {
        x if x < 0 => "negative"    // guard — runs only when the condition holds
        0 => "zero"
        _ => "positive"
    }
}

fun is_weekend(d: Day) -> Bool {
    match d {
        Sat | Sun => true           // or-pattern — any alternative matches
        _ => false
    }
}
```

A guarded arm doesn't count toward exhaustiveness (it might not run), so a
`match` still needs unguarded arms or a catch-all to cover every case.
Or-pattern alternatives may not bind variables.

Integer arms can also use **range patterns** (`lo..hi` half-open, `lo..=hi`
inclusive), and **`@` bindings** capture the whole value while destructuring:

```kupl
match n {
    0 => "zero"
    1..10 => "small"            // 1 ≤ n < 10
    10..=99 => "medium"         // 10 ≤ n ≤ 99
    _ => "large"
}

match shape {
    whole @ Circle(r) if r > 5 => big(whole)   // binds both `whole` and `r`
    other => small(other)
}
```

`Option` and `Result` (below) are themselves ADTs you match on.

---

## 5. Records and newtypes

A single-variant `type` with named fields is a **record**:

```kupl
type User = { name: Str, age: Int }

fun birthday(u: User) -> User {
    u with age: u.age + 1       // record update — returns a new value
}

fun main() uses io {
    let ada = User(name: "Ada", age: 36)
    print(ada.name)                 // field access: Ada
    print(birthday(ada).age)        // 37  (ada is unchanged)
}
```

Fields are read with `.name`. `with` produces a **new** record with some fields
replaced (values are immutable).

A **newtype** wraps one value in a distinct type — a cheap bulwark against
mixing up same-shaped values (an ID vs a raw string):

```kupl
type UserId = new Str

fun main() uses io {
    let id = UserId("u-42")
    print(id.value)                 // u-42  (the wrapped value)
}
```

---

## 6. Collections

**Lists** are immutable; methods return new lists:

```kupl
fun main() uses io {
    let xs = [4, 8, 15, 16, 23, 42]
    print(xs.len())                              // 6
    print(xs.map(fn n { n * 2 }))                // [8, 16, 30, 32, 46, 84]
    print(xs.filter(fn n { n > 15 }))            // [16, 23, 42]
    print(xs.fold(0, fn acc, n { acc + n }))     // 108
    print(xs.sum())                              // 108
    print(xs.sort())                             // [4, 8, 15, 16, 23, 42]
    print(xs.get(2).unwrap_or(0))                // 15
}
```

**Maps** are insertion-ordered and immutable:

```kupl
fun main() uses io {
    var ages = Map()
    ages = ages.insert("ada", 36)
    ages = ages.insert("alan", 41)
    ages = ages.insert("ada", 37)        // update keeps position
    print(ages.get("alan").unwrap_or(0)) // 41
    print(ages.keys())                   // ["ada", "alan"]
    print(ages.len())                    // 2
}
```

**Sets** are insertion-ordered with order-insensitive equality:

```kupl
fun main() uses io {
    let a = Set([1, 2, 3, 2, 1])         // dupes dropped
    let b = Set([3, 4, 5])
    print(a.union(b))                    // Set{1, 2, 3, 4, 5}
    print(a.intersect(b))                // Set{3}
    expect Set([1, 2, 3]) == Set([3, 1, 2])   // equality ignores order
}
```

See the **[Standard Library](../reference/STDLIB.md)** for the full method list.

---

## 7. Error handling: `Option`, `Result`, `?`

There is **no null**. Absence is `Option[T]` (`Some(v)` / `None`); fallible
results are `Result[T, E]` (`Ok(v)` / `Err(e)`):

```kupl
fun half(n: Int) -> Result[Int, Str] {
    if n % 2 == 0 {
        Ok(n / 2)
    } else {
        Err("cannot halve odd number {n}")
    }
}

fun main() uses io {
    match half(10) {
        Ok(v) => print("got {v}")
        Err(e) => print("error: {e}")
    }
}
```

```text
got 5
```

The **`?` operator** propagates errors: if the value is `Err`/`None`, the enclosing
function returns it early.

```kupl
fun quarter(n: Int) -> Result[Int, Str] {
    let h = half(n)?        // returns early on Err
    half(h)
}

fun main() uses io {
    print(quarter(20))      // Ok(5)
    print(quarter(6))       // Err("cannot halve odd number 3")
}
```

`.unwrap_or(default)` extracts a value with a fallback; `.map`, `.is_some`,
`.is_ok` and friends are on the [`Option`/`Result` types](../reference/STDLIB.md).

**Combinators** chain fallible steps without a pyramid of `match` — `Option` and
`Result` both have `.map`/`.and_then`/`.unwrap_or`, `Option` adds `.filter`/
`.ok_or`, `Result` adds `.map_err`/`.ok`. A method chain may span lines when the
line starts with `.`:

```kupl
fun main() uses io {
    let r = "8".parse_int()
        .map(fn x { x * 2 })
        .filter(fn x { x > 10 })
        .unwrap_or(0)
    print(r)                              // 16
    print("5".parse_int().ok_or("nan").map(fn x { x * 10 }))   // Ok(50)
    print("x".parse_int().ok_or("nan"))                        // Err("nan")
}
```

For a quick conditional unwrap, **`if let`** and **`while let`** bind a pattern
inline (they desugar to `match`, so any pattern works):

```kupl
if let Some(n) = xs.find(fn x { x > 0 }) {
    print("found {n}")
} else {
    print("none positive")            // else is optional
}

while let Some(job) = queue_pop() {   // loop until it stops matching
    run(job)
}
```

---

## 8. Strings

Strings are immutable UTF-8. Interpolation embeds any expression:

```kupl
fun main() uses io {
    let name = "Ada"
    let n = 3
    print("Hello {name}, you have {n + 1} messages")
    print("a literal brace: \{ and a newline:\nsecond line")
    print("split -> {"a,b,c".split(",")}")
    print("upper -> {"hi".to_upper()}, len -> {"héllo".len()}")
}
```

```text
Hello Ada, you have 4 messages
a literal brace: { and a newline:
second line
split -> ["a", "b", "c"]
upper -> HI, len -> 5
```

Because `{` starts interpolation, a **literal** brace is `\{`. `.len()` counts
characters (not bytes).

---

## 9. Generics

Functions can be generic over type parameters in `[...]`, instantiated fresh at
each call site:

```kupl
fun first[T](xs: List[T]) -> Option[T] {
    xs.get(0)
}

fun apply2[T, U](f: fn(T) -> U, a: T, b: T) -> List[U] {
    [f(a), f(b)]
}

fun main() uses io {
    print(first([10, 20, 30]))                 // Some(10)
    print(first(["x", "y"]))                    // Some("x")
    print(apply2(fn n { n * n }, 3, 4))         // [9, 16]
}
```

**Types** can be generic too — parameters in `[...]` after the name, referenced in
the variant fields:

```kupl
type Box[T] = Box(v: T)
type Pair[A, B] = Pair(first: A, second: B)
type Tree[T] = Leaf | Node(value: T, left: Tree[T], right: Tree[T])

fun unwrap[T](b: Box[T]) -> T { b.v }

fun main() uses io {
    print(unwrap(Box(v: 5)))          // 5     (Box[Int], inferred)
    print(unwrap(Box(v: "hi")))       // hi    (Box[Str])
    let p = Pair(first: 1, second: "one")
    print("{p.first} {p.second}")     // 1 one (Pair[Int, Str])
}
```

Construction infers the type arguments (`Box(v: 5)` is `Box[Int]`), each
instantiation is distinct (a `Box[Int]` can't hold a `Str`), and type parameters
are **erased at runtime** — generics cost nothing at run time. Bounds (`[T: Ord]`)
are still **[design]**: ordered generic code passes an explicit compare function
(see `examples/collections.kupl`).

---

## 9a. Operator overloading

Define `add`/`sub`/`mul`/`div`/`rem` (for `+ - * / %`) or `lt`/`le`/`gt`/`ge` (for
`< <= > >=`) on your own type and the operator works on it — like a method call.
`==`/`!=` are always structural, so they need no definition.

```kupl
type Vec2 = { x: Int, y: Int }
fun add(a: Vec2, b: Vec2) -> Vec2 { Vec2(x: a.x + b.x, y: a.y + b.y) }
fun lt(a: Vec2, b: Vec2) -> Bool { a.x * a.x + a.y * a.y < b.x * b.x + b.y * b.y }

fun main() uses io {
    let s = Vec2(x: 1, y: 2) + Vec2(x: 3, y: 4)
    print("({s.x}, {s.y})")                            // (4, 6)
    print(Vec2(x: 1, y: 1) < Vec2(x: 3, y: 3))         // true
}
```

---

## 10. Effects

KUPL functions are **pure by default**. Side effects are named and declared with
`uses`, and the compiler enforces that declaration at public boundaries:

```kupl
fun add(a: Int, b: Int) -> Int {
    a + b                       // pure: no `uses`
}

fun shout(msg: Str) uses io {   // performs io (print)
    print(msg.to_upper())
}

fun main() uses io {
    print(add(2, 3))
    shout("hello")
}
```

Effects are **hierarchical**: `io` covers its sub-effects `io.fs` (files),
`io.env` (environment/args), `io.net` (network), and `io.time` (the wall clock).
Declaring `uses io` covers all of them; declaring `uses io.fs` is the precise
capability. A `pub` function that performs an effect **must** declare it (this is
a compile error otherwise) — the effect surface of your API is explicit.

```kupl
fun read_config(path: Str) -> Result[Str, Str] uses io.fs {
    read_file(path)
}
```

---

## 11. Components

A **component** is KUPL's unit of structure: an isolated actor with typed ports,
private state, and message handlers. This is the language's defining feature.

```kupl
component Counter {
    intent "Counts clicks and publishes the running total."

    in click: Event          // an input port carrying no payload
    out value: Int           // an output port carrying an Int

    state count: Int = 0     // private, mutable state

    on click {
        count += 1
        emit value(count)    // send a message on the out port
    }

    example {
        send click
        send click
        expect value == 2    // the last value emitted on `value`
    }
}
```

- **`intent`** documents the component's purpose (and feeds tooling).
- **Ports** (`in`/`out`) are the typed message interface. `Event` is a
  payload-less signal.
- **`state`** is private and mutable within handlers.
- **`on <port>`** is a message handler; `emit <port>(value)` sends on an out port.
- **`example`** blocks are inline tests: `send` a message, `advance` the clock,
  `expect` a condition. Run them with `kupl test`.

An **`app`** is the runnable top-level component. It wires children together:

```kupl
component Doubler {
    intent "Doubles each number it receives."
    in input: Int
    out output: Int
    on input(n) {
        emit output(n * 2)
    }
}

app Main {
    intent "Feeds a counter's value into a doubler and prints the result."

    let counter = Counter()
    let doubler = Doubler()

    wire counter.value -> doubler.input   // connect an out port to an in port

    on start {
        // drive the counter a few times on startup
    }
}
```

Emissions on an **unwired** out port are printed as `Component.port = value` by
`kupl run`, so components are observable without extra plumbing. Components also
support **timers** (`on every 5s`, `on after 2s`, on a deterministic virtual
clock), **supervision** (`supervise child restart on_failure`), and **exposes**
(callable methods) — see the [Language Reference](../reference/LANGUAGE-REFERENCE.md).

---

## 12. Contracts and dependency injection

A **contract** is an interface — a set of `expose` signatures a component can
fulfill. A prop typed by a contract accepts any fulfilling component, dispatched
dynamically:

```kupl
contract Store {
    intent "A tiny key/value store."
    expose fun put(key: Str, value: Int)
    expose fun get(key: Str) -> Option[Int]
}

component MemStore fulfills Store {
    intent "An in-memory Store."
    state data: Map[Str, Int] = Map()
    expose fun put(key: Str, value: Int) {
        data = data.insert(key, value)
    }
    expose fun get(key: Str) -> Option[Int] {
        data.get(key)
    }
}
```

A component that requires a `Store` names the contract as a prop type; injecting a
non-fulfilling component is a type error. This gives testable, swappable
dependencies without a framework.

---

## 13. The AI-native core

KUPL treats LLM calls as a **language feature**. An `ai fun` is a typed prompt
function: its return **type** drives structured output. A `-> Str` return gives
the model's text; any other supported type is requested as JSON and parsed into a
real KUPL value.

```kupl
type Sentiment = { label: Str, score: Float }

ai fun classify(review: Str) -> Result[Sentiment, Str] {
    intent "Classify the review's sentiment (positive/negative/neutral) with a
            confidence score between 0 and 1."
}

fun main() uses io {
    match classify("This is fantastic, I love it!") {
        Ok(s) => print("label={s.label} score={s.score}")
        Err(e) => print("error: {e}")
    }
}
```

- The **`intent`** is the prompt; it can interpolate parameters
  (`intent "Reply to {msg}"`).
- The return type drives the JSON schema — use a **record** (or `List[...]`) for
  structured output, `-> Str` for free text, and wrap in `Result[T, Str]` to
  capture provider failures as values.
- **Tool use:** `ai fun … tools [f, g]` lets the model call your KUPL functions;
  the runtime drives the model↔tool loop.
- **Agent components** persist conversation state across turns.
- Provider-agnostic (Anthropic, OpenAI-compatible, Ollama) plus a **deterministic
  mock provider** (`KUPL_AI_MOCK*` environment variables) that makes AI-driven
  code **unit-testable** and reproducible — no network:

```text
$ KUPL_AI_MOCK_CLASSIFY='{"label":"positive","score":0.95}' kupl run classify.kupl
label=positive score=0.95
```

`ai fun` carries the `ai` effect implicitly. It runs on the interpreter, KVM, and
`.kx`/bundle; the pure-native `kupl native` path defers it (use `kupl bundle`).

---

## 14. Testing: `example`, `law`, `forall`

KUPL bakes testing into the language. Three mechanisms, all run by `kupl test`:

**Component `example` blocks** (seen above) drive a component and assert on its
emissions.

**Top-level `law`s** are free-standing tests:

```kupl
fun reverse_twice[T](xs: List[T]) -> List[T] {
    xs.reverse().reverse()
}

law "reversing twice is identity" {
    expect reverse_twice([1, 2, 3]) == [1, 2, 3]
}
```

**Property-based testing** with `forall` generates many cases and shrinks a
failure to a minimal counterexample:

```kupl
law "addition commutes" {
    forall a: Int, b: Int {
        expect a + b == b + a
    }
}
```

```text
$ kupl test props.kupl
ok    law "reversing twice is identity"
ok    law "addition commutes"

2 passed, 0 failed, 0 skipped
```

`forall` generates 100 deterministic cases (fixed seed → reproducible) for
`Int`/`Bool`/`Float`/`Str`/`List`/`Option`/records.

---

## 15. Packages and modules

Within one package, `use` merges another file's items:

```kupl
use util          // loads util.kupl (same directory)
use lib.stats     // loads lib/stats.kupl
```

A directory with a **`kupl.toml`** is a package. Its `[dependencies]` name other
local packages by path:

```toml
[project]
name = "app"
version = "0.1.0"
entry = "main.kupl"

[dependencies]
math = { path = "../math" }
```

Cross-package names are **qualified** — after `use math`, call `math.add(1, 2)`.
Namespaces are isolated (each package's names are mangled internally), so two
dependencies can define the same name without colliding. A dependency can pin an
exact `version`, and `kupl pkg lock` writes a `kupl.lock` for reproducibility.

---

## 16. Concurrency

Components are isolated actors — the right foundation for concurrency. For
**data parallelism**, `par_map` / `par_filter` process a list independently:

```kupl
fun heavy(n: Int) -> Int {
    var acc = 0
    var i = 0
    while i < 100 {
        acc = acc + (n * i) % 97
        i += 1
    }
    acc
}

fun main() uses io {
    var xs: List[Int] = []
    var i = 0
    while i < 1000 {
        xs = xs.push(i)
        i += 1
    }
    print(xs.par_map(heavy).sum())
}
```

When the callback is a **pure** top-level function and the list is large,
`par_map`/`par_filter` run across **real OS threads** (on both the interpreter and
the KVM). Because a pure function can't observe I/O, the clock, randomness, or
shared state — and results are placed back by input index — the output is
**deterministic and byte-identical** whether it ran on one thread or many. Smaller
lists or impure/closure callbacks run sequentially. General `async`/`await` and
coroutines are **[design]**.

---

## 17. Sized numerics

Beyond the default 64-bit `Int`, KUPL has fixed-width integers `i8`…`i64` /
`u8`…`u64` (write a literal with a suffix) and single-precision `f32`:

```kupl
fun main() uses io {
    let r: u8 = 200u8
    let g: u8 = 55u8
    print(r + g)                    // 255
    print(0xFFu8)                   // 255  (hex literal + suffix)
    print((255u8).wrapping_add(1u8)) // 0    (modular, never panics)
    print((200u8).saturating_add(100u8)) // 255 (clamps)
    print((255u8).to_int() + 1)     // 256  (convert to Int)
    let ratio: f32 = 22.0f32 / 7.0f32
    print(ratio)                    // 3.142857
}
```

Sized-integer arithmetic is checked (out-of-range **panics**); use `.wrapping_*`
or `.saturating_*` for modular/clamping behavior. Mixing widths is a type error;
convert explicitly. These compile natively too.

Beyond machine integers, the **exact numeric tower** never loses precision:
`big(...)` is an arbitrary-precision `BigInt` and `rat(n, d)` an exact `Rational`
(reduced fraction). And `Float.fmt(decimals)` formats a fixed-point decimal:

```kupl
fun main() uses io {
    print(big(2).pow(100))                        // 1267650600228229401496703205376
    print(rat(1, 3) + rat(1, 6))                  // 1/2  (exact)
    print(3.14159.fmt(2))                         // 3.14
}
```

Everything above — sized numerics, `BigInt`, `Rational`, and `Float.fmt` — is
exact and **byte-identical on every engine**, native included.

---

## 18. Compiling to native code

Any KUPL program (except those using `ai fun`) compiles to a real native
executable via generated C:

```text
$ kupl native analytics.kupl -o analytics
$ ./analytics
```

The native binary produces **byte-identical output** to `kupl run` — components,
the full numeric surface, JSON, CSV, URL, regex, and HTTP all lower to machine
code. For component apps that use `ai fun`, use `kupl bundle` to produce a
self-contained executable (the runtime plus your compiled module):

```text
$ kupl bundle app.kupl -o app
$ ./app
```

---

## 18a. A web server

KUPL can serve HTTP. `http_serve(port, handler)` binds a port and calls your
`handler(method, path) -> Str` for each request (it blocks and serves forever):

```kupl
fun route(method: Str, path: Str) -> Str {
    if path == "/health" { "ok" } else { "you sent {method} {path}" }
}

fun main() uses io {
    match http_serve(8080, route) {   // then: curl http://127.0.0.1:8080/health
        Ok(_) => print("stopped")
        Err(e) => print("could not start: {e}")
    }
}
```

Combine it with the built-in `Json` type (`json_parse`/`json_stringify`) for a
JSON API — see `examples/demos/api.kupl` for a worked REST service.

---

## 19. Where to go from here

Every domain has a worked example in [`examples/`](../../examples):

| Domain | Example |
|---|---|
| Pipelines / data | `showcase.kupl`, `analytics.kupl`, `jq.kupl` |
| Web backend | `demos/api.kupl`, `demos/server.kupl` |
| Language implementation | `calc.kupl` (interpreter), `vm.kupl` (compiler + stack VM) |
| Algorithms / simulation | `sudoku.kupl`, `life.kupl` |
| Generics / data structures | `generic.kupl`, `collections.kupl` |
| Numerics | `bigint.kupl`, `rational.kupl`, `stats.kupl`, `tensors.kupl` |
| Language features | `operators.kupl`, `combinators.kupl`, `format.kupl`, `sets.kupl` |

- `showcase.kupl` (JSON → file → regex → parallel pipeline) and `analytics.kupl`
  (CSV → regex → grouping → JSON) tie the stack together.
- Keep the **[Language Reference](../reference/LANGUAGE-REFERENCE.md)** and
  **[Standard Library](../reference/STDLIB.md)** at hand.
- See the **[CLI reference](../reference/CLI.md)** for every `kupl` subcommand.
- Curious how KUPL stacks up? Read **[KUPL vs. the Field](../COMPARISON.md)**.

Welcome to KUPL — happy building.
