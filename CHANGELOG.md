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
