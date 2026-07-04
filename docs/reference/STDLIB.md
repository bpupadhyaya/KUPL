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
| `Map()` | `() -> Map[K, V]` | empty map |
| `Set()` / `Set(xs)` | `() -> Set[T]` / `(List[T]) -> Set[T]` | duplicates dropped |
| `tensor(xs)` | `(List[Float]) -> Tensor` | Int elements are accepted and widened |
| `zeros(n)` | `(Int) -> Tensor` | n zeros; negative n panics |
| `arange(n)` | `(Int) -> Tensor` | `[0.0, 1.0, …, n-1]` |
| `read_file(path)` | `(Str) -> Result[Str, Str]` — **uses `io.fs`** | whole file as text; `Err` carries the OS message |
| `write_file(path, s)` | `(Str, Str) -> Result[Unit, Str]` — **uses `io.fs`** | creates or truncates |
| `append_file(path, s)` | `(Str, Str) -> Result[Unit, Str]` — **uses `io.fs`** | creates if missing |
| `delete_file(path)` | `(Str) -> Result[Unit, Str]` — **uses `io.fs`** | |
| `file_exists(path)` | `(Str) -> Bool` — **uses `io.fs`** | any filesystem entry |
| `json_parse(text)` | `(Str) -> Result[Json, Str]` | pure; `Err` on malformed input |
| `json_stringify(j)` | `(Json) -> Str` | compact; object key order preserved |
| `args()` | `() -> List[Str]` — **uses `io.env`** | the program's command-line arguments |
| `env_var(name)` | `(Str) -> Option[Str]` — **uses `io.env`** | environment variable, or `None` |
| `eprint(v)` | `(any) -> Unit` — **uses `io`** | prints Display form + newline to stderr |
| `exit(code)` | `(Int) -> !` | flushes stdout and terminates the process |

`args`/`env_var` read ambient input, so they carry the `io.env` effect (a
sub-effect of `io`). `args()` is everything after `--` when run through the
toolchain (`kupl run prog.kupl -- a b`) and `argv[1..]` for a native binary.
`exit` diverges (like `panic`) so it needs no effect.

File builtins carry the `io.fs` effect (a sub-effect of `io`, so `uses io`
covers them; `uses io.fs` is the precise capability). The `Err` message is a
human-readable OS description whose exact wording is engine/platform-dependent —
match `Ok`/`Err` structurally rather than on the text.

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
| `.par_map(f)` | `(fn(T) -> U) -> List[U]` | parallel map — independent per element; deterministic (input order) |
| `.par_filter(f)` | `(fn(T) -> Bool) -> List[T]` | parallel filter; deterministic (input order) |
| `.par_each(f)` | `(fn(T) -> U) -> Unit` | parallel for-effect; result discarded |
| `.find(f)` | `(fn(T) -> Bool) -> Option[T]` | first match |
| `.sum()` | `-> T` | Int or Float lists; Int overflow panics |
| `.contains(v)` | `(T) -> Bool` | structural equality |
| `.push(v)` | `(T) -> List[T]` | returns a **new** list (lists are immutable) |
| `.fold(init, f)` | `(A, fn(A, T) -> A) -> A` | left fold |
| `.any(f)` / `.all(f)` | `(fn(T) -> Bool) -> Bool` | short-circuiting |
| `.sort()` | `-> List[T]` | Int/Float/Str elements; stable ascending |
| `.take(n)` / `.drop(n)` | `(Int) -> List[T]` | clamped to list bounds |
| `.get(i)` | `(Int) -> Option[T]` | safe indexing |
| `.index_of(v)` | `(T) -> Option[Int]` | first occurrence |
| `.first()` / `.last()` | `-> Option[T]` | |
| `.reverse()` | `-> List[T]` | |
| `.join(sep)` | `(Str) -> Str` | elements rendered with Display |
| `.is_empty()` | `-> Bool` | |
| `.concat(other)` | `(List[T]) -> List[T]` | appends another list |
| `.unique()` | `-> List[T]` | drops later duplicates, preserves order |
| `.init()` / `.tail()` | `-> List[T]` | all but the last / all but the first |
| `.product()` | `-> T` | Int or Float lists; Int overflow panics |
| `.min()` / `.max()` | `-> Option[T]` | Int/Float/Str elements; `None` if empty |
| `.flatten()` | `List[List[T]] -> List[T]` | one level of nesting |
| `.count(f)` | `(fn(T) -> Bool) -> Int` | how many satisfy `f` |
| `.flat_map(f)` | `(fn(T) -> List[U]) -> List[U]` | map then flatten |
| `.window(n)` | `(Int) -> List[List[T]]` | sliding windows of width n (n ≥ 1) |
| `.chunk(n)` | `(Int) -> List[List[T]]` | consecutive chunks of size n (last may be shorter) |

### Str

