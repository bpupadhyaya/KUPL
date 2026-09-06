# KUPL Roadmap: 0.1.0 → 1.0.0

This is the working plan from the first tagged release (`0.1.0`) to a real
`1.0.0` — a sequence of incremental releases, each with a specific theme and
concrete deliverables, cross-checked against `docs/VERSIONING.md`'s own
"Path to `1.0.0` Stable" checklist and `docs/PRODUCTION.md`'s Known
Limitations so this plan doesn't quietly contradict either.

**Read this section before any of the release plans below.** Not everything
gating `1.0.0` is closeable by writing code, and this plan does not pretend
otherwise:

- **Closeable by engineering work** (what the release plans below target):
  cross-platform reliability, the effect system's known soundness gap, a
  real hosted package registry, live-provider AI test coverage, a sandbox
  wrapper.
- **Not closeable by engineering work, no matter how much**: a real,
  non-toy production usage history without a correctness regression
  (`VERSIONING.md`'s own explicit `1.0.0` criterion) requires calendar time
  and someone other than the author actually depending on it. An
  independent (non-self) security audit requires a party that isn't the
  project. Neither has a release number attached below because neither is
  a feature to build — see "The usage window" after 0.5.0.

---

## 0.2.0 — Cross-platform reliability

**Theme:** KUPL builds, tests green, and ships an installable artifact on
all three major desktop platforms — not just macOS.

**Status:** in progress. A real, previously-undiscovered bug was already
found and fixed this cycle: `kupl native`'s generated C code was never
linked against `libm`, so every native compile failed on Linux from the
day CI was first added (invisible on macOS, where libm is folded into
libSystem). Fixed in commit `eae91e7`.

**Remaining work:**
1. **Resolve the GitHub Actions billing/quota situation.** Two consecutive
   CI reruns on `ubuntu-latest` were killed externally (exit 143, "runner
   has received a shutdown signal") at a suspiciously consistent ~8m33s
   mark, while `macos-latest` completed cleanly in the same run. Working
   theory: the account's Actions-minutes quota (macOS runners cost 10x the
   multiplier) is exhausted from weeks of failing/hanging CI runs. Needs a
   check of github.com/settings/billing — this is the one item in this
   whole release that depends on the user, not on more code.
2. **Confirm Linux CI is genuinely green** post-`libm` fix, once the above
   is resolved (a clean, uninterrupted run).
3. **Get Windows CI green** — real, previously-untested territory (the CI
   matrix has only ever had `ubuntu-latest`/`macos-latest`). Likely real
   issues to find and fix:
   - `kupl native`'s C compiler resolution (`buildcache.rs::cc()` defaults
     to the `cc` env var or literal `"cc"` — Windows has no `cc` by
     default; needs either a documented MinGW-w64/MSVC prerequisite,
     matching the existing "a C compiler, `kupl native` only" prerequisite
     pattern already used for macOS/Linux, or CI-side MinGW setup).
   - Possible path-separator assumptions (hardcoded `/` vs `PathBuf`).
   - Possible `\r\n` vs `\n` line-ending assumptions in string
     comparisons/golden-output tests.
   - `exec()`/subprocess semantics differences.
   - Socket/networking behavior differences affecting the `weight
     distributed` test suite.
   - The core interpreter/VM/KVM path is pure Rust with no OS-specific C
     dependency, so it should need little to no change — `kupl native`
     is where the real risk concentrates.
4. **Build and publish real artifacts.** Once both platforms are green,
   use their own GitHub-hosted runners (not local cross-compilation, which
   isn't verifiable from this macOS dev machine — no Docker/mingw/musl
   cross-toolchain available locally) to produce real
   `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` binaries.
   Attach to a `v0.2.0` release alongside the existing macOS artifact.
5. **Update install docs** — remove the "only macOS published" caveat from
   the README once Linux/Windows artifacts exist.

---

## 0.3.0 — Security hardening

**Theme:** close the most concrete, code-addressable security and
soundness gaps named in `docs/PRODUCTION.md`.

1. **The effect system's known indirect-propagation gap.** Today, effect
   tracking through a component instance is precise for two syntactically
   provable cases (`let s = SomeComponent()` used same-function, and a
   component-typed parameter) but not when the instance arrives through a
   record field, a generic wrapper, a match-bound pattern, or a
   reassignment. Two possible directions, to be decided based on
   investigation cost/benefit once started:
   - Extend the existing effect-inference pass with a broader (but still
     sound, still conservative) points-to approximation covering the
     remaining cases, closing the gap for real; or
   - If a sound extension proves too invasive for this stage, at minimum:
     tighten the documentation of exactly which patterns are unproven (already
     partially done), add explicit test coverage pinning the CURRENT
     boundary so a future change can't silently widen the gap further
     un-noticed, and evaluate whether a conservative "unknown effect,
     assume the worst" fallback is viable without breaking too much
     existing code.
