> **Native complete (it42):** f32 now compiles to native too — a K_F32 KValue
> (a C `float`, literals reconstructed from the exact 32-bit pattern), f32
> arithmetic/compare/eq, f32↔Float conversions, and a Display that reuses the
> shortest-round-trip formatter (loop `%.*g`, comparing `strtof`) so non-integer
> f32 prints byte-identically to Rust. examples/sized.kupl (sized ints AND f32)
> is now fully native. **Sized numerics are on ALL FOUR engines.**
>
> **Native (it40):** sized integers (i8..i64/u8..u64) now COMPILE to machine
> code — a K_SIZEDINT KValue (boxed __int128 + width) mirroring the interpreter's
> i128 range-check/wrap/saturate semantics exactly, incl. u64 values above
> i64::MAX and the overflow/out-of-range panic messages. Sized ints are on ALL
> four engines. f32 native codegen is deferred: its non-integer Display needs a
> shortest-round-trip float formatter to stay byte-identical with Rust — a
> follow-up. So `kupl native` compiles sized-int programs; f32 programs still
> defer with a clear message.

> **Progress:** iter 1 landed (it27) — integer widths i8..u64 end-to-end on
> interpreter + KVM + .kx (checked arithmetic matching i64; overflow panics).
> **f32 (it28) + wrapping/saturating/bitwise + full conversion matrix (it29) landed** — the
> sized-numerics user surface is complete. Native sized codegen
> codegen are later slices. NOTE: the `Value` enum was already 32 bytes at
> baseline (no discriminant niche), not 24 as this design assumed — sized ints
> box their payload so the enum does not grow.

# Big-arc design: Sized numerics i8..u64, f32 (type-system depth)

**Feasibility:** high · **Risk:** medium · **Estimated effort:** ~5 /loop iterations
_(Produced by a parallel design workflow, 2026-07-04. Grounded in the actual source.)_

## Summary
Add sized integer and f32 types via a single width-tagged runtime value (Value::SizedInt(i128, IntW) + Value::F32(f32)) plus new Ty variants, not per-width Value variants and not pure type-directed compilation. Arithmetic stays checked/panic-on-overflow to match KUPL's i64 semantics, with wrapping/saturating offered as explicit methods; interp==KVM stays byte-identical for free because both delegate to raw_binary_op/shared_method, and native codegen defers with a clear error initially (as ai/json already do). i128 storage makes every i8..u64 value exact and range-checks trivial while fitting inside the enum's existing max variant size.

## Key files
, , , , , , , , , , , , 

## Byte-identical / determinism impact
interp==KVM is preserved automatically: all sized-int semantics live in the shared `raw_binary_op`/`shared_method` (imported by vm.rs at line 11, called at 401/716) and in the shared `Value::Display`, so both engines execute the identical Rust code — no divergence is possible. Overflow uses the same message strings as the existing i64 path, so panic output matches. `.kx` (engine 3) stays consistent by adding matching encode/decode tags in kx.rs; the KVM const pool is `Vec<Value>` so tagged literals flow through `compile.rs` unchanged and intern correctly via derived `PartialEq`. Native (engine 4) is kept honest by a clear-error defer in cgen.rs (same precedent as ai/json/regex), so it is complete-or-explicit-error rather than silently divergent; a later iteration turns it on with C `intN_t`/`__int128` range checks and matching printf. Tests stay deterministic because arithmetic is pure, checked, and produces fixed values/messages — no clock, IO, or ordering involved — and the existing `differential()` harness asserts interp==KVM byte-for-byte.

