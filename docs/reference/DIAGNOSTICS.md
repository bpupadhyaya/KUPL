# KUPL Diagnostics Index

**Version:** 1.0-alpha. Every diagnostic has a stable code, a precise source
span, and (via `kupl check --json`) a machine-readable form. Codes are grouped
by compiler phase; codes are never reused with a different meaning.

Severity: **E** = error, **W** = warning.

## K00xx — Lexer

| Code | Sev | Meaning |
|---|---|---|
| K0001 | E | unexpected character |
| K0002 | E | unterminated block comment |
| K0003 | E | invalid float literal |
| K0004 | E | integer literal does not fit in Int (64-bit) |
| K0005 | E | unterminated string literal |
| K0006 | E | unknown escape sequence |
| K0007 | E | unterminated `{` interpolation in string |
| K0008 | E | NUL (`\0` or a raw NUL byte) in a string literal — `Str` is NUL-free UTF-8 text |
| K0009 | E | integer literal out of range for its width suffix (`256u8`) |
| K0010 | E | single `&` is not an operator (did you mean `&&`?) |

## K01xx — Parser

| Code | Sev | Meaning |
|---|---|---|
| K0100 | E | expected *token*, found *other* |
| K0101 | E | expected identifier |
| K0102 | E | expected end of statement |
| K0103 | E | expected a declaration (`fun`, `type`, `component`, `app`, …) |
| K0104 | E | `intent` expects a string literal |
| K0105 | E | `on` expects a port name, `start`, `stop`, `every <dur>`, or `after <dur>` |
| K0106 | E | example blocks contain `send`, `expect`, and `advance` steps |
| K0107 | E | unexpected token in component body |
| K0108 | E | invalid assignment target |
| K0109 | E | expected `,` or newline between match arms |
| K0110 | E | expected an expression |
| K0111 | E | expected integer after `-` in pattern |
| K0112 | E | string patterns cannot contain interpolation |
| K0113 | E | expected a pattern |
| K0114 | E | expected a type |
| K0115 | E | `law` expects a name string |
| K0116 | E | unexpected token in contract body |
| K0117 | E | expected `restart` after the child name in `supervise` |
| K0118 | E | unknown restart policy (use `on_failure` or `never`) |
| K0119 | E | an `ai fun` body is `intent "…"` optionally followed by `model "…"` |
| K0120 | E | malformed duration literal (expected `<int><unit>`, unit in `ms`/`s`/`m`/`h`) |
| K0121 | E | expression nesting too deep (bounded to keep the checker fast on pathological input) |

## K02xx — Type & semantic checker

