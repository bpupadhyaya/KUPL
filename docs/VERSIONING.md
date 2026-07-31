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

While KUPL is `0.x`/`1.0.0-alpha`, breaking changes may still ship in a
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
      third-party packages) — the client side has been ready for a while;
      this is the single largest remaining gap.
- [ ] The real-provider AI path (`anthropic`/`openai`/`ollama`) has been
      hardened against real network conditions (timeouts, retries, rate
      limits, partial responses), not just mock-tested.
- [ ] The concurrency story beyond `par_map`/`par_filter` (the structured
      `par { }` block, `par_each`, component handler dispatch) has a
      documented, deliberate design — not necessarily multi-threaded by
      default, but no longer "later, semantics-preserving step, TBD."
- [ ] The project has run in a real, non-toy production workload for a
      meaningful period without a correctness regression.

Until then, expect `1.0.0-alpha`/`-beta`-style pre-release identifiers and
this document's pre-1.0 caveat to apply.
