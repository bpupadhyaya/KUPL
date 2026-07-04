> **Progress:** slice 1 landed (it36) — native SINGLE-COMPONENT apps. `kupl
> native` now compiles an `app` with instance state + an `on start` handler to
> machine code (a KCompMeta COMPS[] table, a KInstance runtime with
> k_instantiate/k_state_get/k_state_set, and an app `main()` that instantiates
> then runs @start). Children, wires, `emit`, cross-component calls, and timers
> defer with a CLEAR compile-time error ("use kupl bundle"). native stdout ==
> `kupl run` (cc-guarded test + examples/native-counter.kupl). fun-main native
> programs are unchanged. Next: child components + emit/wire.

# Big-arc design: Native components + KIR (audit #2, performance)

**Feasibility:** high · **Risk:** high · **Estimated effort:** ~5 /loop iterations
_(Produced by a parallel design workflow, 2026-07-04. Grounded in the actual source.)_

## Summary
Do NOT build a typed SSA KIR first — the existing register bytecode is already the right granularity, and cgen.rs already lowers every non-component op to correct C by mirroring interp.rs. The whole gap to native components is a C-side runtime: an instance array, a FIFO message queue, a component-metadata table, a global "current instance", a driver mirroring vm.rs run_app/drain/advance, and setjmp/longjmp for supervision restarts. KIR (unboxing tagged KValues) is a separate, much larger, optional performance layer that should be gated on profiling AFTER native components are correct — never a prerequisite.

## Key files
, , , , , , 

## Byte-identical / determinism impact
The invariant is preserved by continuing cgen's existing discipline: every new C runtime helper mirrors a specific `vm.rs`/`interp.rs` behavior (enumerated in section c). The scheduler-order invariants — creation-order instance ids, `0..ninsts` @start order, FIFO queue, first-match handler scan, push-order wire fan-out, `(time,instance,decl)` timer tie-break, `run_timers(100)` bound, `print_unwired` format, `[supervise]` stderr text — are the only new sources of divergence and are copied verbatim from vm.rs. Determinism is kept by the arena memory model (no finalizer/refcount ordering to diverge) and the already-bounded execution model (drain-to-quiescence + 100-fire timer cap), so native terminates with identical output. Tests stay deterministic via a cc-guarded inline differential test in cgen.rs (native stdout == `kupl run` stdout) plus the existing all-examples regression; `emit_c` sets `print_unwired = true` and selects the app entry exactly as `run_module` does.

## Dependencies & ordering
Self-contained arc with a strict internal order: iter 1 (instances/state/@start) is the foundation; iter 2 (children/wires/emit/queue) depends on it; iter 3 (timers) depends on 1-2; iter 4 (supervision, needs setjmp/longjmp) depends on the dispatch paths from 2-3. The KIR performance layer is explicitly NOT a dependency of any of these and should follow, gated on profiling. Cross-arc: this arc consumes bytecode/ComponentMeta produced by compile.rs unchanged — it needs no changes to the interpreter, checker, effects, or bytecode ops, so it does not conflict with language-feature arcs; it only touches cgen.rs (+ its tests) and the emit_c entry-selection in run.rs. The native json/regex/csv/ai defers overlap with any 'native stdlib' arc but are independent iterations; coordinate only to avoid duplicating the C-mirror of a given stdlib module.