## Dependencies & ordering
Self-contained arc; depends only on the current shared-semantics architecture, no other arc. Internal ordering is strict: iter 1 (integer widths: value/type/lexer/parser/check/interp/kvm/kx + native-defer) is the foundation everything else builds on. Iter 2 (wrapping+saturating+bitwise methods, sized↔sized conversions, width-aware to_hex/to_binary) depends on iter 1. Iter 3 (f32: Value::F32, Ty::Float32, 1.5f32 literal, f32 arithmetic/Display, Float↔f32 conversions) is independent of iter 2 and can swap order, but shares the enum-churn so doing it right after iter 1 minimizes repeated edits to value.rs/kx.rs. Iter 4 (native codegen: remove the cgen defer, add C intN_t/__int128 + f32 printf parity, extend the differential harness to native for fun-main) depends on iters 1-3 being stable. Optional iter 5 (sized-literal patterns in match; binary-format helpers to_bytes/from_bytes that exploit widths) depends on iter 1-2. No conflicts with concurrency, effects, or component arcs. One cross-cutting caution: any other arc that adds a `Value` or `Ty` variant will collide in the same exhaustive matches, so land this arc's enum changes in a single early iteration to reduce rebase churn.

## First iteration (shippable slice)
Ship the integer-width skeleton end-to-end for interp + KVM + kx, with native deferring via a clear error. Scope kept tight but coherent and fully tested.

1. value.rs: add `enum IntW { I8..U64 }` (with `min()/max()->i128`, `check_range`, `mask`), and `Value::SizedInt(i128, IntW)`. Update `type_name` (return "i8".."u64"), `PartialEq` (compare value+width), `Display` (write the i128). Also add `Value::F32(f32)` stub arms now (Display/eq/type_name) even though f32 literals land in iter 3, to avoid a second enum churn — OR omit F32 entirely this iteration; recommend omit to keep the diff minimal.
2. types.rs: add `Ty::IntW(IntW)`; `is_numeric` true; `Display`; `unify` same-width arm; no apply/occurs change.
3. token.rs/lexer.rs: add `Tok::SizedInt(i128, IntW)`; parse an int suffix (i8..u64, incl `0xFFu8`) at the end of `lex_number`, range-check at lex time with a new K0009 diag on overflow. Add lexer tests: `255u8`, `256u8`→err, `0xFFu8`, `-1` via `0xFF..u8` range, `1000i16`.
4. ast.rs/parser.rs: add `ExprKind::SizedInt(i128, IntW)`; map the token in primary parsing.
5. check.rs: `resolve_ty` names i8..u64; infer `SizedInt` literal → `Ty::IntW(w)`; ensure arithmetic/comparison paths accept it (they already gate on `is_numeric` after unify); add method sigs `sized.to_int()->Int` and `Int.to_i8()..to_u64()->i8..u64`.
6. interp.rs: in `raw_binary_op` add the `(SizedInt,SizedInt)` arm — checked `+ - * / %` in i128 with range-check panics ("integer overflow in addition" etc.) and comparisons; in `shared_method` add `sized.to_int()` (checked) and `Int.to_iN/uN()` (checked, panic out of range). eval arm for `ExprKind::SizedInt`.
7. compile.rs: lower `ExprKind::SizedInt` to `const_reg(Value::SizedInt(..))`. vm.rs needs no logic change (delegates), but confirm any `Value` matches in vm.rs are exhaustive.
8. kx.rs: add encode/decode tag for `SizedInt` (tag + 16 bytes i128 + 1 width byte) in the const/value paths.
9. cgen.rs: clear-error defer when a `SizedInt` literal/type is reached ("sized integers not yet supported in native codegen").
10. Tests: add differential tests (the `differential(...)` helper already used in vm.rs:1262) proving interp==KVM for: `(200u8 + 55u8)` = 255, `(200u8 + 100u8)` panics with the overflow message, signed wrap boundary `127i8 + 1i8` panics, `1000000i32 * 1000000i32` panics, comparisons, `Display` of each width, `(255u8).to_int()`, `(300).to_u8()` panics, `(65535).to_u16()` ok. Plus a cgen test asserting the clear-error defer. Run the all-examples regression + cargo test; commit; push.

## Design
## Goal & constraints

