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

## 0.2.0 — Cross-platform reliability — **RELEASED 2026-09-06**

**Theme:** KUPL builds, tests green, and ships an installable artifact on
all three major desktop platforms — not just macOS.

**Tag:** `v0.2.0` (branch `release/0.2.0`) —
github.com/bpupadhyaya/KUPL/releases/tag/v0.2.0. One item (Linux CI's
own billing/quota block, item 1 below) is still genuinely open and not
retired by this release — see the "DONE" note further down for how the
release itself worked around it without waiting on it.

Real, previously-undiscovered bugs found and fixed this cycle, each
confirmed live via actually running CI on a platform this project had
never tested before (or had never checked the results of):

- `kupl native`'s generated C code was never linked against `libm`, so
  every native compile failed on Linux from the day CI was first added
  (invisible on macOS, where libm is folded into libSystem). Fixed in
  commit `eae91e7`.
- `native_remove_dir_does_not_follow_symlinks_out_of_the_tree` used
  `std::os::unix::fs::symlink` unconditionally, failing to even COMPILE
  on Windows (`std::os::unix` doesn't exist there). Fixed by gating the
  whole test `#[cfg(unix)]` — it tests POSIX-only `lstat` semantics, no
  clean Windows equivalent exists. Commit `0bb07b2`.
- `native_exec_error_message_is_not_truncated_by_a_fixed_size_buffer`
  hardcoded an exact OS error string that differs by platform (a
  400-byte filename triggers `ENAMETOOLONG` on Linux, `ENOENT` on macOS)
  — the feature was correct on both, only the test's assumption wasn't
  portable. Fixed to check the actual regression (length, untruncated
  content) instead. Commit `0bb07b2`.
- `cc_hash()`'s manual `$PATH` search never found `cc.exe` on Windows (no
  extension handling, unlike `Command::new`'s own PATHEXT-aware
  resolution) — `cc_available()` and `cc_hash()` disagreed about whether
  the same `cc` even resolves. Fixed with a proper Windows PATHEXT-aware
  search. Commit `7f971a4`.
- **The big one**: `kupl native`/`kupl bundle` genuinely don't work on
  Windows at all today. Confirmed live: `cc` resolves to a real, working
  MinGW-w64 GCC on `windows-latest` — but every generated program's
  shared C runtime preamble unconditionally includes `<sys/wait.h>` and
  uses `fork`/`pipe`/`waitpid` for the `exec` builtin, POSIX-only APIs
  with no MinGW equivalent (Windows has no `fork` in the POSIX sense at
  all). `cc_available()`'s own test-suite gate used to just check
  `cc --version` succeeds, which is true here — it now actually compiles
  the same runtime preamble every real program gets, so native-codegen
  tests correctly SKIP on Windows instead of failing. See
  `docs/PRODUCTION.md`'s Known Limitations for the full writeup. This is
  now tracked as its own, separate, larger future item (see "Explicitly
  out of scope for 0.2.0" below) — not something to force into this
  release. The interpreter, `kupl run --vm`, and `.kx` bytecode are
  unaffected (pure Rust, no C codegen) and confirmed working on Windows.

**Status update:** `windows-latest` CI is now GREEN (confirmed live,
commit `a14dfd0`) — the full journey was 388 real/spurious failures →
10 → 1 → 0, each step a genuine bug found and fixed (see the list
above), ending with a test-timeout margin widened for a slower CI
tier. `macos-latest` also surfaced one genuine, pre-existing timing
flake under real 3-job CI contention (a `weight distributed` + `durable`
race already named in `docs/design/DISTRIBUTION.md`'s own "no
second-pass wait for confirmation" v1 simplification) — hardened with
a test-side delay in commit `6f2a186`, not a protocol fix (that's a
separate, bigger item).

**Remaining work:**
1. **Resolve the GitHub Actions billing/quota situation.** Repeated CI
   runs on `ubuntu-latest` — now confirmed across SIX separate attempts,
   including after every real fix landed in this arc — get killed
   externally (exit 143, "runner has received a shutdown signal") at a
   consistent point in test progression, while `macos-latest`/
   `windows-latest` in the same run complete (successfully, once their
   own real bugs were fixed). Working theory unchanged: the account's
   Actions-minutes quota (macOS runners cost a 10x multiplier) is
   exhausted from weeks of failing/hanging CI runs before this arc even
   started. Needs a check of github.com/settings/billing — this remains
   the one item in this whole release that depends on the user, not on
   more code. Every other platform is now confirmed clean.
2. **Confirm Linux CI is genuinely green** once the above is resolved (a
   clean, uninterrupted run — every failure seen on Linux so far has
   either been the now-fixed `libm` bug or this external kill, never a
   hang or a genuine remaining test failure). Still open — the billing
   check is the user's own action, not something this release could wait
   on indefinitely (see "**DONE**" note below for how this was worked
   around for the release itself).

**DONE, 2026-09-06 — `v0.2.0` shipped.** Rather than block the whole
release on the `ubuntu-latest` full-test-suite billing question (item 1,
still open), added `.github/workflows/release.yml`: a SEPARATE,
lightweight workflow that just runs `cargo build --release` + a smoke
test (`kupl version`, run a real example) on each platform's own native
runner — no full test suite, so it isn't exposed to the same
long-running-job quota constraint. All three platforms
(`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`x86_64-pc-windows-msvc`) built and smoke-tested cleanly on the very
first run. Tagged `v0.2.0` (branch `release/0.2.0`), published at
github.com/bpupadhyaya/KUPL/releases/tag/v0.2.0 with all three
binaries + `SHA256SUMS`. This does NOT retire item 1 above — the full
Linux test suite still needs to actually finish clean at some point,
just not as a release blocker anymore now that a real, verified Linux
binary exists via a path unaffected by the billing question.
4. **Update install docs** — DONE. README's Option C now covers all
   three platforms with platform-specific install snippets and the
   Windows `kupl native` caveat stated inline.

**Explicitly out of scope for 0.2.0** (a separate, future item, not a
release blocker): making `kupl native`'s C runtime genuinely portable to
Windows — replacing the `fork`/`pipe`/`waitpid`-based `exec` 
implementation with a `CreateProcess`-based one behind a build-time
`#ifdef _WIN32`, and auditing the rest of the generated runtime for other
POSIX-only assumptions. This is real, substantial engineering (a new
process-spawning backend, not a flag flip) and deserves its own scoped
effort rather than being rushed into a cross-platform-reliability release.

---

## 0.3.0 — Security hardening

**Theme:** close the most concrete, code-addressable security and
soundness gaps named in `docs/PRODUCTION.md`.

**Status:** item 2 (the sandbox wrapper) has a working macOS v1 landed on
`master` (`src/sandbox.rs`, `kupl run --sandbox`) — live-verified to actually
block a real network connection and a real filesystem write outside the temp
dir, not just documented. Linux (`bubblewrap`/seccomp) and Windows backends
are follow-up work, not blocking 0.3.0's release — `--sandbox` reports a
clean "not supported on this platform yet" error there rather than silently
running unconfined. Item 3 also landed: `aead.rs` and `guards.rs` each got a
seeded, deterministic fuzz suite (round-trip, single-bit-flip tamper
detection on ciphertext/tag/AAD, key/nonce avalanche, and an independent-
ground-truth check for K1010's cross-protocol guard collision detector across
random protocol counts/orderings). Items 1 and 4 below are still open.

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