## First iteration (shippable slice)
Ship native single-component apps. Concretely: (1) In `cgen.rs::emit_c`, when there is no `fun main` but `module.components` has an `is_app` component, take an app path instead of erroring (mirror `run.rs::run_module`'s selection). If any reachable component declares children, wires, `emit`, or timers, return a clear `"not yet supported by the native backend — use kupl bundle"` error (keep the slice honest and small). (2) Emit a `const KCompMeta COMPS[]` table from `ComponentMeta` (name, is_app, nslots, init_chunk, `@start` handler chunk idx) — model the emission on the existing `CTORS[]` block. (3) Add to the `RUNTIME` string: a growable `KInstance` array; a global `int k_cur_inst`; `k_instantiate(comp_idx)` that appends an instance, resizes slots to `nslots` with `k_unit()`, saves/sets/restores `k_cur_inst` around a call to the init chunk; and `k_state_get(slot)`/`k_state_set(slot, v)` reading `k_insts[k_cur_inst].slots[slot]`. (4) In `emit_op`, replace the `StateGet`/`StateSet` stubs with `regs[d] = k_state_get(slot);` / `k_state_set(slot, regs[s]);`. (5) Emit a `main()` that: instantiates the app, runs its `@start` chunk (with the instance current), then returns 0 (drain is a no-op with no emits, so a trivial safe drain loop over an empty queue is fine). (6) Add an inline `#[cfg(test)]` in `cgen.rs`, guarded on `cc` being available, that compiles a small app like `app Counter { state n: Int = 0  on start { n = n + 1  print("n={n}") } }`, runs the binary, and asserts stdout equals `kupl run` on the same source. Target an existing single-component example under `examples/` (e.g. `counter.kupl` if it has no timers) for the all-examples regression. Leave `MakeInstance/WireOp/EmitOp` op stubs and timers as clear defers so the build stays green and the invariant holds.

## Design
## Native components — design

### The central question (a): new KIR, or lower existing bytecode?

**Recommendation: lower the existing bytecode. Defer KIR entirely.** Grounded in `src/cgen.rs`: it already compiles every function chunk to a C function (`fun_i(KValue* caps, KValue* args)`), registers to a local `KValue regs[]` array, jumps to `goto L{pc}` labels, and ships a ~1500-line embedded C runtime (`RUNTIME`) whose `k_add`/`k_eq`/`k_display`/`k_cmp`/... **mirror `interp.rs` byte-for-byte** — that mirroring is exactly why native stays byte-identical today. Every non-component op is already handled. The only ops that `emit_op` stubs out with `k_panic("components are not supported")` are `StateGet/StateSet/MakeInstance/WireOp/EmitOp/CallComp` (cgen.rs lines 266-270).

Crucially, **all six of those ops are runtime concerns, not codegen concerns.** They already exist at the right granularity in `bytecode.rs`. `StateGet(reg, slot)` just needs a C helper that reads `instances[cur].slots[slot]`; `EmitOp` needs a C helper that looks up wires and pushes to a queue. None of this needs a new IR — it needs the C analogue of the `Vm` struct's `instances`, `queue`, and `now` fields (`vm.rs` lines 49-58) plus the driver methods.

**Why KIR is the wrong first step.** A typed SSA IR buys one thing: unboxing the 16-byte tagged `KValue` (value.rs) so monomorphic `Int`/`Float`/tensor loops run in raw registers instead of tag-dispatched helpers. That is a real but *orthogonal* performance win, and it is a 10+ iteration arc on its own (SSA construction with phi nodes, a type-driven unboxing analysis, re-proving byte-identical float/overflow edge cases, register allocation or reliance on the C compiler). Even without it, native-compiled components already beat the KVM meaningfully: no bytecode dispatch loop, no `Rc<Value>` clone per op, `cc -O2` inlines the helpers. **Ship correct native components on boxed KValues first; measure; only then decide whether KIR's unboxing is worth it.** Gating components on KIR would make the largest arc even larger for no correctness benefit.

### (b) The component runtime in C

Mirror `vm.rs` structurally. New pieces in the `RUNTIME` string (or a second generated section):

- **Instances.** `typedef struct { int comp; KValue* slots; int nslots; KWire* wires; int restart_on_failure; KTimer* timers; int ntimers; } KInstance;` plus a growable global `KInstance* k_insts; int k_ninsts;`. Mirrors `VmInstance` (vm.rs 40-47). Instance id = array index = creation order.
- **Current instance.** A global `int k_cur_inst = -1`. This is the C analogue of `Frame.inst: Option<usize>` (vm.rs 27). I verified closures inherit the ambient instance in the VM (`push_closure_frame`/`call_value_nested` thread `inst` through, vm.rs 342/353/362) and that lambdas compiled inside a component inherit `comp` context and may emit `StateGet` (`compile.rs:961 lc.comp = self.comp.clone()`). A single global is therefore **exactly right**: only component-context chunks ever emit `StateGet/StateSet/EmitOp`, and they always run while their instance is the ambient one. Every entry point that runs a chunk with an explicit instance (instantiate, run_lifecycle, drain-handler, timer fire, expose, restart) must **save/restore** `k_cur_inst` around the call — the same discipline `vm.rs` gets for free from its frame stack. `Op::Call` (top-level) must NOT change it; `Op::CallComp` keeps it (vm.rs 466-481). The `CHUNKS[]` function-pointer table and signatures stay uniform `(caps, args)` — no signature churn, so `k_call`/`CallValue` are untouched.
- **Component metadata table.** Generate `const KCompMeta COMPS[]` from `module.components` (`ComponentMeta`, bytecode.rs 130-147): name, is_app, nslots, init_fn_idx, restart_fn_idx, a `{port, chunk_idx, has_param}[]` handler list, an `{name, chunk_idx}[]` expose list, `out_ports[]`, and a `{chunk, every, interval_ms}[]` timer list. Exactly analogous to the existing `CTORS[]` table emission (cgen.rs 40-54).
- **Message queue.** A FIFO of `(int id, const char* port, KValue value)` mirroring `VecDeque` (vm.rs 54). A simple growable ring/linked list. `drain()` pops front, finds the handler by **linear first-match** on the instance's handler list (vm.rs 122-126 uses `.find`), calls it with `k_cur_inst` set, catches restart. Order is the byte-identical crux (see c).
- **Dispatch.** `k_emit(port, value)`: look up the current instance's wires (append-order list, mirroring the push-order `Vec` behind the `HashMap` in vm.rs 911-915), enqueue to each target; if none and `print_unwired`, print `"{comp}.{port} = {value}\n"` (vm.rs 927-931). `k_instantiate(comp_idx, props)`: append instance, resize slots to nslots, run init chunk with the new id current (vm.rs 155-170). `k_wire(from,out,to,in)`, `k_state_get/set`.
- **Virtual-clock timers.** Global `int64_t k_now`. `KTimer { int chunk; int every; int64_t interval, next_fire; int active; }`. Port `arm_timers` / `advance` / `run_timers(100)` (vm.rs 172-247) verbatim, including the `(next_fire, instance_id, decl_index)` tie-break tuple and draining between fires.
- **Supervision.** `restart_on_failure` per instance; on a handler/timer panic, run the restart chunk + re-run `@start` + re-arm, and print the `[supervise] {name} restarted after panic: {msg}` line to stderr (vm.rs 142-151). This needs a C panic **catch**: today `k_panic` calls `exit(101)`. Introduce a `sigjmp`/`setjmp` landing pad saved around each supervised dispatch; `k_panic` `longjmp`s to it when a pad is active (and still `exit(101)` at top level). Mirrors `call_chunk_nested`'s truncate-and-return-Err unwind (vm.rs 292-300).

### (b) Memory: GC vs arena

**Keep the arena (never-free malloc) for all component iterations.** The whole deterministic test model is *bounded*: `kupl run` drains to quiescence and fires at most `run_timers(100)` timers, so every program terminates (vm.rs 102/230, run.rs 304). Arena is not just adequate, it is *safer for byte-identity*: there are no finalizer/refcount-drop orderings that could ever diverge from the interpreter. A real long-running deployment (unbounded timers) would need reclamation, but that is out of scope for the sacred invariant because the tests never run unbounded. Punt GC to its own optional iteration (per-instance arena freed on `@stop`, or a simple mark-sweep), gated by actual deployment need — not correctness.

### (c) Keeping native byte-identical for components

The existing model (mirror interp/KVM helper-for-helper; a differential test compares stdout) extends directly. The **new** invariants to reproduce exactly, all read out of `vm.rs`:
1. **Creation order = instance ids.** `instantiate` appends the id *before* running init, so children (created by `MakeInstance` inside init) get higher ids — DFS pre-order, app = id 0 (vm.rs 160-168). C appends-then-runs-init identically.
2. **@start order** = `for id in 0..ninsts` after all instances exist (vm.rs 97-100): parents before children.
3. **Queue FIFO** and **handler first-match** linear scans (vm.rs 120-126).
4. **Wire fan-out in push order** (vm.rs 911-915, 933-935) — use append-order arrays, not a hash map, in C.
5. **Timer tie-break** `(time, instance, decl)` and drain-between-fires (vm.rs 197-224); `run_timers` bound of 100.
6. **print_unwired format** and the `[supervise] ...` stderr text.
7. Panic → exit(101) message text already matches; float/overflow/display already mirrored.

Validation: add an inline `#[cfg(test)]` in `cgen.rs` that shells to `cc` (guarded on `cc` availability, as env-dependent tests already are) for 2-3 canonical component apps and asserts native stdout == `kupl run` stdout; keep the existing all-examples `/loop` regression as the broad net. `emit_c` must switch its entry selection to mirror `run.rs::run_module` (app component first, else `fun main`) and set `print_unwired = true` to match `kupl run`.

### (d) Native-deferred features (tensors / ai / regex / json / csv / http)

These are **orthogonal to the component arc** and should stay clear-error defers on the component critical path:
- **Tensors:** rank-1 float ops are already in the C runtime (`k_bt_tensor/zeros/arange`, `k_tensor_binop`, cgen.rs 430-453, 637-650). Shapes/dtypes/matmul genuinely await KIR (value.rs comment says so) — leave as-is.
- **json/regex/csv/url-query:** each is a self-contained zero-dep Rust module (`json.rs`, `regex.rs`, `csv.rs`, `url.rs`). Native support = mirror each into C byte-for-byte, exactly how `encoding.rs`/`time.rs` were already mirrored (they note it in their headers). Each is its own future iteration; not needed for the first component slices.
- **ai/http:** need a curl subprocess from C (`popen("curl ...")`) to honor the zero-dep/system-curl rule — its own arc, kept as the current clean `kupl native` rejection (cgen.rs 19-24, 172-178).

### (e) Realistic minimal first slice

A **flat single-component app** — no children, no wires, no timers, no supervision. It exercises the irreducible core (instance, slots, init, `@start`, state get/set, calling component/top-level funs, print) and nothing else. `emit_c` gains an app path; any use of children/wires/emit/timers triggers a *clear* "not yet — use `kupl bundle`" defer so the slice is honest and small. See first_iteration_plan.

### Iteration breakdown (recommended scope = native components, iters 1-4)

- **Iter 1 — single-component apps:** instance array, global `k_cur_inst` + save/restore, `COMPS[]` table, `k_instantiate`, `StateGet/StateSet`, `@start`, empty-safe `drain`, app-entry driver in `emit_c`. Defer children/wires/emit/timers with clear errors.
- **Iter 2 — multi-component:** `MakeInstance` (children), `WireOp`, `EmitOp`, the FIFO queue, cross-instance handler dispatch, drain-to-quiescence, `print_unwired`.
- **Iter 3 — virtual-clock timers:** `KTimer`, `arm_timers`, `advance`, `run_timers(100)`, tie-break ordering.
- **Iter 4 — supervision:** setjmp/longjmp panic pad, `restart_on_failure`, restart chunk, `[supervise]` line.
- **Iter 5+ (optional, separate):** KIR typed-SSA unboxing for performance; per-module native json/regex/csv; native GC — each gated on measured need, none blocking correctness.