2. **An opt-in OS-level sandbox wrapper** (`kupl run --sandbox`) —
   directly answers `docs/PRODUCTION.md`'s own "not a sandbox" caveat with
   something concrete rather than just a warning. Scope: restrict
   filesystem and network access by default unless explicitly granted,
   using the OS's own confinement primitive per platform (`sandbox-exec`
   on macOS, a `bubblewrap`/seccomp-based restriction on Linux — vendoring
   a minimal implementation or documenting `bwrap` as an optional
   dependency, TBD during implementation). This is additive and opt-in;
   the existing "run KUPL inside your own OS-level sandbox" guidance in
   `PRODUCTION.md` remains the documented baseline regardless.
3. **Expand adversarial/fuzz test coverage** on the newest, least-battle-
   tested surfaces specifically: `aead.rs` (the ChaCha20-Poly1305
   implementation — property tests beyond the RFC 8439 vectors already
   in place), `guards.rs`'s collision-detection logic (K1010), and
   `agent`/`protocol` combinations more broadly.
4. **A fresh, deliberate self-review pass** over every security-critical
   module (`aead.rs`, `distribution.rs`, `agent_persist.rs`,
   `encoding.rs`'s crypto functions) — not a substitute for a real
   third-party audit (which this project still doesn't have and can't
   manufacture), but a genuine, documented internal review pass distinct
   from the original implementation review.

---

## 0.4.0 — Package ecosystem, minimally real

**Theme:** close the "no hosted registry" gap for real, even if small —
`docs/VERSIONING.md`'s own `1.0.0` checklist names this explicitly, and
the client side (`kupl pkg publish`/`tree`/`lock`/`fetch`, a project's own
`[registry] url` override) has been fully ready since it140; only the
hosting side is missing.

1. Design a simple static-file registry format (an `index.json` plus
   per-package tarballs) compatible with the existing client's fetch
   expectations — no new client-side protocol work should be needed if
   the existing self-hosting path (it140) is genuinely general.
2. **Host it somewhere real and zero-cost**: GitHub Pages is the natural
   choice — a dedicated repo or `gh-pages` branch serving static JSON and
   tarballs, updated by a small publish script or workflow.
3. **Publish 2-3 real packages** to prove the whole pipeline end to end —
   not placeholders. Candidates: small, genuinely useful utility packages
   factored out of existing example code (e.g. something built on the
   file/path toolkit, or a small JSON-schema-validation helper).
4. Update the default registry URL `kupl.toml`/the toolchain looks up
   to point at this real endpoint (with a documented override for anyone
   who wants their own private registry instead).
5. Document the self-hosting process clearly for third parties.

---

## 0.5.0 — Live AI-provider hardening

**Theme:** real confidence in the `anthropic`/`openai`/`ollama` code paths
beyond the mock provider — `docs/PRODUCTION.md` already flags this
honestly ("still only lightly battle-tested... verified against a local
mock HTTP server, not a live provider").

1. Add an **opt-in, key-gated** integration test tier (e.g.
   `KUPL_TEST_LIVE_AI=1` plus a real API key from the environment) that
   exercises genuine network calls against at least one real provider —
   never run by default in CI (no key available there, and shouldn't be,
   to avoid cost/flakiness on every push), but runnable deliberately
   before a release.
2. Where a live run isn't practical to gate a release on, at least
   upgrade the existing mock-HTTP-server tests to model more realistic
   failure/latency patterns than today's synthetic responses.
3. Audit the tool-use loop against each real provider's actual JSON
   schema quirks — synthetic mocks can miss edge cases a real provider's
   API genuinely produces.

---

## The usage window (no release number)

After `0.5.0`, whichever release is latest becomes the de facto release
candidate for `1.0.0`. This is **not a feature milestone** — it's a
waiting period, and it cannot be scheduled or compressed by more
engineering work:

- `docs/VERSIONING.md`'s own `1.0.0` criterion — "the project has run in a
  real, non-toy production workload for a meaningful period without a
  correctness regression" — only starts counting once someone actually
  depends on a release that way. That could be the user of this project,
  or anyone else who picks it up.
- An independent security review, if one ever happens, would also belong
  in this window — it requires a party other than the project itself.

---

## 1.0.0 — the stability commitment

Once 0.2.0–0.5.0 have shipped and a real usage window has passed without
a correctness regression surfacing:

1. Freeze the language grammar, standard library surface, CLI
   flags/subcommands, and diagnostic codes per `docs/VERSIONING.md`'s own
   breaking-change definitions — this is the actual substance of hitting
   `1.0.0`, more than any single feature.
2. A final full-spec documentation consistency pass across
   `docs/reference/LANGUAGE-REFERENCE.md`, `STDLIB.md`, `CLI.md`,
   `DIAGNOSTICS.md`, and the design docs, confirming nothing drifted
   during 0.2.0–0.5.0.
3. Tag `v1.0.0`, write real release notes summarizing the whole `0.x`
   arc, and — if desired — an actual announcement, since `1.0.0` is the
   point where outreach starts to make sense (before it, "no compatibility
   guarantee yet" makes broad promotion premature).

---

## Tracking

Each release above gets its own `CHANGELOG.md` section and git tag
(`vX.Y.Z`) following the process already documented in
`docs/VERSIONING.md`. This file should be updated as work completes or
scope changes — treat it as a living plan, not a fixed contract.