Add `i8 i16 i32 i64 u8 u16 u32 u64` and `f32` for binary formats, interop and systems work, while preserving the sacred invariant that the tree-walking interpreter (src/interp.rs) and the KVM (src/vm.rs) produce byte-identical results, `.kx` modules (src/kx.rs) round-trip identically, and native (src/cgen.rs, fun-main only) mirrors-or-defers-with-a-clear-error.

Two architectural facts (verified in source) shape the whole design:

1. **The KVM does not re-implement semantics.** `src/vm.rs:11` imports `raw_binary_op` and `shared_method` from interp and calls them at `vm.rs:401` and `vm.rs:716`. So any arithmetic/method semantics I add to those two functions are automatically identical in both engines. Byte-identical for interp==KVM is *free* if the logic lives there and in the shared `Value` `Display`.
2. **The interpreter has no static types at eval time.** `interp.rs` evaluates `ExprKind` nodes against runtime `Value`s; `Ty` never reaches `raw_binary_op`. This is decisive for the representation choice below.

## (a) Runtime representation — the three options weighed

**Option 1: new `Value` variant per width (`I8,I16,…,U64,F32`).** ~9 new variants. Every `match` on `Value` fans out: `value.rs` (`type_name` value.rs:81, `PartialEq` :103, `Display` :134), `interp.rs` (`raw_binary_op` :1167, `shared_method` :1284, arms everywhere), `vm.rs`, `cgen.rs` C union + every `k_*` helper, `kx.rs` encode/decode. Dozens of arms × 9 = high defect surface, and arithmetic still needs per-width wrap/checked logic on top. **Rejected — most invasive.**

**Option 2 (RECOMMENDED): single width-tagged value.** One new integer variant carrying a width tag, plus one `f32` variant:
```rust
#[derive(Clone, Copy, PartialEq)]
pub enum IntW { I8, I16, I32, I64, U8, U16, U32, U64 }
pub enum Value {
    Int(i64),   // unchanged default Int (still panics on overflow)
    Float(f64), // unchanged default Float
    // NEW:
    SizedInt(i128, IntW),
    F32(f32),
}
```
Key move: **store the sized integer in `i128`, not `i64`.** i128 exactly represents every value of every width including the full `u64` range (`u64::MAX < i128::MAX`), so there is no signed/unsigned bit-reinterpretation hassle, `Display` is just `write!("{}", v)`, `PartialEq`/ordering are natural, and overflow checks are a single `if r < w.min() || r > w.max()`. Crucially, i128 (16 bytes) + the `IntW` tag (1 byte) is **smaller than the enum's current largest variant** (`Ctor` = three `Rc` = 24 bytes; `Range(i64,i64,bool)` ≈ 24), so `size_of::<Value>()` does not grow — the representation cost is genuinely free. Fan-out is bounded: 2 new arms in each `Value`/`Ty` match instead of ~18.

**Option 3: pure type-directed compilation** (runtime stays `i64`, the `Ty` carries width, compiler inserts mask/wrap after each op). Attractive on paper, but it fights this architecture: `raw_binary_op` is *value-typed and type-blind*, and the interpreter is a tree-walker with no type annotations threaded through `eval`. To wrap correctly you would have to (a) thread resolved `Ty` into interp's `eval_expr`/`binary_op`, and (b) emit explicit narrowing ops in `compile.rs` for the KVM and in `cgen.rs` for C, keeping three narrowing paths in agreement. That is *more* invasive than Option 2 and breaks the "one shared semantics function" contract that currently guarantees interp==KVM. **Rejected for this codebase** (it would be the right call in a bytecode-only VM with typed operands; KUPL is not that).

**Recommendation: Option 2 with i128 storage.** Least invasive that is still correct, and it keeps semantics inside the two shared functions so the sacred pair stays byte-identical automatically.

## (b) Literal suffixes in the lexer

