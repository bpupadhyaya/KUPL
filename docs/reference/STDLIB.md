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
| `list_dir(path)` | `(Str) -> Result[List[Str], Str]` — **uses `io.fs`** | entry names (no `.`/`..`), **sorted**; `Err` if not a directory |
| `make_dir(path)` | `(Str) -> Result[Unit, Str]` — **uses `io.fs`** | create a directory (incl. parent dirs); `Ok` if it already exists |
| `remove_dir(path)` | `(Str) -> Result[Unit, Str]` — **uses `io.fs`** | remove a directory **recursively** |
| `path_join(a, b)` | `(Str, Str) -> Str` | join with one `/`; empty `a` or absolute `b` → `b`; pure |
| `path_base(p)` / `path_dir(p)` | `(Str) -> Str` | final component / everything before the last `/`; pure |
| `path_ext(p)` | `(Str) -> Str` | extension incl. the dot (`.txt`), or `""` (a leading-dot dotfile has none); pure |
| `json_parse(text)` | `(Str) -> Result[Json, Str]` | pure; `Err` on malformed input |
| `json_stringify(j)` | `(Json) -> Str` | compact; object key order preserved |
| `args()` | `() -> List[Str]` — **uses `io.env`** | the program's command-line arguments |
| `env_var(name)` | `(Str) -> Option[Str]` — **uses `io.env`** | environment variable, or `None` |
| `read_line()` | `() -> Option[Str]` — **uses `io.env`** | one line from stdin (trailing newline stripped); `None` at EOF |
| `read_all()` | `() -> Str` — **uses `io.env`** | all of stdin as one string (empty at EOF) |
| `eprint(v)` | `(any) -> Unit` — **uses `io`** | prints Display form + newline to stderr |
| `exit(code)` | `(Int) -> !` | flushes stdout and terminates the process |
| `random_ints(seed, count)` | `(Int, Int) -> List[Int]` | deterministic; `count ≤ 0` → empty |
| `random_floats(seed, count)` | `(Int, Int) -> List[Float]` | each in `[0.0, 1.0)`; deterministic |
| `shuffle(seed, xs)` | `(Int, List[T]) -> List[T]` | deterministic Fisher-Yates permutation |
| `now()` | `() -> Int` — **uses `io.time`** | current Unix epoch seconds (wall clock) |
| `date_make(y, mo, d, h, mi, s)` | `(Int×6) -> Int` | compose UTC components → epoch seconds; pure |
| `format_time(epoch)` | `(Int) -> Str` | UTC `YYYY-MM-DD HH:MM:SS`; pure |
| `date_iso(epoch)` | `(Int) -> Str` | UTC ISO-8601 `YYYY-MM-DDTHH:MM:SSZ`; pure |
| `parse_iso(s)` | `(Str) -> Result[Int, Str]` | parse `YYYY-MM-DD`, `…THH:MM:SS`, or `… HH:MM:SS` (optional `Z`) → epoch; `Err` if malformed; pure |
| `year_of/month_of/day_of(epoch)` | `(Int) -> Int` | UTC calendar fields; pure |
| `hour_of/minute_of/second_of(epoch)` | `(Int) -> Int` | UTC time fields; pure |
| `weekday_of(epoch)` | `(Int) -> Int` | 0 = Sunday … 6 = Saturday; pure |
| `yearday_of(epoch)` | `(Int) -> Int` | 1 = Jan 1 … 365/366; pure |
| `base64_encode(s)` / `hex_encode(s)` | `(Str) -> Str` | encode the UTF-8 bytes; pure |
| `base64_decode(s)` / `hex_decode(s)` | `(Str) -> Result[Str, Str]` | `Err` on malformed input or non-UTF-8 |
| `hash_fnv(s)` | `(Str) -> Int` | FNV-1a 64-bit; stable, non-cryptographic |
| `csv_parse(text)` | `(Str) -> List[List[Str]]` | RFC 4180; handles quoted fields |
| `csv_stringify(rows)` | `(List[List[Str]]) -> Str` | quotes fields with `,` `"` or newline |
| `url_encode(s)` | `(Str) -> Str` | percent-encode; space → `%20`; keeps `A-Za-z0-9-_.~` |
| `url_decode(s)` | `(Str) -> Result[Str, Str]` | reverse `%XX`; `+` → space; `Err` on bad input |
| `query_parse(s)` | `(Str) -> List[List[Str]]` | `a=1&b=2` → `[[a,1],[b,2]]`, decoded |
| `query_build(pairs)` | `(List[List[Str]]) -> Str` | encode `[key, value]` pairs into `a=1&b=2` |
| `big(x)` | `(Int) -> BigInt` / `(Str) -> BigInt` | arbitrary-precision integer (panics on a malformed string); pure |
| `.pow/.abs/.sign/.is_negative` on BigInt | methods | power (Int exp), absolute value, `-1/0/1`, sign test |
| `rat(n, d)` | `(Int, Int) -> Rational` | exact fraction `n/d`, reduced (panics if `d == 0`); pure |
| `.num/.den/.to_float/.recip` on Rational | methods | numerator/denominator (BigInt), nearest Float, reciprocal |
| `exec(program, args)` | `(Str, List[Str]) -> Result[Str, Str]` — **uses `io.proc`** | run a program (argv, no shell); `Ok` = stdout on exit 0 |
| `http_get(url)` | `(Str) -> Result[Str, Str]` — **uses `io.net`** | GET via system curl; `Ok` = body |
| `http_post(url, body)` | `(Str, Str) -> Result[Str, Str]` — **uses `io.net`** | POST via system curl |
| `http_serve(port, handler)` | `(Int, fn(Str, Str) -> Str) -> Result[Unit, Str]` — **uses `io.net`** | blocking HTTP server; `handler(method, path)` -> body |
| `re_match(pat, text)` | `(Str, Str) -> Bool` | regex search (`^…$` for full match) |
| `re_find(pat, text)` | `(Str, Str) -> Option[Str]` | first match substring |
| `re_find_all(pat, text)` | `(Str, Str) -> List[Str]` | all non-overlapping matches |
| `re_replace(pat, text, repl)` | `(Str, Str, Str) -> Str` | replace all matches with `repl` |

