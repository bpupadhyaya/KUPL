# KUPL Language Design

Proposal v0.1 — 2026-07-03. Everything here is open for revision until spec v1.0.

- File extension: `.kupl` (bytecode: `.kx`, manifest: `.kman.json`)
- Source encoding: UTF-8. Identifiers: Unicode XID. Indentation: 4 spaces (formatter-enforced).
- Paradigm: **component-oriented** + functional core (pure functions, immutable data by
  default) + actor concurrency + data-parallel kernels + tiered systems programming.

---

## 1. The component model

The `component` is the universal unit. One mental model serves UI widgets, backend
services, database adapters, device drivers, and ML pipelines.

```kupl
component Counter {
    intent "Counts clicks and publishes the current count."

    in  click: Event
    in  reset: Event
    out value: Int

    state count: Int = 0

    on click {
        count += 1
        emit value(count)
    }

    on reset {
        count = 0
        emit value(count)
    }

    example {
        send click
        send click
        expect value == 2
        send reset
        expect value == 0
    }
}
```

A component declares, **in this fixed order** (formatter-enforced — every component
reads the same way to humans and models):

1. `intent` — required natural-language purpose. Part of the AST, the manifest, and
   `kupl context` output. The compiler warns when a component's interface changes but
   its intent doesn't.