`lexer.rs:141 lex_number` already parses decimal, hex (`0xFF`), binary (`0b1010`), underscores, floats and exponents, emitting `Tok::Int(i64)` / `Tok::Float(f64)` (token.rs:8-9). Extend it: after the numeric body (both the hex/binary branch at :146 and the decimal branch at :176) peek for an identifier-like suffix and match it against `{i8,i16,i32,i64,u8,u16,u32,u64}` (int context) or `{f32,f64}` (float context). On match:
- Parse the collected digits into `i128`, **range-check against the suffix width** at lex time (`255u8` ok, `256u8` → new diag e.g. K0009 "literal `256` out of range for `u8`"). Hex/binary suffix (`0xFFu8`) reuses the same width check.
- Emit new tokens `Tok::SizedInt(i128, IntW)` and `Tok::F32(f32)` (add to token.rs enum + `describe`). `f64` suffix just yields the existing `Tok::Float`.

`suppresses_newline` / `symbol` need no change. The existing lexer tests (integer_literal_forms etc.) stay green because a bare number with no suffix takes the unchanged path.

**Naming choice:** use lowercase `i8..u64`/`f32` as both the suffix and the *type name* (matches the literal suffix, systems convention, and the arc brief). This differs from KUPL's capitalized primitives (`Int`,`Float`) but reads correctly for this domain; the alternative (`Int8`,`UInt8`,`Float32`) is noted but not recommended.

## AST & parser

`ast.rs:296 ExprKind` has `Int(i64)`/`Float(f64)`; add `SizedInt(i128, IntW)` and `F32(f32)`. The parser's primary-expression path that turns `Tok::Int`→`ExprKind::Int` gains two trivial arms. `TyExprKind::Name` (ast.rs:430) already carries type names as strings, so `i8`/`f32` need no parser change — only `check.rs resolve_ty` learns the names.

## (c) Checked vs wrapping arithmetic & overflow semantics

KUPL's `Int` is checked and **panics on overflow** (`raw_binary_op` interp.rs:1179-1186 uses `checked_add/sub/mul/div`, message family "integer overflow in {addition|…}"). **Keep sized ints consistent: default arithmetic is checked and panics on overflow beyond the width's range.** This is the honest, uniform rule and makes byte-identical trivial (one message string, produced by the shared function).

In `raw_binary_op`, add a `(SizedInt(a,wa), SizedInt(b,wb))` arm:
- Runtime guard `wa==wb` (the type checker already forbids mixing, so this is a safety net → "invalid operand types" like the existing fallthrough).
- Compute `a op b` in **i128** (cannot itself overflow for these widths), then `w.check_range(r)?` → `Value::SizedInt(r, w)`, else `Err("integer overflow in addition")`. `Div`/`Rem` reuse the existing zero-guards and messages. Comparisons return `Bool`.
- `F32` arm mirrors the `Float` arm (interp.rs:1201) but in `f32`, so results carry f32 rounding.

**Wrapping/saturating is opt-in** via `shared_method`: `.wrapping_add/.wrapping_sub/.wrapping_mul`, `.saturating_add/.saturating_sub/.saturating_mul` (mask/clamp into `w`), plus width-masked bitwise `.band/.bor/.bxor/.bnot/.shl/.shr` (mirroring the existing `Int` bitwise methods at check.rs:1793). This satisfies the "binary formats / systems" need without weakening the default.

## (d) Conversions

