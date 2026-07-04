# Big-arc design: Real-thread concurrency for par { } and par_map/par_filter

**Feasibility:** high · **Risk:** medium · **Estimated effort:** ~5 /loop iterations
_(Produced by a parallel design workflow, 2026-07-04. Grounded in the actual source.)_

## Summary
Blanket Rc->Arc is the wrong move: Value transitively holds Env (Rc<RefCell>), so it is not Send even with Arc (RefCell is not Sync) — you'd need Mutex on every env, a massive, slow, whole-codebase rewrite. Instead, keep Rc thread-local and introduce a narrow scatter-gather boundary: a Send `PortableValue` mirror plus a Send `Arc<ProgramImage>` (the AST is already plain, Send+Sync data), so worker threads deep-clone owned copies, run the reference tree-walking pure evaluator locally, and the main thread gathers results by index. Parallelize only provably-PURE work and fall back to the existing sequential path otherwise, which makes real parallelism additive and keeps interp==KVM==native byte-identical by construction.

## Key files
, , , , , , 

## Byte-identical / determinism impact
The invariant is preserved by construction, not by luck. Parallelism is a gated fast-path that only fires for empty-effect (pure) named-function callbacks over plain-data lists; every other case falls through to the current sequential `shared_method`, so its bytes are literally unchanged. When the fast-path fires, workers evaluate through the reference tree-walking pure evaluator for BOTH engines, and results are gathered by index in original order — so parallel-interp == parallel-vm == sequential-interp == sequential-vm, leaning on the already-tested pure interp==KVM equality. No io/ai/component/virtual-clock state is ever touched from a worker, so timers, laws, and example blocks remain reproducible. Native/cgen is unchanged (still sequential, same values). Determinism of failures is kept by surfacing the lowest-index worker panic (matches sequential left-to-right).

## Dependencies & ordering
Self-contained; no dependency on other audit arcs. Internal ordering is strict, tractable-first: (1) pure par_map for Value::Fun callbacks [the shippable slice]; (2) par_filter reusing the same machinery (keep-by-index); leave par_each sequential (a pure par_each is observably a no-op, and an effectful one must stay sequential). (3) Portable closures: snapshot a Closure/VmClosure whose body is pure and whose captured free vars are all plain-data into PortableValues; fall back otherwise. (4) par { } fork-join: parallelize only the all-pure, plain-data-free-var case (compile.rs:1013 / interp.rs:831), same gather-by-index; effectful/ai branches stay sequential. (5) Perf: thread-pool + per-thread ProgramDb cache to amortize image reconstruction, threshold tuning. The effects-map refactor in step 1 is a prerequisite for every later iteration. This arc must NOT be interleaved with any arc that changes Value/Env representation (e.g. an Rc→Arc or mutation-model arc) — it assumes the current Rc-based, thread-local value model.

## First iteration (shippable slice)
Ship real-thread `par_map` for the narrowest provably-safe case, additively (sequential fallback for everything else).

1. **Expose purity (src/effects.rs):** refactor so inference produces `HashMap<String, EffectSet>` in addition to `Vec<Diag>` (extract the fixpoint into `pub fn infer_effects(program) -> HashMap<String,EffectSet>`; have `check_effects` call it). Derive `pure_funs: HashSet<String>` (empty effect set).

2. **Portable boundary (new src/parallel.rs, or in value.rs):** `enum PortableValue` mirroring the plain-data Value variants; `fn to_portable(&Value) -> Option<PortableValue>` (None on Closure/Component/Bound/Fun/VmClosure); `fn from_portable(PortableValue) -> Value`. Unit-test round-trip equality for every plain-data variant.

3. **Program image:** `struct ProgramImage { funs: Arc<HashMap<String, Arc<FunDecl>>>, ctors: …, pure_funs: Arc<HashSet<String>> }`, built once where ProgramDb is built (interp.rs:38 area / run.rs). Assert it is `Send + Sync` with a static bound.

