# KUPL micro-benchmarks

Small, deterministic, CPU-bound programs for tracking engine performance across
the interpreter (`kupl run`), the KVM (`kupl run --vm`), and native
(`kupl native`). Build the release binary first (`cargo build --release`) and time
with `/usr/bin/time` (wall-clock `real`).

| Benchmark | What it stresses |
|---|---|
| `fib.kupl` | recursive calls + integer arithmetic (`fib(32)`) |
| `loop.kupl` | a 5M-iteration `while` loop: variable assignment + arithmetic |
| `listwork.kupl` | `map`/`filter`/`fold` closures over a 1000-element list, ×500 |

## Baseline (Apple Silicon, release build)

Times are wall-clock seconds; lower is better. The interpreter is the reference
semantics; the KVM and native trade compile time for speed.

| Benchmark | interp | KVM | native |
|---|---|---|---|
| fib(32) | 1.52 | 0.60 | 0.26 |
| loop 5M | 0.49 | 0.27 | 0.19 |
| listwork | 0.11 | 0.05 | 0.20 |

The KVM runs ~2–3× faster than the tree-walking interpreter on compute; native is
fastest on tight numeric/recursive code. (`listwork` native is closure-call
bound — a target for later.)

## Optimization history

- **PR-it6**: environment hot-path — `Env::set` updates in place (no `to_string()`
  allocation or double-hash on every assignment) and the variable map uses an
  FNV-1a hasher instead of SipHash. Interpreter: **loop 0.94→0.49s (~48% faster)**,
  fib 1.71→1.52s (~11%), listwork 0.13→0.11s. Byte-identity unaffected.
- **PR-it7**: per-scope bindings use a linear-scan `Vec` instead of a `HashMap` — real scopes hold a handful of vars, so a scan of contiguous memory beats hashing and allocates no hash table per call/scope. Removed the custom hasher. Marginal wall-clock on the micro-benches (fib ~1.22→~1.19s, ~2-3%) but less allocation per scope; byte-identity unaffected.
- **PR-it21**: interpreter function-call fast path — a top-level call by name (not shadowed by a local) dispatches straight to the function, skipping the ~29-arm builtin match, a `Value::Fun` (String+Rc) allocation, and a redundant `db.funs` lookup that the general call path incurred per call. **fib 1.22→~0.96s (~21% faster)**; loop/listwork unchanged (not call-heavy). Byte-identity unaffected.
