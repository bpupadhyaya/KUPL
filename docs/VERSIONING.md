# Versioning Policy

KUPL follows [Semantic Versioning](https://semver.org/) (`MAJOR.MINOR.PATCH`)
once it reaches `1.0.0` stable. This document defines what "breaking" means
for a language toolchain (not just a library), the current pre-1.0 caveat,
the release process, and the checklist for cutting `1.0.0` stable.

## Scope

This policy covers everything a KUPL user or tool depends on:

- **Language syntax and semantics** — grammar, type system, effect system,
  execution semantics of any valid program.
- **Standard library** — every builtin function/method's name, signature, and
  documented behavior.
- **The `.kx` bytecode format** — already versioned independently; a `.kx`
  file built by a different compiler version is rejected with a clear error
  rather than silently misinterpreted.
- **CLI flags and subcommands** — `kupl run`/`build`/`native`/`bundle`/`fmt`/
  `pkg`/`test`/`lsp`/etc. and their flags.
- **Diagnostic codes** (`K0xxx`) — semi-stable: editor tooling, CI scripts,
  and tests may match on a specific code, so removing or renumbering one is a
  breaking change even though the underlying bug/message text is not
  user-facing "API."

## What's a Breaking Change

- Removing or changing the meaning of existing valid syntax.
- Removing a standard library function/method, or changing its signature or
  documented behavior for existing valid inputs.
- Bumping the `.kx` bytecode format version (already handled by the existing
  compiler-version rejection mechanism).
- Removing or renumbering a diagnostic code that was previously stable.
- Removing or renaming a CLI subcommand or flag.
- Any change that makes a previously-valid, byte-identical-across-engines
  program produce different output on any of the four engines (this would
  also violate the project's own core invariant, checked on every build).

## What's Not

- Adding new syntax, new standard library functions, or new CLI
  flags/subcommands.
- Adding new diagnostic codes.
- Performance changes that preserve output (see `docs/PRODUCTION.md` §
  Performance characteristics — time/space complexity is explicitly not part
  of the byte-identity contract).
- Internal engine refactors that preserve four-engine byte-identity.
- Documentation changes.

## Pre-1.0 Caveat

While KUPL is `0.x` (currently `0.1.0` — see "Path to `1.0.0` Stable"
below for exactly what's left), breaking changes may still ship in a
version bump smaller than a hypothetical future `MAJOR`. This policy's
breaking/non-breaking distinction still applies for changelog purposes (see
`CHANGELOG.md`'s Added/Changed/Fixed/Removed categories), but it does not yet
carry a stability *guarantee*. That guarantee begins at `1.0.0` stable.

## Release Process

1. Move `CHANGELOG.md`'s `[Unreleased]` section entries under a new
   `## [X.Y.Z] - YYYY-MM-DD` heading.
2. Bump `version` in `Cargo.toml` to match.
3. Commit, then `git tag vX.Y.Z` and `git push --tags`.
4. Optionally attach `cargo build --release` binaries to a GitHub Release for
   the tag.

## Path to `1.0.0` Stable

Cross-checked against `docs/PRODUCTION.md`'s own "Known limitations" section
so this checklist doesn't contradict it. `1.0.0` stable is warranted once:

- [ ] A hosted package registry exists (a live server at the default
      registry URL, a published index, at least a small set of real
      third-party packages). The CLIENT side, and now self-hosting a v1
      registry (`kupl pkg publish` + any static file host + a project's own
      `[registry] url` override — it140), are both fully ready; what
      remains is external hosting/operational infrastructure at the
      DEFAULT registry URL specifically, not more code.
- [x] The real-provider AI path (`anthropic`/`openai`/`ollama`) has been
      hardened against real network conditions — retries transient
      failures (network errors, HTTP 429/500/502/503/504) with exponential
      backoff on BOTH interp/vm (`ai.rs`) and native (`cgen.rs`), it139.
      Still only lightly battle-tested (verified against a local mock HTTP
      server, not a live provider).
- [x] The concurrency story beyond `par_map`/`par_filter` has a real,
      documented, deliberate design AND implementation, not just a design:
      `concurrent component` (it132-it138, `docs/design/ASYNC.md` §8) gets
      a real dedicated OS thread, blocking `expose` calls, and non-blocking
      wire delivery — deliberately still opt-in, not multi-threaded by
      default, with named remaining gaps (wire-source direction, `example`/
      `:upgrade` support, an M:N scheduler for the ordinary `par { }`/
      `par_each`/component-dispatch path) rather than "TBD."
- [ ] The project has run in a real, non-toy production workload for a
      meaningful period without a correctness regression.

Until then, expect ordinary `0.x.y` semver numbers (not `1.0.0`-prerelease
identifiers — `0.y.z` is semver's own designated range for "anything MAY
change at any time, the public API SHOULD NOT be considered stable,"
which is a more accurate signal here than a `1.0.0-alpha`/`-beta` tag
implying an imminent, scoped path to `1.0.0`) and this document's pre-1.0
caveat to apply. `0.1.0` (this project's first tagged, tracked release —
see `CHANGELOG.md`) is exactly such a release: extensively self-tested
internally, zero hours of real-world usage by anyone else yet, no
compatibility promise attached.