`args`/`env_var`/`read_line`/`read_all` read ambient input, so they carry the
`io.env` effect (a sub-effect of `io`). `args()` is everything after `--` when
run through the toolchain (`kupl run prog.kupl -- a b`) and `argv[1..]` for a
native binary. `read_line`/`read_all` read standard input — the basis of
Unix-filter programs (`echo … | kupl run filter.kupl`); `read_line` strips the
trailing newline and returns `None` at end of input, so `while let Some(l) =
read_line() { … }` drains stdin.
`exit` diverges (like `panic`) so it needs no effect.

`random_ints` / `random_floats` / `shuffle` are **pure** (no effect): a given
seed always yields the same result (xorshift64\*), so simulations and tests are
reproducible. There is no ambient/global RNG — pass a seed explicitly.

`exec(program, args)` runs a program **without a shell** — `program` and each
element of `args` become the process argv verbatim, so arguments with spaces or
shell metacharacters are passed literally (no word-splitting, globbing, or
injection). It captures stdout: `Ok(stdout)` on exit code 0, otherwise `Err`
carrying the trimmed stderr (or `"exited with status N"` if stderr is empty, or
`"cannot run <program>: …"` if it can't be spawned). It carries the `io.proc`
effect (a sub-effect of `io`). Error *message text* is platform-dependent — match
`Ok`/`Err` structurally rather than on the text.

`http_get` / `http_post` shell out to the system `curl` (the same transport the
AI runtime uses) and carry the `io.net` effect. A non-2xx status or unreachable
host is an ordinary `Err` (message text is platform-dependent). Compiles on the
native backend too (via the system `curl`).

**Regex** (`re_*`) is a pure, self-contained engine: literals, `.`, `* + ?`
(greedy), classes `[a-z]`/`[^…]`, `\d \w \s` (+ `\D \W \S`), anchors
`^`/`$`, alternation `|`, groups `(...)`, and `\`-escapes. `re_match` searches
(anchor with `^…$` for a full match). A malformed pattern **panics** with a
clear message. Compiles on the native backend too (byte-oriented, ASCII-correct).

**Time**: `date_make`, `date_iso`, `parse_iso`, `format_time`, and the `*_of`
extractors are pure, deterministic UTC calendar math (epoch seconds ↔ civil
date, correct for negative/pre-1970 timestamps), byte-identical on every engine
including native. Only `now()` reads the wall clock — it carries the `io.time`
effect and is non-deterministic. No locale or leap seconds.

**Number formatting**: `Float.fmt(decimals) -> Str` renders a fixed-point
decimal, **rounding half away from zero** (`3.14159.fmt(2)` → `"3.14"`,
`2.5.fmt(0)` → `"3"`); `decimals` is clamped to `0..=18` and non-finite inputs
render as `nan`/`inf`/`-inf`. Integer bases: `Int.to_hex/to_binary/to_octal()`
and `to_radix(base)` (2..=36) give a lowercase, prefix-free string (`255.to_hex()`
→ `"ff"`), with a leading `-` for negatives. All byte-identical on every engine
including native. See `examples/format.kupl`.

**BigInt** (`big`): arbitrary-precision integers with `+ - * / %`, comparisons,
and `.pow`/`.abs`/`.sign`/`.is_negative`. Division truncates toward zero and the
remainder takes the dividend's sign (like `Int`). Exact and deterministic on
every engine including native (a from-scratch base-1e9 bignum).

**Rational** (`rat`): exact fractions built on `BigInt`, always stored reduced
with a positive denominator, so equality and `to_string` are canonical. `+ - * /`
and comparisons, plus `.num`/`.den` (BigInt), `.to_float`, and `.recip`. No
rounding error — `rat(1,3) + rat(1,6)` is exactly `rat(1,2)`. Native too.

**Encodings** (`base64_*`, `hex_*`, `hash_fnv`) are pure and byte-identical on
every engine including native. They work on the string's UTF-8 bytes; `*_decode`
returns `Err` on malformed input or if the decoded bytes are not valid UTF-8.
`hash_fnv` is deterministic and stable across runs and engines — good for
bucketing/sharding, not for security.

**CSV** (`csv_parse`/`csv_stringify`) follows RFC 4180: `,` field separator,
`\n` or `\r\n` row endings on input (`\n` on output), quoted fields for
values containing `,` `"` or newlines (with `""` for an embedded quote). A
trailing newline yields no extra row; a blank interior line is a one-field row.
Pure and byte-identical on every engine including native.

**URL** (`url_encode`/`url_decode`) is percent-encoding: `url_encode` keeps the
RFC 3986 unreserved set `A-Za-z0-9-_.~` and encodes everything else including
space as `%20`; `url_decode` reverses `%XX`, treats `+` as space, and returns
`Err` on a malformed escape or non-UTF-8. `query_parse`/`query_build` handle
`key=value&…` pairs (each part url-decoded/encoded). `url_encode`/`url_decode`
run on all engines incl. native, as do the `query_*` helpers.

The **path helpers** (`path_join`/`path_base`/`path_dir`/`path_ext`) are pure
(no effect) and operate lexically on `/`-separated paths — no filesystem access.
`list_dir` returns entry names **sorted** (byte order) so output is deterministic
across engines, platforms, and runs (OS directory order is not).

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
| `.min_by(f)` / `.max_by(f)` | `(fn(T) -> K) -> Option[T]` | element with the smallest/largest key |
| `.flatten()` | `List[List[T]] -> List[T]` | one level of nesting |
| `.count(f)` | `(fn(T) -> Bool) -> Int` | how many satisfy `f` |
| `.flat_map(f)` | `(fn(T) -> List[U]) -> List[U]` | map then flatten |
| `.window(n)` | `(Int) -> List[List[T]]` | sliding windows of width n (n ≥ 1) |
| `.chunk(n)` | `(Int) -> List[List[T]]` | consecutive chunks of size n (last may be shorter) |
| `.sort_by(f)` | `(fn(T) -> Int) -> List[T]` | stable sort by an Int key |
| `.position(f)` | `(fn(T) -> Bool) -> Option[Int]` | index of the first element matching the predicate |
| `.partition(f)` | `(fn(T) -> Bool) -> List[List[T]]` | `[matching, non-matching]`, order preserved |

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
| `.rfind(sub)` | `(Str) -> Option[Int]` | character index of the **last** occurrence |
| `.replace_first(from, to)` | `(Str, Str) -> Str` | replace only the first occurrence |
| `.split_once(sep)` | `(Str) -> Option[List[Str]]` | split at the first `sep` → `[before, after]`, else `None` |
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
| `.to_i8()` … `.to_i64()` / `.to_u8()` … `.to_u64()` | `-> i8`…`u64` | checked narrowing; panics if out of range |
| `.band(x)` / `.bor(x)` / `.bxor(x)` | `(Int) -> Int` | bitwise and / or / xor |
| `.bnot()` | `-> Int` | bitwise complement (`~`) |
| `.shl(n)` | `(Int) -> Int` | left shift; `n` in `0..=63` (else panics) |
| `.to_hex()` / `.to_binary()` / `.to_octal()` | `-> Str` | lowercase, no prefix; negatives get a leading `-` |
| `.to_radix(base)` | `(Int) -> Str` | base `2..=36` (else panics) |
| `.isqrt()` | `-> Int` | integer square root (floor); negative panics |
| `.shr(n)` | `(Int) -> Int` | **arithmetic** right shift (sign-preserving) |
| `.ushr(n)` | `(Int) -> Int` | **logical** right shift (zero-fill) |

Integer literals may be written in decimal, hex (`0xFF`, `0xff`), or binary
(`0b1010`), with `_` digit separators (`1_000_000`, `0xDEAD_BEEF`). Hex/binary
literals are read as 64-bit patterns, so `0xFFFFFFFFFFFFFFFF` is `-1`.

### Sized integers (`i8`…`i64`, `u8`…`u64`)

Fixed-width integers for binary formats and interop. Write a literal with a
width suffix: `255u8`, `1000i16`, `0xFFu8`, `0b1010u8`. Out-of-range literals
are a compile error (K0009). Arithmetic is **checked** (overflow panics, like
`Int`); mixing widths is a type error — convert explicitly. There is also a
single-precision **`f32`** float (`1.5f32`, `10f32`, `1e3f32`); its arithmetic
matches `Float` (no overflow panic) and it does not mix with `Float`. Not yet
on the native backend (`kupl run`/`--vm`/`bundle`).

| Method | Signature | Notes |
|---|---|---|
| `.to_int()` | `-> Int` | to i64; panics if a `u64` exceeds `i64::MAX` |
| `.to_str()` / `.to_float()` | `-> Str` / `-> Float` | |
| `.to_i8()` … `.to_u64()` | `-> iN`/`uN` | checked conversion to another width; panics if out of range |
| `.wrapping_add/sub/mul(other)` | `(same width) -> same` | modular wraparound; never panics |
| `.saturating_add/sub/mul(other)` | `(same width) -> same` | clamps to the width's min/max |
| `.band/.bor/.bxor(other)` | `(same width) -> same` | bitwise within the width |
| `.bnot()` | `-> same` | bitwise complement within the width |
| `.shl(n)` / `.shr(n)` | `(Int) -> same` | shift by `n` in `0..=bits-1`; `shr` is arithmetic for signed, logical for unsigned |
| `.to_str()` | `-> Str` | |

`f32` methods: `.to_float()` → `Float`, `.to_str()` → `Str`. Convert a `Float`
to `f32` with `Float.to_f32()`.

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
| `.log2()` / `.cbrt()` | `-> Float` | base-2 log / cube root |
| `.atan2(x)` / `.hypot(x)` | `(Float) -> Float` | `atan2(self, x)` / `sqrt(self²+x²)` |
| `.format(decimals)` | `(Int) -> Str` | fixed-point, rounded; `decimals` in `0..=100` |

### Option[T]

| Method | Signature | Notes |
|---|---|---|
| `.is_some()` / `.is_none()` | `-> Bool` | |
| `.unwrap_or(default)` | `(T) -> T` | |
| `.map(f)` | `(fn(T) -> U) -> Option[U]` | `Some(x)` → `Some(f(x))`; `None` → `None` |
| `.and_then(f)` | `(fn(T) -> Option[U]) -> Option[U]` | flat-map / chain fallible steps |
| `.filter(f)` | `(fn(T) -> Bool) -> Option[T]` | `Some(x)` kept only if `f(x)` |
| `.ok_or(err)` | `(E) -> Result[T, E]` | `Some(x)` → `Ok(x)`; `None` → `Err(err)` |

### Result[T, E]

| Method | Signature | Notes |
|---|---|---|
| `.is_ok()` / `.is_err()` | `-> Bool` | |
| `.unwrap_or(default)` | `(T) -> T` | |
| `.map(f)` | `(fn(T) -> U) -> Result[U, E]` | transforms the `Ok` value; `Err` passes through |
| `.map_err(f)` | `(fn(E) -> F) -> Result[T, F]` | transforms the `Err` value; `Ok` passes through |
| `.and_then(f)` | `(fn(T) -> Result[U, E]) -> Result[U, E]` | chain fallible steps |
| `.ok()` | `-> Option[T]` | `Ok(x)` → `Some(x)`; `Err(_)` → `None` |

These combinators chain, so validation/transformation pipelines read without a
pyramid of `match` (see `examples/combinators.kupl`).

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
| `.filter(f)` | `(fn(K, V) -> Bool) -> Map[K, V]` | keep matching entries (insertion order) |
| `.fold(init, f)` | `(Acc, fn(Acc, K, V) -> Acc) -> Acc` | reduce over entries in insertion order |

### Set[T]

Constructed with `Set()` (empty) or `Set(list)` (duplicates dropped).
Insertion-ordered; equality is order-insensitive.

| Method | Signature | Notes |
|---|---|---|
| `.insert(v)` / `.remove(v)` | `(T) -> Set[T]` | new set |
| `.contains(v)` | `(T) -> Bool` | |
| `.union(s)` / `.intersect(s)` / `.difference(s)` | `(Set[T]) -> Set[T]` | |
| `.symmetric_difference(s)` | `(Set[T]) -> Set[T]` | in exactly one of the two |
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


## Parallelism

`list.par_map(f)` / `list.par_filter(pred)` are semantically identical to
`map`/`filter` — same results, same order. When the callback is a **pure**
top-level function (no effects) and the list is large (≥ 256 elements), they run
across real OS threads (on both the interpreter and the KVM); otherwise they
evaluate sequentially. Because a pure function can't observe I/O, the clock, randomness,
or shared state, and results are placed by input index, the output is
deterministic and byte-for-byte identical whether it ran on one thread or many.