| Method | Signature | Notes |
|---|---|---|
| `.len()` | `-> Int` | counts characters, not bytes |
| `.contains(s)` | `(Str) -> Bool` | |
| `.starts_with(s)` | `(Str) -> Bool` | |
| `.to_upper()` / `.to_lower()` | `-> Str` | |
| `.trim()` | `-> Str` | strips ASCII whitespace at both ends |
| `.split(sep)` | `(Str) -> List[Str]` | non-empty separator |
| `.ends_with(s)` | `(Str) -> Bool` | |
| `.replace(from, to)` | `(Str, Str) -> Str` | all occurrences |
| `.chars()` | `-> List[Str]` | one-character strings |
| `.repeat(n)` | `(Int) -> Str` | n ≥ 0 |
| `.parse_int()` | `-> Option[Int]` | `None` on any malformed input |
| `.parse_float()` | `-> Option[Float]` | |
| `.is_empty()` | `-> Bool` | |
| `.reverse()` | `-> Str` | by characters, not bytes |
| `.lines()` | `-> List[Str]` | splits on `\n`, strips a trailing `\r`; no trailing empty line |
| `.index_of(sub)` | `(Str) -> Option[Int]` | character index of the first occurrence |
| `.count(sub)` | `(Str) -> Int` | non-overlapping occurrences (non-empty `sub`) |
| `.slice(start, end)` | `(Int, Int) -> Str` | substring by character index, clamped |
| `.pad_left(width, fill)` / `.pad_right(width, fill)` | `(Int, Str) -> Str` | pad to `width` chars with the first char of `fill` |

`+` concatenates two Str values; `"…{expr}…"` interpolation renders any value.

### Int

| Method | Signature | Notes |
|---|---|---|
| `.to_str()` | `-> Str` | |
| `.to_float()` | `-> Float` | |
| `.abs()` | `-> Int` | `Int.min.abs()` panics |
| `.min(other)` / `.max(other)` | `(Int) -> Int` | |
| `.pow(exp)` | `(Int) -> Int` | `exp ≥ 0`; overflow panics |
| `.gcd(other)` | `(Int) -> Int` | greatest common divisor (non-negative) |
| `.clamp(lo, hi)` | `(Int, Int) -> Int` | `lo ≤ hi` required |
| `.sign()` | `-> Int` | `-1` / `0` / `1` |
| `.is_even()` / `.is_odd()` | `-> Bool` | |

### Float

| Method | Signature | Notes |
|---|---|---|
| `.to_str()` | `-> Str` | |
| `.to_int()` | `-> Int` | truncates toward zero |
| `.abs()` / `.sqrt()` | `-> Float` | |
| `.floor()` / `.ceil()` / `.round()` | `-> Float` | |
| `.min(other)` / `.max(other)` / `.pow(exp)` | `(Float) -> Float` | |
| `.log()` / `.log10()` / `.exp()` | `-> Float` | natural log / base-10 log / e^x |
| `.sin()` / `.cos()` / `.tan()` | `-> Float` | radians |
| `.clamp(lo, hi)` | `(Float, Float) -> Float` | `lo ≤ hi` required |
| `.sign()` | `-> Float` | `1.0` / `-1.0` / preserves `0.0`, `-0.0`, `NaN` |
| `.is_nan()` / `.is_infinite()` | `-> Bool` | |

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

### Map[K, V]

Constructed with `Map()` (empty) then `.insert`. Insertion-ordered; updating an
existing key keeps its position. Equality is order-insensitive.

| Method | Signature | Notes |
|---|---|---|
| `.insert(k, v)` | `(K, V) -> Map[K, V]` | new map; updates in place positionally |
| `.get(k)` | `(K) -> Option[V]` | |
| `.remove(k)` | `(K) -> Map[K, V]` | |
| `.contains_key(k)` | `(K) -> Bool` | |
| `.keys()` / `.values()` | `-> List[K]` / `-> List[V]` | insertion order |
| `.len()` | `-> Int` | |
| `.is_empty()` | `-> Bool` | |
| `.get_or(k, default)` | `(K, V) -> V` | value for `k`, or `default` |
| `.merge(other)` | `(Map[K, V]) -> Map[K, V]` | `other`'s entries override |
| `.map_values(f)` | `(fn(V) -> W) -> Map[K, W]` | transform values, keep keys/order |

### Set[T]

Constructed with `Set()` (empty) or `Set(list)` (duplicates dropped).
Insertion-ordered; equality is order-insensitive.

| Method | Signature | Notes |
|---|---|---|
| `.insert(v)` / `.remove(v)` | `(T) -> Set[T]` | new set |
| `.contains(v)` | `(T) -> Bool` | |
| `.union(s)` / `.intersect(s)` / `.difference(s)` | `(Set[T]) -> Set[T]` | |
| `.to_list()` | `-> List[T]` | insertion order |
| `.len()` | `-> Int` | |
| `.is_empty()` | `-> Bool` | |
| `.is_subset(other)` | `(Set[T]) -> Bool` | every element is in `other` |

### Json

A built-in ADT (available without an import, via the prelude) for structured
data. `json_parse` / `json_stringify` convert to and from text; build and
inspect values with ordinary constructors and `match`.

```kupl
type Json = JNull | JBool(b: Bool) | JNum(n: Float) | JStr(s: Str)
          | JArr(items: List[Json]) | JObj(fields: Map[Str, Json])
```

- `json_parse(text) -> Result[Json, Str]` — `Err(message)` on malformed input.
- `json_stringify(j) -> Str` — compact output; **object key order is
  preserved** and whole numbers print without a decimal point (`1`, not `1.0`),
  so `json_stringify(json_parse(s))` is stable.
- Numbers parse to `JNum(Float)`. Objects are `JObj(Map[Str, Json])` (insertion
  order kept, last key wins).
- Runs on the interpreter, KVM, `.kx`, and bundles. The **native** backend
  (`kupl native`) does not yet support the JSON builtins and reports a clear
  error — use `kupl run`/`--vm`/`bundle` for JSON programs.

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
