# KUPL Standard Library Reference

**Version:** 1.0-alpha. Everything here is built into the language runtime and
available without imports, identically on all engines (interpreter, KVM,
native). Errors below marked *panics* terminate the component (or program)
unless supervised.

## Built-in functions

| Function | Signature | Notes |
|---|---|---|
| `print(v)` | `(any) -> Unit` — **uses `io`** | prints Display form + newline |
| `to_str(v)` | `(any) -> Str` | Display form of any value |
| `panic(msg)` | `(Str) -> !` | aborts the instance/program with `msg` |
| `tensor(xs)` | `(List[Float]) -> Tensor` | Int elements are accepted and widened |
| `zeros(n)` | `(Int) -> Tensor` | n zeros; negative n panics |
| `arange(n)` | `(Int) -> Tensor` | `[0.0, 1.0, …, n-1]` |

## Built-in constructors

| Constructor | Produces |
|---|---|
| `Some(v)` / `None` | `Option[T]` |
| `Ok(v)` / `Err(e)` | `Result[T, E]` |

## Methods by type

### List[T]

| Method | Signature | Notes |
|---|---|---|
| `.len()` | `-> Int` | |
| `.map(f)` | `(fn(T) -> U) -> List[U]` | |
| `.filter(f)` | `(fn(T) -> Bool) -> List[T]` | |
| `.find(f)` | `(fn(T) -> Bool) -> Option[T]` | first match |
| `.sum()` | `-> T` | Int or Float lists; Int overflow panics |
| `.contains(v)` | `(T) -> Bool` | structural equality |
| `.push(v)` | `(T) -> List[T]` | returns a **new** list (lists are immutable) |
| `.first()` / `.last()` | `-> Option[T]` | |
| `.reverse()` | `-> List[T]` | |
| `.join(sep)` | `(Str) -> Str` | elements rendered with Display |

### Str

| Method | Signature | Notes |
|---|---|---|
| `.len()` | `-> Int` | counts characters, not bytes |
| `.contains(s)` | `(Str) -> Bool` | |
| `.starts_with(s)` | `(Str) -> Bool` | |
| `.to_upper()` / `.to_lower()` | `-> Str` | |
| `.trim()` | `-> Str` | strips ASCII whitespace at both ends |
| `.split(sep)` | `(Str) -> List[Str]` | non-empty separator |

`+` concatenates two Str values; `"…{expr}…"` interpolation renders any value.

### Int

| Method | Signature | Notes |
|---|---|---|
| `.to_str()` | `-> Str` | |
| `.to_float()` | `-> Float` | |
| `.abs()` | `-> Int` | `Int.min.abs()` panics |

### Float

| Method | Signature | Notes |
|---|---|---|
| `.to_str()` | `-> Str` | |
| `.to_int()` | `-> Int` | truncates toward zero |
| `.abs()` / `.sqrt()` | `-> Float` | |

### Option[T]

| Method | Signature | Notes |
|---|---|---|
| `.is_some()` / `.is_none()` | `-> Bool` | |
| `.unwrap_or(default)` | `(T) -> T` | |

### Result[T, E]

| Method | Signature | Notes |
|---|---|---|
| `.is_ok()` / `.is_err()` | `-> Bool` | |
| `.unwrap_or(default)` | `(T) -> T` | |

Prefer `match` or `?` for handling; `?` propagates the `Err` to the caller.

### Tensor

Rank-1 f64 tensors. Operations run as native numeric loops in every engine.

| Method | Signature | Notes |
|---|---|---|
| `.len()` | `-> Int` | |
| `.get(i)` | `(Int) -> Float` | out of range panics |
| `.sum()` / `.mean()` / `.max()` / `.min()` | `-> Float` | mean/max/min of empty tensor panics |
| `.dot(t)` | `(Tensor) -> Float` | length mismatch panics |
| `.scale(k)` | `(Float) -> Tensor` | |
| `.map(f)` | `(fn(Float) -> Float) -> Tensor` | |
| `.to_list()` | `-> List[Float]` | |

Operators: `t1 + t2`, `t1 - t2`, `t1 * t2`, `t1 / t2` are **elementwise**
(length mismatch panics). Display: `Tensor([1.0, 2.5])`.

### Component instances

Any exposed function is callable as a method on the instance reference:
`store.put("k", "v")`, `store.all()`. The call is synchronous and runs with
the instance's state.

## Display forms (what `print`, `to_str`, and `{…}` produce)

| Value | Form |
|---|---|
| `42` | `42` |
| `3.5`, `12.0`, `0.1 + 0.2` | `3.5`, `12.0`, `0.30000000000000004` (shortest round-trip) |
| `true`, `()` | `true`, `()` |
| `"hi"` | `hi` bare · `"hi"` inside containers |
| `[1, "a"]` | `[1, "a"]` |
| `Some(3)`, `None`, `Ok("x")` | `Some(3)`, `None`, `Ok("x")` |
| `Circle(r: 1.5)` | `Circle(1.5)` |
| `0..5`, `0..=5` | `0..5`, `0..=5` |
| tensors | `Tensor([0.0, 1.0])` |
| functions, instances | `<fn>`, `<component #0>` |
