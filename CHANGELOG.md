# Changelog

All notable user-visible changes to KUPL are documented here, in the style of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). See
[`docs/VERSIONING.md`](docs/VERSIONING.md) for what counts as a breaking
change and the release process.

## [Unreleased]

### Added
- `kupl run <file.kupl> --sandbox` (macOS only): opt-in OS-level confinement
  via `sandbox-exec` — denies network access and confines filesystem writes
  to the OS temp directory. Composable with `--vm`/`--timeout`/`--max-memory`.
  Other platforms report a clean, honest error rather than running
  unconfined. See `docs/PRODUCTION.md`'s threat model section.

### Security
- Fixed an unauthenticated remote denial-of-service in `kupl node`: the
  wire-format decoder (`kser::read_value`) recursed with no depth limit,
  reachable on a fresh connection's very first frame BEFORE the shared-secret
  token is ever checked — a single crafted packet could overflow the stack
  and abort the whole node process. Now capped at 150 nesting levels
  (`kser::MAX_VALUE_NESTING_DEPTH`, empirically bisected against this
  specific recursive function's stack cost, not just copied from an
  unrelated parser's own depth cap).
- Fixed a `weight distributed` key/nonce-reuse bug: `SessionKeys::derive`
  was a pure function of the shared token alone, so every connection made
  with the same token derived the identical ChaCha20-Poly1305 keys with
  nonce counters restarting at 0 — a passive observer of two such
  connections could recover the XOR of same-index plaintexts, and Poly1305's
  repeated one-time key is a known forgery condition. The `Auth`/`AuthOk`
  handshake now exchanges a per-connection salt from each side, mixed into
  key derivation, so every connection gets fresh keys regardless of how
  many share the same token.
- `durable agent` state files are now created with owner-only (`0600`)
  permissions on Unix, instead of the OS default (typically world/group-
  readable).

## [0.2.0] - 2026-09-06

Cross-platform reliability release — real, CI-verified support for
Linux and Windows for the first time, plus one real, previously-hidden
security fix. See `docs/ROADMAP.md` for the full release-by-release
plan this fits into.

### Added

- `.github/workflows/release.yml`: a dedicated release-build workflow,
  producing real native binaries on each platform's own GitHub-hosted
  runner (`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`) — triggered by a `v*` tag push or manually.
- `windows-latest` added to the CI test matrix for the first time ever.

### Fixed

- **`kupl native` never linked against `libm`**, so every native
  compile failed on Linux from the day CI was first added (invisible
  on macOS, where libm is folded into `libSystem`).
- **A real security-relevant gap**: a manifest `entry` path like
  `/etc/passwd` (rooted but without a Windows drive prefix) was not
  rejected as absolute on Windows, unlike a genuinely absolute Unix
  path — `Path::is_absolute()` alone isn't sufficient on Windows;
  now also checks `has_root()`.
- `cc_available()`'s test-suite gate used to report `kupl native` as
  usable whenever `cc --version` succeeded — true even on
  `windows-latest`, where the generated C runtime's `<sys/wait.h>`/
  `fork`/`pipe` dependency (needed for the `exec` builtin) has no
  MinGW equivalent. Native-codegen tests now correctly skip instead of
  failing on every platform where the real runtime preamble can't
  compile.
- `cc_hash()`'s own `$PATH` search didn't handle Windows executable
  extensions (`cc.exe`), disagreeing with `cc_available()`'s real
  `Command::new` probe about whether the same compiler resolves at all.
- The LSP's `file://` URI builder didn't normalize Windows path
  separators, breaking hover/definition/rename/workspace-symbol
  results built from a native Windows path.
- Several tests hardcoded Unix-only assumptions (a `std::os::unix`
  symlink call with no Windows path, an exact OS error string that
  differs by platform for an over-length filename, `/tmp/...` literals
  embedded directly in compiled-and-run KUPL source) — fixed to be
  genuinely cross-platform rather than accidentally Unix-only.

### Known limitations

- `kupl native`/`kupl bundle` do not work on Windows yet — the
  generated C runtime's `exec` implementation depends on POSIX-only
  `fork`/`pipe`/`waitpid`, with no MinGW equivalent. `kupl run`,
  `kupl run --vm`, `kupl check`, `kupl build`, and `kupl fmt` are
  unaffected (pure Rust, no C codegen). See `docs/PRODUCTION.md`.

## [0.1.0] - 2026-09-04

First tagged, tracked release — see `docs/VERSIONING.md` for what this
version number does and doesn't promise (pre-1.0, no compatibility
guarantee yet).

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
- `kupl context --json`: the same target+direct-dependency data as
  plain-text `kupl context`, structured for a program to consume
  (mirrors `kupl check --json`). Error paths (missing/ambiguous item)
  emit valid JSON under `--json` too.