4. **Shared helper:** `pub fn try_par_map(recv, name, args, image) -> Option<Result<Value,String>>`. Preconditions: name==\"par_map\"; callback is `Value::Fun(n)` with `n in pure_funs`; all elements `to_portable` OK; `len >= THRESHOLD` (start 256). Use `std::thread::scope` + `std::thread::available_parallelism()` to chunk indices across workers; each worker builds a thread-local pure `Interp` from `image`, converts arg via `from_portable`, calls the fun via `call_value`, converts result via `to_portable`, writes to its index slot; propagate lowest-index panic. Gather into an ordered `Vec<Value>` → `Value::List`.

5. **Wire both engines:** call `try_par_map` at interp.rs:2499 (before `shared_method`) and vm.rs:715 (before `shared_method`); on `None`, fall through to the existing sequential code unchanged.

6. **Tests:** extend the differential harness (vm.rs:1532 pattern): (a) `xs.par_map(pure_fn) == xs.map(pure_fn)` for a large list crossing the threshold; (b) a heavy pure fn to actually exercise threads; (c) confirm a non-pure or closure callback still routes through the sequential path and stays identical; (d) `cargo test` + the all-examples regression stay green. No new example programs need to change output — that is the proof of byte-identity.

## Design
## The core blocker, assessed honestly

`Value` (src/value.rs:10-41) wraps everything in `Rc`, and the `Closure` variant holds an `Env`, which is `Rc<RefCell<EnvInner>>` (value.rs:232). This is decisive:

- **`Rc<T>` is never `Send`.** So `Value` cannot cross a thread boundary as-is.
- **Blanket `Rc`→`Arc` does NOT fix it.** `Arc<T>: Send` requires `T: Send + Sync`. `RefCell` is `Send` but **not `Sync`**, so `Arc<RefCell<EnvInner>>` is still not `Send`. To make an env sendable you must go to `Arc<Mutex<…>>` or `RwLock`, i.e. lock on **every variable read/write** — `Env::get` (value.rs:252) is on the hottest path in the interpreter. That is a whole-codebase rewrite (Value/Env are used in every one of the 30 source files), a large single-threaded perf regression, and it still leaves `Interp` itself (`instances: Vec`, `queue: VecDeque`, `current`, `now` — interp.rs:111-123) nowhere near thread-safe. **Recommendation: do not pursue Rc→Arc.**

Verdict: general, arbitrary-closure real-thread parallelism is **low feasibility** under the current design. But the high-value case — fan out an expensive **pure** function over a list — is **high feasibility** via a deep-clone/owned-copy boundary, because it never needs to *share* an `Rc` across threads.

## Key enabling fact

The AST is **plain owned data**: `grep -c "Rc\|RefCell" src/ast.rs` = 0. `FunDecl`, `Block`, `Expr` are `String`/`Vec`/`Box`/enums → `Send + Sync + 'static`. So a whole program image can be shared across threads behind an `Arc`, and a worker can run the reference tree-walking evaluator **locally** using thread-local `Rc` values it creates itself. `Rc` is perfectly fine *within* one thread — the rule we must never break is sharing one `Rc` allocation *between* threads. The scatter-gather boundary guarantees that: each worker gets its own owned copy.

## Recommended approach: pure scatter-gather with a portable-value boundary

Three new pieces, all zero-dep (std only):

1. **`PortableValue`** — a `Send + 'static` mirror of the plain-data subset of `Value` (Int/Float/Bool/Str/Unit/List/Ctor/Map/Set/Tensor/Range). `to_portable(&Value) -> Option<PortableValue>` returns `None` if the value contains any non-portable variant (Closure-with-env, Component, Bound, Fun, VmClosure). `from_portable(PortableValue) -> Value` rebuilds thread-local `Rc` values on the worker. This is the "serialization/deep-clone boundary" the brief mentions; a `PortableValue` mirror is cleaner and faster than byte-encoding. (Alternative considered: reuse the .kx value serializer in src/kx.rs as the boundary — viable and even less new code, but adds encode/decode overhead and couples parallelism to the module format; prefer the direct mirror.)

2. **`Arc<ProgramImage>`** — built once at load: `Arc<FunDecl>` map + ctor table + the inferred purity set. Everything a pure evaluation needs, and it is `Send + Sync` because the AST is plain data. Workers build a lightweight thread-local `ProgramDb`/`Interp` from this image (an `Interp` with empty `instances`/`queue`) and run `Interp::eval`/`call_fun` unmodified — the existing reference semantics, just on a worker thread.

