# Running KUPL in Production

An honest, code-verified account of KUPL's security posture, resource limits, and
operational behavior — and, just as importantly, of what is **not** yet production-
grade. Every claim here is checked against the implementation; where something is an
alpha-stage gap, this document says so plainly.

KUPL is **1.0-alpha**. It is feature-complete and internally consistent (four
execution engines held byte-identical, verified on every build), but it has not been
battle-tested at scale, has no package ecosystem, and its real-provider AI path is
wired but only mock-tested. Read the [Known Limitations](#known-limitations) section
before depending on it.

---

## Security model

### What is bounded

KUPL enforces a small set of hard resource limits so that a malformed input or a
runaway program fails cleanly instead of taking down the host. Each is enforced in
**every** engine that can hit it.

| Limit | Value | Where enforced |
|---|---|---|
| Recursion / call depth | `10_000` frames | interpreter (`interp.rs` `MAX_CALL_DEPTH`), KVM (`vm.rs`, `frames.len() >= 10_000`), native (`cgen.rs`, thread-local `k_depth`) |
| Tensor length | `100_000_000` elements | interpreter (`interp.rs` `MAX_TENSOR_LEN`), native (`cgen.rs` `K_MAX_TENSOR_LEN`) — `zeros`/`arange` reject oversized requests |
| JSON nesting depth | `500` levels | JSON parser (`json.rs` `MAX_JSON_DEPTH`), native (`cgen.rs` `K_MAX_JSON_DEPTH`) |
| LSP message size | `64 MiB` | language server frame reader (`lsp.rs` `MAX_MESSAGE_LEN`) — refuses an oversized `Content-Length` before allocating |
| String contents | no NUL bytes | lexer rejects `\0` and raw NUL (diagnostic `K0008`) — keeps strings safe across the native C runtime, which is NUL-terminated |

### Crash safety

A top-level panic hook (`main.rs`, `std::panic::set_hook`) converts any internal
panic into a single clean line — `kupl: internal compiler error … — this is a bug in
KUPL, not your program` — and exits `101`. You should never see a Rust backtrace or a
raw abort. The interpreter runs on a 2 GiB stack so the depth guard is reached before
the native stack is exhausted. The CLI subcommands (`run`, `check`, `fmt`, `build`,
`native`, `dis`, `diff`, `manifest`, `context`, `new`, `lsp`, …) have been crash-
fuzzed over hundreds of malformed inputs; they emit diagnostics, never panics.

### Effects

KUPL has a **static** effect discipline. A function that performs side effects must
declare them with `uses`, and the checker enforces that callers propagate the
declaration. The two effects are:

- **`io`** — any interaction with the outside world through the standard builtins
  (`print`, `eprint`, reading args, `exec`, `now`, file/stdin/HTTP operations).
- **`ai`** — calling an `ai fun` (the `ai` keyword is itself the boundary
  declaration; a `pub fun` that calls one must declare `uses ai`).

### Threat model — read this before running untrusted code

**KUPL is not a sandbox.** The effect system is a *compile-time* discipline for
reasoning about and documenting side effects — it is **not** a runtime confinement
mechanism:

- A program that declares `uses io` can do arbitrary I/O, including `exec` (spawning
  subprocesses) and network access. There is no syscall filtering, no filesystem
  jail, and no capability revocation at runtime.
- The resource limits above bound **recursion, tensor allocation, JSON nesting, and
  LSP frame size**. They do **not** bound total memory, total CPU time, wall-clock
  time, file-descriptor count, or output volume. A program can still allocate until
  the OS kills it, or loop forever.

**Do not run untrusted KUPL as a way to sandbox it.** If you need to execute
untrusted code, run KUPL inside an OS-level sandbox (container, VM, seccomp, cgroup
memory/CPU limits) — the same as you would for any other general-purpose language.

---

## Operations

### The four engines

KUPL runs the same program four ways, all byte-identical (this equivalence is the
project's core invariant, checked on every build):

| Engine | Command | Use when |
|---|---|---|
| Tree-walking interpreter | `kupl run file.kupl` | development, the reference semantics |
| KVM register bytecode VM | `kupl run --vm file.kupl` | faster execution of the same program |
| `.kx` compiled module | `kupl build file.kupl` then run | precompiled distribution |
| Native machine code | `kupl native file.kupl -o bin` | fastest; emits C, compiles with the system `cc` |

`kupl bundle` produces a self-contained executable from a multi-file program.

### Exit codes

- `0` — success.
- `1` — a diagnostic error (parse/type/effect error), a failed run, or a load error.
- `101` — an internal compiler error caught by the panic hook (please report it).

The exit code of `kupl run` on a program that calls `exit`/returns a code reflects
that program's own status.

### Environment variables

AI functions select a provider at call time via environment variables (this is what
makes `ai fun`s testable without a network):

| Variable | Effect |
|---|---|
| `KUPL_AI_PROVIDER` | `anthropic` (default), `openai`, `ollama`, `echo` (returns the composed prompt), or `mock` |
| `KUPL_AI_MOCK` / `KUPL_AI_MOCK_<FUN>` | canned response for the mock provider; if set, the mock is used regardless of provider. `<FUN>` is the upper-cased function name |
| `ANTHROPIC_API_KEY` | credential for the `anthropic` provider |
| `KUPL_AI_BASE_URL` | override the provider base URL (e.g. an OpenAI-compatible endpoint) |
| `KUPL_AI_MODEL` | override the model id |

If a mock variable is set, an `ai fun` returns the canned response with no network
call — the recommended way to make AI-using programs deterministic in tests and CI.

### Determinism notes

Valid programs produce **byte-identical** output on all four engines. Two narrow
categories are intentionally engine-dependent, and only ever on **error paths** —
they never affect the value a correct program computes:

- **Malformed-input error *message text*** for JSON parsing and `ai fun` response
  conversion may differ between the native engine and the interpreter (the native C
  runtime produces a more generic message). The accept/reject *decision* and the
  resulting value are identical — match on `Ok`/`Err` structurally, not on the string.
- **Case conversion** (`to_upper`/`to_lower`) is **ASCII-only** by definition, so it
  is identical across engines (the native runtime cannot replicate Rust's full
  Unicode casing, so the common ASCII subset is the contract).

### Performance characteristics

Output is byte-identical across engines, but *time/space complexity is not part of that
contract* — pick the engine and idiom that fit the workload:

- **In-loop accumulation.** `Str` and `List` are immutable values, so `s = s + x` or
  `xs = xs.push(x)` conceptually builds a new value each step. The **interpreter and
  KVM** detect the common self-append shape and mutate in place when the value is
  uniquely owned (no other binding aliases it), so a build loop is **O(n)**. The
  **native** backend has no ownership tracking (its C runtime copies on every append),
  so the same loop is **O(n²)** — e.g. pushing 100 000 elements one at a time takes
  milliseconds on `run`/`--vm` but seconds compiled. A value shared by another binding
  falls back to copying on every engine (value semantics are always preserved).
- **Guidance.** For large accumulation on the native backend, prefer a single bulk pass
  — `.map` / `.filter` / `.fold` / `.flat_map` over a source collection, or `.join` to
  assemble a string — each of which allocates once and is O(n) on all four engines.
  Reserve element-at-a-time `push`/`+` loops for small n or the interp/KVM engines.

---

## Known limitations

Being honest about what is not yet production-grade:

- **No package registry or ecosystem.** There is no dependency manager, no published
  library index, and no third-party packages. Programs use the (substantial, zero-
  dependency) standard library and local multi-file modules only.
- **The real-provider AI path is mock-tested, not battle-tested.** The `anthropic`,
  `openai`, and `ollama` providers are implemented, but the test suite exercises the
  **mock** provider. Real-network behavior (timeouts, retries, rate limits, partial
  responses) has not been hardened. Treat live AI calls as experimental; pin them
  behind the mock in CI.
- **Single-threaded execution.** The runtime is single-threaded; `par` expresses
  concurrency structurally but does not deliver OS-thread parallelism.
- **No incremental or persistent compilation cache.** Each invocation recompiles;
  there is no build cache or daemon.
- **Alpha stability.** The language and `.kx` binary format are versioned (a `.kx`
  built by a different compiler version is rejected with a clear message), but no
  long-term source or ABI stability is promised yet.

For the full design-vs-implemented audit, see [`GAPS.md`](GAPS.md). For the language
itself, see [`reference/LANGUAGE-REFERENCE.md`](reference/LANGUAGE-REFERENCE.md); for
every command and flag, [`reference/CLI.md`](reference/CLI.md).