Consistent with the panic-on-loss philosophy, the **primary conversions are checked (panic on out-of-range)**, with explicit lossy variants:
- Widen to default Int: `n.to_int()` (sized → `Int`; `u64` above `i64::MAX` panics — or offer `.to_u64_int()` note; simplest: `to_int()` checked).
- `Int.to_i8()…Int.to_u64()`: checked narrowing (panic if out of range), mirroring existing `Int.to_float()` (interp.rs:1681).
- Explicit lossy: `.wrapping_to_u8()` (two's-complement mask) and `.saturating_to_u8()` (clamp to width) for the cases where truncation is intended.
- Float: `.to_f32()`/`.to_f64()`; `f32→Int`/`Int→f32` as needed.

This is `checked-by-default, lossy-on-request`, matching arithmetic. (An `Option`-returning `try_to_u8` can arrive later; not needed for v1.)

## (e) f32 vs f64 and Display parity

`Value::F32(f32)` stores real f32 so arithmetic rounds at f32 precision deterministically. `Display` (value.rs:134) reuses the existing Float logic: `if v.fract()==0.0 && v.is_finite() { "{v:.1}" } else { "{v}" }` but on the `f32`. Rust's `f32` `Display` gives the shortest round-trip f32 repr — deterministic and identical between interp and KVM (both Rust). `SizedInt` `Display` is just the i128 value (equals the logical integer for every width). No `%`-format ambiguity because native defers f32 initially (see below), so the only two engines that print f32 in v1 are both Rust.

## (f) Keeping interp==KVM==native byte-identical

- **interp==KVM:** guaranteed by construction — semantics live only in `raw_binary_op`, `shared_method`, and `Value::Display`, all shared. `compile.rs` lowers `ExprKind::SizedInt/F32` into `const_reg(Value::SizedInt/F32(..))`; the const pool is `Vec<Value>` (bytecode.rs:106) so the tagged value flows into the VM unchanged, and `const_idx` interning (compile.rs:526) works via the derived `PartialEq`.
- **.kx (engine 3):** add encode/decode tags in `kx.rs` value-const path (currently tags 0-5 at kx.rs:671-676 and encode at :205-222): `SizedInt` = tag + 16 i128 bytes + 1 width byte; `F32` = tag + 4 bytes. This keeps compiled modules consistent with the live value set.
- **native (engine 4):** in iteration 1, `cgen.rs` emits a **clear-error defer** for any `SizedInt`/`F32` literal or `i8..u64/f32`-typed op ("sized numerics not yet supported in native codegen"), exactly like the existing ai/json/regex deferrals. Native is fun-main-only and already defers whole subsystems, so this preserves the invariant honestly (the sacred *interp==KVM* pair is fully live; native is complete-or-clear-error). A later iteration turns native on using C `int8_t…uint64_t`, `__int128` for range checks, and matching `printf` for f32.

## Effects & purity

Arithmetic and conversions are pure — no changes to effects.rs. No new builtins that touch io.

## Type system details (check.rs / types.rs)

- `types.rs`: add `Ty::IntW(IntW)` and `Ty::Float32`. `is_numeric()` (types.rs:37) returns true for both. `Display`: "i8".."u64","f32". `unify` (types.rs:93): `(IntW(x),IntW(y)) if x==y => Ok`, `(Float32,Float32)=>Ok`; different widths do **not** unify (so `i32 + i16` is a type error — explicit conversion required, which is the correct systems behavior). `apply`/`occurs` need no recursion arms.
- `check.rs resolve_ty` (:394): map the 9 names. Literal inference: `ExprKind::SizedInt(_,w) => Ty::IntW(w)`, `F32 => Ty::Float32` (near :1057). Binary/comparison checking (:1149,:1140) already unifies lhs==rhs then checks `is_numeric()` — sized types pass unchanged. `default_numeric` (:1045) only defaults *unconstrained* vars to `Int`, so sized types never accidentally collapse to Int. Add method signatures for the conversion/wrapping/bitwise methods per width.

## Honest risks

- **Fan-out discipline:** ~2 arms in each of ~6 `match` sites; mechanical but must be complete or the compiler flags non-exhaustiveness (Rust helps here — missing arms won't compile).
- **Conversion surface is combinatorial** (methods × 8 widths). Scoped across iterations; start with Int↔sized and the core arithmetic.
- **u64 → Int edge:** `u64` values above `i64::MAX` can't become `Int`; `to_int()` on them must panic (documented) — i128 storage makes the check exact.
- **Native parity later:** deferring is safe but leaves native feature-incomplete for sized numerics until iteration 4; this matches existing precedent and the arc brief.