- `text_embed(s, dims) -> List[Float]` / `cosine_similarity(a, b) ->
  Float`: a from-scratch, zero-dependency bag-of-words hash embedding
  (the "hashing trick" — no model, no network call) and cosine
  similarity, for building lightweight prompt-context retrieval without
  a hosted embedding model. Byte-identical across all four engines
  including native.
- `kupl patch <target> <ItemName> <replacement> [--write]`: replaces one
  item's entire source span with a replacement file's own single item —
  a component-granular edit, the semantic inverse of `kupl context`.
  Prints the patched file by default; `--write` overwrites in place, and
  (mirroring `kupl fmt --write`) refuses to write a patch that would
  introduce a new compile error, leaving the original untouched.
- `kupl repl`'s new `:upgrade <ComponentName>` command: hot-swap state
  migration for live instances of a just-redefined component (Erlang's
  `code_change` equivalent). A `state` field present in both the old and
  new definition keeps its current value; a field only in the new
  definition gets its own fresh default; methods take effect
  immediately. Refuses (no instance touched) if `props` or `children`
  changed. The existing default behavior (redefinition without
  `:upgrade` leaves live instances frozen to their original shape) is
  unchanged — this is purely additive, opt-in.
- `concurrent component`: every instance gets its own real OS thread.
  `expose fun` calls block for a real reply; a wire whose destination is
  a `concurrent` instance delivers non-blocking. Portability restriction
  on props/ports/exposed signatures (K0306); no wire-source, `example`
  block (K0307), or `law`/`forall` reference (K0308) support yet. See
  `docs/reference/LANGUAGE-REFERENCE.md` §7.2.
- `agent`: a higher-level actor built on `concurrent component`,
  purpose-built to represent a human-like co-worker. `protocol { forbids
  <effect> / guard Name: T { expect result … } }` + `agent Foo follows
  Protocol1, Protocol2` — statically-checked effect rules (K1002) plus
  runtime-checked postconditions on a `guards Name`-tagged exposed fun's
  own return value (K1004/K1005). `weight lightweight|heavyweight|
  distributed` — which concurrency tier backs the agent (K0317/K0125),
  the last a real, shared-secret-authenticated TCP transport to a
  separate node process, ChaCha20-Poly1305 encrypted from `Spawn`
  onward. `durable` — this agent's `state`
  persists to disk across separate `kupl run` invocations, not just
  supervised restarts (K1006). `deterministic` — checker-enforced
  guarantee the agent's exposed funs never transitively reach an `ai
  fun` (K1008/K1009). Full spec: `docs/reference/LANGUAGE-REFERENCE.md`
  §7.3; diagnostics K1000-K1009; design rationale: `docs/design/
  AGENTS.md`; example: `examples/agent_keyword.kupl`.
- `kupl node <file.kupl> --listen <addr> --token <token>`: runs a
  program as a server accepting `weight distributed` agent connections
  authenticated by a shared secret token. See `docs/reference/CLI.md`
  and `docs/design/DISTRIBUTION.md` for the full security posture.
- `kupl agent inspect <AgentName>` / `kupl agent clear <AgentName>`:
  read or reset a `durable` agent's persisted state without running the
  program.
- ChaCha20-Poly1305 AEAD (RFC 8439, `src/aead.rs`), hand-rolled and
  verified against the RFC's own official test vectors: `weight
  distributed`/`kupl node` traffic is now encrypted (not just
  authenticated) from `Spawn` onward, using per-direction keys both
  sides derive from the already-shared token (`SHA-256(token ||
  ":c2s"/":s2c")`, `distribution.rs::SessionKeys`) — no key exchange
  needed. The initial `Auth`/`AuthOk`/`AuthFailed` round-trip itself
  still travels in plaintext. Not full TLS/mTLS: no PKI/certificate-based
  peer identity, no perfect forward secrecy, no MITM protection on the
  very first connection — see `docs/PRODUCTION.md`'s Known Limitations
  for the precise, honest posture.

## [1.0.0-alpha]

The pre-`0.1.0` baseline this project's crate version stayed fixed at for
its entire history before this release. A summary of what shipped before this changelog existed:

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