2. `fulfills` — contracts this component implements.
3. `requires` — capabilities/dependencies injected at instantiation (database, network,
   clock, another component's contract). **The only way to reach the outside world.**
4. `in` / `out` ports — typed message channels. `in` ports trigger handlers; `out`
   ports are emitted to. Ports are the wiring surface Builder draws.
5. `prop` — instantiation-time configuration (immutable after construction).
6. `state` — private mutable state. Invisible outside; mutated only by handlers.
7. `on` handlers — react to `in` ports, lifecycle events (`on start`, `on stop`),
   timers (`on every 5s`), and supervised-child failures.
8. `expose` functions — synchronous request/response interface (a typed call the
   runtime delivers as a message; the caller `await`s the reply).
9. Private functions.
10. `example` / `test` blocks.

### Instantiation, wiring, supervision

```kupl
app TodoApp {
    intent "A todo web application."
    requires db: cap.Sql, http: cap.HttpServer

    let store  = TodoStore(db)                  // child components
    let header = Header(prop title: "My Todos")
    let list   = TodoList(store)
    let search = SearchBox(prop placeholder: "filter…")

    wire search.query -> list.filter            // typed: port types must match
    wire list.changed -> store.save

    supervise store restart on_failure          // Erlang-style supervision
    supervise list restart never
}
```

- `app` is a component that is also an entry point (a `main` composition root).
- Every component instance is an **isolated actor**: its own heap, its own mailbox,
  no shared mutable state. `send`/`emit` pass messages by move or copy.
- `supervise` policies: `restart on_failure [max N in T]`, `restart never`,
  `escalate`. A crashing component never takes the app down unless supervision
  says so. This is the fault-tolerance story — essential when AI writes code.

### Contracts

Contracts are interfaces plus laws (design-by-contract):

```kupl
contract Store[T] {
    intent "Durable keyed storage for values of type T."

    expose fun get(id: Id) -> Option[T] uses io
    expose fun put(id: Id, value: T) -> Result[Unit, StoreError] uses io

    law "put then get returns the value" {
        forall id: Id, v: T {
            put(id, v)?
            expect get(id) == Some(v)
        }
    }
}

component TodoStore fulfills Store[Todo] { ... }
```

`law` clauses compile to property tests (run by `kupl test`) and are included in
`kupl context` so an LLM implementing the contract knows the semantics, not just
the signatures.

---

## 2. Functions, purity, effects

```kupl
pub fun total(items: List[Item]) -> Money {          // pure: no `uses` clause
    items.map(.price).sum()
}

pub fun fetch_user(id: UserId) uses net -> Result[User, NetError] {
    http.get("/users/{id}")?.parse[User]()
}
```

- Functions are **pure by default**. Anything effectful must declare an effect row:
  `uses io`, `uses net`, `uses db.read`, `uses db.write`, `uses time`, `uses rand`,
  `uses gpu`, `uses unsafe`. Effects are inferred inside a component but **must be
  written explicitly on every `pub`/`expose` signature** (boundary explicitness rule).
- Effects are backed by **capabilities**: you can only perform `net` if a `cap.Net`
  (or derived) capability is in scope via `requires`. No ambient authority — AI-generated
  code physically cannot exfiltrate data or touch disk unless the surrounding component
  was granted that capability. Capabilities are attenuable: `cap.Sql.read_only()`,
  `cap.Http.limited_to("api.example.com")`.
- Errors are values: `Result[T, E]` with `?` propagation, `Option[T]` instead of null.
  `panic` exists only for bugs (contract violation, index out of bounds) and is caught
  at component boundaries by supervision — a panic kills the instance, not the program.

---

## 3. Type system

Static, strong, no implicit conversions, no null, no inheritance.

- **Primitives:** `Int` (64-bit), `Float` (64-bit), `Bool`, `Str` (UTF-8, immutable),
  `Byte`, `Char`, `Unit`. Sized numerics for system/kernel tiers: `i8…i64`, `u8…u64`,
  `f16`, `bf16`, `f32`, `f64`. `BigInt`, `Decimal` in std.
- **Records:** `type User = { id: UserId, name: Str, age: Int where age >= 0 }`
  Structural where clauses are checked at construction (runtime) and used by the
  optimizer and property-test generators.
- **Unions (ADTs):** `type Shape = Circle(r: Float) | Rect(w: Float, h: Float)`
  with exhaustive `match`.
- **Newtypes:** `type UserId = new Str` — zero-cost, prevents ID-mixup bugs (a class
  of error LLMs make often).
- **Generics** with contract bounds: `fun sort[T: Ord](xs: List[T]) -> List[T]`.
- **Collections:** `List[T]`, `Map[K, V]`, `Set[T]` (immutable, persistent);
  `Array[T]`, `MutMap[K, V]` (mutable, component-local only — never crosses a port).
- **Tensors:** `Tensor[f32, (batch, 3, 224, 224)]` — dtype and shape in the type;
  shapes may be symbolic (`(n, m)`) and are checked/propagated at compile time where
  static, at dispatch time otherwise.
- **Inference:** full inference inside function/component bodies; **mandatory
  annotations on all public boundaries** (ports, props, `pub`/`expose` signatures).
  This is the human/AI readability contract: you never need global inference to read
  an interface.

Immutability default: `let` bindings and all data crossing ports are immutable.
`var` is allowed only for local variables and `state`.

---

## 4. Concurrency

Two layers, both structured:

1. **Between components:** actors + messages, as above. No locks, no shared memory,
   no data races by construction.
2. **Inside a handler/function:** structured concurrency.

```kupl
on refresh {
    let (posts, ads) = par {                // structured fork-join
        fetch_posts(user)?,
        fetch_ads(user)?
    }
    emit rendered(compose(posts, ads))
}

let result = await store.get(id)            // expose-call to another component
```

- `par { a, b, c }` runs branches concurrently, joins all, cancels siblings on failure.
- `async fun` / `await` for explicit asynchrony; handlers are implicitly async.
- Timers: `on every 30s { ... }`, `after 5s { ... }`.
- No bare threads in the app tier; the runtime multiplexes components on an M:N
  work-stealing scheduler.

---

## 5. Hardware tier: tensors, kernels, placement

```kupl
kernel fun saxpy(a: f32, x: Tensor[f32, (n)], y: Tensor[f32, (n)]) -> Tensor[f32, (n)] {
    par i in 0..n {
        out[i] = a * x[i] + y[i]
    }
}

let z = at(gpu) saxpy(2.0, x, y)     // placement; falls back per policy if absent
let w = at(cpu) saxpy(2.0, x, y)
```

- `kernel fun` is a restricted subset (no allocation, no effects except `gpu`, bounded
  loops or `par` iteration spaces) that the compiler can lower to CPU SIMD, GPU, or
  accelerator ISAs through KIR dialects (see TOOLCHAIN.md).
- `par i in space { }` inside kernels is the data-parallel primitive; `reduce`, `scan`,
  `map` are built on it and fuse.
- `at(target)` is an expression-level placement annotation: `cpu`, `gpu`, `gpu[1]`,
  `tpu`, `npu`, `auto`. Placement is semantics-preserving — the same kernel must
  produce the same result on every target (modulo documented float reassociation).
- Devices are capabilities too: `uses gpu` requires `cap.Gpu`.

---

## 6. System & low tiers (progressive disclosure)

App-tier developers never see this section. It exists so KUPL can implement its own
runtime, drivers, and allocators.

```kupl
system component RingBuffer[T] {
    intent "Lock-free SPSC ring buffer."
    requires cap.unsafe

    state buf: Own[Array[T]]                 // ownership types (move semantics)
    state head: Atomic[u32]
    state tail: Atomic[u32]

    fun push(v: T) -> Bool { ... }           // borrow checking applies here
}

low fun write_reg(addr: u64, v: u32) uses unsafe {
    volatile_store(Ptr[u32].from_addr(addr), v)
}

low fun rdtsc() -> u64 uses unsafe {
    asm("rdtsc" -> (lo: reg.eax, hi: reg.edx))   // named register binding
    (hi as u64 << 32) | lo as u64
}
```

- `system` components opt into ownership/borrowing (`Own[T]`, `&T`, `&mut T`),
  explicit layout (`@layout(c)`, `@align(64)`, `@packed`), manual allocation
  (`alloc`/`free` against an allocator capability), and `Ptr[T]`.
- `low` functions additionally allow `volatile_load/store`, `asm` blocks with typed
  register bindings, memory fences, and interrupt attributes.
- Both require `cap.unsafe`, which must be granted at the composition root — an app
  can be audited for metal access by grepping one file, and AI-generated app-tier
  code can never acquire it silently.

---

## 7. AI-first surface (what makes generation & repair easy)

1. **Canonical form.** `kupl fmt` is normative: fixed member order, fixed layout, no
   configuration. Any two programs with the same AST render identically.
2. **`intent` everywhere**, `example` blocks executable, `law` clauses on contracts —
   the spec is in the artifact. `kupl test` runs examples + laws + tests; drift
   between intent and interface is a warning.
3. **Boundary explicitness** — interfaces read without inference; a model can be
   given only signatures + intents + laws and implement correctly.
4. **`kupl context <item>`** — emits the minimal, dependency-closed context (target +
   transitive contracts/types, no bodies of unrelated code) sized for a model prompt.
5. **Structured diagnostics** — `kupl build --json` yields `{code, span, message,
   explanation, fixes[]}`; fixes are machine-applicable edits.
6. **Semantic diff/patch** — `kupl diff` compares ASTs (renames, moves, and
   reformatting are distinguished from behavior changes); `kupl patch` applies
   component-granular changes. Models edit components, not line ranges.
7. **One way to do it.** No macros (v1), no operator overloading beyond std numeric
   contracts, no implicit conversions, one loop (`for`), one string type. Small
   language, few decisions per token — fewer degrees of freedom means less
   hallucination surface and more reliable review.
8. **Capability security as AI containment** — generated code's blast radius is
   bounded by the capabilities the human wired in.

---

## 8. Keywords (complete v0.1 set, 52)

**Declarations:** `component` `app` `contract` `system` `type` `fun` `kernel`
`let` `var` `const` `module` `use` `pub` `new`

**Component body:** `intent` `fulfills` `requires` `prop` `in` `out` `state`
`on` `expose` `emit` `send` `wire` `spawn` `supervise` `law` `example` `test`
`expect`

**Control flow:** `if` `else` `match` `for` `while` `break` `continue`
`return` `defer`

**Effects & contracts:** `uses` `cap` `where` `forall`

**Concurrency & placement:** `async` `await` `par` `at` `after` `every`

**System/low tier:** `low` `asm` `unsafe`

**Literals/misc:** `true` `false` `self` `as`

(`Option`/`Result`/`Some`/`None`/`Ok`/`Err` are std types, not keywords.
`restart`, `escalate`, `on_failure`, `never`, `start`, `stop` are contextual
keywords valid only in their clauses — keeps the reserved set small.)

**Operators:** `+ - * / % == != < <= > >= && || ! = += -= *= /= .. ..= -> => ? . :: |`
plus `|>` (pipeline). Precedence is conventional; the formatter always
parenthesizes mixed `&&`/`||`.

---

## 9. Grammar sketch (EBNF, core)

The full grammar will be LL(2)-parseable and ambiguity-free by construction (a
property tested in CI: every valid AST renders to source that reparses to the
same AST).

```ebnf
file        = module_decl? use_decl* item* ;
module_decl = "module" path NEWLINE ;
use_decl    = "use" path ("as" IDENT)? NEWLINE ;
item        = component | contract | type_decl | fun_decl | const_decl ;

component   = ("system" | "low")? ("component" | "app") IDENT generics? body ;
body        = "{" intent fulfills* requires* port* prop* state* handler*
              expose* fun_decl* example_or_test* "}" ;

intent      = "intent" STRING NEWLINE ;
fulfills    = "fulfills" type_ref ("," type_ref)* NEWLINE ;
requires    = "requires" binding ("," binding)* NEWLINE ;
port        = ("in" | "out") IDENT ":" type NEWLINE ;
prop        = "prop" binding ("=" expr)? NEWLINE ;
state       = "state" binding ("=" expr)? NEWLINE ;
handler     = "on" (IDENT pattern? | "start" | "stop"
              | "every" duration | "after" duration) block ;
expose      = "expose" fun_decl ;

fun_decl    = "pub"? ("fun" | "kernel" "fun" | "low" "fun" | "async" "fun")
              IDENT generics? "(" params? ")" effects? ("->" type)? block ;
effects     = "uses" effect ("," effect)* ;

stmt        = let | var_assign | expr_stmt | if | match | for | while
            | "return" expr? | "break" | "continue" | "defer" block
            | "emit" IDENT "(" args? ")" | "send" expr
            | "wire" port_ref "->" port_ref
            | "supervise" IDENT policy ;

expr        = literal | path | call | field_access | index | lambda
            | "match" expr "{" arm+ "}" | "if" expr block ("else" block)?
            | "par" "{" expr ("," expr)* "}" | "at" "(" target ")" expr
            | "await" expr | expr "?" | expr "|>" expr | binary | unary ;

type        = path generics_args? | "(" type ("," type)* ")"      (* tuple *)
            | "{" field_list "}"                                   (* record *)
            | type "|" type                                        (* union *)
            | "Tensor" "[" dtype "," shape "]" ;
```

Statement terminator: newline (with the usual continuation rules: an expression
continues if the line ends in an operator, comma, or open bracket). The formatter
makes this unambiguous in practice; semicolons are not part of the language.

---

## 10. Worked example: a small realistic slice

```kupl
module todo

use std.http
use std.sql

type Todo = { id: TodoId, title: Str, done: Bool }
type TodoId = new Str

contract TodoRepo {
    intent "Durable storage for todos."
    expose fun all() uses db.read -> List[Todo]
    expose fun save(t: Todo) uses db.write -> Result[Unit, sql.Error]
}

component SqlTodoRepo fulfills TodoRepo {
    intent "TodoRepo backed by any SQL database via cap.Sql."
    requires db: cap.Sql

    expose fun all() uses db.read -> List[Todo] {
        db.query("select id, title, done from todos").rows[Todo]()
    }

    expose fun save(t: Todo) uses db.write -> Result[Unit, sql.Error] {
        db.exec("insert into todos values (?, ?, ?) on conflict(id) do update …",
                t.id, t.title, t.done)
    }
}

component TodoList {
    intent "Holds the visible todo list; filters on demand; persists changes."
    requires repo: TodoRepo

    in  filter: Str
    in  toggle: TodoId
    out shown: List[Todo]

    state todos: List[Todo] = []

    on start {
        todos = await repo.all()
        emit shown(todos)
    }

    on filter(q) {
        emit shown(todos.filter(fn t { t.title.contains(q) }))
    }

    on toggle(id) {
        todos = todos.map(fn t { if t.id == id { t with done: !t.done } else { t } })
        let changed = todos.find(fn t { t.id == id })!
        await repo.save(changed)?
        emit shown(todos)
    }

    example {
        given repo = FakeRepo([Todo(id: "1", title: "milk", done: false)])
        send toggle("1")
        expect shown.last().find(fn t { t.id == "1" })!.done == true
    }
}

app TodoApp {
    intent "Todo web app: SQL storage, HTTP UI."
    requires db: cap.Sql, server: cap.HttpServer

    let repo = SqlTodoRepo(db)
    let list = TodoList(repo)
    let ui   = TodoPage()          // a ui-kit component; renders shown, emits toggle

    wire ui.toggle   -> list.toggle
    wire ui.search   -> list.filter
    wire list.shown  -> ui.todos

    supervise repo restart on_failure max 3 in 1m
    supervise list restart on_failure
}
```

Note what Builder gets for free: every box on its canvas (`TodoList`, `SqlTodoRepo`,
`TodoPage`), every arrow (`wire`), every property panel (`prop`, `requires`), and
every doc tooltip (`intent`) is literally this source code.

---

## 11. Semantics summary (normative once spec'd)

- **Evaluation:** strict, left-to-right, well-defined argument order.
- **Data:** value semantics for all app-tier data; structural equality; persistent
  collections with O(log n) update via sharing.
- **Messages:** delivered per-sender in FIFO order; at-most-once locally; ports are
  typed channels with configurable buffering (`out value: Int (latest)` keeps only
  the newest — the UI-friendly default is unbounded FIFO).
- **State:** handler executions on one instance are serialized (no intra-component
  races); `state` is invisible externally.
- **Failure:** `panic` unwinds the current component instance only; supervision
  decides restart; `Result` for expected errors.
- **Numerics:** `Int` wraps are a panic (checked); `f*` follow IEEE-754; kernel
  reassociation is opt-in per kernel (`@fastmath`).
- **Memory:** app tier = per-component heaps, generational GC per heap (pauses are
  per-component, bounded, and never global); system tier = ownership; messages
  move (zero-copy) when the sender no longer uses the value, else copy.
- **Modules:** one module per file, `use` is explicit, no cyclic module deps
  (cycles between components are fine — they're wiring, not imports).

## 12. Open questions (to resolve before spec v1.0)

1. Syntax for UI trees: is nesting components enough or do we want JSX-like literal
   composition sugar? (Leaning: a `render` block that is itself just component
   construction — no separate template language.)
2. `Int` default: 64-bit checked vs BigInt-by-default (Python-style)? Leaning i64
   checked; BigInt in std.
3. Effect granularity: is `db.read`/`db.write` the right grain, or user-definable
   effect hierarchies from day one?
4. Hot-swap semantics for `state` migration on component upgrade (Erlang's
   `code_change` equivalent) — needed by Builder live-editing.
5. Package identity & registry: content-addressed packages with signed manifests —
   spec in TOOLCHAIN.md, needs a decision on namespace governance.