| Code | Sev | Meaning |
|---|---|---|
| K0200 | E | type mismatch (expected X, found Y — with context) |
| K0201 | E | type defined more than once |
| K0202 | E | constructor defined more than once |
| K0203 | E | function defined more than once |
| K0204 | E | port declared twice |
| K0205 | E | unknown type |
| K0206 | E | unknown generic type / wrong arity |
| K0207 | E | child declared twice |
| K0208 | E | unknown component |
| K0209 | E | duplicate `on` handler |
| K0210 | E | `on start`/`on stop` take no parameter |
| K0211 | E | `on X`: no such `in` port (hints if X is an `out` port) |
| K0212 | E | Event port has no payload — remove the handler parameter |
| K0213 | E | `wire` references unknown child |
| K0214 | E | component has no such `in`/`out` port |
| K0215 | E | no such prop / too many constructor arguments |
| K0216 | E | missing required prop when constructing a component |
| K0217 | E | `send`: no such `in` port |
| K0218 | E | `send` to an Event port takes no payload |
| K0219 | E | `send` to a typed port needs a payload |
| K0220 | E | unknown variable |
| K0221 | E | variable is immutable (`let`; use `var` or `state`) |
| K0222 | E | `+=`-family needs a numeric variable |
| K0223 | E | field assignment not supported (record update is planned) |
| K0224 | E | `for` needs a Range or List |
| K0225 | E | `emit` is only valid inside a component |
| K0226 | E | no such `out` port (hints if it is an `in` port) |
| K0227 | E | `emit` to an Event port takes no payload |
| K0228 | E | `emit` to a typed port needs a payload |
| K0229 | E | `break`/`continue` outside of a loop |
| K0230 | E | type has no such field |
| K0231 | E | multi-variant type — use `match` to access fields |
| K0232 | E | cannot infer receiver type for field access — annotate |
| K0233 | E | value of this type has no fields |
| K0234 | E | cannot order values of this type |
| K0235 | E | arithmetic needs Int or Float operands |
| K0236 | E | unary `-` needs Int or Float |
| K0237 | E | `?` not allowed in handlers — use `match` |
| K0238 | E | `?` requires the function to return a Result |
| K0240 | E | unknown name |
| K0241 | E | named arguments only for constructors and props |
| K0242 | E | wrong number of call arguments |
| K0243 | E | wrong number of constructor fields |
| K0244 | E | constructor has no such field |
| K0245 | E | `sum` needs List[Int] or List[Float] |
| K0246 | E | `join` needs List[Str] |
| K0247 | E | component does not expose that function |
| K0248 | E | cannot infer receiver type for method call — annotate |
| K0249 | E | type has no such method (suggests a close built-in method / UFCS fn — "did you mean `x`?") |
| K0250 | E | wrong number of method arguments |
| K0251 | E | `Some` pattern takes exactly one argument |
| K0252 | E | `None` pattern takes no arguments |
| K0253 | E | `Ok`/`Err` pattern takes exactly one argument |
| K0254 | E | unknown constructor in pattern |
| K0255 | E | wrong number of pattern fields |
| K0256 | E | `match` over unbounded type needs a catch-all arm |
| K0257 | E | non-exhaustive `match` (lists the missing variants) |
| K0258 | E | an or-pattern alternative cannot bind variables |
| K0267 | E | a required parameter follows one with a default |
| K0268 | E | positional argument after a named argument |
| K0269 | E | argument given more than once |
| K0273 | E | no parameter of that name |
| K0274 | E | missing argument for a required parameter |
| K0275 | E | a parameter cannot have a default value here — only top-level `fun` parameters support defaults, not constructor fields, methods, or contract signatures |
| K0276 | E | `forall` binder's type has no property-test generator (supported: `Int`, `Bool`, `Float`, `Str`, `List[T]`, `Option[T]`, and user-defined record/enum types) |
| K0260 | E | contract defined more than once |
| K0261 | E | `fulfills` names an unknown contract |
| K0262 | E | fulfilling component does not expose a required function |
| K0263 | E | exposed signature does not match the contract |
| K0264 | E | exposed effects exceed the contract's budget |
| K0265 | E | `supervise` references unknown child |
| K0266 | E | timer duration must be positive |
| K0270 | E | `ai fun` must declare a return type (it defines the structured output) |
| K0271 | E | `ai fun` return type not representable as structured output (unsupported/recursive/multi-variant type) |
| K0272 | E | `ai fun` tool is not a monomorphic top-level function with representable parameter/return types |
| K0277 | E | method (private or exposed) is defined more than once in a component |
| K0278 | E | component is defined more than once |
| K0279 | W | a fulfilling method calls a value of function type — its effects cannot be verified against the contract's effect budget |
| K0280 | E | a parameter's default value references another parameter of the same function — defaults are evaluated at the call site, not the function's own scope |
| K0281 | E | a generic function's own type parameter is narrowed/specialized inside its own body — a generic function must treat its type parameters abstractly |
| K0282 | E | component method declares type parameters — component methods do not yet support generics (only top-level `fun`s can be generic) |
| K0283 | E | prop is declared more than once |
| K0284 | E | method is declared more than once in a contract |
| K0285 | E | the same `wire` connection is declared more than once — each emitted value would be delivered twice |
| K0286 | E | the same child is `supervise`d more than once — a `restart on_failure` declaration silently wins over a later `restart never` |

## K03xx — Effects & style

| Code | Sev | Meaning |
|---|---|---|
| K0300 | W | component has no `intent` |
| K0301 | E | public function does not declare its effects (`add uses …`) |
| K0302 | W | declared effect is never used |
| K0303 | W | function calls a value of function type — its effects cannot be verified; declare `uses` for any effect it may perform |

## K04xx — Loader (multi-file)

| Code | Sev | Meaning |
|---|---|---|
| K0400 | E | cannot read module file referenced by `use` |
| K0401 | E | dependency version mismatch (`requires 1.2.0 but found 0.9.0`) |
| K0402 | E | command needs `.kupl` source, not an already-compiled `.kx` module |

## K08xx — KVM backend limits

| Code | Sev | Meaning |
|---|---|---|
| K0801 | E | function too large for KVM v0 (more than 256 registers) |
| K0802 | E | unknown assignment target on the KVM (internal safety net; every free variable a closure captures is already bound as a local before this arm could ever run) |
| K0803 | E | unsupported assignment target on the KVM (internal safety net; the checker rejects any assignment target this could fire for before compilation reaches the KVM) |
| K0804 | E | `forall` runs only under `kupl test` (interpreter); not compiled to the KVM |
| K0805 | E | component has too many props + state fields + children for KVM v0 (more than 256 total) |
| K0806 | E | chunk has too many distinct constants for KVM v0 (more than 65536) |
| K0807 | E | module has too many functions/closures/component methods for KVM v0 (more than 65536 chunks) |

## K09xx — Runtime

| Code | Sev | Meaning |
|---|---|---|
| K0900 | E | panic (message + source span; exit code 101). Common panics: `integer overflow in addition/subtraction/multiplication/negation/abs/sum` · `division by zero` · `remainder by zero` · `expectation failed` · `no match arm matched` · `list index out of range` · `tensor length mismatch` · user `panic(msg)` |

A panic inside a component whose parent declared
`supervise child restart on_failure` does **not** exit: the child's state is
reset, `on start` re-runs, and `[supervise] … restarted after panic: …` is
printed to stderr.