3. **Purity signal at runtime.** Effects are already inferred per function in `check_effects` (src/effects.rs:85-106, fixpoint over the call graph) but the inferred map is currently **discarded** — the function returns only `Vec<Diag>`. Refactor to also return `HashMap<String, EffectSet>` (split inference from diagnostic emission; a small, self-contained change) and store the derived **pure-function set** in `ProgramImage`. A function is parallelizable iff its inferred effect set is empty (no io/io.*, no ai).

### Control flow at the call site

Add a shared helper `try_par_map(recv, name, &args, image, pool) -> Option<Value>` invoked at **both** engine call sites — interp's `builtin_method` (interp.rs:2499) *before* it falls into `shared_method`, and the VM's method dispatch (src/vm.rs:715). It returns `Some(result)` only when every precondition holds:

- method is `par_map`/`par_filter`,
- callback is `Value::Fun(name)` referencing a **pure** top-level fun (iteration 1 restriction — representable identically in both engines),
- the receiver list elements are all portable plain-data (`to_portable` succeeds),
- `items.len() >= THRESHOLD` (amortize thread spawn).

Otherwise it returns `None` and control falls through to the **existing untouched sequential `shared_method`** (interp.rs:1290-1307). This is the crux of the design: **parallelism is purely additive and gated; when any doubt exists we compute exactly as today.**

Workers always evaluate through the **reference tree-walking pure evaluator**, regardless of which engine initiated the call. Combined with the pre-existing, already-tested invariant that interp==KVM for pure functions, this yields: parallel-interp == parallel-vm == sequential-interp == sequential-vm, by construction.

### Determinism (the sacred part)

- **Results gathered by index** into a pre-sized `Vec<Option<PortableValue>>`; the output list is rebuilt in original order. `par_filter` keeps elements whose result is `Bool(true)`, in order. Identical to the sequential loop.
- **No cross-branch effects possible**: the gate requires empty effect sets, so no io, no component state, no `ai`/curl, and no interaction with the virtual clock (`now`) — the clock and component runtime are never touched from a worker. Timers, laws, and example blocks stay reproducible because the parallel path is provably observation-free.
- Panics: a worker maps a `Flow::Panic` to an error carried back with its index; the gather reports the **lowest-index** panic (matching sequential left-to-right failure), so error messages are deterministic too.

## Alternatives weighed

- **(a) Rc→Arc everywhere:** rejected — doesn't even achieve Send (RefCell), enormous blast radius, hot-path locking cost.
- **(b) Parallelize only the KVM, leave interp sequential:** rejected — it *breaks* the byte-identical invariant unless the VM's parallel result is guaranteed equal to interp's sequential result, which requires the same pure-eval determinism argument anyway; doing it once in a shared helper for both engines is strictly better.
- **(c) Owned-copy scatter-gather (recommended):** the only approach that delivers real OS-thread parallelism without touching Value/Env's single-threaded design and without risking determinism.

## Per-engine impact

- **Interp (reference):** new gated fast-path in `builtin_method`; `shared_method` untouched.
- **KVM:** same helper at the vm.rs:715 dispatch site; bytecode/compile unchanged (Par still compiles to MakeList; par_map still a Method op).
- **.kx modules:** no format change (parallelism is a runtime execution strategy, not serialized state).
- **Native/cgen:** **no change in iteration 1.** Generated C keeps its existing sequential loops (cgen.rs:1125-1140); it computes identical values, just serially. Threaded C is a separate, later, optional concern — deferring it keeps native byte-identical.

## Honest limitations

- Iteration 1 accelerates only `xs.par_map(pure_named_fun)` over large plain-data lists. Closures-over-env and the `par { ai_fun(a) ai_fun(b) }` payoff case (vm.rs:1612) are **not** parallelized (ai is effectful/curl-backed and order-sensitive) — they stay sequential, correct, deterministic. Concurrent HTTP is explicitly out of scope for this arc.
- Per-call worker `Interp` reconstruction from the `Arc` image costs O(num_threads × functions cloned). Gated behind the length threshold and capped at `available_parallelism()` workers; a per-thread cache is a later perf iteration.
