# Changelog

All notable user-visible changes to KUPL are documented here, in the style of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). See
[`docs/VERSIONING.md`](docs/VERSIONING.md) for what counts as a breaking
change and the release process.

## [Unreleased]

### Added

- CI workflow (`.github/workflows/ci.yml`): build + full test suite on
  Linux/macOS on every push and PR, plus an interpreter-vs-VM output-parity
  sweep across `examples/*.kupl`.
- `SECURITY.md`, `docs/VERSIONING.md`: a documented vulnerability-reporting
  channel and a versioning/release policy.
- `kupl run`/`kupl run --vm --timeout=<seconds>`: an opt-in wall-clock
  execution limit (off by default). Kills a runaway process with a clean
  `K0901` diagnostic and exit code `124` after the deadline.
- `kupl run`/`kupl run --vm --max-memory=<MB>`: an opt-in total-allocation
  cap (off by default). Prints a `K0902` diagnostic before aborting on the
  first over-cap allocation. Does not apply to `kupl native`'s generated
  executable — see `docs/PRODUCTION.md`'s Known Limitations.
- `sha256(s)` and `hmac_sha256(key, msg)` standard library builtins:
  cryptographic hashing and message authentication, hand-rolled in-tree
  (zero external dependencies), verified against FIPS 180-4/RFC 4231
  known-answer test vectors, byte-identical across all four engines
  including native.
- `log_debug`/`log_info`/`log_warn`/`log_error(v)` standard library
  builtins: minimal structured logging (one `<timestamp> [LEVEL] <v>`
  line to stderr per call), byte-identical across all four engines
  including native.
- `par { }` fork-join branches now run on genuine OS threads, on both
  `kupl run` and `kupl run --vm`, when every branch is a call to a
  statically pure, top-level named function with plain-literal/identifier
  arguments; all other branches fall back to the unchanged sequential path
  (including on `kupl native`, which stays sequential). Strictly additive —
  results and error reporting (including the exact panic span) match the
  sequential reference byte-for-byte. See
  `docs/design/bigarcs/3-real-concurrency.md`.
- `supervise child restart on_failure max N in <duration>`: an opt-in
  BEAM/Erlang-inspired restart-intensity limit. Once a supervised child has
  restarted `N` times within the trailing `duration` (virtual clock), the
  next panic escalates instead of restarting again — a safety valve against
  an unbounded panic/restart crash loop. Omitting `max … in …` preserves
  unlimited restarts. Byte-identical across all four engines including
  native. See `docs/reference/LANGUAGE-REFERENCE.md` §9.
- Bounded generics: `fun mymax[T: Ord](a: T, b: T) -> T { if a > b { a }
  else { b } }`. `Ord` is currently the only supported bound. Comparing two
  values of an Ord-bounded type parameter is permitted inside a generic
  function's body (previously rejected as an unsound narrowing, K0281);
  calling such a function with a concrete type that doesn't support
  ordering is a compile-time error (`K0290`), not a deferred runtime panic.
  Byte-identical across all four engines including native (a pure
  type-checker feature — KUPL's generics are dynamically typed at runtime,
  so no engine's own execution changed). See
  `docs/reference/LANGUAGE-REFERENCE.md` §3.
- `Char`: a single Unicode scalar value literal (`'a'`, `'\n'`, `'π'`),
  ordered by codepoint (comparisons, `.sort()`, `.min()`/`.max()`, and
  the `[T: Ord]` bound all support it); no `Add` — use
  `to_str(a) + to_str(b)` to build a `Str` from two `Char`s. New lexer
  diagnostics `K0011`/`K0012`/`K0013`. Byte-identical across all four
  engines — the interpreter, `kupl run --vm`, `.kx` build/run, `kupl
  bundle`, and `kupl native`.
- `Decimal`: an exact base-10 arbitrary-precision decimal, `dec("3.14")`
  (also accepts an `Int`). `+`/`-`/`*` are exact; `/` rounds to 34 extra
  digits of precision beyond the operands' own scale (`%` is not
  supported — same as `Rational`). Equality/ordering align scale first
  (`dec("2.50") == dec("2.5")`), but `Display` preserves each value's own
  stored scale (`dec("2.50")` prints `2.50`). Byte-identical across all
  four engines — the interpreter, `kupl run --vm`, `.kx` build/run, `kupl
  bundle`, and `kupl native`.

## [1.0.0-alpha]

The current baseline. A summary of what shipped before this changelog existed:

- **Four byte-identical execution engines**: a tree-walking interpreter (the
  reference semantics), a register-based bytecode VM, a compiled `.kx`
  bytecode format, and a native C-codegen compiler — verified byte-identical
  on every build.
- **A modern type system and syntax**: generics over functions and types,
  operator overloading, `Option`/`Result` combinators, exhaustive pattern
  matching, and a hierarchical compile-time effect system (`uses io`,
  `uses ai`, ...).
- **A component model**: isolated actors with typed ports, private state,
  supervision (restart-on-failure), timers, and inline `example`/`law` tests
  (including property-based `forall` testing).
- **`ai fun`**: typed, structured-output, mockable AI functions with tool use
  and agent components — provider-agnostic (Anthropic, OpenAI-compatible,
  Ollama, or a deterministic mock for tests/CI).
- **A comprehensive, zero-dependency standard library**: `List`/`Map`/`Set`/
  `Str` with a full functional toolkit; the numeric tower `Int -> BigInt ->
  Rational` plus sized integers and `f32`; JSON, CSV, URL, regex, HTTP
  (client + server), time, encoding, and random — all hand-rolled in-tree, no
  external crates.
- **Tooling**: a REPL, a language server (hover, go-to-definition,
  completion, find-references, rename, code actions, folding), `kupl fmt`,
  `kupl diff`/`kupl context`, and a local package manager (`kupl pkg`) with
  path dependencies, locking, and hash-verified fetches.

## Prior History

KUPL's pre-`1.0.0-alpha` development history — an initial multi-phase
language-enrichment campaign, followed by an extensive, still-ongoing
production-hardening campaign — is documented in detail in
[`docs/GAPS.md`](docs/GAPS.md) and the git commit history
(`git log --oneline`). This changelog begins tracking releases going forward
from `1.0.0-alpha`; it does not attempt to backfill that history.
