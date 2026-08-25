//! Tree-walking interpreter + single-threaded component runtime.
//!
//! Every component instance is an isolated actor with its own state env and a
//! mailbox; the runtime drains a global FIFO queue deterministically (v0.1 is
//! single-threaded — the semantics are what the future KVM scheduler must match).

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::ast::*;
use crate::check::Checked;
use crate::diag::Span;
use crate::value::{value_key_eq, Closure, Env, IntW, Value};

/// Non-local control flow during evaluation.
pub enum Flow {
    // PRODUCTION-HARDENING (PR-it1194): `already_reported` distinguishes a
    // `Flow::Panic` whose `msg` is ALREADY a complete, self-descriptive
    // test-failure report (currently only `run_forall`'s own counterexample
    // message) from a genuine runtime panic. `run.rs`'s law-reporting logic
    // used to tell these apart via `msg.starts_with("property failed for
    // ")` -- a plain user `panic("property failed for ...")` (not from
    // `run_forall` at all) collided with that exact phrase and silently lost
    // both the "panic: " prefix and the entire rich `error[K0900]` stderr
    // diagnostic. Every other construction site sets this to `false`.
    Panic { msg: String, span: Span, already_reported: bool },
    Return(Value),
    Break,
    Continue,
    /// Concurrency-v2 PR-cv2-10 (`docs/design/ASYNC_IO.md` §5): a
    /// concurrent-actor `on` handler, running on an `ActorPool` worker
    /// thread, hit a top-level `let name = <blocking builtin>(...)`
    /// statement — `K0295` already guarantees this is the ONLY shape a
    /// blocking builtin call can take inside a `concurrent component`,
    /// so no other code path can ever produce this variant. Propagates
    /// up through `exec_block`/`run_handler`/`Interp::send` exactly like
    /// `Panic`/`Return` already do (an ordinary `Err(other) => Err(other)`
    /// at every call site that doesn't specifically care about it) until
    /// `worker_loop`'s own `ActorMsg::Deliver` handling catches it and
    /// spawns a real, `Send`-safe thread to perform the actual I/O
    /// without occupying this worker for its duration. v1 scope note
    /// (see `ASYNC_IO.md`'s own doc, updated alongside this commit): only
    /// `Deliver`-triggered handler execution enables this (a NEW
    /// `Interp::allow_suspend` flag, set only around `run_handler`'s own
    /// `exec_block` call) — a `Call`-triggered `expose fun` reaching the
    /// SAME syntactic shape still executes the blocking builtin inline
    /// today, since suspending a `Call` needs its own reply channel
    /// stashed across the suspend, deliberately deferred rather than
    /// rushed alongside this first, simpler slice.
    ///
    /// Boxed (a REAL, live-caught bug, not preemptive tidiness):
    /// `SuspendedHandler` is 96 bytes, and an unboxed variant that size
    /// makes `Flow`/`EvalResult` 96 bytes too — `EvalResult` is the
    /// return type of `Interp::eval`, KUPL's own recursive expression
    /// evaluator, so every additional byte here is paid on EVERY stack
    /// frame of EVERY recursive `eval` call. Landing this unboxed first
    /// pushed an already-close-to-the-edge deep-recursion differential
    /// test (`vm::tests::diff_carmichael_korselt_vs_fermat`) into a real
    /// stack overflow — caught by running the full suite, not by static
    /// reasoning. `Box<SuspendedHandler>` shrinks this variant to one
    /// pointer (8 bytes), returning `Flow` close to its pre-PR-cv2-10 size.
    Suspend(Box<SuspendedHandler>),
}

/// Concurrency-v2 PR-cv2-10: everything needed to actually perform a
/// suspended handler's blocking I/O call elsewhere and resume execution
/// once it completes — see `Flow::Suspend`'s own doc comment.
pub struct SuspendedHandler {
    /// Which of the 4 blocking builtins to call once resumed off-thread
    /// — `(name, argc)`, mirroring `check.rs::BLOCKING_BUILTINS`'s own
    /// shape exactly, so the two stay in sync by construction.
    pub builtin: &'static str,
    /// The blocking builtin's own arguments, ALREADY evaluated (evaluating
    /// them is ordinary, synchronous `eval` work — only the builtin CALL
    /// itself is deferred).
    pub args: Vec<Value>,
    /// Where to bind the completed I/O result once resumed.
    pub bind_name: String,
    /// The statements AFTER the suspending `let`, in the SAME block —
    /// resuming means running these, in `env` (below), starting fresh
    /// once `bind_name` is bound to the I/O result.
    pub remaining: Vec<Stmt>,
    /// The env `remaining` must run in — already has every binding made
    /// by statements BEFORE the suspending `let`.
    pub env: Env,
}

pub type EvalResult = Result<Value, Flow>;

/// Concurrency-v2 PR-cv2-10: a REAL, live-caught regression guard, not
/// preemptive tidiness. `EvalResult` is the return type of every
/// recursive `Interp::eval` call, so its stack size is paid on EVERY
/// frame of KUPL's own recursive expression evaluator — a large,
/// UNBOXED `Flow` variant (the first cut of `Flow::Suspend` held a bare
/// 96-byte `SuspendedHandler` inline) silently pushed an already-deep
/// differential test (`vm::tests::diff_carmichael_korselt_vs_fermat`)
/// into a genuine stack overflow, caught only by running the full test
/// suite, not by any compiler error or static check. `Box`ing that
/// variant fixed it; this assertion keeps a future large variant from
/// reintroducing the same silent-until-you-run-an-unrelated-deep-
/// recursion-test failure mode. 64 bytes is deliberately generous
/// headroom above `Flow`'s actual current size, not a tight bound.
const _: () = assert!(std::mem::size_of::<Flow>() <= 64, "Flow grew too large -- box any new large variant");

/// Owned, indexed view of the checked program.
///
/// `Clone` (Concurrency-v2 PR-cv2-5): cheap — every field is either a
/// `HashMap<String, Rc<T>>` (cloning the map allocates a fresh table but
/// each VALUE is just an `Rc::clone`, an O(1) refcount bump, never a deep
/// AST copy) or plain owned data with no `Rc` at all. Lets a single
/// `ActorPool` worker share ONE already-built `ProgramDb` across every
/// actor it hosts (`worker_loop`'s own `cached_db`) instead of paying
/// `ProgramImage::actor_db`'s own full deep-clone-every-AST-node cost on
/// EVERY actor spawn.
#[derive(Clone)]
pub struct ProgramDb {
    pub funs: HashMap<String, Rc<FunDecl>>,
    pub components: HashMap<String, Rc<ComponentDecl>>,
    pub contracts: HashMap<String, Rc<ContractDecl>>,
    /// variant name -> (type name, field names)
    pub ctors: HashMap<String, (String, Vec<String>)>,
    /// `ai fun` runtime signatures (from the checker).
    pub ai_funs: HashMap<String, Rc<crate::ai::AiFunMeta>>,
    /// type name -> variants (for `forall` value generation).
    pub type_variants: crate::prop::TypeDb,
    /// top-level function names with no effects — safe to run on worker threads.
    pub pure_funs: std::collections::HashSet<String>,
}

impl ProgramDb {
    pub fn build(program: &Program, checked: &Checked) -> ProgramDb {
        let mut funs = HashMap::new();
        let mut components = HashMap::new();
        let mut contracts = HashMap::new();
        for item in &program.items {
            match item {
                Item::Fun(f) => {
                    funs.insert(f.name.clone(), Rc::new(f.clone()));
                }
                Item::Component(c) => {
                    components.insert(c.name.clone(), Rc::new(c.clone()));
                }
                Item::Contract(ct) => {
                    contracts.insert(ct.name.clone(), Rc::new(ct.clone()));
                }
                Item::Type(_) | Item::Law(_) => {}
            }
        }
        let ctors = checked
            .ctors
            .iter()
            .map(|(name, (ty, fields))| {
                (name.clone(), (ty.clone(), fields.iter().map(|(n, _)| n.clone()).collect()))
            })
            .collect();
        let ai_funs = checked
            .ai_funs
            .iter()
            .map(|(name, meta)| (name.clone(), Rc::new(meta.clone())))
            .collect();
        let mut type_variants = crate::prop::TypeDb::new();
        for item in &program.items {
            if let Item::Type(t) = item {
                let variants = t
                    .variants
                    .iter()
                    .map(|v| {
                        let fields =
                            v.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
                        (v.name.clone(), fields)
                    })
                    .collect();
                type_variants.insert(t.name.clone(), variants);
            }
        }
        let pure_funs = crate::effects::pure_funs(program);
        ProgramDb { funs, components, contracts, ctors, ai_funs, type_variants, pure_funs }
    }
}

/// A live timer on an instance: which handler it fires, whether it recurs, its
/// interval, and its next virtual-time firing.
pub struct TimerState {
    pub handler_idx: usize,
    pub every: bool,
    pub interval: i64,
    pub next_fire: i64,
    pub active: bool,
}

pub struct Instance {
    pub comp: Rc<ComponentDecl>,
    /// Props + state (+ children) — the instance's private heap.
    pub env: Env,
    /// out port -> [(target instance, target in port)]
    pub wires: HashMap<String, Vec<(usize, String)>>,
    pub last_emit: HashMap<String, Value>,
    /// Set by the parent's `supervise child restart on_failure`.
    pub restart_on_failure: bool,
    /// Armed `on every`/`on after` timers.
    pub timers: Vec<TimerState>,
    /// Set by the parent's `supervise child restart on_failure max N in
    /// <duration>` (BEAM/Erlang-inspired restart-intensity limit). `None`
    /// (the default, and the case whenever `max ... in ...` is omitted)
    /// preserves today's exact unlimited-restart behavior.
    pub max_restarts: Option<(u32, i64)>,
    /// Virtual-ms timestamps of past restarts still inside the sliding
    /// window, oldest first — only populated/consulted when `max_restarts`
    /// is `Some`.
    pub restart_history: VecDeque<i64>,
    /// Concurrency-v2 PR-cv2-1 (`docs/design/CONCURRENCY_V2.md` §4.1):
    /// OTHER sibling instance ids that must ALSO be restarted alongside
    /// this one, per this child's own `SuperviseDecl::strategy`. Empty
    /// for the default `RestartStrategy::OneForOne` (today's exact,
    /// unchanged behavior) — never includes this instance's own id.
    /// Computed once, at instantiation (or `:upgrade`) time, by
    /// `wire_supervision_groups`, since `comp.children`'s own declaration
    /// order is fully static (`ASYNC.md` §8.1).
    pub restart_group: Vec<usize>,
}

/// A message crossing the coordinator/actor thread boundary
/// (`docs/design/ASYNC.md` §8.4, matching its already-decided shapes
/// exactly): `Deliver` is non-blocking wire/timer delivery (step 4);
/// `Call` is a blocking `expose` request/reply (step 5) — `chain` is the
/// pending-call-cycle-detection list `§8.4`'s own deadlock hazard needs
/// (see `Interp::pending_remote_calls`'s own doc comment for why this is
/// currently unreachable but kept as a real safety net). Only the
/// coordinator/local-instance -> `Remote`-instance direction is wired up;
/// a `Remote` instance's own `emit`/`expose` reaching back OUT to the
/// coordinator or another actor is deferred (see
/// `Interp::instantiate_local`'s own wire-registration check, which
/// rejects a `concurrent` instance as a wire SOURCE with a clean error —
/// and, structurally, an actor's own code can never even HOLD a
/// `Value::Component`/`Value::Bound` referring to anything outside its
/// own subtree, since those types are never portable, K0306).
enum ActorMsg {
    Deliver(String, crate::parallel::PortableValue),
    Call {
        fn_name: String,
        args: Vec<crate::parallel::PortableValue>,
        #[allow(dead_code)] // threaded through for a future cross-actor Call; unread within a single-hop call
        chain: Vec<usize>,
        reply: std::sync::mpsc::Sender<Result<crate::parallel::PortableValue, (String, Span)>>,
    },
}

thread_local! {
    /// Concurrency-v2 PR-cv2-3 (`docs/design/CONCURRENCY_V2.md` §4.3): set
    /// once, for the whole life of an `ActorPool` worker thread, to that
    /// worker's own id (`worker_loop` sets it before entering its command
    /// loop). `None` on the coordinator's own thread and on every
    /// DEDICATED actor thread (the pre-existing per-actor-thread path,
    /// kept below as a fallback). `instantiate_concurrent` reads this to
    /// decide whether a NEW `concurrent component` may safely join the
    /// shared pool — see `ActorPool`'s own doc comment for the full
    /// deadlock-avoidance reasoning this flag exists to support.
    static POOL_WORKER_ID: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Concurrency-v2 PR-cv2-3 (`docs/design/CONCURRENCY_V2.md` §4.3): a fixed
/// pool of `N` real OS threads (`N = std::thread::available_parallelism()`,
/// mirroring `parallel.rs::par_eval`'s own sizing convention; each with the
/// SAME `WORKER_STACK_SIZE` every other interpreter-work thread in this
/// codebase uses), multiplexing MANY top-level `concurrent component`
/// actors instead of "1 OS thread per actor, always resident" — the
/// pre-existing model this section measured as degrading superlinearly
/// past roughly 1,000-3,000 live actors on real hardware (see the
/// PR-cv2-3 writeup for the actual numbers).
///
/// **Why only TOP-LEVEL actors are pooled, not every one**: `Interp` is
/// not `Send` (holds `Rc`-based `Value`/`Env`/`ProgramDb` — verified live
/// via a scratch `assert_send::<Interp>()` test before writing this code,
/// per this initiative's own "measure, don't assume" discipline; it fails
/// on `Env`'s `Rc<RefCell<EnvInner>>` and `ProgramDb`'s `Rc`-keyed maps).
/// An actor's `Interp` must therefore stay pinned to whichever OS thread
/// creates it for its ENTIRE life — a generic work-stealing scheduler
/// (any of N threads may run any ready actor) is not reachable here
/// without `unsafe impl Send`, which this codebase has zero precedent
/// for and which this document's own §3 already rules similar unsafe
/// stack/thread tricks out of scope for. Given that hard constraint,
/// this pool SHARDS instead: each pooled actor is assigned to exactly
/// one worker thread at creation (round-robin), and that worker's own
/// single-threaded command loop (`worker_loop`) owns and runs that
/// actor's `Interp` for its whole life — many actors safely share ONE
/// real OS thread this way, with no cross-thread `Interp` movement ever
/// needed, and (a genuinely nice side effect) no per-actor "currently
/// being processed" lock needed either, since a worker's own `recv()`
/// loop already processes exactly one command at a time.
///
/// A worker's command loop is fully sequential — if actor A (on worker
/// W) makes a BLOCKING `Call` to actor B, and B is ALSO pinned to W, W
/// deadlocks (busy running A's handler, waiting on a reply B can never
/// produce because W never gets back to B's own command). This is why
/// `instantiate_concurrent` only routes a NEW `concurrent component`
/// through the pool when `POOL_WORKER_ID` is `None` at the spawn site —
/// i.e., the spawning code is NOT itself currently executing inside a
/// pool worker's command loop. A `concurrent component` spawned FROM
/// pooled actor A's own handler code (a nested child) always falls back
/// to a DEDICATED thread instead, exactly like today — so A can safely
/// block on a `Call` to that child without ever risking a same-worker
/// deadlock, regardless of how deep the nesting goes (a child's own
/// FURTHER children, spawned from the child's dedicated thread, correctly
/// rejoin the pool, since blocking on THEM never ties up a pool worker —
/// only the code path that's actually running ON a worker matters, not
/// nesting depth as such). A pooled actor's own blocking `Call` to a
/// dedicated child DOES temporarily occupy its worker for the call's
/// duration — the same accepted tradeoff already documented below for
/// blocking I/O inside a handler, extended here to nested-actor calls.
///
/// **Two more honest, accepted limitations, not silently ignored**: (1)
/// a genuine Rust-level panic inside a worker's own command loop (an
/// internal KUPL bug, not a user-program `panic()`) takes down that
/// WHOLE worker — every OTHER actor sharing it becomes unreachable too,
/// a wider blast radius than today's one-dedicated-thread-per-actor
/// model, where only the single panicking actor is affected. (2) a
/// pooled actor's own slot is never reclaimed/reused after it shuts
/// down (`stop_all`) — a long-lived process that repeatedly creates and
/// destroys many top-level `concurrent` components (e.g. a long REPL
/// session) will accumulate dead slots in its assigned worker's own
/// `Vec` over time. Neither is a correctness bug (no dangling threads,
/// no wrong answers, no hang) — both are real, narrow scope limits for
/// this first version, worth revisiting if either is ever reported as an
/// actual problem for a real program shape.
struct ActorPool {
    workers: Vec<std::sync::mpsc::Sender<WorkerCmd>>,
    next: std::sync::atomic::AtomicUsize,
}

static ACTOR_POOL: std::sync::OnceLock<ActorPool> = std::sync::OnceLock::new();

impl ActorPool {
    fn get() -> &'static ActorPool {
        ACTOR_POOL.get_or_init(|| {
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let workers = (0..n)
                .map(|worker_id| {
                    let (tx, rx) = std::sync::mpsc::channel::<WorkerCmd>();
                    let tx_for_worker = tx.clone();
                    std::thread::Builder::new()
                        .stack_size(crate::parallel::WORKER_STACK_SIZE)
                        .spawn(move || {
                            POOL_WORKER_ID.with(|c| c.set(Some(worker_id)));
                            worker_loop(rx, tx_for_worker);
                        })
                        .expect("failed to spawn an actor-pool worker thread (OS refused a 2GB stack reservation)");
                    tx
                })
                .collect();
            ActorPool { workers, next: std::sync::atomic::AtomicUsize::new(0) }
        })
    }

    /// Round-robin assignment across workers — no load awareness (every
    /// worker gets the SAME stack size and does the SAME kind of work, so
    /// round-robin is a reasonable, simple starting policy; see this
    /// struct's own doc comment for why work-stealing isn't reachable
    /// here at all).
    fn assign(&self) -> std::sync::mpsc::Sender<WorkerCmd> {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.workers.len();
        self.workers[i].clone()
    }
}

/// A command sent to an `ActorPool` worker's shared channel, tagged with
/// which of that worker's OWN pooled actors (by local index into its own
/// `worker_loop`-private `Vec`) it targets — `Spawn` is the exception,
/// which CREATES a new local slot and reports the assigned index back.
enum WorkerCmd {
    Spawn {
        comp_name: String,
        portable_args: Vec<(Option<String>, crate::parallel::PortableValue)>,
        span: Span,
        image: std::sync::Arc<crate::parallel::ProgramImage>,
        ready_tx: std::sync::mpsc::Sender<Option<(String, Span)>>,
        shutdown_panic: std::sync::Arc<std::sync::Mutex<Option<(String, Span)>>>,
        local_id_tx: std::sync::mpsc::Sender<usize>,
    },
    Msg(usize, ActorMsg),
    Stop {
        local_id: usize,
        stopped_tx: std::sync::mpsc::Sender<()>,
    },
    /// Concurrency-v2 PR-cv2-10 (`docs/design/ASYNC_IO.md` §5): a
    /// suspended handler's own blocking I/O call finished off-thread.
    /// `Ok(pv)` is the builtin's own successful Rust-level `Ok(Value)`
    /// (itself a KUPL-level `Result[Str, Str]` — a network failure is
    /// already represented THERE, not here), portable-converted to cross
    /// back safely; `Err(msg)` mirrors the SAME internal/argument-error
    /// class `eval_call`'s own `.map_err(|m| Self::panic_flow(m, span))`
    /// already turns into a panic for a non-suspended call.
    IoComplete { local_id: usize, result: Result<crate::parallel::PortableValue, String> },
}

/// A pooled actor's own persistent state, owned exclusively by its
/// assigned worker's `worker_loop` — never touched from any other thread.
struct PooledActor {
    interp: Interp,
    shutdown_panic: std::sync::Arc<std::sync::Mutex<Option<(String, Span)>>>,
    /// Concurrency-v2 PR-cv2-10: `Some` while this actor's own handler is
    /// paused mid-execution, waiting on a spawned I/O thread's own
    /// `WorkerCmd::IoComplete` reply — see `Flow::Suspend`'s own doc
    /// comment. While suspended, this actor is NOT eligible to start a
    /// NEW `Deliver`/`Call` (see `pending` below) — preserves the
    /// actor model's own per-actor sequential-processing guarantee
    /// without any new queue-selection logic, since the worker's own
    /// shared channel already delivers commands in arrival order; this
    /// field is what makes the worker defer a same-actor command that
    /// arrives too early instead of running it out of order.
    suspended: Option<Box<SuspendedHandler>>,
    /// Concurrency-v2 PR-cv2-10: `Deliver`/`Call` messages that arrived
    /// for this actor WHILE `suspended.is_some()` — processed in order
    /// once the suspend resolves (`WorkerCmd::IoComplete`'s own handling
    /// drains this after a successful resume, one at a time, stopping
    /// early if resuming ITSELF suspends again via a chained blocking
    /// call).
    pending: std::collections::VecDeque<ActorMsg>,
    /// Concurrency-v2 PR-cv2-10 (a REAL bug caught by live verification,
    /// not a hypothetical): `WorkerCmd::Stop` arrives through the SAME
    /// per-worker channel as `Deliver`/`Call`, so a `Stop` queued right
    /// behind a `Deliver` that suspends would otherwise be dequeued and
    /// processed WHILE `suspended.is_some()` — tearing the actor down
    /// (and acking `stop_all`'s own blocking wait) before its spawned I/O
    /// thread's `IoComplete` ever arrives, silently discarding the rest
    /// of the handler's execution with no panic, no diagnostic, just
    /// missing output. `Some(stopped_tx)` here means a `Stop` arrived
    /// while suspended and was deferred; `WorkerCmd::IoComplete`'s own
    /// handling checks this once the actor is idle again (resumed, not
    /// re-suspended, pending queue drained) and performs the real
    /// teardown + ack at that point instead.
    pending_stop: Option<std::sync::mpsc::Sender<()>>,
}

/// The body every `ActorPool` worker thread runs for its whole life —
/// mirrors `instantiate_concurrent`'s existing dedicated-thread closure's
/// OWN startup/message/shutdown logic almost exactly, just multiplexed
/// over many actors (`actors[local_id]`) instead of owning exactly one.
/// `None` in a slot means that actor already shut down (`Stop`) or died
/// (a caught KUPL panic during `Deliver`/startup) — further `Deliver`s to
/// a dead slot are silently dropped (matching the dedicated path's own
/// "best-effort, actor already shut down" comment); a `Call` to a dead
/// slot gets an explicit error reply instead (unlike `Deliver`, a `Call`
/// caller is always blocked waiting on SOME reply, so silently dropping
/// it would hang the caller forever — a real bug caught during design,
/// not a hypothetical).
fn worker_loop(rx: std::sync::mpsc::Receiver<WorkerCmd>, tx: std::sync::mpsc::Sender<WorkerCmd>) {
    let mut actors: Vec<Option<PooledActor>> = Vec::new();
    // Concurrency-v2 PR-cv2-5 (found while investigating PR-cv2-3's own
    // "why does per-actor RSS stay ~1.4MB even after pooling" open
    // question): `ProgramImage::actor_db` deep-clones EVERY function and
    // component AST in the WHOLE program into a fresh `Rc`-based
    // `ProgramDb` (needed once per NEW single-threaded/`Rc` world, since
    // an `Rc` can't be built FROM the cross-thread-shared `Arc` the image
    // holds without copying the underlying data) -- before this fix, that
    // full clone ran again for EVERY SINGLE actor `WorkerCmd::Spawn`, even
    // though PR-cv2-3 already made many actors share ONE worker thread.
    // Cached here instead: build it once (keyed by `Arc::ptr_eq` against
    // the `ProgramImage` it came from, so a genuinely different program
    // image -- e.g. across a long REPL session's own redefinitions --
    // still gets a correctly fresh clone) and reuse a cheap `.clone()`
    // (a new `HashMap`, but `Rc::clone` per entry, never a re-copied AST)
    // for every later actor this worker hosts.
    // Concurrency-v2 PR-cv2-8: `Rc<ProgramDb>`, not an owned `ProgramDb` --
    // every actor on this worker now shares the SAME `Rc` via a cheap
    // `Rc::clone` (an O(1) refcount bump) instead of `ProgramDb::clone()`
    // (a fresh `HashMap` allocation per field, ~3KB/actor by itself, per
    // PR-cv2-6's own refined measurement) -- sound because `ProgramDb`'s
    // contents are never mutated after construction (`Interp`'s own `db`
    // field doc comment has the full grep-confirmed argument).
    let mut cached_db: Option<(std::sync::Arc<crate::parallel::ProgramImage>, std::rc::Rc<ProgramDb>)> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCmd::Spawn { comp_name, portable_args, span, image, ready_tx, shutdown_panic, local_id_tx } => {
                // Reserve this actor's own slot and reply with its
                // `local_id` BEFORE running its actual startup sequence
                // below -- this is what lets `instantiate_concurrent`'s
                // own blocking `local_id_rx.recv()` return quickly, so the
                // COORDINATOR can move on to spawning SIBLING actors
                // (onto other workers) while THIS one's `on start`/timer
                // work runs. A first version replied only AFTER the full
                // startup sequence completed -- a real, live-confirmed
                // regression caught by this codebase's own
                // `concurrent_component_runs_isolated_workers_with_value_
                // determinism_but_timing_races_on_interp_only` test (which
                // asserts two sibling actors' own `on start` prints
                // genuinely RACE across real OS threads): that version
                // serialized every top-level actor's OWN startup relative
                // to its siblings, since the coordinator's own children-
                // construction loop couldn't even REACH the next sibling's
                // spawn call until the current one's full `on start` had
                // already finished -- 100/100 runs observed only ONE
                // ordering instead of both, exactly matching what full
                // serialization predicts. Mirrors the pre-existing
                // dedicated-thread path's own established shape exactly:
                // `instantiate_concurrent`'s `.spawn()` call there ALSO
                // returns immediately (a `JoinHandle`, not a completed
                // startup) -- this reply plays the identical role for the
                // pooled path.
                let local_id = actors.len();
                actors.push(None);
                let _ = local_id_tx.send(local_id);

                let db = match &cached_db {
                    Some((cached_image, db)) if std::sync::Arc::ptr_eq(cached_image, &image) => db.clone(),
                    _ => {
                        let fresh = std::rc::Rc::new(image.actor_db());
                        cached_db = Some((image.clone(), fresh.clone()));
                        fresh
                    }
                };
                // Concurrency-v2 PR-cv2-6: `new_with_image` (not `new`) --
                // `image` is already the exact `Arc<ProgramImage>` this
                // actor must run against (see that constructor's own doc
                // comment for why `Interp::new`'s own internal
                // `ProgramImage::from_db(&db)` rebuild was a second,
                // redundant full-AST deep-clone on top of `actor_db`'s
                // own, found via a live allocation trace).
                let mut actor = Interp::new_with_image(db, image.clone());
                let comp = actor
                    .db
                    .components
                    .get(&comp_name)
                    .cloned()
                    .expect("actor_db mirrors the coordinator's own component table, so this name must resolve");
                let args: Vec<(Option<String>, Value)> = portable_args
                    .into_iter()
                    .map(|(name, pv)| (name, crate::parallel::from_portable(&pv)))
                    .collect();
                let startup = (|| -> Result<(), Flow> {
                    actor.instantiate_local(comp, &args, span)?;
                    actor.start_all()?;
                    actor.run_timers(100)?;
                    Ok(())
                })();
                let startup_panic = match &startup {
                    Err(Flow::Panic { msg, span, .. }) => Some((msg.clone(), *span)),
                    Err(_) => None,
                    Ok(()) => None,
                };
                let _ = ready_tx.send(startup_panic);
                actors[local_id] = if startup.is_ok() {
                    Some(PooledActor {
                        interp: actor,
                        shutdown_panic,
                        suspended: None,
                        pending: std::collections::VecDeque::new(),
                        pending_stop: None,
                    })
                } else {
                    None
                };
            }
            WorkerCmd::Msg(local_id, msg) => {
                let state = actors.get(local_id).and_then(|s| s.as_ref()).map(|s| s.suspended.is_some());
                match state {
                    None => match msg {
                        ActorMsg::Deliver(..) => {}
                        ActorMsg::Call { reply, .. } => {
                            let _ = reply.send(Err((
                                "internal error: concurrent instance already shut down or panicked".to_string(),
                                Span::default(),
                            )));
                        }
                    },
                    // Concurrency-v2 PR-cv2-10: a message for an actor
                    // that's CURRENTLY suspended (mid-way through a
                    // blocking I/O call) queues instead of running --
                    // preserves the actor model's own per-actor
                    // sequential-processing guarantee (running it now
                    // would mean TWO concurrent executions of this
                    // actor's own state, a correctness violation).
                    // Drained by `WorkerCmd::IoComplete`'s own handling
                    // once the suspend resolves.
                    Some(true) => {
                        if let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) {
                            slot.pending.push_back(msg);
                        }
                    }
                    Some(false) => match msg {
                        ActorMsg::Deliver(port, pv) => deliver_to_actor(&mut actors, &tx, local_id, port, pv),
                        ActorMsg::Call { fn_name, args, reply, .. } => {
                            call_actor(&mut actors, local_id, fn_name, args, reply)
                        }
                    },
                }
            }
            WorkerCmd::IoComplete { local_id, result } => {
                let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) else {
                    // The actor already shut down (or panicked) while
                    // this I/O call was still in flight -- best-effort,
                    // discard the (now-orphaned) result, matching every
                    // other "actor already gone" path in this file.
                    continue;
                };
                let Some(suspended) = slot.suspended.take() else {
                    // Should never happen (only `IoComplete` clears a
                    // `Some(suspended)`, and only ONE I/O call is ever in
                    // flight per actor at a time) -- defensive, not a
                    // reachable program state; silently ignored rather
                    // than an internal-compiler-error panic, since
                    // dropping an orphaned result has no observable
                    // wrong-answer effect on anything else.
                    continue;
                };
                let value = match result {
                    Ok(pv) => crate::parallel::from_portable(&pv),
                    Err(msg) => {
                        // Mirrors `eval_call`'s own `.map_err(|m|
                        // Self::panic_flow(m, span))` -- an internal/
                        // argument error, not an ordinary "the request
                        // failed" (already represented as a KUPL-level
                        // `Err(...)` inside `Ok(pv)` above).
                        let mut guard = slot.shutdown_panic.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some((msg, Span::default()));
                        }
                        drop(guard);
                        kill_actor(&mut actors, local_id);
                        continue;
                    }
                };
                suspended.env.define(&suspended.bind_name, value);
                slot.interp.current = Some(0);
                slot.interp.allow_suspend = true;
                let result = slot.interp.exec_stmts_checked(&suspended.remaining, &suspended.env);
                slot.interp.allow_suspend = false;
                slot.interp.current = None;
                let outcome = match result {
                    Ok(_) | Err(Flow::Return(_)) => slot.interp.drain(),
                    Err(other) => Err(other),
                };
                match outcome {
                    Ok(()) => {
                        // Resumed cleanly -- drain whatever queued while
                        // this actor was suspended, one message at a
                        // time, stopping early (naturally, via the
                        // `Some(false)`/`is_none()` guard below) if a
                        // drained message suspends the actor again.
                        loop {
                            let next = match actors.get_mut(local_id).and_then(|s| s.as_mut()) {
                                Some(slot) if slot.suspended.is_none() => slot.pending.pop_front(),
                                _ => None,
                            };
                            let Some(next) = next else { break };
                            match next {
                                ActorMsg::Deliver(port, pv) => {
                                    deliver_to_actor(&mut actors, &tx, local_id, port, pv)
                                }
                                ActorMsg::Call { fn_name, args, reply, .. } => {
                                    call_actor(&mut actors, local_id, fn_name, args, reply)
                                }
                            }
                        }
                        // Truly idle again (not suspended, nothing left
                        // in `pending`) -- if a `Stop` arrived mid-suspend
                        // and was deferred (`pending_stop`), finish it now
                        // instead of leaving `stop_all` blocked forever.
                        let still_suspended = actors
                            .get(local_id)
                            .and_then(|s| s.as_ref())
                            .is_some_and(|s| s.suspended.is_some());
                        if !still_suspended {
                            let deferred = actors
                                .get_mut(local_id)
                                .and_then(|s| s.as_mut())
                                .and_then(|s| s.pending_stop.take());
                            if let Some(stopped_tx) = deferred {
                                stop_actor_now(&mut actors, local_id);
                                let _ = stopped_tx.send(());
                            }
                        }
                    }
                    Err(Flow::Suspend(s)) => {
                        let builtin = s.builtin;
                        let args = s.args.clone();
                        if let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) {
                            slot.suspended = Some(s);
                        }
                        spawn_blocking_io(tx.clone(), local_id, builtin, args);
                    }
                    Err(Flow::Panic { msg, span, .. }) => {
                        if let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) {
                            let mut guard = slot.shutdown_panic.lock().unwrap();
                            if guard.is_none() {
                                *guard = Some((msg, span));
                            }
                        }
                        kill_actor(&mut actors, local_id);
                    }
                    Err(_) => {
                        kill_actor(&mut actors, local_id);
                    }
                }
            }
            WorkerCmd::Stop { local_id, stopped_tx } => {
                let is_suspended =
                    actors.get(local_id).and_then(|s| s.as_ref()).is_some_and(|s| s.suspended.is_some());
                if is_suspended {
                    // Defer: don't ack yet, don't tear down yet — see
                    // `PooledActor::pending_stop`'s own doc comment.
                    // `IoComplete`'s handling finishes this once the
                    // actor is idle again.
                    if let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) {
                        slot.pending_stop = Some(stopped_tx);
                    } else {
                        let _ = stopped_tx.send(());
                    }
                } else {
                    stop_actor_now(&mut actors, local_id);
                    let _ = stopped_tx.send(());
                }
            }
        }
    }
}

/// Concurrency-v2 PR-cv2-10: the actual `on stop` + teardown logic for one
/// actor, factored out of `WorkerCmd::Stop`'s own handling so
/// `WorkerCmd::IoComplete` can run the SAME teardown once a `Stop` that
/// arrived mid-suspend (see `PooledActor::pending_stop`) is finally ready
/// to complete. A no-op if the slot is already empty (actor already died).
/// Concurrency-v2 PR-cv2-10: every place an actor dies (panic, or any
/// other non-`Suspend`/`Return`/`Ok` control flow escaping a handler) used
/// to just do `actors[local_id] = None` directly — fine on its own, but if
/// a `Stop` had arrived and been deferred (`PooledActor::pending_stop`,
/// because the actor was suspended at the time) that sender would be
/// dropped WITHOUT ever being acked, and `stop_all`'s own blocking
/// `stopped_rx.recv()` for this actor would hang forever. Centralizing
/// the teardown here (used everywhere an actor is killed, not just the
/// suspend/resume paths) makes that impossible by construction.
fn kill_actor(actors: &mut [Option<PooledActor>], local_id: usize) {
    if let Some(slot) = actors.get_mut(local_id).and_then(|s| s.take()) {
        if let Some(stopped_tx) = slot.pending_stop {
            let _ = stopped_tx.send(());
        }
    }
}

fn stop_actor_now(actors: &mut [Option<PooledActor>], local_id: usize) {
    if let Some(slot_opt) = actors.get_mut(local_id) {
        if let Some(mut slot) = slot_opt.take() {
            let n = slot.interp.instances.len();
            if let Err(e) = slot.interp.stop_all(n) {
                if let Flow::Panic { msg, span, .. } = e {
                    let mut guard = slot.shutdown_panic.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some((msg, span));
                    }
                }
            }
        }
    }
}

/// Concurrency-v2 PR-cv2-10: runs ONE `Deliver` against an actor already
/// confirmed to exist and NOT currently suspended — shared by
/// `WorkerCmd::Msg`'s own fresh-message handling and
/// `WorkerCmd::IoComplete`'s own post-resume pending-queue drain, so
/// both paths apply identical suspend/panic/success logic (a single
/// implementation, not two copies that could drift apart).
fn deliver_to_actor(
    actors: &mut [Option<PooledActor>],
    tx: &std::sync::mpsc::Sender<WorkerCmd>,
    local_id: usize,
    port: String,
    pv: crate::parallel::PortableValue,
) {
    let v = crate::parallel::from_portable(&pv);
    let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) else { return };
    slot.interp.allow_suspend = true;
    let result = slot.interp.send(0, &port, v);
    slot.interp.allow_suspend = false;
    match result {
        Ok(()) => {}
        Err(Flow::Suspend(suspended)) => {
            let builtin = suspended.builtin;
            let args = suspended.args.clone();
            slot.suspended = Some(suspended);
            spawn_blocking_io(tx.clone(), local_id, builtin, args);
        }
        Err(Flow::Panic { msg, span, .. }) => {
            let mut guard = slot.shutdown_panic.lock().unwrap();
            if guard.is_none() {
                *guard = Some((msg, span));
            }
            drop(guard);
            kill_actor(actors, local_id);
        }
        Err(_) => {
            kill_actor(actors, local_id);
        }
    }
}

/// Concurrency-v2 PR-cv2-10: the pre-existing `ActorMsg::Call` handling,
/// factored into its own function for the identical reason
/// `deliver_to_actor` was — `Call` itself does NOT suspend in v1 (see
/// `Flow::Suspend`'s own doc comment: it needs its own reply-channel
/// stashed across a suspend, deliberately deferred) — a blocking builtin
/// reached via `Call` still executes inline here, byte-for-byte
/// unchanged from pre-PR-cv2-10 behavior.
fn call_actor(
    actors: &mut [Option<PooledActor>],
    local_id: usize,
    fn_name: String,
    args: Vec<crate::parallel::PortableValue>,
    reply: std::sync::mpsc::Sender<Result<crate::parallel::PortableValue, (String, Span)>>,
) {
    let Some(slot) = actors.get_mut(local_id).and_then(|s| s.as_mut()) else {
        let _ = reply.send(Err((
            "internal error: concurrent instance already shut down or panicked".to_string(),
            Span::default(),
        )));
        return;
    };
    let args: Vec<Value> = args.iter().map(crate::parallel::from_portable).collect();
    let result = slot.interp.eval_method(Value::Component(0), &fn_name, args, Span::default());
    let reply_msg = match result {
        Ok(v) => match crate::parallel::to_portable(&v) {
            Some(pv) => Ok(pv),
            None => Err((
                format!(
                    "internal error: concurrent `{fn_name}`'s return value is not portable -- K0306 should have rejected this at check time"
                ),
                Span::default(),
            )),
        },
        Err(Flow::Panic { msg, span, .. }) => Err((msg, span)),
        Err(_) => Err((
            "internal error: unexpected control flow escaped a concurrent expose call".to_string(),
            Span::default(),
        )),
    };
    let _ = reply.send(reply_msg);
}

/// Concurrency-v2 PR-cv2-10: performs a suspended handler's own blocking
/// I/O call on a fresh, plain `std::thread::spawn` thread — `Send`-safe
/// on its own terms, since `http_get`/`http_post`'s own arguments are
/// always `Str` (see `Interp::blocking_builtin_static_name`'s own doc
/// comment for why only these 2 of the 4 blocking builtins reach here):
/// extracted to owned `String`s BEFORE crossing the thread boundary
/// (mirrors `http_builtin`'s own internal `as_str` helper's exact
/// Str-vs-other-Display split, so a non-`Str` argument — reachable only
/// if a future caller passes something else — still converts identically
/// to how a non-suspended call would have handled it), reconstructed as
/// fresh, this-thread-local `Value`s once safely on the spawned thread.
/// The result crosses back via `parallel::to_portable` (the SAME
/// mechanism every other actor-boundary message already uses) — never a
/// raw `Value`, which is not `Send`.
fn spawn_blocking_io(
    tx: std::sync::mpsc::Sender<WorkerCmd>,
    local_id: usize,
    builtin: &'static str,
    args: Vec<Value>,
) {
    let str_args: Vec<String> = args
        .iter()
        .map(|v| match v {
            Value::Str(s) => s.as_str().to_string(),
            other => other.to_string(),
        })
        .collect();
    std::thread::spawn(move || {
        let values: Vec<Value> = str_args.into_iter().map(Value::str).collect();
        let result = http_builtin(builtin, &values).map(|v| {
            crate::parallel::to_portable(&v)
                .expect("http_builtin's own return value is always a portable Result[Str, Str]")
        });
        let _ = tx.send(WorkerCmd::IoComplete { local_id, result });
    });
}

/// How a `Remote` instance's actor code actually runs — see `ActorPool`'s
/// own doc comment for exactly when each variant is chosen.
enum ActorRoute {
    /// The pre-existing model: this actor gets its own OS thread for its
    /// entire life. Used for every `concurrent component` spawned from
    /// CODE already running on a pool worker's own thread (a nested
    /// child), so a parent can safely block a `Call` on it — see
    /// `ActorPool`'s doc comment.
    Dedicated {
        join: Option<std::thread::JoinHandle<()>>,
        inbox: Option<std::sync::mpsc::Sender<ActorMsg>>,
    },
    /// This actor is multiplexed onto a shared `ActorPool` worker thread
    /// alongside other actors, addressed by `local_id` within that
    /// worker's own private `Vec`.
    Pooled {
        worker_tx: std::sync::mpsc::Sender<WorkerCmd>,
        local_id: usize,
        /// `Some` once `stop_all` has sent `WorkerCmd::Stop` — the
        /// receiving half of that command's own one-shot completion
        /// signal, taken (and awaited) by `stop_all`'s second pass.
        stopped_rx: Option<std::sync::mpsc::Receiver<()>>,
        /// Set once `Stop` has been sent, so a `Call`/`Deliver` arriving
        /// after shutdown-has-begun reports "already shut down" instead
        /// of silently hanging or racing the worker's own shutdown --
        /// mirrors `Dedicated`'s `inbox: None` check exactly.
        stop_sent: bool,
    },
}

/// A handle to a `concurrent`-marked instance running on its own actor
/// thread (`docs/design/ASYNC.md` §8.2/§8.10 steps 3-4).
///
/// The actor's closure runs its OWN initial lifecycle — `instantiate` +
/// `start_all` + `run_timers(100)`, mirroring `kupl run`'s own top-level
/// startup sequence for this one instance — then, unlike step 3, does
/// NOT immediately run `stop_all` and exit. Instead it enters a
/// message-servicing loop (`while let Ok(msg) = inbox.recv() { ... }`),
/// staying alive to receive `Deliver` messages until `Interp::stop_all`
/// closes its inbox (dropping the coordinator's own `Sender` half, which
/// ends the actor's `recv()` loop the standard Rust channel way — no
/// explicit shutdown message needed) — at which point the actor runs its
/// OWN `stop_all` and the thread finishes.
///
/// **A real, named limitation, not silently ignored**: a `concurrent`
/// instance's recurring (`on every`) timers only fire during its initial
/// `run_timers(100)` burst at startup — nothing advances its virtual
/// clock again during the message-servicing phase (that needs §8.5's
/// shared next-fire table, deferred past this step).
pub struct ActorHandle {
    /// Concurrency-v2 PR-cv2-3: either a dedicated OS thread (the
    /// pre-existing model) or a shared `ActorPool` worker slot — see
    /// `ActorRoute`'s own doc comment for exactly which is chosen and why.
    route: ActorRoute,
    /// Signaled once, by the actor, right after its own initial
    /// `instantiate_local` + `start_all` + `run_timers(100)` complete —
    /// consumed by `Interp::start_all` so "on start has been delivered to
    /// every instance" stays true by the time IT returns, even though the
    /// actor's own thread keeps running (servicing its inbox) well past
    /// that point. `None` on success; `Some((msg, span))` — production-
    /// hardening 1222, symmetric to `shutdown_panic` below — if that
    /// initial sequence produced a genuine KUPL `Flow::Panic` (this actor's
    /// own `on start` handler, or a portability/instantiation failure, or
    /// one propagated up from a NESTED `concurrent` child's OWN failed
    /// startup). Before this field carried real information, a startup
    /// panic was only ever `eprintln!`-reported inside the actor's own
    /// closure — `Interp::start_all` unconditionally treated ANY signal on
    /// this channel as plain readiness and moved on, so the WHOLE PROCESS
    /// exited 0 despite an unhandled panic, instead of the `error[K0900]`/
    /// exit code 101 an IDENTICAL `on start { panic(…) }` on a plain
    /// (non-`concurrent`) component already correctly produces.
    ready: Option<std::sync::mpsc::Receiver<Option<(String, Span)>>>,
    /// Production-hardening 1221: a REAL, live-confirmed bug — before this
    /// field existed, a genuine KUPL `Flow::Panic` raised by THIS actor's
    /// OWN `on stop` handler (a normal, catchable panic — nothing to do
    /// with PR-it1218's Rust-level-thread-panic concern) was only ever
    /// `eprintln!`-reported inside the actor's own closure, then silently
    /// discarded — the actor's thread still returned normally, so its own
    /// `join()` reported success, and the WHOLE PROCESS exited 0 despite an
    /// unhandled panic, instead of the `error[K0900]: panic: …` / exit code
    /// 101 an IDENTICAL `on stop { panic(…) }` on a plain (non-`concurrent`)
    /// component already correctly produces. Set (once) by the actor's own
    /// closure right before it returns, if `Interp::stop_all`'s own final
    /// call (on the actor's OWN local instance tree) returned
    /// `Err(Flow::Panic { .. })` — read back by `Interp::stop_all` (the
    /// CALLER'S copy, whether that's the top-level coordinator or, for a
    /// nested `concurrent` child, an ANCESTOR actor's own `stop_all`) right
    /// after a clean `join()`, so the original message and span propagate
    /// through however many levels of `concurrent` nesting exist, with the
    /// SAME fidelity a local component's own panic already gets — never
    /// downgraded to a generic message.
    shutdown_panic: std::sync::Arc<std::sync::Mutex<Option<(String, Span)>>>,
}

/// Every one of the 14 functions across this file that used to index
/// `Vec<Instance>` directly now indexes `Vec<InstanceSlot>` instead — see
/// `docs/design/ASYNC.md` §8.2. `Local` is today's exact, unmodified
/// `Instance`; `Remote` is filled in starting at §8.10 step 3.
pub enum InstanceSlot {
    Local(Instance),
    Remote(ActorHandle),
}

impl InstanceSlot {
    /// Every DIRECT `Instance`-field call site in this codebase expects a
    /// `Local` instance — a `Remote` instance's own state lives on its own
    /// thread, never reachable this way; every legitimate cross-thread
    /// interaction goes through `ActorMsg`/`ActorHandle` instead (`emit`/
    /// `send` for `Deliver`, `eval_method` for `Call`), which check
    /// `matches!(_, InstanceSlot::Remote(_))` explicitly BEFORE ever
    /// calling this. Hitting this on a `Remote` slot is therefore a
    /// genuine bug (a call site that forgot that check), not a reachable
    /// runtime case; panicking (rather than silently misbehaving) matches
    /// this codebase's own "clean panic over silent wrong answer"
    /// discipline.
    pub fn unwrap_local(&self) -> &Instance {
        match self {
            InstanceSlot::Local(i) => i,
            InstanceSlot::Remote(_) => {
                panic!("internal error: InstanceSlot::Remote accessed directly, bypassing the ActorMsg/ActorHandle boundary (docs/design/ASYNC.md §8.2) -- this is a KUPL bug, not a reachable program state")
            }
        }
    }

    pub fn unwrap_local_mut(&mut self) -> &mut Instance {
        match self {
            InstanceSlot::Local(i) => i,
            InstanceSlot::Remote(_) => {
                panic!("internal error: InstanceSlot::Remote accessed directly, bypassing the ActorMsg/ActorHandle boundary (docs/design/ASYNC.md §8.2) -- this is a KUPL bug, not a reachable program state")
            }
        }
    }
}

pub struct Interp {
    /// Concurrency-v2 PR-cv2-8: `Rc<ProgramDb>`, not an owned `ProgramDb`
    /// -- found while refining PR-cv2-6's own "~4.2KB/actor, mostly
    /// `ProgramDb::clone()`'s seven small `HashMap`/`HashSet` fields"
    /// measurement. `ProgramDb`'s contents are never mutated after
    /// construction anywhere in this codebase (confirmed by grepping for
    /// `.db.<field>.insert/remove/clear/extend` across `interp.rs` and
    /// `repl.rs` — zero matches; every place that "changes" an `Interp`'s
    /// own program data does so by building a WHOLE NEW `Interp` via
    /// `Interp::new`, never by mutating an existing one's `db` field in
    /// place, e.g. `repl.rs`'s own `:upgrade`/redefinition mechanism).
    /// Given that, sharing it via `Rc::clone` (an O(1) refcount bump)
    /// instead of `ProgramDb::clone()` (a fresh `HashMap` allocation per
    /// field) removes that ~3KB/actor entirely for every actor sharing a
    /// pool worker with an already-spawned sibling — `worker_loop`'s own
    /// `cached_db` now stores this `Rc` directly. Every existing
    /// `self.db.<field>` read call site is unaffected by this change
    /// (`Rc<ProgramDb>` auto-derefs to `&ProgramDb`, so no other call site
    /// in this codebase needed to change at all).
    pub db: std::rc::Rc<ProgramDb>,
    pub instances: Vec<InstanceSlot>,
    pub queue: VecDeque<(usize, String, Value)>,
    /// Instance currently executing a handler (target of `emit`).
    pub current: Option<usize>,
    /// Print unwired emissions (used by `kupl run` for observable output).
    pub print_unwired: bool,
    pub globals: Env,
    /// The virtual clock (milliseconds). Advanced explicitly — never wall-clock,
    /// so timer-driven behavior is deterministic and reproducible.
    pub now: i64,
    /// Send+Sync program snapshot enabling the real-thread `par_map` fast path.
    /// `None` on worker interps (they stay sequential — no nested threading).
    pub image: Option<std::sync::Arc<crate::parallel::ProgramImage>>,
    /// Current user-function call depth. Guards against unbounded recursion so a
    /// deeply-recursive program yields a clean `stack overflow` panic instead of a
    /// fatal, uncatchable native-stack abort — and matches the KVM's 10 000-frame
    /// limit so the two engines stay byte-identical on deep recursion.
    pub call_depth: usize,
    /// Remaining LOOP iteration budget for THIS interp session -- shared by
    /// BOTH `Stmt::While` (production-hardening PR-it1156) and `Stmt::For`
    /// (production-hardening PR-it1179: the SAME "kupl test has no hang
    /// guard" bug class PR-it1156 fixed for `while` was never extended to
    /// `for`, even though a `for` loop over a huge/adversarial `Range` is
    /// just as capable of running a single test item for an unbounded
    /// amount of wall-clock time -- live-confirmed BEFORE this fix: a `for`
    /// loop performing the SAME 20,000,000 trivial iterations a `while`
    /// loop hits its budget on in ~2.5s instead ran to full completion,
    /// unbounded, in ~7.7s; a genuinely large range, e.g. `for i in
    /// 0..100_000_000_000 { }`, would scale proportionally into HOURS with
    /// zero guard). `None` for every ordinary interp session (`kupl run`/
    /// `kupl check`/native-adjacent worker interps) -- a legitimate app's
    /// own `while true` event loop or a genuinely large `for` iteration
    /// must NEVER be capped. Only `run_tests` (`kupl test`) sets this to
    /// `Some(MAX_TEST_LOOP_ITERATIONS)` on a freshly-constructed, per-test-
    /// item `Interp`, so a runaway/accidental infinite `while` OR an
    /// excessively large `for` range/list inside a SINGLE `example`/law/
    /// contract-law body fails with a clean panic (caught by run_tests's
    /// own already-existing `Flow::Panic` handling, exactly like any other
    /// test failure) instead of hanging (or merely running for an
    /// unreasonably long time) the whole `kupl test` invocation. A single
    /// shared budget (not a separate one per loop KIND) also correctly
    /// bounds the TOTAL loop work across nested/sequential while+for
    /// combinations within one test item, not just each construct in
    /// isolation.
    pub test_step_budget: Option<u64>,
    /// Ids of `concurrent` (`Remote`) instances this `Interp` is CURRENTLY
    /// blocked inside a `Call` to (`docs/design/ASYNC.md` §8.4's own
    /// deadlock-cycle-detection decision) — `eval_method` inserts before
    /// sending a `Call` and removes after the reply arrives. If a NEW call
    /// targets an id already in this set, that would mean the actor being
    /// called is (transitively) already waiting on THIS caller — refused
    /// immediately with a clean panic instead of deadlocking. Not actually
    /// reachable today (an actor's own subtree can never hold a reference
    /// back to its caller, since `Value::Component`/`Value::Bound` are
    /// never portable — K0306 — so no cycle can form while `concurrent`
    /// instances can't be wire sources either); kept as a real, tested
    /// safety net for if either restriction is ever lifted, exactly
    /// matching what this design already committed to.
    pub pending_remote_calls: std::collections::HashSet<usize>,
    /// Concurrency-v2 PR-cv2-10 (`docs/design/ASYNC_IO.md` §5): `true`
    /// only while executing a `Deliver`-triggered `on` handler's own body
    /// (`run_handler` sets it, restores the previous value after) --
    /// `exec_block`'s own top-level blocking-builtin-`let` check only
    /// fires when this is `true`, scoping suspend/resume to that ONE
    /// execution path deliberately (see `Flow::Suspend`'s own doc
    /// comment for why `Call`/`expose fun` execution doesn't set this
    /// yet). Default `false` for every ordinary `Interp` (coordinator,
    /// dedicated-thread actor, or a pooled actor NOT currently inside
    /// `run_handler`) — suspending only ever makes sense on an
    /// `ActorPool` worker thread in the first place (checked separately,
    /// via `POOL_WORKER_ID`), but this flag ALSO gates the Call-vs-
    /// Deliver distinction, so both conditions are checked together.
    pub allow_suspend: bool,
}

/// Maximum user-function call depth, shared by the interpreter and the KVM
/// (`vm.rs`) so both report `stack overflow (10000 frames)` at the same point.
pub const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum element count for a `zeros`/`arange` tensor. A sanity bound so a huge
/// or accidental size (e.g. `arange(100000000000)`) fails with a clean panic
/// instead of hanging the process or triggering the OS OOM killer. 100M f64 is
/// 800 MB — generous for real numeric work; the native backend enforces the same
/// limit so all engines agree.
pub const MAX_TENSOR_LEN: u64 = 100_000_000;

/// Bound on messages drained in one quiescence pass, so a wiring cycle (e.g.
/// `wire a.out -> a.in` where the handler re-emits) fails with a clean panic
/// instead of hanging the process. Real apps settle in far fewer; identical on
/// the interpreter, KVM (`vm.rs`), and native runtime (`cgen.rs`).
pub const MAX_COMPONENT_MESSAGES: u64 = 1_000_000;

/// Sanity bound on a SINGLE message's payload size (`Value::approx_byte_size`),
/// so a wiring cycle whose handler grows its payload each hop (e.g. `emit
/// grown(s + s)` on a self-wire) fails with a clean panic instead of climbing
/// toward the OS OOM killer -- `MAX_COMPONENT_MESSAGES` alone doesn't catch
/// this, since exponential growth blows past any reasonable memory budget in
/// a tiny fraction of the message-count cap (confirmed live: 512MB after just
/// 30 messages, 0.003% of 1,000,000). 10MB mirrors `registry.rs`/`interp.rs`'s
/// own `MAX_HTTP_RESPONSE_SIZE` sizing (PR-it751); identical on the
/// interpreter, KVM (`vm.rs`), and native runtime (`cgen.rs`).
pub const MAX_COMPONENT_MESSAGE_BYTES: u64 = 10_000_000;

/// Bound on timer fires processed within a single `advance()` call (an
/// `example` block's `advance <duration>` step). A duration literal's
/// MAGNITUDE is already capped at 100 years (`parser.rs::MAX_DURATION_MS`,
/// PR-it728), but the RATIO between an `advance` step's duration and a
/// timer's interval was never bounded -- both can independently sit at that
/// cap, so an entirely ordinary `on every 1ms { ... }` soak-tested with
/// `advance 100y` requires ~3.156e12 loop iterations (days of wall-clock
/// time, confirmed empirically at ~8.7M fires/sec on this hardware), with
/// no progress output and no way to bound it short of killing the process.
/// 10M mirrors `regex.rs::MATCH_BUDGET`'s "generous for real use, but caps
/// runaway growth" sizing; identical on the interpreter, KVM (`vm.rs`), and
/// native runtime (`cgen.rs`).
pub const MAX_ADVANCE_FIRES: usize = 10_000_000;

/// Bound on `while`-loop AND `for`-loop iterations (combined, one shared
/// budget) within a single `kupl test` example/law/contract-law body
/// (production-hardening PR-it1156 for `while`, extended to `for` at
/// PR-it1179 -- see `Interp::test_step_budget`'s own doc comment for the
/// full writeup of why `for` needed the identical guard `while` already
/// had). Empirically calibrated on this hardware: 20,000,000 trivial
/// `while` iterations took ~5 seconds (`kupl run`), so 10,000,000 bounds a
/// hung/excessively-long test to roughly 2.5 seconds before it cleanly
/// fails -- comfortably beyond what ANY legitimate, deterministic
/// `example`/`law` body should ever need (these are meant to be small,
/// fast unit tests, not loop-heavy batch jobs), while still failing fast
/// enough to be CI-friendly. Value matches `regex.rs::MATCH_BUDGET` and
/// this file's own `MAX_ADVANCE_FIRES` sizing convention exactly, for the
/// same "generous for real use, but caps runaway growth" rationale. Only
/// ever set via `Interp::test_step_budget`, and only by `run_tests` -- see
/// that field's own doc comment for why every OTHER interp session must
/// never be capped.
pub const MAX_TEST_LOOP_ITERATIONS: u64 = 10_000_000;

impl Interp {
    pub fn new(db: ProgramDb) -> Interp {
        let image = Some(crate::parallel::ProgramImage::from_db(&db));
        Interp {
            db: std::rc::Rc::new(db),
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
            image,
            call_depth: 0,
            test_step_budget: None,
            pending_remote_calls: std::collections::HashSet::new(),
            allow_suspend: false,
        }
    }

    /// Concurrency-v2 PR-cv2-6: identical to `new`, except it takes an
    /// ALREADY-BUILT `Arc<ProgramImage>` instead of building a fresh one
    /// from `db` via `ProgramImage::from_db` -- found while investigating
    /// PR-cv2-5's own "residual per-actor cost, not yet identified"
    /// question: `Interp::new`'s own `ProgramImage::from_db(&db)` call was
    /// doing a SECOND full deep-clone of every function/component AST for
    /// EVERY actor (the FIRST being `ProgramImage::actor_db`'s own clone,
    /// already cached per-worker by PR-cv2-5) -- live-confirmed via a
    /// scratch allocation trace as the dominant cost, ~588KB-620KB of the
    /// ~588KB measured per actor. Both `worker_loop`'s `WorkerCmd::Spawn`
    /// handler and `spawn_dedicated_actor` already HAVE a perfectly good
    /// `Arc<ProgramImage>` in scope (the same one `ActorPool`/the
    /// dedicated-thread closure was handed at spawn time) -- reusing it
    /// via a cheap `Arc::clone` instead of rebuilding is always correct,
    /// since a `concurrent` actor's own image must be identical to its
    /// coordinator's own image by construction (there is no mechanism for
    /// an actor to run against a DIFFERENT program than the one it was
    /// spawned from).
    pub fn new_with_image(db: std::rc::Rc<ProgramDb>, image: std::sync::Arc<crate::parallel::ProgramImage>) -> Interp {
        Interp {
            db,
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
            image: Some(image),
            call_depth: 0,
            test_step_budget: None,
            pending_remote_calls: std::collections::HashSet::new(),
            allow_suspend: false,
        }
    }

    /// A worker interpreter for the parallel fast path: no program image, so its
    /// own `par_map` calls stay sequential (no nested thread explosion).
    pub fn new_bare(db: ProgramDb) -> Interp {
        Interp {
            db: std::rc::Rc::new(db),
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
            image: None,
            call_depth: 0,
            test_step_budget: None,
            pending_remote_calls: std::collections::HashSet::new(),
            allow_suspend: false,
        }
    }

    fn panic_flow(msg: impl Into<String>, span: Span) -> Flow {
        Flow::Panic { msg: msg.into(), span, already_reported: false }
    }

    // ---------------- component runtime ----------------

    /// Construct ONE child instance from its own `ChildDecl` (evaluating its
    /// constructor args against `parent_env`) and apply its own supervise
    /// policy from `supervises`, if any. Shared by `instantiate`'s own
    /// children loop and `repl.rs`'s `:upgrade` newly-added-child migration
    /// (it114), so the two stay consistent by construction rather than by
    /// convention — a bug fixed in one would otherwise need remembering to
    /// fix in the other.
    pub(crate) fn instantiate_child(
        &mut self,
        supervises: &[SuperviseDecl],
        child: &ChildDecl,
        parent_env: &Env,
    ) -> EvalResult {
        let mut child_args = Vec::new();
        for a in &child.args {
            let v = self.eval(&a.value, parent_env)?;
            child_args.push((a.name.clone(), v));
        }
        let v = self.instantiate(&child.component, &child_args, child.span)?;
        if let Value::Component(cid) = v {
            let supervise = supervises
                .iter()
                .find(|s| s.child == child.name && s.policy == SupervisePolicy::RestartOnFailure);
            if let Some(s) = supervise {
                self.instances[cid].unwrap_local_mut().restart_on_failure = true;
                self.instances[cid].unwrap_local_mut().max_restarts = s.max_restarts;
            }
        }
        Ok(v)
    }

    /// Concurrency-v2 PR-cv2-1 (`docs/design/CONCURRENCY_V2.md` §4.1):
    /// compute and store each supervised child's own `restart_group` —
    /// the OTHER sibling ids that must ALSO restart when this one fails,
    /// per its own `SuperviseDecl::strategy`. Must run AFTER every
    /// sibling's own id is known (a group can reference a sibling
    /// declared anywhere in `children`), so this is a separate pass, not
    /// folded into `instantiate_child` (which handles exactly one child
    /// at a time, before its own siblings necessarily even exist yet).
    /// Shared by `instantiate_local`'s own children loop and `repl.rs`'s
    /// `:upgrade` path — called for EVERY child (kept or newly
    /// constructed) on an upgrade, deliberately: unlike
    /// `restart_on_failure`/`max_restarts` (simple per-child scalars
    /// `repl.rs`'s own doc comments already treat as an untouched part
    /// of a KEPT child's identity across an upgrade), `restart_group` is
    /// a pure DERIVED cache over "the whole sibling set as it exists
    /// right now" — recomputing it fresh for every child after any
    /// upgrade is the correct behavior, not a special case; a KEPT
    /// child's group must reflect newly-added/removed siblings too, or
    /// it would silently serve a stale view of who its own restart
    /// strategy actually covers.
    pub(crate) fn wire_supervision_groups(
        &mut self,
        supervises: &[SuperviseDecl],
        children: &[ChildDecl],
        child_ids: &HashMap<String, usize>,
    ) {
        for decl in supervises {
            if decl.policy != SupervisePolicy::RestartOnFailure {
                continue;
            }
            let Some(&this_id) = child_ids.get(&decl.child) else {
                continue; // e.g. a KEPT-but-now-removed child during :upgrade; nothing to wire
            };
            let this_pos = children.iter().position(|c| c.name == decl.child);
            let group: Vec<usize> = match decl.strategy {
                RestartStrategy::OneForOne => Vec::new(),
                RestartStrategy::OneForAll => supervises
                    .iter()
                    .filter(|s| s.policy == SupervisePolicy::RestartOnFailure && s.child != decl.child)
                    .filter_map(|s| child_ids.get(&s.child).copied())
                    .collect(),
                RestartStrategy::RestForOne => supervises
                    .iter()
                    .filter(|s| s.policy == SupervisePolicy::RestartOnFailure && s.child != decl.child)
                    .filter(|s| {
                        let s_pos = children.iter().position(|c| c.name == s.child);
                        matches!((this_pos, s_pos), (Some(a), Some(b)) if b > a)
                    })
                    .filter_map(|s| child_ids.get(&s.child).copied())
                    .collect(),
            };
            self.instances[this_id].unwrap_local_mut().restart_group = group;
        }
    }

    /// Create an instance of `comp_name`; args are already-evaluated prop
    /// values. Dispatches to `instantiate_concurrent` for a `concurrent`
    /// component (`docs/design/ASYNC.md` §8.10 step 3) — every OTHER call
    /// site in this codebase (including `instantiate_concurrent` itself,
    /// constructing the ROOT instance it was asked to host) goes through
    /// `instantiate_local` directly, bypassing this check, since that
    /// specific instance has already been decided to run HERE.
    pub fn instantiate(
        &mut self,
        comp_name: &str,
        args: &[(Option<String>, Value)],
        span: Span,
    ) -> EvalResult {
        let Some(comp) = self.db.components.get(comp_name).cloned() else {
            return Err(Self::panic_flow(format!("unknown component `{comp_name}`"), span));
        };
        if comp.concurrent {
            return self.instantiate_concurrent(comp, args, span);
        }
        self.instantiate_local(comp, args, span)
    }

    /// The ordinary, single-threaded construction path — every field of
    /// `comp` is used exactly as before `concurrent` existed (props,
    /// state, children, wires, all on THIS `Interp`/thread). Called
    /// directly (bypassing `instantiate`'s own concurrent-check) by
    /// `instantiate_concurrent`'s spawned closure to construct the root
    /// instance it was asked to host; called indirectly, via the ordinary
    /// `instantiate` dispatcher above, for every non-concurrent instance
    /// (which is every instance in every program that doesn't use
    /// `concurrent` at all).
    fn instantiate_local(
        &mut self,
        comp: Rc<ComponentDecl>,
        args: &[(Option<String>, Value)],
        span: Span,
    ) -> EvalResult {
        let env = self.globals.child();

        // props: by name or position, else default. Production-hardening
        // PR-it1079: a positional arg used to resolve via `*j == i` (its own
        // raw index in `args` equal to the prop's own declared index `i`),
        // which is only correct when every named arg's own list position
        // happens to align with its target prop's declared index -- a named
        // arg for a LATER prop appearing BEFORE a positional one broke this
        // (the positional's raw index landed on the prop just claimed by
        // name, so NEITHER that prop NOR the one actually meant ever got a
        // match, panicking "missing required prop" for the WRONG prop).
        // Fixed with the same cursor algorithm as `check.rs::check_ctor_args`
        // (which this function must stay consistent with, now that check.rs
        // accepts a call shape this function must resolve identically):
        // resolve every arg's target prop index in ONE pass up front,
        // advancing a cursor past any prop slot an EARLIER arg (name or
        // position) already claimed, then read each prop's slot below.
        let mut supplied_by_idx: Vec<Option<Value>> = vec![None; comp.props.len()];
        let mut next_positional = 0usize;
        for (name, v) in args {
            let idx = match name {
                Some(n) => comp.props.iter().position(|p| &p.name == n),
                None => {
                    while next_positional < comp.props.len()
                        && supplied_by_idx[next_positional].is_some()
                    {
                        next_positional += 1;
                    }
                    let idx = (next_positional < comp.props.len()).then_some(next_positional);
                    next_positional += 1;
                    idx
                }
            };
            if let Some(idx) = idx {
                supplied_by_idx[idx] = Some(v.clone());
            }
        }
        for (i, prop) in comp.props.iter().enumerate() {
            let value = match (supplied_by_idx[i].take(), &prop.default) {
                (Some(v), _) => v,
                (None, Some(d)) => self.eval(d, &env)?,
                (None, None) => {
                    return Err(Self::panic_flow(
                        format!("missing required prop `{}` for `{}`", prop.name, comp.name),
                        span,
                    ))
                }
            };
            env.define(&prop.name, value);
        }

        // state
        for s in &comp.state {
            let v = self.eval(&s.init, &env)?;
            env.define(&s.name, v);
        }

        let id = self.instances.len();
        // Every instance is `Local` until §8.10 step 3 gives `concurrent`
        // components their own actor thread — `comp.concurrent` is not
        // consulted here yet (docs/design/ASYNC.md §8.2).
        self.instances.push(InstanceSlot::Local(Instance {
            comp: comp.clone(),
            env: env.clone(),
            wires: HashMap::new(),
            last_emit: HashMap::new(),
            restart_on_failure: false,
            timers: Vec::new(),
            max_restarts: None,
            restart_history: VecDeque::new(),
            restart_group: Vec::new(),
        }));

        // children (constructed after the parent exists, in declaration order)
        let mut child_ids: HashMap<String, usize> = HashMap::new();
        for child in &comp.children {
            let v = self.instantiate_child(&comp.supervises, child, &env)?;
            if let Value::Component(cid) = v {
                child_ids.insert(child.name.clone(), cid);
            }
            env.define(&child.name, v);
        }
        // Concurrency-v2 PR-cv2-1: wire up `one_for_all`/`rest_for_one`
        // restart groups now that every child's own id is known (a
        // group can reference a SIBLING declared anywhere in
        // `comp.children`, so this must run AFTER the whole loop above,
        // not per-child inside `instantiate_child` -- see that
        // function's own doc comment for why it's still the single
        // shared place BOTH this loop and `repl.rs`'s `:upgrade` path
        // apply a child's OWN `restart_on_failure`/`max_restarts`).
        self.wire_supervision_groups(&comp.supervises, &comp.children, &child_ids);

        // wires: registered on the source child instance
        for wire in &comp.wires {
            let (from_child, from_port) = &wire.from;
            let (to_child, to_port) = &wire.to;
            let (Some(&src), Some(&dst)) = (child_ids.get(from_child), child_ids.get(to_child)) else {
                return Err(Self::panic_flow("wire references unknown child", wire.span));
            };
            // `docs/design/ASYNC.md` §8.10 step 4: a wire's routing table
            // lives on its SOURCE instance -- for a `Remote` source, that
            // table lives on a DIFFERENT thread with its OWN, entirely
            // separate id space (`dst` here is only meaningful in THIS
            // `Interp`'s own space), which is exactly the "cross-Interp
            // wire" question §6 found to be the hard, unresolved half of
            // real concurrency. Rather than attempt it here, a `concurrent`
            // instance as a wire SOURCE fails with a clean, specific
            // diagnostic -- step 4 only wires up the OTHER direction
            // (`Interp::send` below routes a message INTO a `Remote`
            // instance's inbox when IT is the wire's destination).
            if matches!(&self.instances[src], InstanceSlot::Remote(_)) {
                return Err(Self::panic_flow(
                    format!(
                        "wire `{from_child}.{from_port} -> {to_child}.{to_port}`: a `concurrent` component cannot be a wire's SOURCE yet (only its destination) -- see docs/design/ASYNC.md §8.10 step 4"
                    ),
                    wire.span,
                ));
            }
            self.instances[src].unwrap_local_mut()
                .wires
                .entry(from_port.clone())
                .or_default()
                .push((dst, to_port.clone()));
        }

        Ok(Value::Component(id))
    }

    /// Construct a `concurrent` component on its own OS thread
    /// (`docs/design/ASYNC.md` §8.2/§8.10 steps 3-4). The actor's closure
    /// runs its own initial startup (`instantiate_local` + `start_all` +
    /// `run_timers(100)`, mirroring `kupl run`'s own top-level startup
    /// sequence for this one instance), signals readiness, then stays
    /// alive servicing `Deliver` messages on its inbox until
    /// `Interp::stop_all` closes it — see `ActorHandle`'s own doc comment
    /// for the full lifecycle.
    ///
    /// `args` are already-evaluated `Value`s (the caller's own scope may be
    /// referenced by the argument EXPRESSIONS, which is why evaluation
    /// itself stays on THIS thread) — each is converted to `PortableValue`
    /// here, which `check.rs::check_portable_ty`'s own K0306 diagnostic
    /// guarantees will always succeed for a `concurrent` component's props;
    /// a conversion failure here would mean that guarantee was violated,
    /// which is a genuine compiler bug, not a reachable user-facing case.
    fn instantiate_concurrent(
        &mut self,
        comp: Rc<ComponentDecl>,
        args: &[(Option<String>, Value)],
        span: Span,
    ) -> EvalResult {
        let mut portable_args: Vec<(Option<String>, crate::parallel::PortableValue)> =
            Vec::with_capacity(args.len());
        for (name, v) in args {
            let Some(pv) = crate::parallel::to_portable(v) else {
                return Err(Self::panic_flow(
                    format!(
                        "internal error: `concurrent component {}`'s argument could not be converted to a portable value -- K0306 should have rejected this at check time",
                        comp.name
                    ),
                    span,
                ));
            };
            portable_args.push((name.clone(), pv));
        }
        let image = self.image.clone().expect(
            "a coordinator Interp always has an image; concurrent components cannot be instantiated from inside a par_map worker",
        );
        // `Rc<ComponentDecl>` isn't `Send` (matching every OTHER `Value`/
        // `Instance` type in this codebase — see ASYNC.md §3.1's own
        // `Rc`-not-`Send` blocker) -- the closure below re-resolves the
        // component decl on its OWN thread, from its OWN `actor_db()`
        // (which already holds `Arc`-derived, thread-local `Rc`s per
        // `ProgramImage::actor_db`'s own doc comment), exactly mirroring
        // how the ordinary `instantiate` dispatcher resolves `comp_name`
        // against `self.db.components` in the first place. Only the plain
        // `String` name crosses the thread boundary here.
        let comp_name = comp.name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, Span)>>();
        let shutdown_panic: std::sync::Arc<std::sync::Mutex<Option<(String, Span)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        // Concurrency-v2 PR-cv2-3 (`docs/design/CONCURRENCY_V2.md` §4.3):
        // route through the shared `ActorPool` whenever it's safe to
        // (this spawn isn't happening from inside another pool worker's
        // own command loop) -- see `ActorPool`'s own doc comment for the
        // full reasoning. Falls back to the pre-existing dedicated-thread
        // path below, UNCHANGED, for a nested spawn.
        let route = if POOL_WORKER_ID.with(|c| c.get()).is_none() {
            let worker_tx = ActorPool::get().assign();
            let (local_id_tx, local_id_rx) = std::sync::mpsc::channel::<usize>();
            worker_tx
                .send(WorkerCmd::Spawn {
                    comp_name,
                    portable_args,
                    span,
                    image,
                    ready_tx,
                    shutdown_panic: shutdown_panic.clone(),
                    local_id_tx,
                })
                .expect("actor-pool worker thread ended unexpectedly -- this is a bug in KUPL, not your program");
            let local_id = local_id_rx.recv().expect(
                "actor-pool worker thread ended unexpectedly while spawning a new actor -- this is a bug in KUPL, not your program",
            );
            ActorRoute::Pooled { worker_tx, local_id, stopped_rx: None, stop_sent: false }
        } else {
            let (inbox_tx, inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
            let shutdown_panic_writer = shutdown_panic.clone();
            let join = Self::spawn_dedicated_actor(comp_name, portable_args, span, image, ready_tx, shutdown_panic_writer, inbox_rx);
            ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) }
        };
        let id = self.instances.len();
        self.instances.push(InstanceSlot::Remote(ActorHandle { route, ready: Some(ready_rx), shutdown_panic }));
        Ok(Value::Component(id))
    }

    /// The pre-existing "1 OS thread per actor, always resident" path
    /// (unchanged in substance from before Concurrency-v2 PR-cv2-3, just
    /// extracted into its own function so `instantiate_concurrent` can
    /// call it conditionally instead of unconditionally) — used for every
    /// `concurrent component` spawned from CODE already running on an
    /// `ActorPool` worker's own thread. See `ActorRoute`/`ActorPool`'s own
    /// doc comments for exactly why.
    ///
    /// Production-hardening 1213: a REAL, live-confirmed bug -- plain
    /// `std::thread::spawn` gets the OS default stack (~2-8MiB), unlike
    /// EVERY other thread this codebase spawns for interpreter work
    /// (`main.rs`'s own top-level worker thread, `parallel.rs`'s
    /// `par_map`/`par_filter` workers), which explicitly size a 2GB
    /// stack to match `MAX_CALL_DEPTH = 10_000`. Confirmed live before
    /// this fix: `deep(9000)` (well under the 10,000-frame guard,
    /// returns cleanly on the main thread) reached through a
    /// `concurrent component`'s own `expose fun` crashed the ENTIRE
    /// PROCESS with `fatal runtime error: stack overflow, aborting`
    /// (SIGABRT) -- bypassing the custom panic hook entirely (no
    /// "internal compiler error" message ever printed) and bypassing
    /// `call_remote`'s own clean "actor thread already shut down or
    /// panicked" handling. `.expect(...)` on the `Builder::spawn` below
    /// (rather than propagating a `Result`) matches every sibling site's
    /// own convention -- an OS refusing a 2GB stack reservation is the
    /// same "should never happen on any realistic target" case
    /// `main.rs`/`parallel.rs` already treat as an unrecoverable setup
    /// failure, not a normal runtime error a KUPL program could react to.
    fn spawn_dedicated_actor(
        comp_name: String,
        portable_args: Vec<(Option<String>, crate::parallel::PortableValue)>,
        span: Span,
        image: std::sync::Arc<crate::parallel::ProgramImage>,
        ready_tx: std::sync::mpsc::Sender<Option<(String, Span)>>,
        shutdown_panic_writer: std::sync::Arc<std::sync::Mutex<Option<(String, Span)>>>,
        inbox_rx: std::sync::mpsc::Receiver<ActorMsg>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .stack_size(crate::parallel::WORKER_STACK_SIZE)
            .spawn(move || {
            let db = std::rc::Rc::new(image.actor_db());
            // Concurrency-v2 PR-cv2-6: `new_with_image`, not `new` -- see
            // that constructor's own doc comment; the SAME redundant
            // `ProgramImage::from_db` rebuild `worker_loop`'s pooled path
            // had also applied here, on the dedicated-thread path.
            let mut actor = Interp::new_with_image(db, image.clone());
            let comp = actor
                .db
                .components
                .get(&comp_name)
                .cloned()
                .expect("actor_db mirrors the coordinator's own component table, so this name must resolve");
            let args: Vec<(Option<String>, Value)> = portable_args
                .into_iter()
                .map(|(name, pv)| (name, crate::parallel::from_portable(&pv)))
                .collect();
            let report = |what: &str, err: &Flow| {
                if let Flow::Panic { msg, span, .. } = err {
                    eprintln!("error: concurrent component `{comp_name}`: {what}: panic: {msg} (at {span:?})");
                }
            };
            let startup = (|| -> Result<(), Flow> {
                actor.instantiate_local(comp, &args, span)?;
                actor.start_all()?;
                actor.run_timers(100)?;
                Ok(())
            })();
            // Production-hardening 1222: symmetric to `shutdown_panic`
            // (PR-it1221) -- a genuine KUPL panic here used to stop at
            // `eprintln!`. Now the actual message/span rides the SAME
            // `ready` signal `Interp::start_all` already blocks on, so it
            // renders with `error[K0900]` fidelity via the caller's own
            // `stop_all` propagation instead of being downgraded to a
            // generic stderr line. `report(...)` remains only as a
            // defensive fallback for a non-`Panic` `Flow` (not constructible
            // by this closure's own body today, but kept in case that ever
            // changes, so no `Flow` variant's diagnostic is ever silently
            // dropped).
            let startup_panic = match &startup {
                Err(Flow::Panic { msg, span, .. }) => Some((msg.clone(), *span)),
                Err(e) => {
                    report("startup", e);
                    None
                }
                Ok(()) => None,
            };
            // Signal readiness regardless of outcome -- `Interp::start_all`
            // (coordinator side) is waiting on this to know the actor's
            // own initial lifecycle has settled one way or the other.
            let _ = ready_tx.send(startup_panic);
            if startup.is_ok() {
                // Service `Deliver`/`Call` messages until the coordinator
                // closes the inbox (`Interp::stop_all` drops its `Sender`
                // half) — `recv()` returning `Err` is the standard Rust "no
                // more senders" signal, needing no explicit shutdown
                // message.
                while let Ok(msg) = inbox_rx.recv() {
                    match msg {
                        ActorMsg::Deliver(port, pv) => {
                            let v = crate::parallel::from_portable(&pv);
                            if let Err(e) = actor.send(0, &port, v) {
                                // Production-hardening 1223: a REAL,
                                // live-confirmed bug, a THIRD instance of
                                // the PR-it1221/1222 lifecycle-panic-
                                // swallowing class -- a genuine KUPL panic
                                // triggered by an incoming wire `Deliver`
                                // (this actor's own handler for the port it
                                // was sent on) used to stop at `eprintln!`
                                // here, same as startup/shutdown did before
                                // those fixes. Confirmed via a live fixture
                                // BEFORE this fix that the IDENTICAL
                                // scenario on a plain (non-`concurrent`)
                                // wire target already correctly propagates
                                // through `drain()` as a real `Err` (exit
                                // 101) unless the instance is supervised --
                                // this actor's own `actor.send(0, ...)`
                                // call uses that SAME `drain()` underneath,
                                // so the panic is already being detected
                                // correctly here; only the "hand it to the
                                // caller" step was missing. Reuses
                                // `shutdown_panic` (no new field needed,
                                // matching PR-it1222's own lesson about
                                // preferring an existing communication path)
                                // -- the actor falls through to its own
                                // `stop_all` immediately after this `break`,
                                // so `shutdown_panic` is exactly where the
                                // CALLER's `stop_all` already looks. "First
                                // panic wins": only recorded if nothing has
                                // claimed this slot yet, so this earlier,
                                // more specific failure is never overwritten
                                // by a later, unrelated shutdown outcome.
                                if let Flow::Panic { msg, span, .. } = e {
                                    let mut guard = shutdown_panic_writer.lock().unwrap();
                                    if guard.is_none() {
                                        *guard = Some((msg, span));
                                    }
                                } else {
                                    report("message delivery", &e);
                                }
                                break;
                            }
                        }
                        ActorMsg::Call { fn_name, args, reply, .. } => {
                            let args: Vec<Value> = args.iter().map(crate::parallel::from_portable).collect();
                            // `Value::Component(0)` is this actor's own
                            // root instance -- always `Local` from ITS OWN
                            // `Interp`'s point of view, so this recurses
                            // into the ordinary (non-Remote) `eval_method`
                            // path unchanged, exactly like any other
                            // expose call.
                            let result = actor.eval_method(Value::Component(0), &fn_name, args, Span::default());
                            let reply_msg = match result {
                                Ok(v) => match crate::parallel::to_portable(&v) {
                                    Some(pv) => Ok(pv),
                                    None => Err((
                                        format!("internal error: concurrent `{fn_name}`'s return value is not portable -- K0306 should have rejected this at check time"),
                                        Span::default(),
                                    )),
                                },
                                Err(Flow::Panic { msg, span, .. }) => Err((msg, span)),
                                Err(_) => Err((
                                    "internal error: unexpected control flow escaped a concurrent expose call".to_string(),
                                    Span::default(),
                                )),
                            };
                            let _ = reply.send(reply_msg);
                        }
                    }
                }
                if let Err(e) = actor.stop_all(actor.instances.len()) {
                    // Production-hardening 1221: don't let a genuine KUPL
                    // panic (this actor's own `on stop`, or one propagated
                    // up from a NESTED `concurrent` child's shutdown) stop
                    // at `eprintln!` -- hand the real message/span back to
                    // whoever joins this thread, so it surfaces with the
                    // same `error[K0900]`/exit-101 fidelity a local
                    // component's panic already gets, via the SAME
                    // `report_panic_map` rendering path the top level
                    // already uses (source snippet + precise span, not this
                    // closure's own coarse `eprintln!`). See
                    // `shutdown_panic`'s own doc comment. `stop_all`'s
                    // signature guarantees `e` is always `Flow::Panic` here
                    // (its only error variant) -- the `report(...)` fallback
                    // below exists only in case that ever changes, so no
                    // Flow's diagnostic is ever silently dropped.
                    //
                    // Production-hardening 1223: "first panic wins" -- if
                    // an earlier `Deliver`-triggered handler panic already
                    // claimed this slot (see `ActorMsg::Deliver`'s own
                    // handling above), that ORIGINAL failure is what should
                    // reach the caller, not whatever THIS (likely
                    // unrelated, or itself a downstream consequence)
                    // shutdown outcome happens to be.
                    if let Flow::Panic { msg, span, .. } = e {
                        let mut guard = shutdown_panic_writer.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some((msg, span));
                        }
                    } else {
                        report("shutdown", &e);
                    }
                }
            }
        })
        .expect("failed to spawn a concurrent component's actor thread (OS refused a 2GB stack reservation)")
    }

    /// Deliver `on start` to instance `id` and all its descendants (creation order).
    ///
    /// A `Remote` (`concurrent`) instance's own `on start` already ran, on
    /// its own thread, as part of `instantiate_concurrent`'s spawned
    /// closure — skipped here, then this function waits (via each actor's
    /// own `ready` signal, NOT a full thread join — the actor keeps
    /// running past this point, servicing its inbox per `ActorHandle`'s
    /// own doc comment, ASYNC.md §8.10 step 4) for every spawned actor's
    /// OWN initial startup to finish, so that by the time this function
    /// returns, "on start has been delivered to every instance" remains
    /// true for callers exactly as it was before `concurrent` existed,
    /// whether that instance ran locally or on its own thread. Because
    /// every actor is spawned during `instantiate` (which happens BEFORE
    /// this loop even starts, while the component tree is being built)
    /// rather than spawned HERE, multiple sibling `concurrent` instances'
    /// own `on start`/timer work already overlaps in real wall-clock time
    /// with each other and with this loop's own local work — genuine
    /// parallelism, even in this narrowest slice.
    pub fn start_all(&mut self) -> Result<(), Flow> {
        for id in 0..self.instances.len() {
            if matches!(&self.instances[id], InstanceSlot::Remote(_)) {
                continue;
            }
            self.run_lifecycle(id, &Trigger::Start)?;
            self.arm_timers(id);
        }
        self.drain()?;
        // Production-hardening 1222: a REAL, live-confirmed bug, symmetric
        // to PR-it1221's shutdown-panic fix -- a genuine KUPL `Flow::Panic`
        // during a Remote instance's OWN startup (its `on start` handler,
        // or a portability/instantiation failure, or one propagated up
        // from a NESTED `concurrent` child's own failed startup) used to
        // be discarded here (`let _ = ready.recv();`), even though the
        // actor's own closure already carries the real message/span on
        // this exact channel. Every handle is still drained regardless (no
        // early return), matching `stop_all`'s own "always finish waiting
        // on every handle" shape -- only the FIRST panic encountered is
        // surfaced.
        let mut startup_panic: Option<Flow> = None;
        for slot in &mut self.instances {
            if let InstanceSlot::Remote(handle) = slot {
                if let Some(ready) = handle.ready.take() {
                    if let Ok(Some((msg, span))) = ready.recv() {
                        if startup_panic.is_none() {
                            startup_panic = Some(Self::panic_flow(msg, span));
                        }
                    }
                }
            }
        }
        if let Some(flow) = startup_panic {
            return Err(flow);
        }
        Ok(())
    }

    /// Deliver `on stop` to the first `upto` instances (creation order), the
    /// SAME range `start_all` fired `on start` for.
    ///
    /// A REAL, live-confirmed dead-code bug found+fixed (production-
    /// hardening PR-it1144, an Explore-agent survey side-finding,
    /// independently re-verified live before implementing): `Trigger::Stop`
    /// is fully parsed (parser.rs), type-checked (check.rs), round-trip
    /// formatted (fmt.rs), and compiled (compile.rs/cgen.rs all emit real
    /// code for it, and `run_lifecycle` here has ALWAYS correctly matched
    /// against it) -- but before this fix, NO call site anywhere in the
    /// crate ever actually PASSED `Trigger::Stop` to `run_lifecycle`, on any
    /// of the three engines. `on stop` was consequently silent, permanent
    /// dead code on every engine: a documented language construct
    /// (docs/design/LANGUAGE.md, docs/reference/LANGUAGE-REFERENCE.md) that
    /// parsed and checked cleanly but could never fire, with zero
    /// diagnostic. Neither doc ever specifies exactly WHEN `on stop` should
    /// fire (unlike `on start`'s own explicit "`kupl run` ... delivers `on
    /// start` to every instance in creation order, then drains the queue to
    /// quiescence"), so this closes the gap with the one unambiguous,
    /// already-existing lifecycle boundary in the current design: the
    /// natural end of a `kupl run`/`kupl run --vm`/`kupl native` program's
    /// own execution, mirroring `on start`'s own delivery exactly (creation
    /// order, only for instances that were part of the original started
    /// batch) rather than firing for every instance that merely happens to
    /// still exist by then -- a component instantiated ad-hoc from ordinary
    /// code (e.g. `examples/agent_component.kupl`'s `let bot = Assistant()`
    /// inside `fun main()`, entirely outside any `app`'s own declarative
    /// child list) never receives `on start` either today, so symmetry
    /// requires it not receive `on stop` either. `upto` is the caller's own
    /// snapshot of `self.instances.len()` taken right where `start_all` was
    /// called, so this only ever touches exactly the instances that were
    /// actually started.
    /// `docs/design/ASYNC.md` §8.10 step 4: a `Remote` instance stays alive
    /// (servicing its inbox) past `start_all`, so shutting it down now
    /// takes two passes: first CLOSE every remote actor's inbox (dropping
    /// the coordinator's own `Sender` half ends that actor's own
    /// `recv()` loop, letting it proceed to its OWN `stop_all` and exit),
    /// THEN join their threads — split into two loops, not fused into one,
    /// so every actor's shutdown is SIGNALED together before this function
    /// blocks waiting on any single one, preserving the same "shutdown
    /// overlaps in real wall-clock time" property `start_all` already
    /// established for startup.
    pub fn stop_all(&mut self, upto: usize) -> Result<(), Flow> {
        for id in 0..upto.min(self.instances.len()) {
            if let InstanceSlot::Remote(handle) = &mut self.instances[id] {
                // Concurrency-v2 PR-cv2-3: `Dedicated` signals shutdown by
                // dropping the Sender (closes the channel, ending that
                // actor's OWN `recv()` loop); `Pooled` shares its worker's
                // channel with other actors, so it can't just drop a
                // Sender -- send an explicit `Stop` command instead, and
                // remember the one-shot completion receiver for the
                // second pass below to wait on (mirrors `join` exactly).
                match &mut handle.route {
                    ActorRoute::Dedicated { inbox, .. } => {
                        inbox.take(); // drop the Sender -> closes the channel
                    }
                    ActorRoute::Pooled { worker_tx, local_id, stopped_rx, stop_sent } => {
                        if !*stop_sent {
                            let (stopped_tx, rx) = std::sync::mpsc::channel();
                            let _ = worker_tx.send(WorkerCmd::Stop { local_id: *local_id, stopped_tx });
                            *stopped_rx = Some(rx);
                            *stop_sent = true;
                        }
                    }
                }
                continue;
            }
            self.run_lifecycle(id, &Trigger::Stop)?;
        }
        self.drain()?;
        // Production-hardening 1218: a REAL, live-confirmed-by-code-reading
        // gap found+fixed -- `join()`'s `Result` used to be discarded via
        // `let _ = ...`, silently swallowing ANY genuine Rust-level panic
        // inside an actor's own thread (a normal KUPL `panic()` call, e.g.
        // from that actor's own `on stop` handler, is a SEPARATE concern --
        // production-hardening 1221 below closes that one; this is
        // specifically about an actor thread crashing with an actual Rust
        // panic, e.g.
        // an internal "should have been rejected at check time"-style
        // invariant violation reached only on that thread). Before
        // PR-it1213's stack-size fix, a stack overflow was the one
        // CONFIRMED live trigger for this -- but a stack overflow is a
        // process ABORT (SIGABRT), which never reaches `join()` as an
        // `Err` at all (the whole process is already gone); this
        // `let _ =` was ALWAYS about a genuinely different, still-open
        // class of trigger: any of the interpreter's OWN many internal
        // `.expect()`/`panic!()` invariant checks (the same "internal
        // compiler error, this is a bug in KUPL, not your program" class
        // used throughout this codebase) firing on an actor thread instead
        // of the main one. On the main thread such a panic crashes the
        // whole process loudly, by design; on an actor thread it used to
        // vanish completely -- the coordinator kept running and could
        // still exit 0, silently masking a genuine internal bug. Every
        // Remote handle is still joined regardless (no early return),
        // matching this loop's own original "always finish draining every
        // handle" shape -- only the FIRST panic encountered is surfaced,
        // mirroring `run_lifecycle`'s own `?`-early-return-on-first-error
        // convention used two lines above for local instances.
        let mut actor_panic: Option<Flow> = None;
        for id in 0..upto.min(self.instances.len()) {
            if let InstanceSlot::Remote(handle) = &mut self.instances[id] {
                // Concurrency-v2 PR-cv2-3: `Dedicated` waits via `join()`
                // (a genuine Rust-level thread panic surfaces as `Err`
                // here); `Pooled` has no per-actor thread to join, so it
                // waits on the completion signal `Stop` (above) armed --
                // a dropped/errored receiver here means the WHOLE WORKER
                // thread died (a Rust-level panic inside `worker_loop`,
                // taking every OTHER actor sharing that worker down with
                // it too -- a wider blast radius than `Dedicated`'s
                // one-actor-per-thread isolation, an honest, documented
                // tradeoff of the pool's sharded design, see `ActorPool`'s
                // own doc comment).
                let thread_panicked = match &mut handle.route {
                    ActorRoute::Dedicated { join, .. } => join.take().map(|j| j.join().is_err()).unwrap_or(false),
                    ActorRoute::Pooled { stopped_rx, .. } => {
                        stopped_rx.take().map(|rx| rx.recv().is_err()).unwrap_or(false)
                    }
                };
                {
                    if thread_panicked {
                        if actor_panic.is_none() {
                            actor_panic = Some(Self::panic_flow(
                                "a `concurrent component` actor thread panicked during shutdown \
                                 — this is a bug in KUPL, not your program"
                                    .to_string(),
                                Span::default(),
                            ));
                        }
                    } else if actor_panic.is_none() {
                        // Production-hardening 1221: a REAL, live-confirmed
                        // bug -- a genuine KUPL `Flow::Panic` from this
                        // actor's OWN `on stop` handler (or propagated up
                        // from a nested `concurrent` child's shutdown, since
                        // this same field is populated identically at every
                        // nesting level) used to end here: the actor's
                        // thread returned normally (no Rust-level panic, so
                        // `join()` above reports success), and
                        // `instantiate_concurrent`'s closure had already
                        // `eprintln!`-reported it and moved on -- so the
                        // WHOLE PROCESS exited 0 despite an unhandled panic,
                        // unlike the IDENTICAL `on stop { panic(…) }` on a
                        // plain (non-`concurrent`) component, which
                        // correctly produces `error[K0900]: panic: …` and
                        // exit code 101. `shutdown_panic` carries the panic's
                        // OWN message and span (not a generic "bug in KUPL"
                        // placeholder — this is the user's own program
                        // panicking, not an internal invariant violation),
                        // so it renders with the exact same fidelity here.
                        if let Some((msg, span)) = handle.shutdown_panic.lock().unwrap().take() {
                            actor_panic = Some(Self::panic_flow(msg, span));
                        }
                    }
                }
            }
        }
        if let Some(flow) = actor_panic {
            return Err(flow);
        }
        Ok(())
    }

    /// Arm the instance's timers relative to the current virtual time.
    fn arm_timers(&mut self, id: usize) {
        let comp = self.instances[id].unwrap_local_mut().comp.clone();
        let now = self.now;
        let mut timers = Vec::new();
        for (i, h) in comp.handlers.iter().enumerate() {
            let (every, interval) = match &h.trigger {
                Trigger::Every(ms) => (true, *ms),
                Trigger::After(ms) => (false, *ms),
                _ => continue,
            };
            timers.push(TimerState {
                handler_idx: i,
                every,
                interval,
                next_fire: now + interval,
                active: true,
            });
        }
        self.instances[id].unwrap_local_mut().timers = timers;
    }

    /// Advance the virtual clock by `dur` ms, firing every due timer in time
    /// order (ties broken by instance then declaration order — deterministic).
    /// Recurring timers reschedule; one-shots deactivate.
    ///
    /// A REAL, non-adversarial DoS bug found+fixed (production-hardening
    /// PR-it734): this loop fires one timer event per iteration with NO
    /// bound on the iteration count, which is `dur / timer_interval` --
    /// unbounded, since PR-it728 only capped each duration LITERAL's
    /// magnitude (100 years), never the RATIO between an `advance` step's
    /// duration and a timer's interval, both of which can independently sit
    /// at that cap. An entirely ordinary-looking `example` block -- `on
    /// every 1ms { ... }` soak-tested with `advance 100000000ms` (100M ms,
    /// ~27.8 virtual hours -- not an extreme value) -- confirmed LIVE to
    /// take 11.5s wall-clock for 100M fires; extrapolating to the parser's
    /// own legal maximum (`advance` of 100 years against a 1ms timer) is
    /// ~4.2 DAYS of pegged CPU, with no progress output and no timeout
    /// anywhere in the CLI to bound it -- a two-line test file silently
    /// wedging a CI runner for days, not a crash. Same threat class as this
    /// file's own PR-it559 (panicking handler wedges the server) and
    /// PR-it577 (a NUL byte hangs forever): an entirely ordinary input with
    /// no error, just unbounded wall-clock time. Fixed with the SAME
    /// safety-valve shape `run_timers` already uses one function below --
    /// `MAX_ADVANCE_FIRES` bounds fires within a single `advance` call,
    /// reporting a clean panic instead of grinding indefinitely.
    pub fn advance(&mut self, dur: i64) -> Result<(), Flow> {
        if dur < 0 {
            return Err(Self::panic_flow("cannot advance the clock by a negative duration", Span::default()));
        }
        let target = self.now + dur;
        let mut fires = 0usize;
        loop {
            // earliest active timer with next_fire <= target
            let mut best: Option<(i64, usize, usize)> = None;
            for (iid, slot) in self.instances.iter().enumerate() {
                // A `Remote` (`concurrent`) instance's own timers are
                // entirely self-managed on its own thread (its closure
                // already ran them to completion via its own `run_timers`
                // before this coordinator-side loop ever runs) — skipped
                // here rather than reached via a shared next-fire table
                // (docs/design/ASYNC.md §8.5), which only becomes
                // necessary once step 4/5 let a `Remote` instance keep
                // running concurrently with the coordinator instead of
                // finishing before `instantiate` even returns.
                let InstanceSlot::Local(inst) = slot else { continue };
                for (ti, t) in inst.timers.iter().enumerate() {
                    if t.active && t.next_fire <= target {
                        let cand = (t.next_fire, iid, ti);
                        if best.map_or(true, |b| cand < b) {
                            best = Some(cand);
                        }
                    }
                }
            }
            let Some((fire_time, iid, ti)) = best else { break };
            fires += 1;
            if fires > MAX_ADVANCE_FIRES {
                return Err(Self::panic_flow(
                    format!("`advance` would fire more than {MAX_ADVANCE_FIRES} timer events; use a smaller duration or a longer timer interval"),
                    Span::default(),
                ));
            }
            self.now = fire_time;
            let handler_idx = self.instances[iid].unwrap_local_mut().timers[ti].handler_idx;
            let comp = self.instances[iid].unwrap_local_mut().comp.clone();
            let h = comp.handlers[handler_idx].clone();
            // SOUNDNESS FIX (PR-it509): a panicking timer handler that triggers a
            // supervised restart must NOT also get the ordinary post-fire update
            // below -- `restart` already calls `arm_timers`, which freshly
            // re-schedules EVERY timer on this instance (next_fire = now +
            // interval, active = true) relative to the CURRENT virtual time.
            // Applying `next_fire += interval` / `active = false` on TOP of that
            // fresh state double-delayed every recurring timer by a full extra
            // interval per restart (and immediately deactivated a freshly
            // re-armed one-shot), silently starving a supervised component's
            // timers under repeated failures -- confirmed empirically: an
            // always-panicking `on every 10ms` timer fired only 5 times in a
            // 100ms window instead of the correct 10.
            let restarted = match self.run_handler(iid, &h, Value::Unit) {
                Ok(()) => false,
                Err(Flow::Panic { msg, .. }) if self.instances[iid].unwrap_local_mut().restart_on_failure => {
                    self.restart_with_group(iid, &msg)?;
                    true
                }
                Err(other) => return Err(other),
            };
            self.drain()?;
            if !restarted {
                let t = &mut self.instances[iid].unwrap_local_mut().timers[ti];
                if t.every {
                    t.next_fire += t.interval;
                } else {
                    t.active = false;
                }
            }
        }
        self.now = target;
        Ok(())
    }

    /// For `kupl run`: fire up to `max_fires` timer events by advancing the
    /// clock to each next firing — bounds recurring timers so an app produces
    /// finite, deterministic output.
    pub fn run_timers(&mut self, max_fires: usize) -> Result<(), Flow> {
        for _ in 0..max_fires {
            let mut best: Option<(i64, usize, usize)> = None;
            for (iid, slot) in self.instances.iter().enumerate() {
                // A `Remote` (`concurrent`) instance's own timers are
                // entirely self-managed on its own thread (its closure
                // already ran them to completion via its own `run_timers`
                // before this coordinator-side loop ever runs) — skipped
                // here rather than reached via a shared next-fire table
                // (docs/design/ASYNC.md §8.5), which only becomes
                // necessary once step 4/5 let a `Remote` instance keep
                // running concurrently with the coordinator instead of
                // finishing before `instantiate` even returns.
                let InstanceSlot::Local(inst) = slot else { continue };
                for (ti, t) in inst.timers.iter().enumerate() {
                    if t.active {
                        let cand = (t.next_fire, iid, ti);
                        if best.map_or(true, |b| cand < b) {
                            best = Some(cand);
                        }
                    }
                }
            }
            let Some((fire_time, _, _)) = best else { break };
            self.advance(fire_time - self.now)?;
        }
        Ok(())
    }

    pub(crate) fn run_lifecycle(&mut self, id: usize, trigger: &Trigger) -> Result<(), Flow> {
        let comp = self.instances[id].unwrap_local_mut().comp.clone();
        let want_start = matches!(trigger, Trigger::Start);
        for h in &comp.handlers {
            let matches = matches!(
                (&h.trigger, want_start),
                (Trigger::Start, true) | (Trigger::Stop, false)
            );
            if matches {
                self.run_handler(id, h, Value::Unit)?;
            }
        }
        Ok(())
    }

    /// Queue a message and process until the queue is empty.
    pub fn send(&mut self, id: usize, port: &str, value: Value) -> Result<(), Flow> {
        // `docs/design/ASYNC.md` §8.4: delivering INTO a `Remote` instance
        // is the non-blocking `Deliver` message, routed through its own
        // inbox instead of this `Interp`'s own `queue` (which `drain`
        // below can no longer reach into -- the target `Instance` lives on
        // a different thread entirely). A conversion failure here would
        // mean K0306 failed to reject a non-portable port type, a compiler
        // bug, not a reachable user-facing case.
        if let InstanceSlot::Remote(handle) = &self.instances[id] {
            let Some(pv) = crate::parallel::to_portable(&value) else {
                return Err(Self::panic_flow(
                    format!("internal error: message to concurrent instance {id} on port `{port}` is not portable -- K0306 should have rejected this at check time"),
                    Span::default(),
                ));
            };
            // Best-effort: if the actor has already fully shut down, the
            // send fails silently -- matching an ordinary message arriving
            // after a program has already finished, which has no
            // observable effect either.
            match &handle.route {
                ActorRoute::Dedicated { inbox: Some(inbox), .. } => {
                    let _ = inbox.send(ActorMsg::Deliver(port.to_string(), pv));
                }
                ActorRoute::Dedicated { inbox: None, .. } => {}
                ActorRoute::Pooled { worker_tx, local_id, stop_sent, .. } => {
                    if !*stop_sent {
                        let _ = worker_tx.send(WorkerCmd::Msg(*local_id, ActorMsg::Deliver(port.to_string(), pv)));
                    }
                }
            }
            return Ok(());
        }
        self.queue.push_back((id, port.to_string(), value));
        self.drain()
    }

    fn drain(&mut self) -> Result<(), Flow> {
        let mut processed: u64 = 0;
        while let Some((id, port, value)) = self.queue.pop_front() {
            processed += 1;
            if processed > MAX_COMPONENT_MESSAGES {
                return Err(Self::panic_flow(
                    format!(
                        "component message limit exceeded ({MAX_COMPONENT_MESSAGES}) — a `wire` cycle?"
                    ),
                    crate::diag::Span::default(),
                ));
            }
            if value.approx_byte_size() > MAX_COMPONENT_MESSAGE_BYTES {
                return Err(Self::panic_flow(
                    format!(
                        "component message payload too large (limit {MAX_COMPONENT_MESSAGE_BYTES} bytes) — unbounded growth in a `wire` cycle?"
                    ),
                    crate::diag::Span::default(),
                ));
            }
            let comp = self.instances[id].unwrap_local_mut().comp.clone();
            for h in &comp.handlers {
                if matches!(&h.trigger, Trigger::Port(p) if p == &port) {
                    match self.run_handler(id, h, value.clone()) {
                        Ok(()) => {}
                        Err(Flow::Panic { msg, .. }) if self.instances[id].unwrap_local_mut().restart_on_failure => {
                            self.restart_with_group(id, &msg)?;
                        }
                        Err(other) => return Err(other),
                    }
                }
            }
        }
        Ok(())
    }

    /// Re-evaluate instance `id`'s own `state` field initializers against its
    /// existing `env`, overwriting their current values -- resets state back
    /// to fresh/just-instantiated values in place, touching neither props,
    /// children, wires, nor the instance's own identity/id. Shared by
    /// `restart` (supervision) and `forall_case` (property-test isolation,
    /// production-hardening PR-it903 -- see that function's own doc comment).
    fn reset_instance_state(&mut self, id: usize) -> Result<(), Flow> {
        let comp = self.instances[id].unwrap_local_mut().comp.clone();
        let env = self.instances[id].unwrap_local_mut().env.clone();
        for s in &comp.state {
            let v = self.eval(&s.init, &env)?;
            env.define(&s.name, v);
        }
        Ok(())
    }

    /// Supervision restart: reset state fields to their initial values, keep
    /// props/children/wires, re-run `on start`.
    ///
    /// A restart-intensity limit (`supervise child restart on_failure max N
    /// in <duration>`, BEAM/Erlang-inspired `max_restarts`/`max_seconds`) is
    /// checked FIRST: if this instance has already restarted `N` times
    /// within the trailing `window_ms` (virtual-clock, so this stays
    /// deterministic and reproducible, matching timers' own discipline),
    /// this call escalates instead — returning the panic as an ordinary
    /// `Err`, exactly as if `restart_on_failure` were `false`, so every
    /// EXISTING call site's own `self.restart(id, &msg)?` already handles
    /// this correctly with zero changes (the `?` just propagates it
    /// upward). No `max ... in ...` clause (the default) preserves today's
    /// exact unlimited-restart behavior.
    fn restart(&mut self, id: usize, panic_msg: &str) -> Result<(), Flow> {
        if let Some((max_n, window_ms)) = self.instances[id].unwrap_local_mut().max_restarts {
            let now = self.now;
            let history = &mut self.instances[id].unwrap_local_mut().restart_history;
            while let Some(&oldest) = history.front() {
                if now - oldest > window_ms {
                    history.pop_front();
                } else {
                    break;
                }
            }
            if history.len() as u32 >= max_n {
                let comp_name = self.instances[id].unwrap_local_mut().comp.name.clone();
                eprintln!(
                    "[supervise] {comp_name} exceeded {max_n} restart(s) within {window_ms}ms — escalating instead of restarting"
                );
                return Err(Self::panic_flow(panic_msg.to_string(), Span::default()));
            }
            self.instances[id].unwrap_local_mut().restart_history.push_back(now);
        }
        let comp = self.instances[id].unwrap_local_mut().comp.clone();
        eprintln!("[supervise] {} restarted after panic: {panic_msg}", comp.name);
        self.reset_instance_state(id)?;
        for h in &comp.handlers {
            if matches!(h.trigger, Trigger::Start) {
                self.run_handler(id, h, Value::Unit)?;
            }
        }
        self.arm_timers(id);
        Ok(())
    }

    /// Concurrency-v2 PR-cv2-1: `restart`, plus any group-restart cascade
    /// from a `one_for_all`/`rest_for_one` strategy. Restarts `id` first
    /// (exactly `restart`'s own existing single-instance logic,
    /// unchanged), then — only if that succeeded — restarts every
    /// sibling in `id`'s own precomputed `restart_group` (empty for the
    /// default `one_for_one` strategy, so an existing program with no
    /// strategy keyword is provably unaffected: this function's own
    /// behavior for such an instance is IDENTICAL to calling `restart`
    /// directly). A group-cascaded restart calls plain `restart`, not
    /// `restart_with_group`, for each sibling — deliberately bounded to
    /// ONE level of cascade, so a sibling's OWN group (if it has one)
    /// never recursively re-triggers; this keeps the blast radius of any
    /// single failure easy to reason about and avoids a cyclic-group
    /// configuration ever cascading unboundedly. A group member hitting
    /// ITS OWN restart-intensity limit escalates the WHOLE operation —
    /// matching Erlang/OTP's own supervisor semantics: a supervisor that
    /// cannot successfully complete its restart strategy gives up rather
    /// than leaving the process tree partially restarted.
    fn restart_with_group(&mut self, id: usize, panic_msg: &str) -> Result<(), Flow> {
        self.restart(id, panic_msg)?;
        let group = self.instances[id].unwrap_local_mut().restart_group.clone();
        for sibling_id in group {
            self.restart(sibling_id, panic_msg)?;
        }
        Ok(())
    }

    fn run_handler(&mut self, id: usize, h: &Handler, payload: Value) -> Result<(), Flow> {
        let env = self.instances[id].unwrap_local_mut().env.child();
        if let Some(param) = &h.param {
            env.define(param, payload);
        }
        let saved = self.current.replace(id);
        let result = self.exec_block(&h.body, &env);
        self.current = saved;
        match result {
            Ok(_) => Ok(()),
            Err(Flow::Return(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    fn emit(&mut self, port: &str, value: Value, span: Span) -> Result<(), Flow> {
        let Some(id) = self.current else {
            return Err(Self::panic_flow("`emit` outside of a component handler", span));
        };
        self.instances[id].unwrap_local_mut().last_emit.insert(port.to_string(), value.clone());
        let targets = self.instances[id].unwrap_local_mut().wires.get(port).cloned().unwrap_or_default();
        if targets.is_empty() {
            if self.print_unwired {
                let comp = self.instances[id].unwrap_local_mut().comp.name.clone();
                println!("{comp}.{port} = {value}");
            }
        } else {
            // `docs/design/ASYNC.md` §8.10 step 4: a LOCAL target still goes
            // straight onto `self.queue` exactly as before (unchanged
            // behavior, verified byte-identical for every program that
            // doesn't use `concurrent`); a `Remote` target routes through
            // `Interp::send`, which already knows how to reach a
            // `concurrent` instance's inbox -- this was the ONE remaining
            // direct `self.queue.push_back` bypassing that check (`send`
            // itself never bypasses it), found live while testing this
            // step, not anticipated in the design doc.
            for (dst, dport) in targets {
                if matches!(&self.instances[dst], InstanceSlot::Remote(_)) {
                    self.send(dst, &dport, value.clone())?;
                } else {
                    self.queue.push_back((dst, dport, value.clone()));
                }
            }
        }
        Ok(())
    }

    // ---------------- statements ----------------

    /// Execute a single statement against a live environment (REPL entry point).
    pub fn exec_stmt_public(&mut self, stmt: &Stmt, env: &Env) -> EvalResult {
        self.exec_stmt(stmt, env)
    }

    pub fn exec_block(&mut self, block: &Block, env: &Env) -> EvalResult {
        // A block introduces a new scope only to hold its own `let` bindings; `Let`
        // is the sole statement that defines a name into the block scope. When the
        // block has none, running its statements directly in the parent env is
        // semantically identical (assignments walk the chain; nested while/for/if
        // make their own scopes) and skips a per-call Env allocation — the hot path
        // for loop bodies that only assign (e.g. `while … { s = s + i; i = i + 1 }`).
        if block.stmts.iter().any(|s| matches!(s, Stmt::Let { .. })) {
            let scope = env.child();
            self.exec_stmts_checked(&block.stmts, &scope)
        } else {
            self.exec_stmts_checked(&block.stmts, env)
        }
    }

    /// Concurrency-v2 PR-cv2-10 (`docs/design/ASYNC_IO.md` §5): runs
    /// `stmts` in `env`, checking each `Stmt::Let` for the blocking-
    /// builtin top-level shape `K0295` already restricts to this EXACT
    /// position — when `self.allow_suspend` is set (only true during a
    /// `Deliver`-triggered handler's own body, see `allow_suspend`'s own
    /// doc comment) AND this `Interp` is running on an `ActorPool`
    /// worker thread (`POOL_WORKER_ID`), evaluates the builtin's own
    /// arguments then bails out with `Flow::Suspend` instead of calling
    /// it inline. Every other statement (and every case where suspend
    /// isn't currently armed — the coordinator thread, a dedicated actor
    /// thread, or a `Call`-triggered `expose fun`) runs exactly as
    /// `exec_stmt` always has, so this is a strict superset of the
    /// pre-PR-cv2-10 behavior, not a parallel code path that could drift
    /// out of sync with it.
    ///
    /// Shared by both the FIRST execution of a block (`exec_block`) and
    /// RESUMING a previously-suspended handler's own `remaining`
    /// statements (`worker_loop`'s `WorkerCmd::IoComplete` handling) —
    /// factored out so a CHAINED blocking call (`remaining` itself
    /// containing ANOTHER top-level blocking `let`) is handled by the
    /// identical logic, not a second, easy-to-drift-out-of-sync copy.
    fn exec_stmts_checked(&mut self, stmts: &[Stmt], env: &Env) -> EvalResult {
        let mut last = Value::Unit;
        for (i, stmt) in stmts.iter().enumerate() {
            if self.allow_suspend {
                if let Stmt::Let { name, init, .. } = stmt {
                    if let Some(builtin) = Self::blocking_builtin_static_name(init) {
                        // A REAL bug found+fixed (Concurrency-v2 PR-cv2-12,
                        // caught while adding a live test for exactly this
                        // shape — `docs/design/ASYNC_IO.md` §7's own item
                        // (a)): `SuspendedHandler` captures ONLY the
                        // CURRENT block's own remaining statements + bind
                        // name + env — a single stack frame's worth of
                        // continuation, not a real call-stack capture (see
                        // §2/§3 of that doc for why: a genuine CPS
                        // transform covering arbitrary nesting is
                        // explicitly out of scope). When the blocking call
                        // is nested inside a PRIVATE `fun` called from the
                        // handler (`self.call_depth > 0`, via `call_fun`),
                        // suspending here would resume ONLY that private
                        // fun's own remaining statements on `IoComplete` —
                        // its return value would never make it back to the
                        // handler's own `let x = helper(...)` binding, and
                        // the handler's own remaining statements (anything
                        // after that call) would silently NEVER RUN. No
                        // crash, no diagnostic — just incomplete handler
                        // execution, confirmed live via a test that
                        // printed nothing at all before this fix. Only
                        // attempt the flat, single-frame suspend when
                        // `call_depth == 0` (directly in the Deliver-
                        // triggered handler's own top-level body, the ONE
                        // shape `SuspendedHandler` can actually represent
                        // correctly) — a nested call falls through to
                        // ordinary inline (blocking) execution instead,
                        // exactly like `http_get_with`/`read_file_with`
                        // already do for their own, different reason.
                        // K0295 still accepts this shape syntactically
                        // (it covers `c.funs` too); this is a RUNTIME
                        // scope limitation, not a checker gap.
                        if self.call_depth == 0 && POOL_WORKER_ID.with(|c| c.get()).is_some() {
                            let ExprKind::Call { args, .. } = &init.kind else { unreachable!() };
                            let mut vals = Vec::with_capacity(args.len());
                            for a in args {
                                vals.push(self.eval(&a.value, env)?);
                            }
                            return Err(Flow::Suspend(Box::new(SuspendedHandler {
                                builtin,
                                args: vals,
                                bind_name: name.clone(),
                                remaining: stmts[i + 1..].to_vec(),
                                env: env.clone(),
                            })));
                        }
                    }
                }
            }
            last = self.exec_stmt(stmt, env)?;
        }
        Ok(last)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &Env) -> EvalResult {
        match stmt {
            Stmt::Let { name, init, .. } => {
                let v = self.eval(init, env)?;
                env.define(name, v);
                Ok(Value::Unit)
            }
            Stmt::Assign { target, op, value, span } => {
                // Fast path for `x = x + <expr>` (string self-append): append in place
                // when `x` is a uniquely-owned Str, avoiding a full realloc each time
                // — turns the common O(n^2) string-building loop into O(n). Any other
                // shape (shared string, non-Str, different lhs) falls through to the
                // identical general path below, so behavior is unchanged.
                if *op == AssignOp::Set {
                    if let (ExprKind::Ident(tname), ExprKind::Binary { op: BinOp::Add, lhs, rhs }) =
                        (&target.kind, &value.kind)
                    {
                        if matches!(&lhs.kind, ExprKind::Ident(l) if l == tname) {
                            // A REAL, LIVE-CONFIRMED silent-wrong-value bug found+
                            // fixed (production-hardening PR-it1001, a close-read
                            // survey of this loop): this (and its three siblings
                            // below, `push`/Map-`insert`/Set-`insert`) used to
                            // evaluate `rhs`/the method args BEFORE reading
                            // `tname`'s own value -- so a `rhs` whose evaluation
                            // has a side effect that reassigns `tname` ITSELF
                            // (e.g. `count = count + bump()` where `bump()`
                            // mutates `count`) silently combined with the
                            // POST-side-effect value instead of the value `tname`
                            // held at the START of the statement -- backwards
                            // from `ExprKind::Binary`'s own lhs-before-rhs order
                            // used everywhere ELSE in this file, and from what
                            // vm.rs/cgen.rs/kx all actually do for the identical
                            // shape. Live-confirmed: a component method `count =
                            // count + bump()` (`bump()` sets `count = 1`) printed
                            // `2` on `kupl run` but `1` (correct) on `kupl run
                            // --vm`/`kupl native` -- interp.rs was the SOLE
                            // odd-one-out among all four engines, invisible to
                            // every "matches interp.rs" differential test this
                            // campaign has ever run, since those only catch
                            // divergence FROM interp.rs, never interp.rs itself
                            // being wrong relative to the language's own intended
                            // left-to-right semantics. Fixed by capturing
                            // `tname`'s value BEFORE evaluating `rhs`, then
                            // checking -- via `Rc::as_ptr` IDENTITY, not a full
                            // value compare -- whether `rhs`'s evaluation
                            // reassigned `tname` out from under us. If not (the
                            // overwhelming common case), the snapshot is dropped
                            // before attempting the in-place append/push/insert
                            // so it doesn't spuriously defeat that fast path's OWN
                            // uniqueness check (preserving its O(n), not O(n^2),
                            // build-loop guarantee); if `tname` WAS reassigned
                            // mid-`rhs`, the in-place path is skipped entirely and
                            // the ORIGINAL pre-`rhs` snapshot is combined with the
                            // already-evaluated result instead, matching standard
                            // left-to-right assignment semantics.
                            let before = env.get(tname).ok_or_else(|| {
                                Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                            })?;
                            let before_ptr =
                                if let Value::Str(rc) = &before { Some(Rc::as_ptr(rc)) } else { None };
                            let rv = self.eval(rhs, env)?;
                            let unchanged = before_ptr.is_some()
                                && matches!(env.get(tname), Some(Value::Str(ref rc)) if Some(Rc::as_ptr(rc)) == before_ptr);
                            if unchanged {
                                if let Value::Str(rs) = &rv {
                                    drop(before);
                                    if env.append_str_in_place(tname, rs) {
                                        return Ok(Value::Unit);
                                    }
                                    let lv = env.get(tname).ok_or_else(|| {
                                        Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                                    })?;
                                    let nv = self.binary_or_overload(BinOp::Add, lv, rv, value.span)?;
                                    if !env.set(tname, nv) {
                                        return Err(Self::panic_flow(
                                            format!("unknown variable `{tname}`"),
                                            *span,
                                        ));
                                    }
                                    return Ok(Value::Unit);
                                }
                            }
                            let nv = self.binary_or_overload(BinOp::Add, before, rv, value.span)?;
                            if !env.set(tname, nv) {
                                return Err(Self::panic_flow(
                                    format!("unknown variable `{tname}`"),
                                    *span,
                                ));
                            }
                            return Ok(Value::Unit);
                        }
                    }
                    // Fast path for `xs = xs.push(<expr>)` (list self-push): push in
                    // place when `xs` is a uniquely-owned List — turns the O(n^2)
                    // list-building loop into O(n). Shared/other shapes fall through.
                    if let (ExprKind::Ident(tname), ExprKind::MethodCall { recv, name, args }) =
                        (&target.kind, &value.kind)
                    {
                        if name == "push"
                            && args.len() == 1
                            && matches!(&recv.kind, ExprKind::Ident(r) if r == tname)
                        {
                            // PR-it1001 (see the Str self-append fast path above
                            // for the full writeup): capture `tname` BEFORE
                            // evaluating the arg, in case the arg's evaluation
                            // reassigns `tname` itself as a side effect.
                            let before = env.get(tname).ok_or_else(|| {
                                Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                            })?;
                            let before_ptr =
                                if let Value::List(rc) = &before { Some(Rc::as_ptr(rc)) } else { None };
                            let item = self.eval(&args[0].value, env)?;
                            let unchanged = before_ptr.is_some()
                                && matches!(env.get(tname), Some(Value::List(ref rc)) if Some(Rc::as_ptr(rc)) == before_ptr);
                            if unchanged {
                                drop(before);
                                match env.push_list_in_place(tname, item) {
                                    None => return Ok(Value::Unit),
                                    Some(item) => {
                                        // shared list or non-List receiver: fall back to
                                        // the normal push via the usual method dispatch,
                                        // reusing the already-evaluated arg (no re-eval).
                                        let recv_val = env.get(tname).ok_or_else(|| {
                                            Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                                        })?;
                                        let nv =
                                            self.eval_method(recv_val, "push", vec![item], value.span)?;
                                        if !env.set(tname, nv) {
                                            return Err(Self::panic_flow(
                                                format!("unknown variable `{tname}`"),
                                                *span,
                                            ));
                                        }
                                        return Ok(Value::Unit);
                                    }
                                }
                            }
                            let nv = self.eval_method(before, "push", vec![item], value.span)?;
                            if !env.set(tname, nv) {
                                return Err(Self::panic_flow(
                                    format!("unknown variable `{tname}`"),
                                    *span,
                                ));
                            }
                            return Ok(Value::Unit);
                        }
                        // Fast path for `m = m.insert(k, v)` (Map self-insert): update
                        // in place when `m` is a uniquely-owned Map, avoiding the O(n)
                        // clone `.insert` would otherwise pay per call. (2 args => Map
                        // insert; Set insert takes 1 arg, so it never matches here.)
                        // NOTE (production-hardening PR-it983): this does NOT make the
                        // build loop O(n) overall like its Str/List siblings above --
                        // `insert_map_in_place`'s own duplicate-key scan is still O(n)
                        // per call, so an n-iteration loop remains O(n^2) TIME; only
                        // the per-call ALLOCATION drops from O(n) to O(1) amortized.
                        // See value.rs::insert_map_in_place's doc comment for the full,
                        // live-benchmarked correction (this comment previously implied
                        // full O(n), unchallenged since the fast path's original PR-it91).
                        if name == "insert"
                            && args.len() == 2
                            && matches!(&recv.kind, ExprKind::Ident(r) if r == tname)
                        {
                            // PR-it1001 (see the Str self-append fast path above
                            // for the full writeup): capture `tname` BEFORE
                            // evaluating either arg, in case an arg's evaluation
                            // reassigns `tname` itself as a side effect.
                            let before = env.get(tname).ok_or_else(|| {
                                Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                            })?;
                            let before_ptr =
                                if let Value::Map(rc) = &before { Some(Rc::as_ptr(rc)) } else { None };
                            let key = self.eval(&args[0].value, env)?;
                            let val = self.eval(&args[1].value, env)?;
                            let unchanged = before_ptr.is_some()
                                && matches!(env.get(tname), Some(Value::Map(ref rc)) if Some(Rc::as_ptr(rc)) == before_ptr);
                            if unchanged {
                                drop(before);
                                match env.insert_map_in_place(tname, key, val) {
                                    None => return Ok(Value::Unit),
                                    Some((key, val)) => {
                                        let recv_val = env.get(tname).ok_or_else(|| {
                                            Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                                        })?;
                                        let nv = self.eval_method(
                                            recv_val,
                                            "insert",
                                            vec![key, val],
                                            value.span,
                                        )?;
                                        if !env.set(tname, nv) {
                                            return Err(Self::panic_flow(
                                                format!("unknown variable `{tname}`"),
                                                *span,
                                            ));
                                        }
                                        return Ok(Value::Unit);
                                    }
                                }
                            }
                            let nv = self.eval_method(before, "insert", vec![key, val], value.span)?;
                            if !env.set(tname, nv) {
                                return Err(Self::panic_flow(
                                    format!("unknown variable `{tname}`"),
                                    *span,
                                ));
                            }
                            return Ok(Value::Unit);
                        }
                        // Fast path for `s = s.insert(v)` (Set self-insert, 1 arg):
                        // same in-place uniqueness optimization, avoiding the per-call
                        // clone -- but (production-hardening PR-it983) the dedup scan
                        // in `insert_set_in_place` is still O(n) per call, so this does
                        // NOT make the build loop O(n) overall, unlike Str/List above;
                        // prefer `Set(list)` (a genuine O(n log n) bulk path, PR-it826)
                        // over an incremental insert loop when building a large Set.
                        if name == "insert"
                            && args.len() == 1
                            && matches!(&recv.kind, ExprKind::Ident(r) if r == tname)
                        {
                            // PR-it1001 (see the Str self-append fast path above
                            // for the full writeup): capture `tname` BEFORE
                            // evaluating the arg, in case the arg's evaluation
                            // reassigns `tname` itself as a side effect.
                            let before = env.get(tname).ok_or_else(|| {
                                Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                            })?;
                            let before_ptr =
                                if let Value::Set(rc) = &before { Some(Rc::as_ptr(rc)) } else { None };
                            let v = self.eval(&args[0].value, env)?;
                            let unchanged = before_ptr.is_some()
                                && matches!(env.get(tname), Some(Value::Set(ref rc)) if Some(Rc::as_ptr(rc)) == before_ptr);
                            if unchanged {
                                drop(before);
                                match env.insert_set_in_place(tname, v) {
                                    None => return Ok(Value::Unit),
                                    Some(v) => {
                                        let recv_val = env.get(tname).ok_or_else(|| {
                                            Self::panic_flow(format!("unknown variable `{tname}`"), *span)
                                        })?;
                                        let nv =
                                            self.eval_method(recv_val, "insert", vec![v], value.span)?;
                                        if !env.set(tname, nv) {
                                            return Err(Self::panic_flow(
                                                format!("unknown variable `{tname}`"),
                                                *span,
                                            ));
                                        }
                                        return Ok(Value::Unit);
                                    }
                                }
                            }
                            let nv = self.eval_method(before, "insert", vec![v], value.span)?;
                            if !env.set(tname, nv) {
                                return Err(Self::panic_flow(
                                    format!("unknown variable `{tname}`"),
                                    *span,
                                ));
                            }
                            return Ok(Value::Unit);
                        }
                    }
                }
                let ExprKind::Ident(name) = &target.kind else {
                    return Err(Self::panic_flow("unsupported assignment target", *span));
                };
                // A REAL, LIVE-CONFIRMED silent-wrong-value bug found+fixed
                // (production-hardening PR-it1057, found via a background
                // close-read survey of interp.rs's own exec_stmt): this used
                // to evaluate `value` (the RHS) BEFORE reading `name`'s
                // "old" value for `+=`/`-=`/`*=`/`/=` -- the exact bug
                // shape the fast-path block above (PR-it1001/PR-it1052) and
                // `check.rs`'s own K0222 doc comment (PR-it996, which
                // EXPLICITLY states `AssignOp::Add` "desugars into the exact
                // SAME `BinOp::Add` an ordinary `s = s + \"b\"` uses")
                // already establish as this codebase's OWN intended
                // semantics -- but that fast path only recognizes the
                // DESUGARED `x = x + e` AST shape (gated on `*op ==
                // AssignOp::Set`), never the SUGARED `x += e` form, which
                // falls straight through to this general path instead.
                // Live-confirmed BEFORE this fix: `var x = 5; x += { x = 99
                // \n 1 }; print(x)` printed `100` (99 + 1, silently using
                // the MUTATED x) instead of the correct `6` (5 + 1, matching
                // what the ALREADY-fixed `x = x + { x = 99 \n 1 }` sibling
                // shape correctly produces) -- on BOTH interp and vm (this
                // is engine-CONSISTENT, not a cross-engine divergence,
                // exactly the "engine-agreeing-but-wrong-relative-to-the-
                // language's-own-documented-semantics" blind spot PR-it1001
                // itself was originally found to close). Fixed by reading
                // `old` BEFORE evaluating `value`, mirroring the fast path's
                // own established snapshot-before-rhs discipline.
                let new_value = if *op == AssignOp::Set {
                    self.eval(value, env)?
                } else {
                    let old = env.get(name).ok_or_else(|| {
                        Self::panic_flow(format!("unknown variable `{name}`"), *span)
                    })?;
                    let rhs = self.eval(value, env)?;
                    let bin = match op {
                        AssignOp::Add => BinOp::Add,
                        AssignOp::Sub => BinOp::Sub,
                        AssignOp::Mul => BinOp::Mul,
                        AssignOp::Div => BinOp::Div,
                        AssignOp::Set => unreachable!(),
                    };
                    self.binary_or_overload(bin, old, rhs, *span)?
                };
                if !env.set(name, new_value) {
                    return Err(Self::panic_flow(format!("unknown variable `{name}`"), *span));
                }
                Ok(Value::Unit)
            }
            Stmt::Expr(e) => self.eval(e, env),
            Stmt::Return(v, _) => {
                let value = match v {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                Err(Flow::Return(value))
            }
            Stmt::While { cond, body, span } => {
                loop {
                    let c = self.eval(cond, env)?;
                    let Value::Bool(b) = c else {
                        return Err(Self::panic_flow("`while` condition must be Bool", *span));
                    };
                    if !b {
                        break;
                    }
                    // `kupl test`'s own hang guard (PR-it1156, extended to
                    // `Stmt::For`'s own loop below at PR-it1179 -- see
                    // `test_step_budget`'s own doc comment on the struct):
                    // `None` for every session except a fresh per-test-item
                    // interp under `run_tests`, so this is a no-op check for
                    // `kupl run`/`kupl check`/native-adjacent sessions.
                    if let Some(budget) = &mut self.test_step_budget {
                        if *budget == 0 {
                            return Err(Self::panic_flow(
                                "test exceeded its `while`-loop iteration budget (likely an infinite loop)",
                                *span,
                            ));
                        }
                        *budget -= 1;
                    }
                    match self.exec_block(body, env) {
                        Ok(_) => {}
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Unit)
            }
            Stmt::For { var, iter, body, span } => {
                let it = self.eval(iter, env)?;
                // Run the body once with `var` bound to `item`. Returns Ok(true) to
                // keep looping, Ok(false) on `break`, Err to propagate.
                macro_rules! step {
                    ($item:expr) => {{
                        // A REAL, live-confirmed HANG-adjacent gap found+fixed
                        // (production-hardening PR-it1179, a direct follow-up
                        // investigation after this iteration's surveys came up
                        // empty): `Stmt::While`'s own PR-it1156 hang guard was
                        // never extended to `Stmt::For` -- a `for` loop over a
                        // huge/adversarial `Range` (or a very large `List`) inside
                        // a `kupl test` example/law/contract-law had NO iteration
                        // budget at all. Live-confirmed BEFORE this fix: the SAME
                        // 20,000,000 trivial iterations a `while` loop hits its
                        // budget on in ~2.5s instead ran to full, unbounded
                        // completion in ~7.7s via `for`; a genuinely large range
                        // (`for i in 0..100_000_000_000 { }`) would scale into
                        // HOURS with zero guard, functionally indistinguishable
                        // from a hang for CI purposes. Shares the EXACT SAME
                        // `test_step_budget` counter as `Stmt::While` above (a
                        // single combined budget correctly bounds the TOTAL loop
                        // work across nested/sequential while+for combinations
                        // within one test item, not just each construct alone) --
                        // `None` for every session except a fresh per-test-item
                        // interp under `run_tests`, so this is a no-op check for
                        // `kupl run`/`kupl check`/native-adjacent sessions.
                        if let Some(budget) = &mut self.test_step_budget {
                            if *budget == 0 {
                                return Err(Self::panic_flow(
                                    "test exceeded its `for`-loop iteration budget (likely an excessively large range or list)",
                                    *span,
                                ));
                            }
                            *budget -= 1;
                        }
                        let scope = env.child();
                        scope.define(var, $item);
                        match self.exec_block(body, &scope) {
                            Ok(_) | Err(Flow::Continue) => {}
                            Err(Flow::Break) => break,
                            Err(other) => return Err(other),
                        }
                    }};
                }
                // Iterate LAZILY: a Range never materializes a Vec (was
                // `(lo..hi).map(Value::Int).collect()` — O(n) upfront); a List is
                // iterated over its shared Rc by reference (was a full `.clone()`).
                // KUPL lists are value-semantic (mutation yields a new list), so the
                // held Rc is an immutable snapshot — a body that rebuilds the source
                // list can't affect this iteration.
                match it {
                    // A REAL, LIVE-CONFIRMED bug found+fixed (production-
                    // hardening PR-it846, found alongside vm.rs's/cgen.rs's
                    // own Op::IterLen overflow bug, see that fix's doc
                    // comment for the general finding): converting an
                    // inclusive range to exclusive via `hi + 1` overflows
                    // `i64` when `hi == i64::MAX` -- in a DEBUG build this
                    // panicked ("internal compiler error" crash); in a
                    // RELEASE build it wrapped to `i64::MIN`, so `lo..hi`
                    // (with `lo` presumably far greater than the wrapped
                    // `hi`) became an EMPTY range and silently skipped a
                    // loop body that should have run. Live-confirmed:
                    // `for i in (i64::MAX - 2)..=i64::MAX { count += 1 }`
                    // crashed in debug and printed `count = 0` (instead of
                    // the correct `3`) in release. Fixed by using Rust's own
                    // `RangeInclusive` iterator (`lo..=hi`) directly for the
                    // inclusive case, instead of manually converting to an
                    // exclusive range first -- `RangeInclusive`'s standard-
                    // library `Iterator` implementation tracks exhaustion
                    // internally rather than computing `hi + 1`, so it
                    // handles `hi == i64::MAX` correctly with no overflow
                    // possible, by construction.
                    Value::Range(lo, hi, incl) => {
                        if incl {
                            for i in lo..=hi {
                                step!(Value::Int(i));
                            }
                        } else {
                            for i in lo..hi {
                                step!(Value::Int(i));
                            }
                        }
                    }
                    Value::List(ref items) => {
                        for item in items.iter() {
                            step!(item.clone());
                        }
                    }
                    other => {
                        return Err(Self::panic_flow(
                            format!("`for` needs a Range or List, found {}", other.type_name()),
                            *span,
                        ))
                    }
                }
                Ok(Value::Unit)
            }
            Stmt::Emit { port, arg, span } => {
                let value = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                self.emit(port, value, *span)?;
                Ok(Value::Unit)
            }
            Stmt::Expect(expr, span) => {
                let v = self.eval(expr, env)?;
                if v != Value::Bool(true) {
                    // Name the failing expression (rendered from source) so a failed
                    // `expect`/law says WHAT failed, not just "expectation failed".
                    return Err(Flow::Panic {
                        msg: format!("expectation failed: {}", crate::fmt::expr_str(expr, 0)),
                        span: *span,
                        already_reported: false,
                    });
                }
                Ok(Value::Unit)
            }
            Stmt::Forall { vars, body, span } => self.run_forall(vars, body, *span, env),
            Stmt::Break(_) => Err(Flow::Break),
            Stmt::Continue(_) => Err(Flow::Continue),
        }
    }

    /// Run a `forall` property: generate `CASES` deterministic bindings, run the
    /// body for each, and on the first failure shrink to a minimal counterexample
    /// and panic with a descriptive message. `expect`-failures and any panic in
    /// the body both count as a falsifying case.
    fn run_forall(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        span: Span,
        env: &Env,
    ) -> EvalResult {
        let types = self.db.type_variants.clone();
        let mut rng = crate::prop::Rng::new(crate::prop::SEED);
        for _ in 0..crate::prop::CASES {
            let mut vals = Vec::with_capacity(vars.len());
            for (_, ty) in vars {
                match crate::prop::generate(ty, &mut rng, &types, 0) {
                    Ok(v) => vals.push(v),
                    Err(e) => return Err(Self::panic_flow(e, span)),
                }
            }
            // if this case fails, shrink and report
            if self.forall_case(vars, body, &vals, env)?.is_some() {
                let vals = self.shrink_forall(vars, body, vals, env);
                let msg = self.forall_case(vars, body, &vals, env)?.unwrap_or_default();
                let binding: Vec<String> = vars
                    .iter()
                    .zip(&vals)
                    .map(|((n, _), v)| format!("{n} = {}", crate::prop::render(v)))
                    .collect();
                // PRODUCTION-HARDENING (PR-it771): `msg` for the common case (an
                // `expect` inside the property body) is `"expectation failed:
                // {rendered cond}"` (Stmt::Expect above) -- it already names the
                // SPECIFIC condition that failed. The old `starts_with(...)` check
                // threw that text away entirely, leaving just "property failed for
                // n = -26" with zero indication of which `expect` failed or why --
                // unlike the byte-for-byte-identical logic as a plain (non-forall)
                // law, which shows `` `expect doubled >= -50` was not satisfied ``
                // (run.rs's own snippet-based rendering). Reuse the already-computed
                // condition text instead of discarding it, matching that wording.
                let detail = if let Some(cond) = msg.strip_prefix("expectation failed: ") {
                    format!(" (`{cond}` was not satisfied)")
                } else if msg.is_empty() {
                    String::new()
                } else {
                    format!(" (panic: {msg})")
                };
                // PR-it1194: constructed directly (not via `panic_flow`) so
                // `already_reported` can be set `true` -- this message is
                // ALREADY complete (see the doc comment on `Flow::Panic`
                // itself); `run.rs`'s law-reporting logic must not re-wrap
                // it in "panic: " or emit a spurious `error[K0900]` block.
                return Err(Flow::Panic {
                    msg: format!("property failed for {}{}", binding.join(", "), detail),
                    span,
                    already_reported: true,
                });
            }
        }
        Ok(Value::Unit)
    }

    /// Run the body with one binding. `Ok(None)` = passed, `Ok(Some(msg))` =
    /// failed (msg is the panic message), `Err(flow)` = unexpected control flow.
    ///
    /// A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening PR-it903,
    /// an Explore survey finding, agentId a5870a9744357585b, independently
    /// re-verified live before implementing): a `forall` inside a contract
    /// `law` runs its body against the SAME, single, already-instantiated
    /// component instance for every one of `CASES` (100) generated cases,
    /// AND for every candidate `shrink_forall` tries -- `run.rs`'s law
    /// runner instantiates the fulfilling component ONCE per law and binds
    /// its exposed functions (`Value::Bound(id, ..)`) to that ONE instance
    /// for the law's entire body. If the property depends on the component's
    /// own `state` (KUPL's headline stateful-component feature, and the
    /// sanctioned pattern `examples/contracts.kupl` itself demonstrates for
    /// testing components via their exposed interface), state silently
    /// ACCUMULATES across cases/shrink-candidates with NO reset between
    /// them -- so a later case can "fail" purely because of how much prior
    /// state has built up, not because of its OWN generated value, and the
    /// greedy shrinker then collapses onto whatever candidate is tried
    /// FIRST (e.g. the empty string, first in `prop::shrink`'s own Str
    /// candidate order) simply because state has ALREADY crossed the
    /// property's threshold by that point -- a PHANTOM counterexample, not
    /// a real one. Live-confirmed: a `Store` contract's law `forall k: Str {
    /// put(k, "x"); expect size() <= 3 }` against an append-only
    /// `MemoryStore` reported `property failed for k = ""`, but a standalone
    /// law running the IDENTICAL body against a FRESH instance with that
    /// EXACT literal value (`put(""); expect size() <= 3`) PASSES cleanly
    /// (size becomes 1, well under 3) -- an airtight proof the reported
    /// "minimal counterexample" does not actually reproduce, exactly the
    /// kind of false report that would send a developer chasing a bug that
    /// doesn't exist. Fixed by resetting every component instance this
    /// scope references (found via `Env::bound_instance_ids`, walking `env`
    /// and its ancestor scopes for `Value::Bound` bindings -- typically the
    /// single instance a contract law's setup bound, but written generally
    /// since a `forall` may reference more) back to fresh state before
    /// EVERY case, so each case/candidate is judged purely on its own
    /// generated value against a consistent baseline, matching what the
    /// reported "property failed for k = X" message implies to a reader. A
    /// `forall` with no bound component instance in scope (an ordinary,
    /// stateless property) finds zero ids here and is completely unaffected.
    fn forall_case(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        vals: &[Value],
        env: &Env,
    ) -> Result<Option<String>, Flow> {
        let mut instance_ids = std::collections::HashSet::new();
        env.bound_instance_ids(&mut instance_ids);
        // Transitively pull in children: a bound instance's own children
        // live as ordinary values in ITS OWN internal env (`instantiate`'s
        // `env.define(&child.name, v)`), not in the value graph reachable
        // from the outer `env` at all -- a plain `Value::Component(id)` is
        // just an opaque instance id, so a parent bound here whose STATE is
        // held by a child instead (delegated to via `child.exposedFun()`)
        // needs that child's own env walked too, and so on for
        // grandchildren (production-hardening PR-it906 -- the fourth
        // distinct reachability path to this bug class, after PR-it903/
        // it904/it905's direct/nested/captured paths).
        let mut frontier: Vec<usize> = instance_ids.iter().copied().collect();
        while let Some(id) = frontier.pop() {
            let child_env = self.instances[id].unwrap_local_mut().env.clone();
            let mut found = std::collections::HashSet::new();
            child_env.own_bound_instance_ids(&mut found);
            for cid in found {
                if instance_ids.insert(cid) {
                    frontier.push(cid);
                }
            }
        }
        // A REAL bug found+fixed (production-hardening PR-it955, survey
        // #108's breadth-first fuzzing pass over contract/law interactions):
        // this loop only re-ran `reset_instance_state` (state field
        // initializers), never `on start` -- unlike `restart` (supervision),
        // which ALSO re-runs every `on start` handler and re-arms timers
        // after the SAME `reset_instance_state` call. The real execution
        // path a law actually runs against (`run.rs`'s own contract-law
        // loop) does `instantiate()` then `start_all()` -- which runs `on
        // start` -- exactly ONCE before the law's body (and everything
        // inside its `forall`) begins, so only the FIRST case ever saw a
        // properly-started instance; every later case/shrink-candidate's
        // reset silently reverted any state `on start` had established,
        // landing on a state no real running instance could ever be in.
        // Confirmed live via a `Divider` contract whose `SafeDivider`
        // component seeds `state divisor: Int = 0` then sets it to a
        // nonzero value in `on start`: a `forall x: Int { divide(x) }` law
        // reported a spurious `property failed for x = 0 (panic: division
        // by zero)`, while the IDENTICAL body run via the real single-shot
        // law path (no `forall`, same `instantiate`+`start_all` route) and
        // an isolation control (divisor seeded entirely by the bare state
        // initializer, no `on start` needed) both passed cleanly -- proving
        // the reported counterexample was a phantom, unreachable by any
        // real running instance. This is the FIFTH distinct reachability
        // path to this campaign's own "forall-phantom-counterexample" bug
        // class, after PR-it903/it904/it905/it906's four (contract-law
        // Bound bindings; plain Component let-bindings; container/closure
        // nesting; transitive child-instance delegation) -- previously
        // believed exhausted absent a genuinely new mechanism (it906's own
        // NEXT-note), now shown not to be. Fixed by mirroring `restart`'s
        // own established post-reset pattern exactly: re-run `on start`
        // (via `run_lifecycle`, the same helper `start_all` itself uses)
        // and re-arm timers for every reset instance, so a per-case reset
        // is fully equivalent to a freshly-instantiated AND freshly-started
        // instance, not merely a freshly-initialized one.
        for id in instance_ids {
            self.reset_instance_state(id)?;
            self.run_lifecycle(id, &Trigger::Start)?;
            self.arm_timers(id);
        }
        let scope = env.child();
        for ((name, _), v) in vars.iter().zip(vals) {
            scope.define(name, v.clone());
        }
        match self.exec_block(body, &scope) {
            Ok(_) => Ok(None),
            Err(Flow::Panic { msg, .. }) => Ok(Some(msg)),
            Err(other) => Err(other),
        }
    }

    /// Greedily shrink a failing binding toward a minimal counterexample: for
    /// each position, try candidate smaller values; keep any that still fails.
    fn shrink_forall(
        &mut self,
        vars: &[(String, TyExpr)],
        body: &Block,
        mut vals: Vec<Value>,
        env: &Env,
    ) -> Vec<Value> {
        let mut budget = 1000usize;
        loop {
            let mut improved = false;
            for i in 0..vals.len() {
                for cand in crate::prop::shrink(&vals[i]) {
                    if budget == 0 {
                        return vals;
                    }
                    budget -= 1;
                    let mut trial = vals.clone();
                    trial[i] = cand;
                    // a candidate that itself triggers unexpected flow is skipped
                    if matches!(self.forall_case(vars, body, &trial, env), Ok(Some(_))) {
                        vals = trial;
                        improved = true;
                        break;
                    }
                }
                if improved {
                    break;
                }
            }
            if !improved {
                return vals;
            }
        }
    }

    // ---------------- expressions ----------------

    pub fn eval(&mut self, expr: &Expr, env: &Env) -> EvalResult {
        match &expr.kind {
            ExprKind::Int(v) => Ok(Value::Int(*v)),
            ExprKind::SizedInt(v, w) => Ok(Value::SizedInt(Box::new((*v, *w)))),
            ExprKind::F32(v) => Ok(Value::F32(*v)),
            ExprKind::Float(v) => Ok(Value::Float(*v)),
            ExprKind::Bool(v) => Ok(Value::Bool(*v)),
            ExprKind::Char(c) => Ok(Value::Char(*c)),
            ExprKind::Unit => Ok(Value::Unit),
            ExprKind::Str(pieces) => {
                let mut out = String::new();
                for p in pieces {
                    match p {
                        StrPiece::Text(t) => out.push_str(t),
                        StrPiece::Expr(e) => {
                            let v = self.eval(e, env)?;
                            out.push_str(&v.to_string());
                        }
                    }
                }
                Ok(Value::str(out))
            }
            ExprKind::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for item in items {
                    vs.push(self.eval(item, env)?);
                }
                Ok(Value::List(Rc::new(vs)))
            }
            ExprKind::Range { lo, hi, inclusive } => {
                let l = self.eval(lo, env)?;
                let h = self.eval(hi, env)?;
                match (l, h) {
                    (Value::Int(a), Value::Int(b)) => Ok(Value::Range(a, b, *inclusive)),
                    _ => Err(Self::panic_flow("range bounds must be Int", expr.span)),
                }
            }
            ExprKind::Ident(name) => self.eval_ident(name, expr.span, env),
            ExprKind::Call { callee, args } => self.eval_call(callee, args, expr.span, env),
            ExprKind::MethodCall { recv, name, args } => {
                let r = self.eval(recv, env)?;
                let mut avs = Vec::with_capacity(args.len());
                for a in args {
                    avs.push(self.eval(&a.value, env)?);
                }
                self.eval_method(r, name, avs, expr.span)
            }
            ExprKind::Field { recv, name } => {
                let r = self.eval(recv, env)?;
                match r {
                    Value::Ctor { ref ty, ref variant, ref fields } => {
                        let field_names = self
                            .db
                            .ctors
                            .get(variant.as_str())
                            .map(|(_, names)| names.clone())
                            .unwrap_or_default();
                        // A REAL bug found+fixed (production-hardening PR-it758):
                        // `field_names` comes from `self.db.ctors` -- the CURRENT
                        // program db -- but `fields` may belong to a value built
                        // under a PRIOR db, if the REPL redefined this ctor's own
                        // `type` after the value was constructed (`repl.rs`
                        // deliberately carries `interp.instances`/`globals`
                        // forward across a redefinition, with no shape-
                        // compatibility check at all). `field_names.len()`
                        // growing past `fields.len()` (e.g. a redefined type
                        // gaining a field) made `fields[i]` a raw Rust `Vec`
                        // index panic -- an uncatchable process abort that killed
                        // the WHOLE REPL session, not just this one statement.
                        // Live-confirmed BEFORE this fix: `type T = A(x: Int)`,
                        // `let v = A(1)`, `type T = A(x: Int, y: Int)`, `v.y`
                        // aborted the entire `kupl repl` process (exit 101,
                        // "internal compiler error"). `.get(i)` reports a clean,
                        // catchable panic instead -- matching this module's own
                        // established "a value this pass cannot resolve gets a
                        // clean Err, not an OOB index" convention used
                        // throughout the codebase's `.kx`-corruption fixes.
                        match field_names.iter().position(|f| f == name) {
                            Some(i) => match fields.get(i) {
                                Some(v) => Ok(v.clone()),
                                None => Err(Self::panic_flow(
                                    format!(
                                        "`{ty}` value's shape no longer matches its current \
                                         definition (was it redefined at the REPL after this \
                                         value was created?) -- cannot read field `{name}`"
                                    ),
                                    expr.span,
                                )),
                            },
                            None => Err(Self::panic_flow(
                                format!("`{ty}` value has no field `{name}`"),
                                expr.span,
                            )),
                        }
                    }
                    other => Err(Self::panic_flow(
                        format!("{} has no fields", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // short-circuit logic first
                if matches!(op, BinOp::And | BinOp::Or) {
                    let l = self.eval(lhs, env)?;
                    let Value::Bool(lb) = l else {
                        return Err(Self::panic_flow("logical operand must be Bool", lhs.span));
                    };
                    if (*op == BinOp::And && !lb) || (*op == BinOp::Or && lb) {
                        return Ok(Value::Bool(lb));
                    }
                    let r = self.eval(rhs, env)?;
                    let Value::Bool(rb) = r else {
                        return Err(Self::panic_flow("logical operand must be Bool", rhs.span));
                    };
                    return Ok(Value::Bool(rb));
                }
                let l = self.eval(lhs, env)?;
                let r = self.eval(rhs, env)?;
                self.binary_or_overload(*op, l, r, expr.span)
            }
            ExprKind::Unary { op, operand } => {
                let v = self.eval(operand, env)?;
                raw_unary_op(*op, v).map_err(|msg| Self::panic_flow(msg, expr.span))
            }
            ExprKind::If { cond, then_block, else_block } => {
                let c = self.eval(cond, env)?;
                let Value::Bool(b) = c else {
                    return Err(Self::panic_flow("`if` condition must be Bool", cond.span));
                };
                if b {
                    self.exec_block(then_block, env)
                } else {
                    match else_block {
                        Some(e) => self.eval(e, env),
                        None => Ok(Value::Unit),
                    }
                }
            }
            ExprKind::BlockExpr(b) => self.exec_block(b, env),
            ExprKind::Match { scrutinee, arms } => {
                let v = self.eval(scrutinee, env)?;
                for arm in arms {
                    let scope = env.child();
                    if match_pattern(&arm.pattern, &v, &scope) {
                        // a guard is checked with the pattern's bindings in
                        // scope; a false guard falls through to the next arm
                        if let Some(guard) = &arm.guard {
                            if !matches!(self.eval(guard, &scope)?, Value::Bool(true)) {
                                continue;
                            }
                        }
                        return self.eval(&arm.body, &scope);
                    }
                }
                // A REAL cross-engine byte-identity divergence found+fixed
                // (production-hardening PR-it759): this message used to
                // include the actual runtime VALUE that failed to match
                // (`format!("no match arm matched value \`{v}\`")`), but
                // `compile.rs`'s own shared `Op::Panic` emission for this
                // SAME fallback (line ~1072, `"no match arm matched"`, no
                // value) is what vm.rs/cgen.rs/kx.rs all render -- the
                // value can't be embedded there at COMPILE time (unlike
                // this tree-walking interpreter, which evaluates `v`
                // directly), so those three engines never had it to begin
                // with. This made interp.rs the sole odd-engine-out among
                // the four "byte-identical" execution engines on a path
                // reachable through ordinary, valid KUPL syntax (a
                // genuinely non-exhaustive `match` on a scalar-typed ADT
                // field position, e.g. `Circle(5) => .., Square(_) => ..`
                // on `Circle(r: Int) | Square(s: Int)`, compiles cleanly --
                // `check.rs`'s exhaustiveness checker's own scalar-field
                // limitation is a separate, already-accepted scope
                // decision, unrelated to this fix). Live-confirmed BEFORE
                // this fix: `kupl run` printed `"no match arm matched
                // value \`Circle(7)\`"` while `kupl run --vm`/`kupl
                // native`/a compiled `.kx` module all printed the plain
                // `"no match arm matched"` for the IDENTICAL program and
                // input. Dropping the value here (rather than threading a
                // NEW dynamic-value-formatting mechanism through all three
                // OTHER engines' shared `Op::Panic`, a much larger change)
                // restores byte-identical text across all four engines --
                // three independently-derived engines already agreed on
                // this exact wording.
                Err(Self::panic_flow("no match arm matched".to_string(), expr.span))
            }
            ExprKind::Lambda { params, body } => {
                // Capture free LOCALS by value (snapshot), like the KVM/native
                // MakeClosure: names not in scope (top-level funs, ctors, builtins)
                // resolve via the DB at call time and aren't captured.
                let mut bound: std::collections::HashSet<String> =
                    params.iter().map(|p| p.name.clone()).collect();
                let mut free: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                crate::compile::free_vars_block(body, &mut bound, &mut free);
                let captures: Vec<(Box<str>, Value)> = free
                    .iter()
                    .filter_map(|n| env.get(n).map(|v| (n.as_str().into(), v)))
                    .collect();
                Ok(Value::Closure(Rc::new(Closure {
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    body: Rc::new(body.clone()),
                    captures,
                    origin_instance: self.current,
                })))
            }
            ExprKind::With { recv, updates } => {
                let base = self.eval(recv, env)?;
                let Value::Ctor { ref ty, ref variant, ref fields } = base else {
                    return Err(Self::panic_flow(
                        format!("{} has no fields to update", base.type_name()),
                        expr.span,
                    ));
                };
                let names = self
                    .db
                    .ctors
                    .get(variant.as_str())
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default();
                let mut new_fields = fields.as_ref().clone();
                for (field, value) in updates {
                    let v = self.eval(value, env)?;
                    // A REAL sibling bug to `ExprKind::Field`'s identical fix,
                    // same root cause (production-hardening PR-it758): `names`
                    // comes from the CURRENT `self.db.ctors`, but `new_fields`
                    // is cloned from a value that may have been built under a
                    // PRIOR db if the REPL redefined this ctor's `type` after
                    // the value was constructed. `new_fields[i] = v` was a raw
                    // Rust `Vec` index-assignment panic when `i` (a position
                    // in the CURRENT, possibly-grown field list) exceeded the
                    // stale value's actual field count -- live-confirmed BEFORE
                    // this fix to abort the whole `kupl repl` process the same
                    // way the `ExprKind::Field` read path did.
                    match names.iter().position(|f| f == field) {
                        Some(i) => match new_fields.get_mut(i) {
                            Some(slot) => *slot = v,
                            None => {
                                return Err(Self::panic_flow(
                                    format!(
                                        "`{ty}` value's shape no longer matches its current \
                                         definition (was it redefined at the REPL after this \
                                         value was created?) -- cannot update field `{field}`"
                                    ),
                                    expr.span,
                                ))
                            }
                        },
                        None => {
                            return Err(Self::panic_flow(
                                format!("`{ty}` has no field `{field}`"),
                                expr.span,
                            ))
                        }
                    }
                }
                Ok(Value::Ctor { ty: ty.clone(), variant: variant.clone(), fields: Rc::new(new_fields) })
            }
            ExprKind::Try(inner) => {
                let v = self.eval(inner, env)?;
                match &v {
                    // Ok(x)/Some(x) unwrap to x; Err(e)/None short-circuit the enclosing
                    // function, returning the Err/None value unchanged.
                    Value::Ctor { variant, fields, .. }
                        if variant.as_str() == "Ok" || variant.as_str() == "Some" =>
                    {
                        Ok(fields.first().cloned().unwrap_or(Value::Unit))
                    }
                    Value::Ctor { variant, .. }
                        if variant.as_str() == "Err" || variant.as_str() == "None" =>
                    {
                        Err(Flow::Return(v))
                    }
                    other => Err(Self::panic_flow(
                        format!("`?` needs a Result or Option, found {}", other.type_name()),
                        expr.span,
                    )),
                }
            }
            ExprKind::Await(inner) => self.eval(inner, env),
            ExprKind::Par(branches) => {
                // Real-thread fast path (KUPL universal-language concurrency
                // arc, continuing docs/design/bigarcs/3-real-concurrency.md's
                // own deferred step 4, "par { } fork-join: parallelize only
                // the all-pure, plain-data-free-var case"): fires only when
                // EVERY branch is a plain call `f(args...)` to a statically
                // pure, top-level named function (the SAME `pure_funs` set
                // `par_map`/`par_filter`'s own gate already uses) with
                // portable arguments. Args are evaluated HERE, sequentially,
                // in the real calling environment (they may reference local
                // bindings a worker thread has no access to) — only the
                // function CALLS themselves, not argument evaluation, run
                // concurrently. Any branch that doesn't qualify (not a bare
                // call, an impure/unknown/closure callee, a non-portable
                // argument or result) falls all the way back to today's
                // exact, unchanged sequential loop — parallelism is strictly
                // additive, never a behavior change.
                if let Some(image) = self.image.clone() {
                    if let Some(resolved) = self.resolve_par_branches(branches, env, &image)? {
                        match crate::parallel::try_par_block(&resolved, &image) {
                            Some(results) => {
                                // Mirrors the sequential loop's own `?`
                                // early-return: the FIRST branch (by index,
                                // i.e. source order) that panicked is the
                                // one whose message surfaces — matching
                                // exactly what evaluating branches one at a
                                // time, left to right, would have reported.
                                // The error's span is the REAL, original span
                                // from deep inside the panicking branch's own
                                // callee body — NOT the branch's own call-site
                                // span, and NOT the whole `par { }` block.
                                // Unlike `xs.map(f)`/`xs.par_map(f)` (whose
                                // dispatch wrapper always rewraps a callback
                                // panic with the method call's own span, on
                                // both the sequential and parallel paths), a
                                // `par { }` branch is a DIRECT function call —
                                // its sequential reference (`self.eval(b,
                                // env)?`) does not rewrap the error at all, so
                                // this must match that exactly (see
                                // `ParBranchOutcome`'s own doc comment in
                                // parallel.rs).
                                let mut values = Vec::with_capacity(results.len());
                                for r in results.into_iter() {
                                    match r {
                                        Ok(v) => values.push(v),
                                        Err((msg, span)) => return Err(Self::panic_flow(msg, span)),
                                    }
                                }
                                return Ok(Value::List(Rc::new(values)));
                            }
                            // Something was fundamentally unrunnable (spawn
                            // failure or a non-portable callback RESULT,
                            // discovered only after actually calling it) —
                            // fall through to the sequential path below,
                            // safe to redundantly re-run since every
                            // resolved branch is a call to a PURE function.
                            None => {}
                        }
                    }
                }
                // Fork-join fallback: evaluate each independent branch and
                // collect the results into a list, in deterministic branch
                // order. Branches share only the (immutable) enclosing
                // scope, so evaluation order does not affect results.
                let mut results = Vec::with_capacity(branches.len());
                for b in branches {
                    results.push(self.eval(b, env)?);
                }
                Ok(Value::List(Rc::new(results)))
            }
        }
    }

    /// Try to resolve `par { }`'s own branches into `(fn_name, portable_args)`
    /// pairs for the real-thread fast path (see `ExprKind::Par`'s own eval
    /// arm above for the full design rationale). Returns `Ok(None)` if ANY
    /// branch doesn't qualify — never a hard error, always safe to fall back
    /// to the sequential path.
    ///
    /// Each argument expression is deliberately restricted to a plain
    /// identifier or literal (`Int`/`Float`/`Bool`/`Unit`) — never a nested
    /// call or compound expression — so evaluating EVERY branch's own
    /// arguments upfront, before any of the actual (potentially reordered/
    /// concurrent) function calls happen, can never diverge from the
    /// sequential reference's own left-to-right order: a variable lookup or
    /// literal has zero possibility of a side effect or a panic, so the
    /// ORDER they're evaluated in is unobservable. A nested call or
    /// arithmetic sub-expression as an argument is excluded for exactly
    /// this reason — it COULD panic or have an effect, and evaluating it
    /// eagerly here, before an EARLIER branch's own call has even run,
    /// could surface a panic out of the sequential reference's own
    /// left-to-right order. A future iteration could widen this once a
    /// per-expression purity/panic-freedom analysis exists to justify it.
    fn resolve_par_branches(
        &mut self,
        branches: &[Expr],
        env: &Env,
        image: &std::sync::Arc<crate::parallel::ProgramImage>,
    ) -> Result<Option<Vec<(String, Vec<crate::parallel::PortableValue>, Span)>>, Flow> {
        let mut resolved = Vec::with_capacity(branches.len());
        for b in branches {
            let ExprKind::Call { callee, args } = &b.kind else { return Ok(None) };
            let ExprKind::Ident(name) = &callee.kind else { return Ok(None) };
            // A REAL, LIVE-CONFIRMED bug found+fixed in this SAME it99
            // increment (a fresh live check against `eval_call`'s own
            // precedence, prompted by widening this fast path's coverage in
            // a later iteration): this check used to be JUST
            // `image.pure_funs.contains(name)`, with no check for whether
            // `name` actually resolves to the top-level function at all --
            // `eval_call`'s own dispatch (the sequential reference this
            // fast path must match byte-for-byte) checks a LOCAL binding
            // (`env.get(name)`) and a component-private/exposed fun of the
            // same name BEFORE ever falling back to the top-level table.
            // Live-confirmed: `let add1 = fn(x) { x + 100 }` shadowing a
            // pure top-level `fun add1(x) { x + 1 }`, called as
            // `par { add1(5), add1(6) }`, printed `[6, 7]` (incorrectly
            // calling the top-level fun) on `kupl run` while `kupl run
            // --vm` (sequential only, pre-VM-wiring) correctly printed
            // `[105, 106]` (calling the local closure) -- a genuine
            // cross-engine value divergence. Same shape for a component's
            // own private fun of the same name as a pure top-level fun,
            // called bare inside that component's own handler.
            if env.get(name).is_some() {
                return Ok(None);
            }
            if let Some(id) = self.current {
                let comp = self.instances[id].unwrap_local_mut().comp.clone();
                if comp.funs.iter().chain(comp.exposes.iter()).any(|f| f.name == *name) {
                    return Ok(None);
                }
            }
            // A THIRD instance of the SAME shadowing bug class, found while
            // implementing the VM's own sibling gate (it101): a top-level
            // fun sharing a NAME with a builtin call form is still routed
            // to the BUILTIN by `eval_call`'s own bare-call dispatch
            // (builtins are checked FIRST, unconditionally, in a giant
            // match on the literal name) -- see `BUILTIN_CALL_NAMES`'s own
            // doc comment (bytecode.rs) for the live-confirmed repro and
            // the deliberate name-only (not arity-precise) conservatism.
            if crate::bytecode::BUILTIN_CALL_NAMES.contains(&name.as_str()) {
                return Ok(None);
            }
            if !image.pure_funs.contains(name.as_str()) {
                return Ok(None);
            }
            let mut portable_args = Vec::with_capacity(args.len());
            for a in args {
                if a.name.is_some() {
                    return Ok(None); // named args: keep the gate simple, fall back
                }
                let safe = matches!(
                    &a.value.kind,
                    ExprKind::Ident(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Unit
                );
                if !safe {
                    return Ok(None);
                }
                let v = self.eval(&a.value, env)?;
                match crate::parallel::to_portable(&v) {
                    Some(pv) => portable_args.push(pv),
                    None => return Ok(None),
                }
            }
            resolved.push((name.clone(), portable_args, b.span));
        }
        Ok(Some(resolved))
    }

    fn eval_ident(&mut self, name: &str, span: Span, env: &Env) -> EvalResult {
        if let Some(v) = env.get(name) {
            return Ok(v);
        }
        if self.db.funs.contains_key(name) {
            return Ok(Value::Fun(Rc::new(name.to_string())));
        }
        if name == "None" {
            return Ok(Value::none());
        }
        // component-local function referenced as a value
        if let Some(id) = self.current {
            let comp = self.instances[id].unwrap_local_mut().comp.clone();
            if comp.funs.iter().chain(comp.exposes.iter()).any(|f| f.name == name) {
                return Ok(Value::Bound(id, Rc::new(name.to_string())));
            }
        }
        if let Some((tyname, fields)) = self.db.ctors.get(name).cloned() {
            if fields.is_empty() {
                return Ok(Value::Ctor {
                    ty: Rc::new(tyname),
                    variant: Rc::new(name.to_string()),
                    fields: Rc::new(vec![]),
                });
            }
        }
        Err(Self::panic_flow(format!("unknown name `{name}`"), span))
    }

    /// Concurrency-v2 PR-cv2-10: `Some(name)` (the STATIC builtin name,
    /// for `SuspendedHandler::builtin`) if `e` is a direct call to a
    /// blocking builtin this runtime ACTUALLY knows how to suspend for.
    ///
    /// Deliberately narrower than `check.rs::is_blocking_builtin_call`
    /// (which restricts all 4 blocking builtins syntactically, via
    /// K0295): `http_get_with`/`read_file_with` take a `CapNet`/`CapFs`
    /// capability as their FIRST argument, and `parallel::to_portable`
    /// explicitly returns `None` for both (`value.rs`'s own opaque-by-
    /// design variants) — meaning a captured `SuspendedHandler`'s own
    /// arguments could NOT be safely handed to a spawned `Send`-safe I/O
    /// thread for those two. Found live, before writing any spawn code,
    /// not assumed. `http_get`/`http_post` take only `Str` arguments
    /// (a URL, and for `http_post` a body), fully portable — those two
    /// suspend for real in this v1. A call to `http_get_with`/
    /// `read_file_with` still satisfies K0295's OWN syntactic
    /// restriction (so it compiles), but at RUNTIME still executes
    /// inline (blocking), identical to today's pre-PR-cv2-10 behavior —
    /// an honest, documented v1 scope limit, not a silent gap.
    fn blocking_builtin_static_name(e: &Expr) -> Option<&'static str> {
        let ExprKind::Call { callee, args } = &e.kind else { return None };
        let ExprKind::Ident(name) = &callee.kind else { return None };
        match (name.as_str(), args.len()) {
            ("http_get", 1) => Some("http_get"),
            ("http_post", 2) => Some("http_post"),
            _ => None,
        }
    }

    fn eval_call(&mut self, callee: &Expr, args: &[Arg], span: Span, env: &Env) -> EvalResult {
        if let ExprKind::Ident(name) = &callee.kind {
            match (name.as_str(), args.len()) {
                ("print", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    println!("{v}");
                    return Ok(Value::Unit);
                }
                ("to_str", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::str(v.to_string()));
                }
                ("panic", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Err(Self::panic_flow(v.to_string(), span));
                }
                ("Map", 0) => return Ok(Value::Map(Rc::new(Vec::new()))),
                ("Set", 0) => return Ok(Value::Set(Rc::new(Vec::new()))),
                ("Set", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return set_from_list(&v).map_err(|m| Self::panic_flow(m, span));
                }
                ("tensor", 1) | ("zeros", 1) | ("arange", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return tensor_builtin(name, &v).map_err(|m| Self::panic_flow(m, span));
                }
                ("read_file", 1) | ("write_file", 2) | ("append_file", 2)
                | ("delete_file", 1) | ("file_exists", 1) | ("list_dir", 1)
                | ("make_dir", 1) | ("remove_dir", 1) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return fs_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("big", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return big_builtin(&v).map_err(|m| Self::panic_flow(m, span));
                }
                ("rat", 2) => {
                    let n = self.eval(&args[0].value, env)?;
                    let d = self.eval(&args[1].value, env)?;
                    return rat_builtin(&n, &d).map_err(|m| Self::panic_flow(m, span));
                }
                ("dec", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return dec_builtin(&v).map_err(|m| Self::panic_flow(m, span));
                }
                ("text_embed", 2) => {
                    let s = self.eval(&args[0].value, env)?;
                    let d = self.eval(&args[1].value, env)?;
                    return text_embed_builtin(&s, &d).map_err(|m| Self::panic_flow(m, span));
                }
                ("cosine_similarity", 2) => {
                    let a = self.eval(&args[0].value, env)?;
                    let b = self.eval(&args[1].value, env)?;
                    return cosine_similarity_builtin(&a, &b).map_err(|m| Self::panic_flow(m, span));
                }
                ("path_join", 2) | ("path_base", 1) | ("path_dir", 1) | ("path_ext", 1) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return path_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("json_parse", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    let s = match &v {
                        Value::Str(s) => s.as_str().to_string(),
                        other => other.to_string(),
                    };
                    return Ok(match crate::json::parse(&s) {
                        Ok(j) => Value::ok(j),
                        Err(e) => Value::err(Value::str(e)),
                    });
                }
                ("json_stringify", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return crate::json::stringify(&v)
                        .map(Value::str)
                        .map_err(|m| Self::panic_flow(m, span));
                }
                ("env_var", 1) | ("eprint", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return proc_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("log_debug", 1) | ("log_info", 1) | ("log_warn", 1) | ("log_error", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return log_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("args", 0) | ("read_line", 0) | ("read_all", 0) => {
                    return proc_builtin(name, &[]).map_err(|m| Self::panic_flow(m, span))
                }
                ("random_ints", 2) | ("random_floats", 2) | ("shuffle", 2) => {
                    let mut vals = Vec::with_capacity(2);
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return random_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("exec", 2) => {
                    let mut vals = Vec::with_capacity(2);
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return exec_builtin(&vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("http_serve", 2) => {
                    let port = match self.eval(&args[0].value, env)? {
                        Value::Int(n) => n,
                        other => {
                            return Err(Self::panic_flow(
                                format!("http_serve port must be an Int, found {}", other.type_name()),
                                span,
                            ))
                        }
                    };
                    let handler = self.eval(&args[1].value, env)?;
                    let mut call = |m: String, p: String, b: String| -> Result<String, String> {
                        match self.call_value(
                            handler.clone(),
                            vec![Value::str(m), Value::str(p), Value::str(b)],
                            span,
                        ) {
                            Ok(v) => Ok(v.to_string()),
                            Err(Flow::Panic { msg, .. }) => Err(msg),
                            Err(_) => Err("http_serve handler used non-local control flow".into()),
                        }
                    };
                    return Ok(match serve_http(port, &mut call) {
                        Ok(()) => Value::ok(Value::Unit),
                        Err(e) => Value::err(Value::str(e)),
                    });
                }
                ("http_get", 1) | ("http_post", 2) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return http_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("http_get_with", 2) => {
                    let cap = self.eval(&args[0].value, env)?;
                    let url = self.eval(&args[1].value, env)?;
                    let Value::Str(url) = &url else {
                        return Err(Self::panic_flow("http_get_with needs a Str url", span));
                    };
                    return http_get_with(&cap, url).map_err(|m| Self::panic_flow(m, span));
                }
                ("cap_net_root", 0) => return Ok(cap_net_root()),
                ("read_file_with", 2) => {
                    let cap = self.eval(&args[0].value, env)?;
                    let path = self.eval(&args[1].value, env)?;
                    let Value::Str(path) = &path else {
                        return Err(Self::panic_flow("read_file_with needs a Str path", span));
                    };
                    return read_file_with(&cap, path).map_err(|m| Self::panic_flow(m, span));
                }
                ("cap_fs_root", 0) => return Ok(cap_fs_root()),
                ("re_match", 2) | ("re_find", 2) | ("re_find_all", 2) | ("re_replace", 3) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return regex_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("format_time", 1) | ("year_of", 1) | ("month_of", 1) | ("day_of", 1)
                | ("hour_of", 1) | ("minute_of", 1) | ("second_of", 1) | ("weekday_of", 1)
                | ("yearday_of", 1) | ("date_iso", 1) | ("parse_iso", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return time_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("date_make", 6) => {
                    let mut vals = Vec::with_capacity(6);
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return time_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("now", 0) => return Ok(Value::Int(now_seconds())),
                ("base64_encode", 1) | ("base64_decode", 1) | ("hex_encode", 1)
                | ("hex_decode", 1) | ("hash_fnv", 1) | ("sha256", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return encoding_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("hmac_sha256", 2) => {
                    let mut vals = Vec::with_capacity(2);
                    for a in args {
                        vals.push(self.eval(&a.value, env)?);
                    }
                    return encoding_builtin(name, &vals).map_err(|m| Self::panic_flow(m, span));
                }
                ("csv_parse", 1) | ("csv_stringify", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return csv_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("url_encode", 1) | ("url_decode", 1) | ("query_parse", 1) | ("query_build", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return url_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
                }
                ("exit", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    let code = match v {
                        Value::Int(n) => n as i32,
                        _ => 0,
                    };
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    std::process::exit(code);
                }
                ("Some", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::some(v));
                }
                ("Ok", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::ok(v));
                }
                ("Err", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return Ok(Value::err(v));
                }
                _ => {}
            }
            // component-local function (private or exposed) with live state
            if let Some(id) = self.current {
                if env.get(name).is_none() {
                    let comp = self.instances[id].unwrap_local_mut().comp.clone();
                    if let Some(decl) = comp
                        .funs
                        .iter()
                        .chain(comp.exposes.iter())
                        .find(|f| f.name == *name)
                    {
                        let decl = decl.clone();
                        let mut avs = Vec::with_capacity(args.len());
                        for a in args {
                            avs.push(self.eval(&a.value, env)?);
                        }
                        let base = self.instances[id].unwrap_local_mut().env.clone();
                        return self.call_fun(&decl, avs, &base, span);
                    }
                }
            }
            // user constructor
            //
            // A REAL, LIVE-CONFIRMED bug found+fixed (production-hardening
            // PR-it931, a close-read survey finding): unlike EVERY other
            // dispatch branch in this match (component-local fun above,
            // top-level fun below), this branch had NO check for whether
            // `name` is shadowed by a local binding or a same-named top-
            // level fun — `compile.rs`'s own analogous ctor-dispatch
            // ALREADY guards against exactly this (`ctor_idx.get(name).
            // filter(...)`, checking `!fun_names.contains(name) &&
            // self.lookup(name).is_none()`), so the VM and native paths
            // (both driven by `compile.rs`'s bytecode) already correctly
            // deferred to the shadowing binding — only the tree-walking
            // interpreter (this campaign's OWN reference engine) got it
            // wrong. Live-confirmed: `type Pair = Pair(a: Int, b: Int)`
            // alongside `fun weird(a: Int, b: Int) -> Pair { Pair(a: b, b:
            // a) }` and `let Pair = weird; let p = Pair(1, 2)` — `kupl
            // check` reports ZERO diagnostics, `kupl run` printed `1,2`
            // (silently ignoring the shadow, always constructing) while
            // `kupl run --vm` and `kupl native` both correctly printed
            // `2,1` (calling the shadowing `weird`) — a genuine silent
            // cross-engine VALUE divergence on a well-typed program, not
            // just a diagnostic-text difference. Fixed by adding the SAME
            // guard `compile.rs` already has, matching this file's OWN
            // sibling checks immediately above/below.
            if !self.db.funs.contains_key(name) && env.get(name).is_none() {
                if let Some((tyname, field_names)) = self.db.ctors.get(name).cloned() {
                    let mut fields = vec![Value::Unit; field_names.len()];
                    // Same cursor fix as `check.rs::check_named_args`'s own
                    // (PR-it1079): a positional arg fills the next field NOT
                    // YET claimed by an earlier arg in THIS call (name or
                    // position), not simply `field_names[i]` by its own raw
                    // list index -- preserves source-order evaluation
                    // (PR-it1004's own concern) since the cursor only
                    // depends on args already processed in this same pass.
                    let mut supplied = vec![false; field_names.len()];
                    let mut next_positional = 0usize;
                    for a in args.iter() {
                        let v = self.eval(&a.value, env)?;
                        let idx = match &a.name {
                            Some(n) => field_names.iter().position(|f| f == n).ok_or_else(|| {
                                Self::panic_flow(format!("`{name}` has no field `{n}`"), a.value.span)
                            })?,
                            None => {
                                while next_positional < field_names.len() && supplied[next_positional] {
                                    next_positional += 1;
                                }
                                let idx = next_positional;
                                next_positional += 1;
                                idx
                            }
                        };
                        if idx < fields.len() {
                            fields[idx] = v;
                            supplied[idx] = true;
                        }
                    }
                    return Ok(Value::Ctor {
                        ty: Rc::new(tyname),
                        variant: Rc::new(name.to_string()),
                        fields: Rc::new(fields),
                    });
                }
            }
            // component construction (same shadowing gap as the ctor branch
            // above, same PR-it931 fix, same fix shape: `compile.rs`'s own
            // `instance_expr` caller-side guard already checks `self.lookup
            // (name).is_none()` before treating a name as a component to
            // construct). Live-confirmed with a component `Widget` shadowed
            // by `let Widget = makeFake` (an ordinary Str -> Str function):
            // `kupl run` printed `<component #0>` (silently instantiated a
            // REAL Widget component, with whatever side effects its own
            // lifecycle handlers carry, ignoring the shadow entirely) while
            // `kupl run --vm` correctly printed `fake:hi` (calling the
            // shadowing function) — the interpreter path is strictly worse
            // here than the constructor case, since it can trigger real
            // component instantiation side effects the user's code never
            // intended.
            if !self.db.funs.contains_key(name)
                && env.get(name).is_none()
                && self.db.components.contains_key(name)
            {
                let comp_name = name.clone();
                let mut avs = Vec::new();
                for a in args {
                    let v = self.eval(&a.value, env)?;
                    avs.push((a.name.clone(), v));
                }
                return self.instantiate(&comp_name, &avs, span);
            }
            // Fast path: a top-level function called directly by name and not
            // shadowed by a local binding. Equivalent to the general path below,
            // but skips materializing a `Value::Fun` (a String + Rc allocation per
            // call) and the redundant second `db.funs` lookup — hot for recursive/
            // call-heavy code.
            if env.get(name).is_none() {
                if let Some(decl) = self.db.funs.get(name).cloned() {
                    let mut avs = Vec::with_capacity(args.len());
                    for a in args {
                        avs.push(self.eval(&a.value, env)?);
                    }
                    return self.call_fun(&decl, avs, &self.globals.clone(), span);
                }
            }
        }
        // general call
        let f = self.eval(callee, env)?;
        let mut avs = Vec::with_capacity(args.len());
        for a in args {
            avs.push(self.eval(&a.value, env)?);
        }
        self.call_value(f, avs, span)
    }

    /// Evaluate a binary operator, falling back to an overloaded operator
    /// function when the operands are user-defined values (`a + b` -> `add(a, b)`).
    fn binary_or_overload(&mut self, op: BinOp, l: Value, r: Value, span: Span) -> EvalResult {
        match raw_binary_op(op, &l, &r) {
            Ok(v) => Ok(v),
            Err(msg) => {
                if let Value::Ctor { .. } = l {
                    if let Some(fname) = op_overload_name(op) {
                        if let Some(decl) = self.db.funs.get(fname).cloned() {
                            let env = self.globals.clone();
                            return self.call_fun(&decl, vec![l, r], &env, span);
                        }
                    }
                }
                Err(Flow::Panic { msg, span, already_reported: false })
            }
        }
    }

    pub fn call_value(&mut self, f: Value, args: Vec<Value>, span: Span) -> EvalResult {
        match f {
            Value::Bound(id, ref name) => self.eval_method(Value::Component(id), name, args, span),
            Value::Fun(ref name) => {
                let Some(decl) = self.db.funs.get(name.as_str()).cloned() else {
                    return Err(Self::panic_flow(format!("unknown function `{name}`"), span));
                };
                self.call_fun(&decl, args, &self.globals.clone(), span)
            }
            Value::Closure(ref c) => {
                if c.params.len() != args.len() {
                    return Err(Self::panic_flow(
                        format!("closure takes {} argument(s), {} given", c.params.len(), args.len()),
                        span,
                    ));
                }
                // SOUNDNESS FIX (PR-it500): unlike the named-function path just above
                // (which routes through call_fun's call_depth guard), invoking a closure
                // used to skip the recursion-depth check entirely -- a closure that
                // recurses (e.g. a self-application/fixed-point closure wrapped in a
                // recursive ADT, or any HOF callback that recurses, since map/filter/etc.
                // all funnel through this same call_value) never hit the 10 000-frame
                // limit and instead ran until it exhausted the REAL native Rust stack --
                // an uncatchable abort, exactly what call_depth exists to prevent. Worse,
                // the KVM's equivalent path (push_closure_frame -> push_frame) DOES
                // enforce the same limit, so this was also a genuine interp/KVM
                // byte-identity divergence on a well-typed program (confirmed via a
                // closure wrapped in a recursive ADT: KVM panics "stack overflow (10000
                // frames)"; interp previously ran to completion). Now symmetric with
                // call_fun.
                if self.call_depth >= MAX_CALL_DEPTH {
                    return Err(Self::panic_flow("stack overflow (10000 frames)".to_string(), span));
                }
                self.call_depth += 1;
                // Fresh scope over the module globals: bind the captured snapshot
                // then the params. Rebinding the captures per call (rather than
                // sharing an env) gives value-capture semantics — a mutation of a
                // captured name is call-local, matching the KVM/native.
                let scope = self.globals.child();
                for (n, v) in &c.captures {
                    scope.define(n, v.clone());
                }
                for (p, a) in c.params.iter().zip(args) {
                    scope.define(p, a);
                }
                // A component-local function called FROM WITHIN this closure's
                // body must resolve against the instance that CREATED the
                // closure, not whatever instance is ambiently "current" at the
                // call site — bind `self.current` to the closure's origin for
                // the duration of the call, matching the KVM's push_closure_frame
                // (which threads the closure's captured origin_inst, not the
                // caller's cur_inst) and native's k_cur_inst save/restore.
                let saved_current = std::mem::replace(&mut self.current, c.origin_instance);
                let result = match self.exec_block(&c.body, &scope) {
                    Err(Flow::Return(v)) => Ok(v),
                    other => other,
                };
                self.current = saved_current;
                self.call_depth -= 1;
                result
            }
            other => Err(Self::panic_flow(
                format!("{} is not callable", other.type_name()),
                span,
            )),
        }
    }

    // `pub(crate)` as of it128: `repl.rs`'s `:upgrade` migration-hook
    // support needs to invoke a NEW component's own `migrate_<field>`
    // private fun directly (the `Value::Bound`/`call_value` convenience
    // path resolves through `self.instances[id].comp`, which still holds
    // the OLD, not-yet-swapped component at the point `:upgrade` needs
    // this -- mirrors `run_lifecycle`'s own identical `pub(crate)`
    // widening at it119, for the same class of reason).
    pub(crate) fn call_fun(&mut self, decl: &FunDecl, args: Vec<Value>, base_env: &Env, span: Span) -> EvalResult {
        if decl.params.len() != args.len() {
            return Err(Self::panic_flow(
                format!(
                    "`{}` takes {} argument(s), {} given",
                    decl.name,
                    decl.params.len(),
                    args.len()
                ),
                span,
            ));
        }
        // Recursion guard (matches the KVM's 10 000-frame limit): a clean panic
        // rather than exhausting the native stack and aborting uncatchably.
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(Self::panic_flow("stack overflow (10000 frames)".to_string(), span));
        }
        self.call_depth += 1;
        let result = self.call_fun_body(decl, args, base_env);
        self.call_depth -= 1;
        result
    }

    fn call_fun_body(&mut self, decl: &FunDecl, args: Vec<Value>, base_env: &Env) -> EvalResult {
        if let Some(ai) = &decl.ai {
            let Some(meta) = self.db.ai_funs.get(&decl.name).cloned() else {
                return Err(Self::panic_flow(
                    format!("ai fun `{}` has no runtime signature", decl.name),
                    decl.span,
                ));
            };
            // resolve the interpolated intent in a scope holding the arguments
            let scope = base_env.child();
            for (p, a) in decl.params.iter().zip(&args) {
                scope.define(&p.name, a.clone());
            }
            let intent = self.eval(&ai.intent_expr, &scope)?.to_string();
            // SOUNDNESS FIX (PR-it522): a tool-loop/provider failure inside `ai_call` (unknown
            // tool, missing tool argument, tool-loop round limit exceeded, the underlying tool
            // itself panicking, ...) used to attribute the panic to the CALL SITE's span --
            // but the KVM's equivalent path (Op::CallAi, compiled with the ai fun's OWN
            // declaration span baked in, since the "call" the model makes has no KUPL-syntax
            // call-site of its own) always attributed it to the ai fun's DECLARATION. Same
            // panic MESSAGE on both engines (the part differential() checks), but a DIFFERENT
            // reported location -- confirmed via a real multi-scenario probe (unknown tool,
            // missing arg, round-limit exceeded, tool-internal panic) before fixing. Use the
            // declaration span here too, matching the KVM -- byte-identical full CLI output,
            // not just the message.
            return crate::ai::ai_call(&meta, &intent, &args, self)
                .map_err(|m| Self::panic_flow(m, decl.span));
        }
        let scope = base_env.child();
        for (p, a) in decl.params.iter().zip(args) {
            scope.define(&p.name, a);
        }
        match self.exec_block(&decl.body, &scope) {
            Err(Flow::Return(v)) => Ok(v),
            other => other,
        }
    }

    /// `expose` call on a `concurrent` instance: the blocking `Call`
    /// message `docs/design/ASYNC.md` §8.4 already decided on. Every
    /// argument/the return value is portable-converted — K0306 guarantees
    /// this succeeds for a `concurrent` component's exposed-fun params and
    /// return type.
    fn call_remote(&mut self, id: usize, name: &str, args: Vec<Value>, span: Span) -> EvalResult {
        // Deadlock-cycle check (§8.4) -- see `pending_remote_calls`'s own
        // doc comment for why this is a defensive safety net, not
        // currently reachable given today's other restrictions.
        if self.pending_remote_calls.contains(&id) {
            return Err(Self::panic_flow(
                format!("concurrent call cycle through instance {id} (calling `{name}`) -- refused instead of deadlocking"),
                span,
            ));
        }
        let mut portable_args = Vec::with_capacity(args.len());
        for a in &args {
            let Some(pv) = crate::parallel::to_portable(a) else {
                return Err(Self::panic_flow(
                    format!("internal error: argument to concurrent instance {id}'s `{name}` is not portable -- K0306 should have rejected this at check time"),
                    span,
                ));
            };
            portable_args.push(pv);
        }
        let InstanceSlot::Remote(handle) = &self.instances[id] else {
            unreachable!("caller already confirmed this slot is Remote");
        };
        let already_shutdown = match &handle.route {
            ActorRoute::Dedicated { inbox, .. } => inbox.is_none(),
            ActorRoute::Pooled { stop_sent, .. } => *stop_sent,
        };
        if already_shutdown {
            return Err(Self::panic_flow(
                format!("concurrent instance {id} has already shut down; cannot call `{name}`"),
                span,
            ));
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let call_msg = ActorMsg::Call { fn_name: name.to_string(), args: portable_args, chain: vec![id], reply: reply_tx };
        let sent = match &handle.route {
            ActorRoute::Dedicated { inbox: Some(inbox), .. } => inbox.send(call_msg).is_ok(),
            ActorRoute::Dedicated { inbox: None, .. } => false,
            ActorRoute::Pooled { worker_tx, local_id, .. } => worker_tx.send(WorkerCmd::Msg(*local_id, call_msg)).is_ok(),
        };
        self.pending_remote_calls.insert(id);
        let reply = if sent { reply_rx.recv().ok() } else { None };
        self.pending_remote_calls.remove(&id);
        match reply {
            Some(Ok(pv)) => Ok(crate::parallel::from_portable(&pv)),
            Some(Err((msg, panic_span))) => Err(Self::panic_flow(msg, panic_span)),
            None => Err(Self::panic_flow(
                format!("concurrent instance {id} did not respond to `{name}` (its actor thread already shut down or panicked)"),
                span,
            )),
        }
    }

    fn eval_method(&mut self, recv: Value, name: &str, args: Vec<Value>, span: Span) -> EvalResult {
        // component expose call
        if let Value::Component(id) = recv {
            if matches!(&self.instances[id], InstanceSlot::Remote(_)) {
                return self.call_remote(id, name, args, span);
            }
            let comp = self.instances[id].unwrap_local_mut().comp.clone();
            let Some(decl) = comp.exposes.iter().chain(comp.funs.iter()).find(|f| f.name == name) else {
                return Err(Self::panic_flow(
                    format!("component `{}` does not expose `{name}`", comp.name),
                    span,
                ));
            };
            let instance_env = self.instances[id].unwrap_local_mut().env.clone();
            let saved = self.current.replace(id);
            let result = self.call_fun(&decl.clone(), args, &instance_env, span);
            self.current = saved;
            // SOUNDNESS FIX (production-hardening PR-it967): a panic from an
            // ORDINARY exposed-method call on a supervised child (as opposed
            // to a port/timer-triggered handler panic, already handled by
            // `drain()`/`advance()`) used to bypass supervision entirely --
            // propagating straight past the component boundary to crash the
            // WHOLE PROGRAM with the exact same exit code/diagnostic as an
            // unsupervised panic, contradicting this language's own
            // documented semantics ("panic unwinds the current component
            // instance only; supervision decides restart," docs/design/
            // LANGUAGE.md). The panic itself STILL propagates to the caller
            // of this call (there is no sensible value to synthesize for a
            // still-in-flight expression, unlike drain/advance's fire-and-
            // forget handler dispatch) -- but the child is now ALSO
            // restarted so it is back in a clean, usable state for any
            // FUTURE call/message, matching Erlang-style supervision (a
            // crashed synchronous call surfaces to the caller AND restarts
            // the supervised process).
            let mut should_drain = result.is_ok();
            if let Err(Flow::Panic { ref msg, .. }) = result {
                if self.instances[id].unwrap_local_mut().restart_on_failure {
                    self.restart_with_group(id, msg)?;
                    should_drain = true;
                }
            }
            // A REAL, live-confirmed silent-wrong-answer bug found+fixed
            // (production-hardening PR-it991, an Explore survey finding):
            // every OTHER path that can enqueue a message via `emit`
            // (`start_all`'s lifecycle dispatch, `advance`'s timer dispatch,
            // `send`) calls `self.drain()` afterward -- but an ORDINARY
            // exposed-method call reachable here never did, even though
            // `emit` is legal inside ANY component method, not just an `on`
            // handler (`check.rs` only requires `emit` be "inside a
            // component," confirmed via a direct read). A component whose
            // exposed method emits (e.g. an explicit `poke()`-style trigger
            // on a wired producer) silently queued the message and NEVER
            // delivered it -- the wired sibling's own handler never fired,
            // so its state stayed at its OLD value with zero error anywhere.
            // Live-confirmed on ALL THREE engines (interp/KVM/native all
            // share this bug identically, not a cross-engine divergence):
            // a `Trigger` component's `expose fun press() { emit fired(7) }`
            // wired to a `Counter`'s `in bump: Int` / `on bump(n) { total =
            // total + n }` left `Counter.read()` at `0` instead of `7` after
            // `trigger.press()` on every engine. Draining on the SAME
            // success/restarted-panic conditions `should_drain` already
            // tracks above mirrors the established `advance()` precedent
            // exactly (drain after a restarted panic too, so any messages
            // queued before the panic still reach their destination; skip
            // draining only when the panic propagates un-restarted, since
            // the caller receives an `Err` and the program is unwinding
            // regardless).
            if should_drain {
                self.drain()?;
            }
            return result;
        }
        // UFCS: if there's no built-in method, fall back to a top-level function
        // `name(recv, args…)`. Built-in methods take precedence (tried first).
        //
        // A REAL, live-confirmed silent-panic-swallowing bug found+fixed
        // (production-hardening PR-it1193, found via a fresh Explore survey
        // targeting a genuinely cold spot -- this exact retry GATE, as
        // opposed to the various `shared_method` arms' own "has no method"
        // wording, already covered by PR-it1053): this retry used to decide
        // "the receiver genuinely has no such builtin method, safe to retry
        // as UFCS" purely by SUBSTRING-matching the panic MESSAGE against
        // `"has no method"` -- but `shared_method`'s own `call` closure
        // flattens ANY nested `Flow::Panic` from a user callback into a
        // plain, untagged `String` (see `builtin_method` below), and a
        // user's own `panic(...)` call can supply ANY text. A callback that
        // legitimately panics with a message that merely CONTAINS that
        // phrase (e.g. an app-level error like `panic("user has no method
        // to reset their password")`) was silently DISCARDED here and
        // replaced with the result of calling a same-named top-level
        // function instead -- an explicit, intentional `panic()` call,
        // which should always abort the program, produced NO error at all.
        // Confirmed live BEFORE this fix, with a same-named shadow `fun
        // map`: `xs.map(fn x { panic("boom: this value has no method
        // here") })` printed the SHADOW function's own unrelated result
        // (`[999]`) at exit 0 on BOTH `kupl run` and `kupl run --vm`
        // (`vm.rs`'s own UFCS retry shares the IDENTICAL substring-match
        // gate) -- while `kupl native` correctly panicked with the exact
        // message and exited 101, since native resolves builtin-vs-UFCS
        // STRUCTURALLY (by the receiver's own runtime tag), never by
        // inspecting a panic message's text. This was therefore BOTH a
        // correctness bug (an explicit panic silently vanishing) and a
        // genuine interp+VM-vs-native THREE-WAY divergence, not merely a
        // cosmetic message-wording issue. Fixed by tracking whether
        // `shared_method` ever actually INVOKED a user callback for this
        // specific dispatch attempt (via `callback_invoked` below) --
        // a genuine "no such method" failure is a purely STATIC rejection
        // of `(recv, name)` that `shared_method`'s own match falls through
        // WITHOUT ever calling into user code at all, so retrying as UFCS
        // is now gated on BOTH the message text (preserving today's exact
        // wording-based behavior for the genuine case) AND the callback
        // never having run (excluding any nested-panic false positive,
        // regardless of what text it happens to contain).
        if self.db.funs.contains_key(name) {
            let callback_invoked = std::cell::Cell::new(false);
            match builtin_method(recv.clone(), name, args.clone(), span, self, &callback_invoked) {
                Err(Flow::Panic { msg, .. }) if !callback_invoked.get() && msg.contains("has no method") => {
                    let decl = self.db.funs.get(name).cloned().unwrap();
                    let mut full = Vec::with_capacity(args.len() + 1);
                    full.push(recv);
                    full.extend(args);
                    let env = self.globals.clone();
                    return self.call_fun(&decl, full, &env, span);
                }
                other => return other,
            }
        }
        builtin_method(recv, name, args, span, self, &std::cell::Cell::new(false))
    }
}

impl crate::ai::ToolHost for Interp {
    /// The model asked to run tool `name`: call the top-level KUPL function of
    /// that name with the converted arguments. A panic in the tool surfaces as
    /// an `Err` so the ai fun can capture it (or panic itself).
    fn call_tool(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        let Some(decl) = self.db.funs.get(name).cloned() else {
            return Err(format!("tool `{name}` is not a top-level function"));
        };
        let env = self.globals.clone();
        match self.call_fun(&decl, args, &env, Span::default()) {
            Ok(v) => Ok(v),
            Err(Flow::Panic { msg, .. }) => Err(msg),
            Err(_) => Err(format!("tool `{name}` used non-local control flow")),
        }
    }
}

// ---------------- operators, patterns, builtin methods ----------------
// The raw (span-free) semantics live here and are SHARED by the tree-walking
// interpreter and the KVM — one implementation, no drift.

pub fn raw_unary_op(op: UnOp, v: Value) -> Result<Value, String> {
    match (op, v) {
        (UnOp::Neg, Value::Int(i)) => {
            i.checked_neg().map(Value::Int).ok_or_else(|| "integer overflow in negation".to_string())
        }
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Neg, Value::F32(f)) => Ok(Value::F32(-f)),
        (UnOp::Neg, Value::SizedInt(ref b)) => {
            let (v, w) = **b;
            if w.check_range(-v) {
                Ok(Value::SizedInt(Box::new((-v, w))))
            } else {
                Err("integer overflow in negation".into())
            }
        }
        (UnOp::Neg, Value::BigInt(ref b)) => Ok(Value::BigInt(Rc::new(b.negate()))),
        (UnOp::Neg, Value::Rational(ref r)) => Ok(Value::Rational(Rc::new(r.negate()))),
        // A REAL bug found+fixed (it108, caught while re-auditing this exact
        // function during a fresh `kupl native` scoping pass, mirroring
        // it105's own live-verification discipline): `Decimal` is
        // `is_numeric()`, so `-dec("3.14")` type-checked fine (K0236) but
        // this match had no `Value::Decimal` arm, panicking "invalid
        // operand type Decimal" at runtime on BOTH interp and the KVM
        // (which shares this function) -- confirmed live before this fix.
        (UnOp::Neg, Value::Decimal(ref d)) => Ok(Value::Decimal(Rc::new(crate::decimal::Decimal::negate(d)))),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (_, other) => Err(format!("invalid operand type {}", other.type_name())),
    }
}

pub fn raw_binary_op(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    use BinOp::*;
    let overflow = |what: &str| format!("integer overflow in {what}");
    match op {
        Eq => return Ok(Value::Bool(l == r)),
        Ne => return Ok(Value::Bool(l != r)),
        _ => {}
    }
    match (l, r) {
        (Value::BigInt(a), Value::BigInt(b)) => {
            use std::cmp::Ordering;
            let result = match op {
                Add => a.add(b),
                Sub => a.sub(b),
                Mul => a.mul(b),
                Lt => return Ok(Value::Bool(a.cmp(b) == Ordering::Less)),
                Le => return Ok(Value::Bool(a.cmp(b) != Ordering::Greater)),
                Gt => return Ok(Value::Bool(a.cmp(b) == Ordering::Greater)),
                Ge => return Ok(Value::Bool(a.cmp(b) != Ordering::Less)),
                Div => match a.divmod(b) {
                    Some((q, _)) => q,
                    None => return Err("division by zero".into()),
                },
                Rem => match a.divmod(b) {
                    Some((_, r)) => r,
                    None => return Err("remainder by zero".into()),
                },
                _ => unreachable!(),
            };
            // A REAL bug found+fixed (production-hardening PR-it639): pow
            // (it637) and from_str (it638) already reject a request that
            // would newly exceed MAX_BIGINT_LIMBS in ONE step -- but ordinary
            // repeated multiplication (a hand-written squaring loop, `r =
            // r.mul(&r)` many times over) can walk an already-in-range
            // BigInt past the cap one legitimate-looking `*` at a time,
            // bypassing pow's guard entirely without ever calling pow.
            // Checked HERE, the shared KUPL-operator-dispatch boundary
            // (reached from ordinary `+`/`-`/`*`/`/` syntax on BOTH engines),
            // rather than inside BigInt::add/sub/mul themselves, which stay
            // uncapped internal building blocks used throughout this crate
            // on values already known to be safely bounded.
            if result.exceeds_max_size() {
                return Err(format!(
                    "BigInt arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                    crate::bigint::MAX_BIGINT_LIMBS,
                    crate::bigint::MAX_BIGINT_LIMBS * 9
                ));
            }
            Ok(Value::BigInt(Rc::new(result)))
        }
        (Value::Rational(a), Value::Rational(b)) => {
            use std::cmp::Ordering;
            // A REAL, LIVE-CONFIRMED bug (PR-it718): Rational::cmp's cross-
            // multiplication is an uncapped internal building block just like
            // add/sub/mul -- but unlike those (checked AFTER computing, below),
            // a comparison never stores a result, so checking after the fact
            // means already paying the cost. Confirmed live: two Rationals
            // each built from an ordinary near-cap `big("...")` string ran a
            // single `<` for OVER TWO MINUTES without completing before this
            // check. See `Rational::cmp_would_be_too_expensive`'s doc comment.
            if matches!(op, Lt | Le | Gt | Ge) && a.cmp_would_be_too_expensive(b) {
                return Err(format!(
                    "Rational comparison would require a BigInt multiplication too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                    crate::bigint::MAX_BIGINT_LIMBS,
                    crate::bigint::MAX_BIGINT_LIMBS * 9
                ));
            }
            let result = match op {
                Add => a.add(b)?,
                Sub => a.sub(b)?,
                Mul => a.mul(b)?,
                Div => a.div(b)?,
                Lt => return Ok(Value::Bool(a.cmp(b) == Ordering::Less)),
                Le => return Ok(Value::Bool(a.cmp(b) != Ordering::Greater)),
                Gt => return Ok(Value::Bool(a.cmp(b) == Ordering::Greater)),
                Ge => return Ok(Value::Bool(a.cmp(b) != Ordering::Less)),
                Rem => return Err("Rational remainder is not supported".into()),
                _ => unreachable!(),
            };
            // Same size-cap check as BigInt above (PR-it639) -- Rational's
            // OWN add/sub/mul each cross-multiply numerator/denominator
            // BigInts internally, so its components can grow the SAME way.
            if result.exceeds_max_size() {
                return Err(format!(
                    "Rational arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                    crate::bigint::MAX_BIGINT_LIMBS,
                    crate::bigint::MAX_BIGINT_LIMBS * 9
                ));
            }
            Ok(Value::Rational(Rc::new(result)))
        }
        (Value::Decimal(a), Value::Decimal(b)) => {
            use std::cmp::Ordering;
            let result = match op {
                Add => crate::decimal::Decimal::add(a, b)?,
                Sub => crate::decimal::Decimal::sub(a, b)?,
                Mul => crate::decimal::Decimal::mul(a, b),
                Div => crate::decimal::Decimal::div(a, b)?,
                Lt => return Ok(Value::Bool(crate::decimal::Decimal::cmp(a, b)? == Ordering::Less)),
                Le => return Ok(Value::Bool(crate::decimal::Decimal::cmp(a, b)? != Ordering::Greater)),
                Gt => return Ok(Value::Bool(crate::decimal::Decimal::cmp(a, b)? == Ordering::Greater)),
                Ge => return Ok(Value::Bool(crate::decimal::Decimal::cmp(a, b)? != Ordering::Less)),
                Rem => return Err("Decimal remainder is not supported".into()),
                _ => unreachable!(),
            };
            // Same size-cap check as BigInt/Rational above (PR-it639's own
            // "ordinary repeated ops can walk an in-range value past the
            // cap one step at a time" lesson) -- Decimal's own add/sub/mul
            // can each grow `sig`/`scale` the same way.
            if result.exceeds_max_size() {
                return Err(format!(
                    "Decimal arithmetic result would be too large to compute (limit ~{} limbs / {}-digit scale)",
                    crate::bigint::MAX_BIGINT_LIMBS,
                    crate::decimal::MAX_DECIMAL_SCALE
                ));
            }
            Ok(Value::Decimal(Rc::new(result)))
        }
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            Ok(match op {
                Add => Value::Int(a.checked_add(b).ok_or_else(|| overflow("addition"))?),
                Sub => Value::Int(a.checked_sub(b).ok_or_else(|| overflow("subtraction"))?),
                Mul => Value::Int(a.checked_mul(b).ok_or_else(|| overflow("multiplication"))?),
                Div => {
                    if b == 0 {
                        return Err("division by zero".into());
                    }
                    Value::Int(a.checked_div(b).ok_or_else(|| overflow("division"))?)
                }
                Rem => {
                    if b == 0 {
                        return Err("remainder by zero".into());
                    }
                    // checked_rem catches i64::MIN % -1 (overflow) — a raw `%` would
                    // panic and escape as an ICE; this matches Div's clean overflow.
                    Value::Int(a.checked_rem(b).ok_or_else(|| overflow("remainder"))?)
                }
                Lt => Value::Bool(a < b),
                Le => Value::Bool(a <= b),
                Gt => Value::Bool(a > b),
                Ge => Value::Bool(a >= b),
                _ => unreachable!(),
            })
        }
        // Sized ints: same-width only (mixed widths fall through to the type
        // error below — the checker already forbids them). Add/Sub are done in
        // plain i128, which cannot overflow for any i8..u64 operands (max
        // magnitude ~2^65, well under i128's ~2^127) then range-checked against
        // the width, panicking with the same messages as `Int`. Mul is NOT safe
        // in plain i128 (PR-it671, confirmed live: `u64::MAX * u64::MAX` is
        // ~2^128, past i128::MAX's ~2^127 -- this used to be a genuine
        // `internal compiler error` crash, not the intended "integer overflow
        // in multiplication" panic) -- `checked_mul` catches the i128-level
        // overflow itself, which is a stronger condition than the width's own
        // (much narrower) range, so treating an i128 overflow as a
        // width-overflow is exactly correct, not just crash-avoidance.
        (Value::SizedInt(x), Value::SizedInt(y)) if x.1 == y.1 => {
            let (a, b, w) = (x.0, y.0, x.1);
            let checked = |r: i128, what: &str| -> Result<Value, String> {
                if w.check_range(r) {
                    Ok(Value::SizedInt(Box::new((r, w))))
                } else {
                    Err(overflow(what))
                }
            };
            match op {
                Add => checked(a + b, "addition"),
                Sub => checked(a - b, "subtraction"),
                Mul => match a.checked_mul(b) {
                    Some(r) => checked(r, "multiplication"),
                    None => Err(overflow("multiplication")),
                },
                Div => {
                    if b == 0 {
                        return Err("division by zero".into());
                    }
                    checked(a / b, "division")
                }
                Rem => {
                    if b == 0 {
                        return Err("remainder by zero".into());
                    }
                    checked(a % b, "remainder")
                }
                Lt => Ok(Value::Bool(a < b)),
                Le => Ok(Value::Bool(a <= b)),
                Gt => Ok(Value::Bool(a > b)),
                Ge => Ok(Value::Bool(a >= b)),
                _ => unreachable!(),
            }
        }
        (Value::Float(a), Value::Float(b)) => Ok(match op {
            Add => Value::Float(a + b),
            Sub => Value::Float(a - b),
            Mul => Value::Float(a * b),
            Div => Value::Float(a / b),
            Rem => Value::Float(a % b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            _ => unreachable!(),
        }),
        // f32: same semantics as Float, computed in f32 (never panics)
        (Value::F32(a), Value::F32(b)) => Ok(match op {
            Add => Value::F32(a + b),
            Sub => Value::F32(a - b),
            Mul => Value::F32(a * b),
            Div => Value::F32(a / b),
            Rem => Value::F32(a % b),
            Lt => Value::Bool(a < b),
            Le => Value::Bool(a <= b),
            Gt => Value::Bool(a > b),
            Ge => Value::Bool(a >= b),
            _ => unreachable!(),
        }),
        (Value::Tensor(a), Value::Tensor(b)) => {
            if a.len() != b.len() {
                return Err(format!("tensor length mismatch ({} vs {})", a.len(), b.len()));
            }
            let zip = a.iter().zip(b.iter());
            let data: Vec<f64> = match op {
                Add => zip.map(|(x, y)| x + y).collect(),
                Sub => zip.map(|(x, y)| x - y).collect(),
                Mul => zip.map(|(x, y)| x * y).collect(),
                Div => zip.map(|(x, y)| x / y).collect(),
                _ => return Err("invalid tensor operation".into()),
            };
            Ok(Value::Tensor(std::rc::Rc::new(data)))
        }
        (Value::Str(a), Value::Str(b)) => match op {
            Add => Ok(Value::str(format!("{a}{b}"))),
            Lt => Ok(Value::Bool(a < b)),
            Le => Ok(Value::Bool(a <= b)),
            Gt => Ok(Value::Bool(a > b)),
            Ge => Ok(Value::Bool(a >= b)),
            _ => Err("invalid string operation".into()),
        },
        // `Char` is ordered by codepoint (Rust's own `char: Ord` already
        // implements exactly this) but deliberately has no `Add` -- unlike
        // `Str`, concatenating two single characters isn't a meaningful
        // "arithmetic" operation on the type itself (use `to_str(a) +
        // to_str(b)` to build a `Str` from two `Char`s).
        (Value::Char(a), Value::Char(b)) => match op {
            Lt => Ok(Value::Bool(a < b)),
            Le => Ok(Value::Bool(a <= b)),
            Gt => Ok(Value::Bool(a > b)),
            Ge => Ok(Value::Bool(a >= b)),
            _ => Err("invalid char operation".into()),
        },
        _ => Err(format!(
            "invalid operand types: {} and {}",
            l.type_name(),
            r.type_name()
        )),
    }
}

/// Fixed-precision decimal formatting, rounding half away from zero. A manual
/// algorithm (not the platform float formatter) so the interpreter, KVM, and the
/// native C backend all produce byte-identical strings. `decimals` is clamped to
/// `0..=18`; non-finite inputs render as `nan`/`inf`/`-inf`.
pub fn format_float(x: f64, decimals: i64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let d = decimals.clamp(0, 18) as u32;
    let scale: u64 = 10u64.pow(d);
    let scaled = (x.abs() * scale as f64 + 0.5).floor() as u64;
    let sign = if x < 0.0 && scaled != 0 { "-" } else { "" };
    if d == 0 {
        format!("{sign}{scaled}")
    } else {
        let int_part = scaled / scale;
        let frac = scaled % scale;
        format!("{sign}{int_part}.{frac:0width$}", width = d as usize)
    }
}

/// Operator overloading: the top-level function a binary operator on a
/// user-defined type resolves to (`a + b` -> `add(a, b)`, `a < b` -> `lt(a, b)`).
/// `==`/`!=` stay structural, so they are not overloadable.
pub fn op_overload_name(op: BinOp) -> Option<&'static str> {
    Some(match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        BinOp::Lt => "lt",
        BinOp::Le => "le",
        BinOp::Gt => "gt",
        BinOp::Ge => "ge",
        _ => return None,
    })
}

pub fn match_pattern(pat: &Pattern, value: &Value, env: &Env) -> bool {
    match (&pat.kind, value) {
        (PatternKind::Wildcard, _) => true,
        (PatternKind::Bind(name), v) => {
            env.define(name, v.clone());
            true
        }
        (PatternKind::Int(a), Value::Int(b)) => a == b,
        (PatternKind::Bool(a), Value::Bool(b)) => a == b,
        (PatternKind::Str(a), Value::Str(b)) => a == b.as_str(),
        (PatternKind::Ctor { name, args }, Value::Ctor { variant, fields, .. }) => {
            if name != variant.as_str() {
                return false;
            }
            if args.is_empty() {
                return true;
            }
            if args.len() != fields.len() {
                return false;
            }
            args.iter().zip(fields.iter()).all(|(p, v)| match_pattern(p, v, env))
        }
        (PatternKind::Or(alts), v) => alts.iter().any(|p| match_pattern(p, v, env)),
        (PatternKind::At { name, inner }, v) => {
            if match_pattern(inner, v, env) {
                env.define(name, v.clone());
                true
            } else {
                false
            }
        }
        (PatternKind::Range { lo, hi, inclusive }, Value::Int(v)) => {
            *v >= *lo && (if *inclusive { *v <= *hi } else { *v < *hi })
        }
        _ => false,
    }
}

/// Callback used by function-taking methods (`map`, `filter`, `find`) to call
/// back into whichever engine is running.
pub type Caller<'a> = dyn FnMut(Value, Vec<Value>) -> Result<Value, String> + 'a;

/// Builtin method semantics, shared by interpreter and KVM.
pub fn shared_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    call: &mut Caller,
) -> Result<Value, String> {
    match (recv, name) {
        (Value::List(items), "len") => Ok(Value::Int(items.len() as i64)),
        // `map` and `par_map` share one implementation: par_map declares the
        // per-element work independent (safe to run in parallel); execution is
        // deterministic (input order) today — a real scheduler is a later,
        // semantics-preserving step. Same for `filter`/`par_filter`.
        (Value::List(items), "map") | (Value::List(items), "par_map") => {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(call(f.clone(), vec![item.clone()])?);
            }
            Ok(Value::List(Rc::new(out)))
        }
        // combine two lists element-wise with `f`, stopping at the shorter one
        (Value::List(items), "zip_with") => {
            let mut it = args.into_iter();
            let other = it.next().ok_or("`zip_with` needs a second list")?;
            let f = it.next().ok_or("`zip_with` needs a function")?;
            let Value::List(ref other) = other else {
                return Err("`zip_with` needs a List".into());
            };
            let n = items.len().min(other.len());
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(call(f.clone(), vec![items[i].clone(), other[i].clone()])?);
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "filter") | (Value::List(items), "par_filter") => {
            let f = args.into_iter().next().ok_or("`filter` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    out.push(item.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "take_while") => {
            let f = args.into_iter().next().ok_or("`take_while` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    out.push(item.clone());
                } else {
                    break;
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "drop_while") => {
            let f = args.into_iter().next().ok_or("`drop_while` needs a function")?;
            let mut i = 0;
            while i < items.len() {
                if let Value::Bool(true) = call(f.clone(), vec![items[i].clone()])? {
                    i += 1;
                } else {
                    break;
                }
            }
            Ok(Value::List(Rc::new(items[i..].to_vec())))
        }
        (Value::List(items), "par_each") => {
            let f = args.into_iter().next().ok_or("`par_each` needs a function")?;
            for item in items.iter() {
                call(f.clone(), vec![item.clone()])?;
            }
            Ok(Value::Unit)
        }
        (Value::List(items), "find") => {
            let f = args.into_iter().next().ok_or("`find` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::some(item.clone()));
                }
            }
            Ok(Value::none())
        }
        // `sum`/`product` on List[SizedInt]/List[F32]/List[BigInt]/List[Rational] (a REAL
        // bug found+fixed, PR-it548: `Ty::is_numeric()` type-checks `.sum()`/`.product()` on
        // ANY of these element types, but the runtime only ever implemented Int/Float,
        // panicking "cannot sum <type>" for every other numeric list -- the exact same
        // checker/runtime completeness gap as it547's unary `-`, just in a List method
        // instead of an operator). Dispatch on the first element's variant; Int/Float keep
        // their EXISTING loop (and its own overflow wording) below, unchanged.
        (Value::List(items), "sum") if matches!(items.first(), Some(Value::SizedInt(_) | Value::F32(_) | Value::BigInt(_) | Value::Rational(_) | Value::Decimal(_))) => {
            match items.first().unwrap() {
                Value::SizedInt(b) => {
                    let w = b.1;
                    let mut acc: i128 = 0;
                    for item in items.iter() {
                        let Value::SizedInt(b) = item else { unreachable!() };
                        acc += b.0;
                        if !w.check_range(acc) {
                            return Err("integer overflow in sum".into());
                        }
                    }
                    Ok(Value::SizedInt(Box::new((acc, w))))
                }
                Value::F32(_) => {
                    let mut acc: f32 = 0.0;
                    for item in items.iter() {
                        let Value::F32(v) = item else { unreachable!() };
                        acc += v;
                    }
                    Ok(Value::F32(acc))
                }
                // A REAL bug found+fixed (production-hardening PR-it943, the
                // SAME class as PR-it639's `raw_binary_op` fix, found via a
                // targeted audit of every OTHER caller of BigInt/Rational's
                // add/sub/mul after PR-it942's fix to those exact functions):
                // `raw_binary_op` checks `exceeds_max_size()` after EVERY
                // `+`/`-`/`*`/`/`, but this loop's own accumulator calls the
                // SAME uncapped `add` building block directly, bypassing that
                // check entirely -- `[a, a, a].sum()` (three copies of an
                // individually-legal, near-cap BigInt) silently built a
                // result 3x past the documented cap while the equivalent
                // `a + a + a` cleanly panicked. Checked HERE, after each
                // accumulation step (fail-fast, matching `BigInt::pow`'s own
                // precedent), not just once at the end, so a single wildly
                // out-of-range item can't force one huge intermediate
                // allocation before the check ever runs.
                Value::BigInt(_) => {
                    let mut acc = crate::bigint::BigInt::zero();
                    for item in items.iter() {
                        let Value::BigInt(b) = item else { unreachable!() };
                        acc = acc.add(b);
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "BigInt arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::bigint::MAX_BIGINT_LIMBS * 9
                            ));
                        }
                    }
                    Ok(Value::BigInt(Rc::new(acc)))
                }
                Value::Rational(_) => {
                    let mut acc = crate::rational::Rational::from_ints(0, 1).unwrap();
                    for item in items.iter() {
                        let Value::Rational(r) = item else { unreachable!() };
                        acc = acc.add(r)?;
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "Rational arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::bigint::MAX_BIGINT_LIMBS * 9
                            ));
                        }
                    }
                    Ok(Value::Rational(Rc::new(acc)))
                }
                // Same PR-it943-shaped fix as BigInt/Rational just above --
                // the shared `raw_binary_op` boundary checks
                // `exceeds_max_size()` after every `+`, but this loop's own
                // accumulator calls `Decimal::add` directly, so it needs the
                // SAME per-step check to fail fast instead of silently
                // building a result past `MAX_DECIMAL_SCALE`/
                // `MAX_BIGINT_LIMBS` three summands past the cap.
                Value::Decimal(_) => {
                    let mut acc = crate::decimal::Decimal::zero();
                    for item in items.iter() {
                        let Value::Decimal(d) = item else { unreachable!() };
                        acc = crate::decimal::Decimal::add(&acc, d)?;
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "Decimal arithmetic result would be too large to compute (limit ~{} limbs / {}-digit scale)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::decimal::MAX_DECIMAL_SCALE
                            ));
                        }
                    }
                    Ok(Value::Decimal(Rc::new(acc)))
                }
                _ => unreachable!(),
            }
        }
        (Value::List(items), "sum") => {
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut is_float = false;
            for item in items.iter() {
                match item {
                    Value::Int(v) => {
                        int_sum = int_sum
                            .checked_add(*v)
                            .ok_or("integer overflow in sum")?
                    }
                    Value::Float(v) => {
                        is_float = true;
                        float_sum += v;
                    }
                    other => return Err(format!("cannot sum {}", other.type_name())),
                }
            }
            if is_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        (Value::List(items), "fold") => {
            let mut it = args.into_iter();
            let mut acc = it.next().ok_or("`fold` needs an initial value")?;
            let f = it.next().ok_or("`fold` needs a function")?;
            for item in items.iter() {
                acc = call(f.clone(), vec![acc, item.clone()])?;
            }
            Ok(acc)
        }
        // Like `fold`, but returns each running accumulator instead of just the last —
        // e.g. [1, 2, 3].scan(0, fn a x { a + x }) == [1, 3, 6] (prefix sums). The
        // initial value seeds the first step but is not itself included.
        (Value::List(items), "scan") => {
            let mut it = args.into_iter();
            let mut acc = it.next().ok_or("`scan` needs an initial value")?;
            let f = it.next().ok_or("`scan` needs a function")?;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                acc = call(f.clone(), vec![acc, item.clone()])?;
                out.push(acc.clone());
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "any") => {
            let f = args.into_iter().next().ok_or("`any` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        (Value::List(items), "all") => {
            let f = args.into_iter().next().ok_or("`all` needs a function")?;
            for item in items.iter() {
                if let Value::Bool(false) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        (Value::List(items), "sort") => {
            // Delegates to `sort_order`, not `list_order` (production-hardening
            // PR-it711: see `sort_order`'s own doc comment for why `.sort()` needs a
            // GENUINE total order under NaN -- Rust's `sort_by` crashes without one --
            // while min/max/min_by/max_by keep `list_order`'s original NaN-inert fold
            // unchanged). Every orderable element type -- Int/Float/Str plus SizedInt/
            // F32/BigInt/Rational as of PR-it549 -- stays supported either way.
            let mut out = items.as_ref().clone();
            let mut err = None;
            out.sort_by(|a, b| match sort_order(a, b) {
                Ok(ord) => ord,
                Err(e) => {
                    err = Some(e);
                    std::cmp::Ordering::Equal
                }
            });
            match err {
                Some(e) => Err(e),
                None => Ok(Value::List(Rc::new(out))),
            }
        }
        (Value::List(items), "take") => match args.into_iter().next() {
            Some(Value::Int(n)) => {
                let n = (n.max(0) as usize).min(items.len());
                Ok(Value::List(Rc::new(items[..n].to_vec())))
            }
            _ => Err("`take` needs an Int".into()),
        },
        (Value::List(items), "drop") => match args.into_iter().next() {
            Some(Value::Int(n)) => {
                let n = (n.max(0) as usize).min(items.len());
                Ok(Value::List(Rc::new(items[n..].to_vec())))
            }
            _ => Err("`drop` needs an Int".into()),
        },
        (Value::List(items), "get") => match args.into_iter().next() {
            Some(Value::Int(i)) => Ok(if i >= 0 && (i as usize) < items.len() {
                Value::some(items[i as usize].clone())
            } else {
                Value::none()
            }),
            _ => Err("`get` needs an Int".into()),
        },
        (Value::List(items), "index_of") => {
            let needle = args.into_iter().next().ok_or("`index_of` needs a value")?;
            Ok(items
                .iter()
                .position(|v| *v == needle)
                .map(|i| Value::some(Value::Int(i as i64)))
                .unwrap_or_else(Value::none))
        }
        (Value::List(items), "contains") => {
            let needle = args.into_iter().next().ok_or("`contains` needs a value")?;
            Ok(Value::Bool(items.iter().any(|v| *v == needle)))
        }
        (Value::List(items), "push") => {
            let v = args.into_iter().next().ok_or("`push` needs a value")?;
            let mut out = items.as_ref().clone();
            out.push(v);
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "first") => Ok(items.first().cloned().map(Value::some).unwrap_or_else(Value::none)),
        (Value::List(items), "last") => Ok(items.last().cloned().map(Value::some).unwrap_or_else(Value::none)),
        (Value::List(items), "reverse") => {
            let mut out = items.as_ref().clone();
            out.reverse();
            Ok(Value::List(Rc::new(out)))
        }
        // Cyclically shift elements: rotate_left(n) moves the first n to the end,
        // rotate_right(n) moves the last n to the front. n is taken modulo the length so any
        // shift (including n > len) is well-defined; an empty list is unchanged.
        (Value::List(items), "rotate_left") | (Value::List(items), "rotate_right") => {
            match args.into_iter().next() {
                Some(Value::Int(n)) => {
                    let len = items.len();
                    if len == 0 {
                        return Ok(Value::List(Rc::new(items.as_ref().clone())));
                    }
                    // reduce n into [0, len) with a floor-mod so negative shifts also work
                    let mut k = (n % len as i64) as isize;
                    if k < 0 {
                        k += len as isize;
                    }
                    let mut k = k as usize;
                    if name == "rotate_right" {
                        k = (len - k) % len;
                    }
                    let mut out = Vec::with_capacity(len);
                    out.extend_from_slice(&items[k..]);
                    out.extend_from_slice(&items[..k]);
                    Ok(Value::List(Rc::new(out)))
                }
                _ => Err(format!("`{name}` needs an Int").into()),
            }
        }
        // Insert `sep` between each pair of adjacent elements: [1,2,3].intersperse(0) =
        // [1,0,2,0,3]. Empty and singleton lists are returned unchanged.
        (Value::List(items), "intersperse") => match args.into_iter().next() {
            Some(sep) => {
                let mut out: Vec<Value> = Vec::with_capacity(items.len().saturating_mul(2));
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(sep.clone());
                    }
                    out.push(it.clone());
                }
                Ok(Value::List(Rc::new(out)))
            }
            None => Err("`intersperse` needs a separator".into()),
        },
        (Value::List(items), "join") => {
            let sep = match args.into_iter().next() {
                Some(Value::Str(ref s)) => s.as_str().to_string(),
                _ => return Err("`join` needs a Str separator".into()),
            };
            let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
            Ok(Value::str(parts.join(&sep)))
        }
        (Value::List(items), "is_empty") => Ok(Value::Bool(items.is_empty())),
        (Value::List(items), "concat") => match args.into_iter().next() {
            Some(Value::List(ref other)) => {
                let mut out = items.as_ref().clone();
                out.extend(other.iter().cloned());
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`concat` needs a List".into()),
        },
        (Value::List(items), "unique") => {
            // A REAL, live-confirmed severe latency divergence found+fixed
            // (production-hardening PR-it825): the naive O(n^2) scan below
            // (each element linearly rescans the whole accumulator built so
            // far) took 78s on a compiled NATIVE binary to deduplicate a
            // 100,000-element List[Int] -- an ordinary, non-adversarial
            // operation (deduplicating IDs/log-lines/tags is mundane).
            // FAST PATH: sort-then-adjacent-dedup is O(n log n), reusing the
            // SAME `sort_order` comparator `.sort()` was already fixed with
            // (PR-it711/it818's native `k_list_order` counterpart) -- but
            // UNLIKE `.sort()` (K0234-restricted to orderable types),
            // `.unique()` has NO type restriction and must keep working on
            // EVERY `List[T]` (Bool, ADTs, nested List/Map/Set, …), so this
            // only fires for the types below and falls back to the ORIGINAL
            // O(n^2) `==`-based scan otherwise -- a list is homogeneous
            // (KUPL's static typing), so checking just the FIRST element's
            // tag decides it for the whole list. Rational is DELIBERATELY
            // EXCLUDED even though `sort_order` technically supports it:
            // `Rational`'s `==` is a cheap derived structural comparison,
            // but `sort_order`'s `<`-based ordering goes through
            // `cmp_would_be_too_expensive`'s cross-multiplication guard
            // (PR-it718) -- switching `.unique()` to the sort-based path
            // for Rational would introduce a NEW resource-exhaustion/error
            // risk for huge-Rational lists that the cheap `==`-based path
            // never had, a genuine behavioral regression this fix must not
            // introduce. The adjacent-duplicate check after sorting uses
            // `==` (`PartialEq`), NOT `sort_order`'s own notion of
            // equality, specifically so a run of `sort_order`-tied-but-not-
            // `==`-equal elements (the ONLY case: multiple NaNs, which
            // `sort_order` treats as mutually "equal" for ordering purposes
            // but `==` correctly keeps as IEEE-distinct) still keeps every
            // element, preserving `.unique()`'s existing, already-tested
            // "duplicate NaNs are NOT collapsed" behavior exactly.
            fn unique_fast_eligible(v: &Value) -> bool {
                matches!(
                    v,
                    Value::Int(_)
                        | Value::Float(_)
                        | Value::F32(_)
                        | Value::Str(_)
                        | Value::SizedInt(_)
                        | Value::BigInt(_)
                )
            }
            if items.len() > 1 && items.first().is_some_and(unique_fast_eligible) {
                let mut indexed: Vec<(usize, &Value)> = items.iter().enumerate().collect();
                indexed.sort_by(|a, b| sort_order(a.1, b.1).unwrap_or(std::cmp::Ordering::Equal));
                let mut kept: Vec<(usize, &Value)> = Vec::with_capacity(indexed.len());
                for pair in indexed {
                    if kept.last().is_none_or(|last: &(usize, &Value)| last.1 != pair.1) {
                        kept.push(pair);
                    }
                }
                kept.sort_by_key(|(idx, _)| *idx);
                Ok(Value::List(Rc::new(kept.into_iter().map(|(_, v)| v.clone()).collect())))
            } else {
                let mut out: Vec<Value> = Vec::new();
                for it in items.iter() {
                    if !out.iter().any(|x| x == it) {
                        out.push(it.clone());
                    }
                }
                Ok(Value::List(Rc::new(out)))
            }
        }
        // Collapse runs of CONSECUTIVE equal elements (Unix `uniq`) — unlike `unique`, a value can
        // reappear later if it isn't adjacent to its previous occurrence.
        (Value::List(items), "dedup") => {
            let mut out: Vec<Value> = Vec::new();
            for it in items.iter() {
                if out.last().is_none_or(|last| last != it) {
                    out.push(it.clone());
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "init") => {
            let n = items.len().saturating_sub(1);
            Ok(Value::List(Rc::new(items[..n].to_vec())))
        }
        (Value::List(items), "tail") => {
            let start = if items.is_empty() { 0 } else { 1 };
            Ok(Value::List(Rc::new(items[start..].to_vec())))
        }
        (Value::List(items), "product") if matches!(items.first(), Some(Value::SizedInt(_) | Value::F32(_) | Value::BigInt(_) | Value::Rational(_) | Value::Decimal(_))) => {
            match items.first().unwrap() {
                Value::SizedInt(b) => {
                    let w = b.1;
                    let mut acc: i128 = 1;
                    for item in items.iter() {
                        let Value::SizedInt(b) = item else { unreachable!() };
                        // A REAL, SIBLING bug to it671's SizedInt-mul fix (PR-it672):
                        // `acc *= b.0` in plain i128 can itself overflow i128, the same
                        // way the `*`/wrapping_mul/saturating_mul call sites did --
                        // reachable trivially with just `[u64::MAX, u64::MAX].product()`
                        // (confirmed live before this fix: crashed with an actual Rust
                        // overflow panic, not the intended "integer overflow in product"
                        // one). `sum`'s plain `+=` just above is NOT at risk the same
                        // way -- summing i8..u64-range terms would need on the order of
                        // 2^64 elements to overflow i128, which is not a reachable list
                        // size, unlike multiplication's much faster growth.
                        acc = acc.checked_mul(b.0).ok_or_else(|| "integer overflow in product".to_string())?;
                        if !w.check_range(acc) {
                            return Err("integer overflow in product".into());
                        }
                    }
                    Ok(Value::SizedInt(Box::new((acc, w))))
                }
                Value::F32(_) => {
                    let mut acc: f32 = 1.0;
                    for item in items.iter() {
                        let Value::F32(v) = item else { unreachable!() };
                        acc *= v;
                    }
                    Ok(Value::F32(acc))
                }
                // Same PR-it943 fix as `sum`'s BigInt/Rational arms above --
                // see that comment for the full rationale.
                Value::BigInt(_) => {
                    let mut acc = crate::bigint::BigInt::from_i64(1);
                    for item in items.iter() {
                        let Value::BigInt(b) = item else { unreachable!() };
                        acc = acc.mul(b);
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "BigInt arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::bigint::MAX_BIGINT_LIMBS * 9
                            ));
                        }
                    }
                    Ok(Value::BigInt(Rc::new(acc)))
                }
                Value::Rational(_) => {
                    let mut acc = crate::rational::Rational::from_ints(1, 1).unwrap();
                    for item in items.iter() {
                        let Value::Rational(r) = item else { unreachable!() };
                        acc = acc.mul(r)?;
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "Rational arithmetic result would be too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::bigint::MAX_BIGINT_LIMBS * 9
                            ));
                        }
                    }
                    Ok(Value::Rational(Rc::new(acc)))
                }
                Value::Decimal(_) => {
                    let mut acc = crate::decimal::Decimal::from_i64(1);
                    for item in items.iter() {
                        let Value::Decimal(d) = item else { unreachable!() };
                        acc = crate::decimal::Decimal::mul(&acc, d);
                        if acc.exceeds_max_size() {
                            return Err(format!(
                                "Decimal arithmetic result would be too large to compute (limit ~{} limbs / {}-digit scale)",
                                crate::bigint::MAX_BIGINT_LIMBS,
                                crate::decimal::MAX_DECIMAL_SCALE
                            ));
                        }
                    }
                    Ok(Value::Decimal(Rc::new(acc)))
                }
                _ => unreachable!(),
            }
        }
        (Value::List(items), "product") => {
            let mut int_prod: i64 = 1;
            let mut float_prod: f64 = 1.0;
            let mut is_float = false;
            for item in items.iter() {
                match item {
                    Value::Int(v) => {
                        int_prod = int_prod
                            .checked_mul(*v)
                            .ok_or("integer overflow in product")?
                    }
                    Value::Float(v) => {
                        is_float = true;
                        float_prod *= v;
                    }
                    other => return Err(format!("cannot multiply {}", other.type_name())),
                }
            }
            if is_float {
                Ok(Value::Float(float_prod * int_prod as f64))
            } else {
                Ok(Value::Int(int_prod))
            }
        }
        (Value::List(items), "min") | (Value::List(items), "max") => {
            let want_min = name == "min";
            let mut best: Option<Value> = None;
            for item in items.iter() {
                let take = match &best {
                    None => true,
                    Some(b) => {
                        let ord = list_order(b, item)?;
                        if want_min {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        }
                    }
                };
                if take {
                    best = Some(item.clone());
                }
            }
            Ok(best.map(Value::some).unwrap_or_else(Value::none))
        }
        (Value::List(items), "min_by") | (Value::List(items), "max_by") => {
            let f = args.into_iter().next().ok_or("`min_by`/`max_by` needs a function")?;
            let want_min = name == "min_by";
            let mut best: Option<(Value, Value)> = None; // (element, its key)
            for item in items.iter() {
                let key = call(f.clone(), vec![item.clone()])?;
                let take = match &best {
                    None => true,
                    Some((_, bk)) => {
                        let ord = list_order(bk, &key)?;
                        if want_min {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        }
                    }
                };
                if take {
                    best = Some((item.clone(), key));
                }
            }
            Ok(best.map(|(v, _)| Value::some(v)).unwrap_or_else(Value::none))
        }
        (Value::List(items), "flatten") => {
            let mut out = Vec::new();
            for item in items.iter() {
                match item {
                    Value::List(inner) => out.extend(inner.iter().cloned()),
                    other => return Err(format!("`flatten` needs a List of Lists, found {}", other.type_name())),
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "count") => {
            let f = args.into_iter().next().ok_or("`count` needs a function")?;
            let mut n = 0i64;
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    n += 1;
                }
            }
            Ok(Value::Int(n))
        }
        (Value::List(items), "flat_map") => {
            let f = args.into_iter().next().ok_or("`flat_map` needs a function")?;
            let mut out = Vec::new();
            for item in items.iter() {
                match call(f.clone(), vec![item.clone()])? {
                    Value::List(ref inner) => out.extend(inner.iter().cloned()),
                    other => return Err(format!("`flat_map` function must return a List, got {}", other.type_name())),
                }
            }
            Ok(Value::List(Rc::new(out)))
        }
        (Value::List(items), "sort_by") => {
            let f = args.into_iter().next().ok_or("`sort_by` needs a function")?;
            // compute each element's Int key first, then stable-sort by it
            let mut keyed: Vec<(i64, Value)> = Vec::with_capacity(items.len());
            for item in items.iter() {
                match call(f.clone(), vec![item.clone()])? {
                    Value::Int(k) => keyed.push((k, item.clone())),
                    other => return Err(format!("`sort_by` key function must return Int, got {}", other.type_name())),
                }
            }
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Value::List(Rc::new(keyed.into_iter().map(|(_, v)| v).collect())))
        }
        (Value::List(items), "group_by") => {
            let f = args.into_iter().next().ok_or("`group_by` needs a function")?;
            // A REAL, live-confirmed severe latency divergence found+fixed
            // (production-hardening PR-it827), the FIFTH instance of this
            // campaign's recurring "naive O(n^2) collection algorithm" bug
            // class (after Int.pow it814, List.sort it818, List.unique
            // it825, set_from_list it826): the naive fallback below finds
            // each item's bucket via a LINEAR SCAN through the buckets
            // seen so far, so grouping by a mostly-distinct key is O(n^2)
            // (live-confirmed: 1.38s/22.08s for 5,000/20,000 distinct-key
            // Ints, ~16x time for 4x size). Keys are computed EAGERLY, in
            // original list order, BEFORE branching on which path runs, so
            // `f`'s call count/order (and any side effects) are IDENTICAL
            // either way. FAST PATH (type-gated exactly like PR-it825/
            // it826, on the KEY's runtime type -- guaranteed homogeneous by
            // KUPL's static typing the same way the list's OWN element
            // type is, Rational excluded for the same cheap-`==`-vs-
            // expensive-`sort_order` asymmetry): sort by (key via
            // `sort_order`, then original index) so equal-key runs are
            // contiguous AND already in original list order -- the index
            // tiebreaker is needed because `qsort`'s C mirror isn't
            // guaranteed stable. Runs are split via `value_key_eq`
            // (matching `Map`'s OWN key identity, NaN-collapsing, same as
            // PR-it826) -- `sort_order`-equal implies `value_key_eq`-equal
            // for every type in this fast-path set (NaN-clustering agrees
            // with NaN-collapsing here, as PR-it826 already established),
            // so no non-contiguous equal-key elements can be missed.
            // Group order is then restored to FIRST-SEEN order (the
            // documented contract) via each run's smallest original index.
            fn group_by_fast_eligible(v: &Value) -> bool {
                matches!(
                    v,
                    Value::Int(_)
                        | Value::Float(_)
                        | Value::F32(_)
                        | Value::Str(_)
                        | Value::SizedInt(_)
                        | Value::BigInt(_)
                )
            }
            let mut keyed: Vec<(Value, Value, usize)> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let key = call(f.clone(), vec![item.clone()])?;
                keyed.push((key, item.clone(), i));
            }
            if keyed.len() > 1 && keyed.first().is_some_and(|(k, _, _)| group_by_fast_eligible(k)) {
                keyed.sort_by(|a, b| sort_order(&a.0, &b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.2.cmp(&b.2)));
                let mut groups: Vec<(Value, Vec<Value>, usize)> = Vec::new();
                let mut i = 0;
                while i < keyed.len() {
                    let mut j = i + 1;
                    while j < keyed.len() && value_key_eq(&keyed[i].0, &keyed[j].0) {
                        j += 1;
                    }
                    let first_idx = keyed[i].2;
                    let bucket = keyed[i..j].iter().map(|(_, v, _)| v.clone()).collect();
                    groups.push((keyed[i].0.clone(), bucket, first_idx));
                    i = j;
                }
                groups.sort_by_key(|(_, _, first_idx)| *first_idx);
                let pairs = groups.into_iter().map(|(k, vs, _)| (k, Value::List(Rc::new(vs)))).collect();
                return Ok(Value::Map(Rc::new(pairs)));
            }
            // first-seen key order preserved (Map is insertion-ordered)
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for (key, item, _) in keyed {
                match groups.iter_mut().find(|(k, _)| value_key_eq(k, &key)) {
                    Some((_, list)) => list.push(item),
                    None => groups.push((key, vec![item])),
                }
            }
            let pairs = groups
                .into_iter()
                .map(|(k, vs)| (k, Value::List(Rc::new(vs))))
                .collect();
            Ok(Value::Map(Rc::new(pairs)))
        }
        (Value::List(items), "position") => {
            let f = args.into_iter().next().ok_or("`position` needs a function")?;
            for (i, item) in items.iter().enumerate() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    return Ok(Value::some(Value::Int(i as i64)));
                }
            }
            Ok(Value::none())
        }
        (Value::List(items), "partition") => {
            let f = args.into_iter().next().ok_or("`partition` needs a function")?;
            let (mut yes, mut no) = (Vec::new(), Vec::new());
            for item in items.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![item.clone()])? {
                    yes.push(item.clone());
                } else {
                    no.push(item.clone());
                }
            }
            Ok(Value::List(Rc::new(vec![Value::List(Rc::new(yes)), Value::List(Rc::new(no))])))
        }
        (Value::List(items), "window") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 1 => {
                let n = n as usize;
                let mut out = Vec::new();
                if items.len() >= n {
                    for i in 0..=items.len() - n {
                        out.push(Value::List(Rc::new(items[i..i + n].to_vec())));
                    }
                }
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`window` needs a positive Int".into()),
        },
        (Value::List(items), "chunk") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 1 => {
                let n = n as usize;
                let out: Vec<Value> = items
                    .chunks(n)
                    .map(|c| Value::List(Rc::new(c.to_vec())))
                    .collect();
                Ok(Value::List(Rc::new(out)))
            }
            _ => Err("`chunk` needs a positive Int".into()),
        },
        // it116: attenuation narrows, never widens (CAPABILITIES.md §5's own
        // open question, resolved here) -- an already-limited capability
        // can only be narrowed to the SAME host again (a no-op) or refused;
        // only an unrestricted (root) capability can be freshly limited.
        (Value::CapNet(c), "limited_to") => match args.into_iter().next() {
            Some(Value::Str(ref host)) => match &c.allowed_host {
                None => Ok(Value::CapNet(Rc::new(crate::value::CapNetInner {
                    allowed_host: Some((**host).clone()),
                }))),
                Some(h) if *h == **host => Ok(Value::CapNet(c.clone())),
                Some(h) => Err(format!(
                    "cannot widen a capability already limited to `{h}` to a different host `{host}`"
                )),
            },
            _ => Err("`limited_to` needs a Str".into()),
        },
        // it118: mirrors CapNet's own arm exactly.
        (Value::CapFs(c), "limited_to") => match args.into_iter().next() {
            Some(Value::Str(ref prefix)) => match &c.allowed_prefix {
                None => Ok(Value::CapFs(Rc::new(crate::value::CapFsInner {
                    allowed_prefix: Some((**prefix).clone()),
                }))),
                Some(p) if *p == **prefix => Ok(Value::CapFs(c.clone())),
                Some(p) => Err(format!(
                    "cannot widen a capability already limited to `{p}` to a different prefix `{prefix}`"
                )),
            },
            _ => Err("`limited_to` needs a Str".into()),
        },
        (Value::Str(s), "len") => Ok(Value::Int(s.chars().count() as i64)),
        (Value::Str(s), "contains") => match args.into_iter().next() {
            Some(Value::Str(ref n)) => Ok(Value::Bool(s.contains(n.as_str()))),
            _ => Err("`contains` needs a Str".into()),
        },
        (Value::Str(s), "starts_with") => match args.into_iter().next() {
            Some(Value::Str(ref n)) => Ok(Value::Bool(s.starts_with(n.as_str()))),
            _ => Err("`starts_with` needs a Str".into()),
        },
        // ASCII-only case mapping: non-ASCII characters pass through unchanged.
        // Full Unicode case mapping needs large tables that the zero-dependency
        // native C runtime can't carry, so all engines agree on ASCII-only (this
        // keeps `to_upper`/`to_lower` byte-identical across interp/KVM/native).
        (Value::Str(s), "to_upper") => Ok(Value::str(s.to_ascii_uppercase())),
        (Value::Str(s), "to_lower") => Ok(Value::str(s.to_ascii_lowercase())),
        (Value::Str(s), "capitalize") => {
            // ASCII casing (matching to_upper/to_lower): the first char is uppercased and the
            // rest lowercased; non-ASCII bytes are left unchanged, and an empty string stays
            // empty. get_mut(0..1) is Some only when the first char is single-byte ASCII.
            let mut out = s.to_ascii_lowercase();
            if let Some(first) = out.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            Ok(Value::str(out))
        }
        (Value::Str(s), "swapcase") => {
            // ASCII casing: swap the case of each ASCII letter; every other char (digits,
            // punctuation, non-ASCII) is left unchanged. "Hello, WÖRLD" -> "hELLO, wÖRLD".
            let out: String = s
                .chars()
                .map(|c| {
                    if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            Ok(Value::str(out))
        }
        (Value::Str(s), "trim") => Ok(Value::str(s.trim().to_string())),
        // trim ` \t\n\r` from one side (the same set as `trim`, matching the C mirror)
        (Value::Str(s), "trim_start") => {
            Ok(Value::str(s.trim_start_matches([' ', '\t', '\n', '\r']).to_string()))
        }
        (Value::Str(s), "trim_end") => {
            Ok(Value::str(s.trim_end_matches([' ', '\t', '\n', '\r']).to_string()))
        }
        (Value::Str(s), "ends_with") => match args.into_iter().next() {
            Some(Value::Str(ref n)) => Ok(Value::Bool(s.ends_with(n.as_str()))),
            _ => Err("`ends_with` needs a Str".into()),
        },
        (Value::Str(s), "replace") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Str(ref from)), Some(Value::Str(ref to))) => {
                    if from.is_empty() {
                        return Err("`replace` needs a non-empty pattern".into());
                    }
                    Ok(Value::str(s.replace(from.as_str(), to.as_str())))
                }
                _ => Err("`replace` needs two Str arguments".into()),
            }
        }
        (Value::Str(s), "chars") => Ok(Value::List(Rc::new(
            s.chars().map(|c| Value::str(c.to_string())).collect(),
        ))),
        (Value::Str(s), "repeat") => match args.into_iter().next() {
            Some(Value::Int(n)) if n >= 0 => {
                if s.len().saturating_mul(n as usize) > 100_000_000 {
                    return Err("`repeat` result too large".into());
                }
                Ok(Value::str(s.repeat(n as usize)))
            }
            _ => Err("`repeat` needs a non-negative Int".into()),
        },
        (Value::Str(s), "parse_int") => Ok(s
            .parse::<i64>()
            .map(|v| Value::some(Value::Int(v)))
            .unwrap_or_else(|_| Value::none())),
        (Value::Str(s), "parse_radix") => match args.into_iter().next() {
            // Inverse of `to_radix`: parse an Int in base 2..=36 (accepts an optional +/-
            // sign, digits/letters valid for the base case-insensitively; NO 0x prefix, NO
            // whitespace — same strictness as `parse_int`). None on any malformed input.
            Some(Value::Int(b)) if (2..=36).contains(&b) => Ok(i64::from_str_radix(s, b as u32)
                .map(|v| Value::some(Value::Int(v)))
                .unwrap_or_else(|_| Value::none())),
            Some(Value::Int(_)) => Err("`parse_radix` base must be in 2..=36".into()),
            _ => Err("`parse_radix` needs an Int base".into()),
        },
        (Value::Str(s), "parse_float") => Ok(s
            .parse::<f64>()
            .map(|v| Value::some(Value::Float(v)))
            .unwrap_or_else(|_| Value::none())),
        (Value::Str(s), "split") => match args.into_iter().next() {
            Some(Value::Str(ref sep)) if !sep.is_empty() => Ok(Value::List(Rc::new(
                s.split(sep.as_str()).map(Value::str).collect(),
            ))),
            Some(Value::Str(_)) => Err("`split` needs a non-empty separator".into()),
            _ => Err("`split` needs a Str separator".into()),
        },
        (Value::Str(s), "is_empty") => Ok(Value::Bool(s.is_empty())),
        (Value::Str(s), "reverse") => Ok(Value::str(s.chars().rev().collect::<String>())),
        (Value::Str(s), "rfind") => match args.into_iter().next() {
            Some(Value::Str(ref sub)) => Ok(match s.rfind(sub.as_str()) {
                // byte offset -> character index (matches `index_of`)
                Some(byte) => Value::some(Value::Int(s[..byte].chars().count() as i64)),
                None => Value::none(),
            }),
            _ => Err("`rfind` needs a Str".into()),
        },
        (Value::Str(s), "replace_first") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Str(ref from)), Some(Value::Str(ref to))) => {
                    if from.is_empty() {
                        return Err("`replace_first` needs a non-empty pattern".into());
                    }
                    Ok(Value::str(s.as_str().replacen(from.as_str(), to.as_str(), 1)))
                }
                _ => Err("`replace_first` needs two Str arguments".into()),
            }
        }
        (Value::Str(s), "split_once") => match args.into_iter().next() {
            Some(Value::Str(ref sep)) => Ok(match s.as_str().split_once(sep.as_str()) {
                Some((a, b)) => Value::some(Value::List(Rc::new(vec![
                    Value::str(a.to_string()),
                    Value::str(b.to_string()),
                ]))),
                None => Value::none(),
            }),
            _ => Err("`split_once` needs a Str".into()),
        },
        (Value::Str(s), "lines") => Ok(Value::List(Rc::new(
            s.lines().map(Value::str).collect(),
        ))),
        (Value::Str(s), "index_of") => match args.into_iter().next() {
            Some(Value::Str(ref sub)) => Ok(match s.find(sub.as_str()) {
                // byte offset -> character index
                Some(byte) => Value::some(Value::Int(s[..byte].chars().count() as i64)),
                None => Value::none(),
            }),
            _ => Err("`index_of` needs a Str".into()),
        },
        (Value::Str(s), "count") => match args.into_iter().next() {
            Some(Value::Str(ref sub)) if !sub.is_empty() => {
                Ok(Value::Int(s.matches(sub.as_str()).count() as i64))
            }
            Some(Value::Str(_)) => Err("`count` needs a non-empty Str".into()),
            _ => Err("`count` needs a Str".into()),
        },
        (Value::Str(s), "slice") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(a)), Some(Value::Int(b))) => {
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let lo = a.clamp(0, len) as usize;
                    // `hi` in [lo, len]. NB: not `b.clamp(a.max(0), len)` — when
                    // a > len the clamp bounds invert (min > max) and Rust panics.
                    let hi = (b.clamp(0, len) as usize).max(lo);
                    Ok(Value::str(chars[lo..hi].iter().collect::<String>()))
                }
                _ => Err("`slice` needs two Int arguments".into()),
            }
        }
        (Value::Str(s), "pad_left") | (Value::Str(s), "pad_right") => {
            let left = name == "pad_left";
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(width)), Some(Value::Str(ref ch))) => {
                    let fill = ch.chars().next().unwrap_or(' ');
                    let cur = s.chars().count() as i64;
                    if cur >= width || width > 100_000_000 {
                        Ok(Value::str(s.as_str().to_string()))
                    } else {
                        let pad: String = std::iter::repeat(fill).take((width - cur) as usize).collect();
                        Ok(Value::str(if left {
                            format!("{pad}{s}")
                        } else {
                            format!("{s}{pad}")
                        }))
                    }
                }
                _ => Err("`pad_left`/`pad_right` need an Int width and a Str fill".into()),
            }
        }
        (Value::Str(s), "center") => {
            // Center within `width` (char count) using `fill`; when the padding is odd the
            // extra fill goes on the RIGHT (lpad = total/2). Mirrors pad_left/pad_right: a
            // width <= current length (or absurdly large) returns the string unchanged.
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(width)), Some(Value::Str(ref ch))) => {
                    let fill = ch.chars().next().unwrap_or(' ');
                    let cur = s.chars().count() as i64;
                    if cur >= width || width > 100_000_000 {
                        Ok(Value::str(s.as_str().to_string()))
                    } else {
                        let total = (width - cur) as usize;
                        let lpad = total / 2;
                        let l: String = std::iter::repeat(fill).take(lpad).collect();
                        let r: String = std::iter::repeat(fill).take(total - lpad).collect();
                        Ok(Value::str(format!("{l}{s}{r}")))
                    }
                }
                _ => Err("`center` needs an Int width and a Str fill".into()),
            }
        }
        (Value::Int(v), "to_str") => Ok(Value::str(v.to_string())),
        (Value::Int(v), "to_float") => Ok(Value::Float(*v as f64)),
        // Int -> sized int: checked narrowing, panics if out of range.
        (Value::Int(v), "to_i8") | (Value::Int(v), "to_i16") | (Value::Int(v), "to_i32")
        | (Value::Int(v), "to_i64") | (Value::Int(v), "to_u8") | (Value::Int(v), "to_u16")
        | (Value::Int(v), "to_u32") | (Value::Int(v), "to_u64") => {
            let w = IntW::from_name(&name[3..]).expect("width method");
            let x = *v as i128;
            if w.check_range(x) {
                Ok(Value::SizedInt(Box::new((x, w))))
            } else {
                Err(format!("{v} out of range for `{}`", w.name()))
            }
        }
        // sized int -> Int (i64), checked (a u64 above i64::MAX panics).
        (Value::SizedInt(b), "to_int") => {
            let v = b.0;
            if v >= i64::MIN as i128 && v <= i64::MAX as i128 {
                Ok(Value::Int(v as i64))
            } else {
                Err(format!("{v} does not fit in Int (i64)"))
            }
        }
        (Value::SizedInt(b), "to_str") => Ok(Value::str(b.0.to_string())),
        (Value::SizedInt(b), "to_float") => Ok(Value::Float(b.0 as f64)),
        // sized int -> another sized width (checked narrowing/widening)
        (Value::SizedInt(b), "to_i8") | (Value::SizedInt(b), "to_i16")
        | (Value::SizedInt(b), "to_i32") | (Value::SizedInt(b), "to_i64")
        | (Value::SizedInt(b), "to_u8") | (Value::SizedInt(b), "to_u16")
        | (Value::SizedInt(b), "to_u32") | (Value::SizedInt(b), "to_u64") => {
            let target = IntW::from_name(&name[3..]).expect("width method");
            if target.check_range(b.0) {
                Ok(Value::SizedInt(Box::new((b.0, target))))
            } else {
                Err(format!("{} out of range for `{}`", b.0, target.name()))
            }
        }
        // wrapping / saturating arithmetic + bitwise on sized ints (same width)
        (Value::SizedInt(b), m)
            if matches!(
                m,
                "wrapping_add" | "wrapping_sub" | "wrapping_mul"
                    | "saturating_add" | "saturating_sub" | "saturating_mul"
                    | "band" | "bor" | "bxor"
            ) =>
        {
            let (a, w) = (b.0, b.1);
            let rhs = match args.into_iter().next() {
                Some(Value::SizedInt(ref o)) if o.1 == w => o.0,
                _ => return Err(format!("`{m}` needs a `{}`", w.name())),
            };
            let bits = w.bits();
            let mask = (1i128 << bits) - 1;
            let r = match m {
                "wrapping_add" => w.wrap(a + rhs),
                "wrapping_sub" => w.wrap(a - rhs),
                // `a * rhs` in plain i128 can itself overflow for U64/I64 operands
                // near their extremes (PR-it671) -- route mul through the
                // overflow-safe helpers instead of the raw `*` operator.
                "wrapping_mul" => w.wrapping_mul(a, rhs),
                "saturating_add" => w.saturate(a + rhs),
                "saturating_sub" => w.saturate(a - rhs),
                "saturating_mul" => w.saturating_mul(a, rhs),
                "band" => w.wrap((a & mask) & (rhs & mask)),
                "bor" => w.wrap((a & mask) | (rhs & mask)),
                "bxor" => w.wrap((a & mask) ^ (rhs & mask)),
                _ => unreachable!(),
            };
            Ok(Value::SizedInt(Box::new((r, w))))
        }
        (Value::SizedInt(b), "bnot") => {
            let (a, w) = (b.0, b.1);
            let mask = (1i128 << w.bits()) - 1;
            Ok(Value::SizedInt(Box::new((w.wrap((a & mask) ^ mask), w))))
        }
        (Value::SizedInt(b), "shl") | (Value::SizedInt(b), "shr") => {
            let (a, w) = (b.0, b.1);
            let n = match args.into_iter().next() {
                Some(Value::Int(n)) if (0..w.bits() as i64).contains(&n) => n as u32,
                Some(Value::Int(_)) => {
                    return Err(format!("shift amount must be in 0..={}", w.bits() - 1))
                }
                _ => return Err(format!("`{name}` needs an Int shift amount")),
            };
            let mask = (1i128 << w.bits()) - 1;
            let r = if name == "shl" {
                w.wrap((a & mask) << n)
            } else if w.is_signed() {
                w.wrap(a >> n) // arithmetic (sign-preserving)
            } else {
                w.wrap((a & mask) >> n) // logical (zero-fill)
            };
            Ok(Value::SizedInt(Box::new((r, w))))
        }
        // f32 <-> Float
        (Value::F32(v), "to_float") => Ok(Value::Float(*v as f64)),
        (Value::F32(v), "to_str") => Ok(Value::str(Value::F32(*v).to_string())),
        (Value::Float(v), "to_f32") => Ok(Value::F32(*v as f32)),
        (Value::Int(v), "abs") => v
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| "integer overflow in abs".to_string()),
        (Value::Int(v), "abs_diff") => match args.into_iter().next() {
            // |a - b| computed in i128 so no intermediate overflow; a result that exceeds
            // i64::MAX (e.g. abs_diff(i64::MIN, 0) = 2^63) is a checked panic, since KUPL Ints
            // are signed and never wrap.
            Some(Value::Int(w)) => {
                let d = (*v as i128 - w as i128).unsigned_abs();
                if d <= i64::MAX as u128 {
                    Ok(Value::Int(d as i64))
                } else {
                    Err("integer overflow in `abs_diff`".into())
                }
            }
            _ => Err("`abs_diff` needs an Int".into()),
        },
        (Value::Int(v), "min") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int((*v).min(w))),
            _ => Err("`min` needs an Int".into()),
        },
        (Value::Int(v), "max") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int((*v).max(w))),
            _ => Err("`max` needs an Int".into()),
        },
        (Value::Int(v), "pow") => match args.into_iter().next() {
            Some(Value::Int(e)) if e >= 0 && e <= u32::MAX as i64 => (*v)
                .checked_pow(e as u32)
                .map(Value::Int)
                .ok_or_else(|| "integer overflow in pow".to_string()),
            Some(Value::Int(_)) => Err("`pow` needs a non-negative exponent".into()),
            _ => Err("`pow` needs an Int".into()),
        },
        (Value::Int(v), "gcd") => match args.into_iter().next() {
            Some(Value::Int(w)) => {
                let (mut a, mut b) = (v.unsigned_abs(), w.unsigned_abs());
                while b != 0 {
                    let t = b;
                    b = a % b;
                    a = t;
                }
                Ok(Value::Int(a as i64))
            }
            _ => Err("`gcd` needs an Int".into()),
        },
        // Euclidean division: rem_euclid's result is ALWAYS non-negative (unlike `%`, which
        // takes the sign of the dividend), and div_euclid rounds toward negative infinity for a
        // positive divisor. Both panic on a zero divisor or the i64::MIN / -1 overflow.
        (Value::Int(v), "rem_euclid") => match args.into_iter().next() {
            Some(Value::Int(w)) => match v.checked_rem_euclid(w) {
                Some(r) => Ok(Value::Int(r)),
                None if w == 0 => Err("division by zero".into()),
                None => Err("integer overflow in `rem_euclid`".into()),
            },
            _ => Err("`rem_euclid` needs an Int".into()),
        },
        (Value::Int(v), "div_euclid") => match args.into_iter().next() {
            Some(Value::Int(w)) => match v.checked_div_euclid(w) {
                Some(q) => Ok(Value::Int(q)),
                None if w == 0 => Err("division by zero".into()),
                None => Err("integer overflow in `div_euclid`".into()),
            },
            _ => Err("`div_euclid` needs an Int".into()),
        },
        (Value::Int(v), "lcm") => match args.into_iter().next() {
            // Least common multiple, the natural companion to gcd: |v|/gcd(v,w) * |w|,
            // always non-negative. lcm(0, _) = lcm(_, 0) = 0 by convention. A result that
            // does not fit in i64 is an overflow panic (matching Int arithmetic).
            Some(Value::Int(w)) => {
                if *v == 0 || w == 0 {
                    Ok(Value::Int(0))
                } else {
                    let (mut a, mut b) = (v.unsigned_abs(), w.unsigned_abs());
                    while b != 0 {
                        let t = b;
                        b = a % b;
                        a = t;
                    }
                    match (v.unsigned_abs() / a).checked_mul(w.unsigned_abs()) {
                        Some(u) if u <= i64::MAX as u64 => Ok(Value::Int(u as i64)),
                        _ => Err("integer overflow in `lcm`".into()),
                    }
                }
            }
            _ => Err("`lcm` needs an Int".into()),
        },
        (Value::Int(v), "clamp") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Int(lo)), Some(Value::Int(hi))) => {
                    if lo > hi {
                        Err("`clamp`: lo must not exceed hi".into())
                    } else {
                        Ok(Value::Int((*v).clamp(lo, hi)))
                    }
                }
                _ => Err("`clamp` needs two Int arguments".into()),
            }
        }
        (Value::Int(v), "sign") => Ok(Value::Int(v.signum())),
        (Value::Int(v), "is_even") => Ok(Value::Bool(v % 2 == 0)),
        (Value::Int(v), "is_odd") => Ok(Value::Bool(v % 2 != 0)),
        (Value::Int(v), "to_hex") => Ok(Value::str(int_to_radix(*v, 16))),
        (Value::Int(v), "to_binary") => Ok(Value::str(int_to_radix(*v, 2))),
        (Value::Int(v), "to_octal") => Ok(Value::str(int_to_radix(*v, 8))),
        (Value::Int(v), "to_radix") => match args.into_iter().next() {
            Some(Value::Int(b)) if (2..=36).contains(&b) => {
                Ok(Value::str(int_to_radix(*v, b as u32)))
            }
            Some(Value::Int(_)) => Err("`to_radix` base must be in 2..=36".into()),
            _ => Err("`to_radix` needs an Int base".into()),
        },
        (Value::Int(v), "isqrt") => {
            if *v < 0 {
                Err("`isqrt` of a negative Int".into())
            } else {
                Ok(Value::Int(int_isqrt(*v)))
            }
        }
        (Value::Int(v), "factorial") => {
            // 0! = 1! = 1; a negative is an error; anything past 20! overflows i64 and is a
            // checked overflow panic (matching KUPL's Int arithmetic), never a wrapped value.
            if *v < 0 {
                Err("`factorial` of a negative Int".into())
            } else {
                let mut acc: i64 = 1;
                let mut k: i64 = 2;
                while k <= *v {
                    match acc.checked_mul(k) {
                        Some(x) => acc = x,
                        None => return Err("integer overflow in `factorial`".into()),
                    }
                    k += 1;
                }
                Ok(Value::Int(acc))
            }
        }
        (Value::Int(v), "band") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v & w)),
            _ => Err("`band` needs an Int".into()),
        },
        (Value::Int(v), "bor") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v | w)),
            _ => Err("`bor` needs an Int".into()),
        },
        (Value::Int(v), "bxor") => match args.into_iter().next() {
            Some(Value::Int(w)) => Ok(Value::Int(v ^ w)),
            _ => Err("`bxor` needs an Int".into()),
        },
        (Value::Int(v), "bnot") => Ok(Value::Int(!v)),
        // Population count over the 64-bit two's-complement representation: a negative counts
        // the set bits of its i64 bit pattern ((-1).count_ones() = 64).
        (Value::Int(v), "count_ones") => Ok(Value::Int(v.count_ones() as i64)),
        // Base-10 digits of |n|, most-significant first: 0 -> [0], and negatives use unsigned_abs
        // so i64::MIN (whose .abs() would overflow) is handled — its magnitude is 2^63.
        (Value::Int(v), "digits") => {
            let mut n = v.unsigned_abs();
            let mut ds: Vec<Value> = Vec::new();
            if n == 0 {
                ds.push(Value::Int(0));
            } else {
                while n > 0 {
                    ds.push(Value::Int((n % 10) as i64));
                    n /= 10;
                }
                ds.reverse();
            }
            Ok(Value::List(Rc::new(ds)))
        }
        // Leading/trailing zero bits of the 64-bit pattern; both are 64 for 0 (matching Rust,
        // and the native impl must guard 0 since C clz/ctz of 0 is undefined behavior).
        (Value::Int(v), "leading_zeros") => Ok(Value::Int(v.leading_zeros() as i64)),
        (Value::Int(v), "trailing_zeros") => Ok(Value::Int(v.trailing_zeros() as i64)),
        (Value::Int(v), "shl") => match args.into_iter().next() {
            Some(Value::Int(n)) if (0..=63).contains(&n) => Ok(Value::Int(v << n)),
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`shl` needs an Int".into()),
        },
        (Value::Int(v), "shr") => match args.into_iter().next() {
            // arithmetic shift right (sign-preserving), matching i64 `>>`
            Some(Value::Int(n)) if (0..=63).contains(&n) => Ok(Value::Int(v >> n)),
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`shr` needs an Int".into()),
        },
        (Value::Int(v), "ushr") => match args.into_iter().next() {
            // logical (unsigned) shift right — zero-fills from the left
            Some(Value::Int(n)) if (0..=63).contains(&n) => {
                Ok(Value::Int(((*v as u64) >> n) as i64))
            }
            Some(Value::Int(_)) => Err("shift amount must be in 0..=63".into()),
            _ => Err("`ushr` needs an Int".into()),
        },
        // PRODUCTION-HARDENING (PR-it1205): this used to call `v.to_string()`
        // directly on the raw `f64`, using Rust's own `Display` (which
        // renders a whole-number float WITHOUT a trailing `.0`, e.g.
        // `5.0_f64.to_string() == "5"`) instead of `Value`'s own `Display`
        // (`impl fmt::Display for Value`, value.rs -- always shows `.0` for
        // a finite, fractionless `Float`, matching this language's own
        // documented float-formatting convention, and what string
        // interpolation / the free-function `to_str(x)` / the sibling
        // `F32.to_str()` arm just above all already correctly use).
        // Live-confirmed BEFORE this fix: `let x = 5.0` then `x.to_str()`
        // printed `"5"` while `"{x}"` and `to_str(x)` on the SAME value
        // both printed `"5.0"` -- an internal inconsistency WITHIN this
        // single reference engine (not merely a cross-engine one), since
        // `kupl native`'s own `k_to_str` always routes through the shared
        // `k_show` display routine and correctly printed `"5.0"` for all
        // three forms. `vm.rs` shares this exact function verbatim (`use
        // crate::interp::shared_method`), so it was affected identically.
        // Fixed by wrapping back into `Value` first, mirroring the `F32`
        // arm's own already-correct pattern exactly.
        (Value::Float(v), "to_str") => Ok(Value::str(Value::Float(*v).to_string())),
        (Value::Float(v), "fmt") => match args.into_iter().next() {
            Some(Value::Int(d)) => Ok(Value::str(format_float(*v, d))),
            _ => Err("`fmt` needs an Int number of decimals".into()),
        },
        (Value::Float(v), "to_int") => Ok(Value::Int(*v as i64)),
        (Value::Float(v), "abs") => Ok(Value::Float(v.abs())),
        (Value::Float(v), "sqrt") => Ok(Value::Float(v.sqrt())),
        (Value::Float(v), "floor") => Ok(Value::Float(v.floor())),
        (Value::Float(v), "ceil") => Ok(Value::Float(v.ceil())),
        (Value::Float(v), "round") => Ok(Value::Float(v.round())),
        // Completing the rounding family: trunc rounds toward zero, fract is the signed
        // fractional part (x - trunc(x)). NaN/inf follow IEEE (fract of an infinity is NaN).
        (Value::Float(v), "trunc") => Ok(Value::Float(v.trunc())),
        (Value::Float(v), "fract") => Ok(Value::Float(v.fract())),
        (Value::Float(v), "min") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.min(w))),
            _ => Err("`min` needs a Float".into()),
        },
        (Value::Float(v), "max") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.max(w))),
            _ => Err("`max` needs a Float".into()),
        },
        (Value::BigInt(b), "pow") => match args.into_iter().next() {
            Some(Value::Int(e)) if e >= 0 => b.pow(e as u64).map(|r| Value::BigInt(Rc::new(r))),
            Some(Value::Int(_)) => Err("`pow` exponent must be non-negative".into()),
            _ => Err("`pow` needs an Int exponent".into()),
        },
        (Value::BigInt(b), "abs") => Ok(Value::BigInt(Rc::new(b.abs()))),
        (Value::BigInt(b), "is_negative") => Ok(Value::Bool(b.is_negative())),
        (Value::BigInt(b), "sign") => Ok(Value::Int(b.sign())),
        (Value::Rational(r), "num") => Ok(Value::BigInt(Rc::new(r.num.clone()))),
        (Value::Rational(r), "den") => Ok(Value::BigInt(Rc::new(r.den.clone()))),
        (Value::Rational(r), "to_float") => Ok(Value::Float(r.to_f64())),
        (Value::Rational(r), "recip") => r
            .recip()
            .map(|x| Value::Rational(Rc::new(x)))
            .map_err(|_| "reciprocal of zero".to_string()),
        (Value::Float(v), "pow") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.powf(w))),
            _ => Err("`pow` needs a Float".into()),
        },
        (Value::Float(v), "log") => Ok(Value::Float(v.ln())),
        (Value::Float(v), "log10") => Ok(Value::Float(v.log10())),
        (Value::Float(v), "log2") => Ok(Value::Float(v.log2())),
        (Value::Float(v), "cbrt") => Ok(Value::Float(v.cbrt())),
        (Value::Float(v), "atan2") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.atan2(w))),
            _ => Err("`atan2` needs a Float".into()),
        },
        (Value::Float(v), "hypot") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.hypot(w))),
            _ => Err("`hypot` needs a Float".into()),
        },
        // Magnitude of the receiver with the sign of the argument (IEEE copysign): the sign
        // comes from the argument's sign BIT, so a -0.0 argument yields a negative result.
        (Value::Float(v), "copysign") => match args.into_iter().next() {
            Some(Value::Float(w)) => Ok(Value::Float(v.copysign(w))),
            _ => Err("`copysign` needs a Float".into()),
        },
        // Fused multiply-add: self * a + b with a SINGLE rounding (more accurate than a*b+c,
        // and can differ in the last bit). The native impl must use C fma() to match.
        (Value::Float(v), "mul_add") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Float(a)), Some(Value::Float(b))) => Ok(Value::Float(v.mul_add(a, b))),
                _ => Err("`mul_add` needs two Floats".into()),
            }
        }
        (Value::Float(v), "format") => match args.into_iter().next() {
            Some(Value::Int(d)) if (0..=100).contains(&d) => {
                Ok(Value::str(format!("{:.*}", d as usize, v)))
            }
            Some(Value::Int(_)) => Err("`format` decimals must be in 0..=100".into()),
            _ => Err("`format` needs an Int number of decimals".into()),
        },
        (Value::Float(v), "exp") => Ok(Value::Float(v.exp())),
        (Value::Float(v), "sin") => Ok(Value::Float(v.sin())),
        (Value::Float(v), "cos") => Ok(Value::Float(v.cos())),
        (Value::Float(v), "tan") => Ok(Value::Float(v.tan())),
        // Angle conversions completing the trig surface; the native impl must use the SAME
        // constants as Rust f64::to_degrees/to_radians to stay bit-identical.
        (Value::Float(v), "to_degrees") => Ok(Value::Float(v.to_degrees())),
        (Value::Float(v), "to_radians") => Ok(Value::Float(v.to_radians())),
        (Value::Float(v), "sign") => Ok(Value::Float(if *v > 0.0 {
            1.0
        } else if *v < 0.0 {
            -1.0
        } else {
            *v // preserves 0.0 / -0.0 / NaN
        })),
        (Value::Float(v), "is_nan") => Ok(Value::Bool(v.is_nan())),
        (Value::Float(v), "is_infinite") => Ok(Value::Bool(v.is_infinite())),
        (Value::Float(v), "clamp") => {
            let mut it = args.into_iter();
            match (it.next(), it.next()) {
                (Some(Value::Float(lo)), Some(Value::Float(hi))) => {
                    if lo > hi {
                        Err("`clamp`: lo must not exceed hi".into())
                    } else {
                        Ok(Value::Float(v.clamp(lo, hi)))
                    }
                }
                _ => Err("`clamp` needs two Float arguments".into()),
            }
        }
        (Value::Map(pairs), "insert") => {
            let mut it = args.into_iter();
            let (k, v) = (
                it.next().ok_or("`insert` needs a key")?,
                it.next().ok_or("`insert` needs a value")?,
            );
            let mut out = pairs.as_ref().clone();
            match out.iter_mut().find(|(pk, _)| value_key_eq(pk, &k)) {
                Some(pair) => pair.1 = v,
                None => out.push((k, v)),
            }
            Ok(Value::Map(Rc::new(out)))
        }
        (Value::Map(pairs), "get") => {
            let k = args.into_iter().next().ok_or("`get` needs a key")?;
            Ok(pairs
                .iter()
                .find(|(pk, _)| value_key_eq(pk, &k))
                .map(|(_, v)| Value::some(v.clone()))
                .unwrap_or_else(Value::none))
        }
        (Value::Map(pairs), "remove") => {
            let k = args.into_iter().next().ok_or("`remove` needs a key")?;
            Ok(Value::Map(Rc::new(
                pairs.iter().filter(|(pk, _)| !value_key_eq(pk, &k)).cloned().collect(),
            )))
        }
        (Value::Map(pairs), "contains_key") => {
            let k = args.into_iter().next().ok_or("`contains_key` needs a key")?;
            Ok(Value::Bool(pairs.iter().any(|(pk, _)| value_key_eq(pk, &k))))
        }
        (Value::Map(pairs), "keys") => Ok(Value::List(Rc::new(
            pairs.iter().map(|(k, _)| k.clone()).collect(),
        ))),
        (Value::Map(pairs), "values") => Ok(Value::List(Rc::new(
            pairs.iter().map(|(_, v)| v.clone()).collect(),
        ))),
        (Value::Map(pairs), "len") => Ok(Value::Int(pairs.len() as i64)),
        (Value::Map(pairs), "is_empty") => Ok(Value::Bool(pairs.is_empty())),
        (Value::Map(pairs), "get_or") => {
            let mut it = args.into_iter();
            let k = it.next().ok_or("`get_or` needs a key")?;
            let default = it.next().ok_or("`get_or` needs a default")?;
            Ok(pairs
                .iter()
                .find(|(pk, _)| value_key_eq(pk, &k))
                .map(|(_, v)| v.clone())
                .unwrap_or(default))
        }
        (Value::Map(pairs), "merge") => match args.into_iter().next() {
            Some(Value::Map(ref other)) => {
                let mut out = pairs.as_ref().clone();
                for (k, v) in other.iter() {
                    match out.iter_mut().find(|(pk, _)| value_key_eq(pk, k)) {
                        Some(pair) => pair.1 = v.clone(),
                        None => out.push((k.clone(), v.clone())),
                    }
                }
                Ok(Value::Map(Rc::new(out)))
            }
            _ => Err("`merge` needs a Map".into()),
        },
        (Value::Map(pairs), "map_values") => {
            let f = args.into_iter().next().ok_or("`map_values` needs a function")?;
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs.iter() {
                out.push((k.clone(), call(f.clone(), vec![v.clone()])?));
            }
            Ok(Value::Map(Rc::new(out)))
        }
        (Value::Map(pairs), "filter") => {
            let f = args.into_iter().next().ok_or("`filter` needs a function")?;
            let mut out = Vec::new();
            for (k, v) in pairs.iter() {
                if let Value::Bool(true) = call(f.clone(), vec![k.clone(), v.clone()])? {
                    out.push((k.clone(), v.clone()));
                }
            }
            Ok(Value::Map(Rc::new(out)))
        }
        (Value::Map(pairs), "fold") => {
            let mut it = args.into_iter();
            let mut acc = it.next().ok_or("`fold` needs an initial value")?;
            let f = it.next().ok_or("`fold` needs a function")?;
            for (k, v) in pairs.iter() {
                acc = call(f.clone(), vec![acc, k.clone(), v.clone()])?;
            }
            Ok(acc)
        }
        (Value::Set(items), "insert") => {
            let v = args.into_iter().next().ok_or("`insert` needs a value")?;
            if items.iter().any(|x| value_key_eq(x, &v)) {
                Ok(Value::Set(items.clone()))
            } else {
                let mut out = items.as_ref().clone();
                out.push(v);
                Ok(Value::Set(Rc::new(out)))
            }
        }
        (Value::Set(items), "remove") => {
            let v = args.into_iter().next().ok_or("`remove` needs a value")?;
            Ok(Value::Set(Rc::new(
                items.iter().filter(|x| !value_key_eq(x, &v)).cloned().collect(),
            )))
        }
        (Value::Set(items), "contains") => {
            let v = args.into_iter().next().ok_or("`contains` needs a value")?;
            Ok(Value::Bool(items.iter().any(|x| value_key_eq(x, &v))))
        }
        (Value::Set(items), "len") => Ok(Value::Int(items.len() as i64)),
        (Value::Set(items), "union") => match args.into_iter().next() {
            Some(Value::Set(ref other)) => {
                // A REAL, live-confirmed severe latency divergence found+fixed
                // (production-hardening PR-it828), a SIXTH instance of this
                // campaign's recurring "naive O(n^2) collection algorithm" bug
                // class (after Int.pow it814, List.sort it818, List.unique
                // it825, set_from_list it826, group_by it827): membership
                // testing (`out.iter().any(|y| value_key_eq(y, x))`) is a
                // LINEAR SCAN, run once per element of `other`, so unioning
                // two mostly-disjoint Sets is O(n*m) (live-confirmed: 0.66s/
                // 10.88s for two 2,000/8,000-element disjoint Sets, ~16.5x
                // time for 4x size). UNLIKE `.unique()`/`set_from_list`/
                // `group_by`, this fix does NOT need to reorder or restore
                // order of the OUTPUT at all: `union`'s existing contract
                // keeps `self`'s items in their original order, followed by
                // `other`'s new items in ITS original order -- neither array
                // is ever resorted in place. Only a SEPARATE, temporary
                // SORTED COPY of `self` is built (once, O(n log n)) purely
                // for FAST membership testing via binary search (`sort_order`
                // -equal implies `value_key_eq`-equal for every type in this
                // fast-path set, the same equivalence PR-it825/it826/it827
                // established, so a `sort_order`-based binary search
                // correctly answers `value_key_eq` membership) -- each of
                // `other`'s `m` items is then tested in O(log n) instead of
                // O(n), an O((n+m) log n) total. Checking the growing `out`
                // in the ORIGINAL naive code (rather than just `items`) was
                // never behaviorally significant: `other` is itself a Set
                // (no internal `value_key_eq` duplicates by construction),
                // so testing against `items` alone is equivalent. Type-gated
                // identically to PR-it825/it826/it827 (Int/Float/F32/Str/
                // SizedInt/BigInt, Rational excluded for the same cheap-`==`
                // -vs-expensive-`sort_order` asymmetry) -- falls back to the
                // ORIGINAL O(n*m) scan when either Set is empty (trivially
                // fast already) or holds an unsupported type.
                fn set_op_fast_eligible(v: &Value) -> bool {
                    matches!(
                        v,
                        Value::Int(_)
                            | Value::Float(_)
                            | Value::F32(_)
                            | Value::Str(_)
                            | Value::SizedInt(_)
                            | Value::BigInt(_)
                    )
                }
                if !items.is_empty() && !other.is_empty() && items.first().is_some_and(set_op_fast_eligible) {
                    let mut sorted_self: Vec<&Value> = items.iter().collect();
                    sorted_self.sort_by(|a, b| sort_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    let mut out = items.as_ref().clone();
                    for x in other.iter() {
                        let found = sorted_self
                            .binary_search_by(|probe| sort_order(probe, x).unwrap_or(std::cmp::Ordering::Equal))
                            .is_ok();
                        if !found {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Set(Rc::new(out)))
                } else {
                    let mut out = items.as_ref().clone();
                    for x in other.iter() {
                        if !out.iter().any(|y| value_key_eq(y, x)) {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Set(Rc::new(out)))
                }
            }
            _ => Err("`union` needs a Set".into()),
        },
        // `intersect`/`difference` (production-hardening PR-it829, the SEVENTH
        // and EIGHTH instances of this campaign's recurring "naive O(n^2)
        // collection algorithm" bug class -- follow-ups explicitly flagged by
        // PR-it828's `union` fix): membership testing was a LINEAR SCAN
        // (`other.iter().any(value_key_eq(...))`), run once per element of
        // `items`, so intersecting/differencing two mostly-overlapping-or-
        // disjoint Sets is O(n*m) (live-confirmed: 0.29s/4.42s for two
        // 2,000/8,000-element Sets, ~15-16x time for 4x size on both).
        // FAST PATH (identical technique to PR-it828's `union`, just testing
        // `items` against a sorted copy of `other` instead of the reverse):
        // neither array is ever resorted in the OUTPUT -- `intersect`/
        // `difference` both keep `self`'s items, filtered by membership in
        // `other`, in `self`'s original order -- so only a throwaway sorted
        // copy of `other` is built once (O(m log m)) for binary-search
        // membership testing (`sort_order`-equal implies `value_key_eq`-equal
        // for every fast-path type, the SAME equivalence PR-it825-828
        // established). Combined into ONE match arm (mirroring cgen.rs's own
        // existing combined `intersect`||`difference` C block) since they
        // differ only in whether "found" or "not found" is kept.
        (Value::Set(items), "intersect") | (Value::Set(items), "difference") => match args.into_iter().next() {
            Some(Value::Set(ref other)) => {
                let want_found = name == "intersect";
                fn set_op_fast_eligible(v: &Value) -> bool {
                    matches!(
                        v,
                        Value::Int(_)
                            | Value::Float(_)
                            | Value::F32(_)
                            | Value::Str(_)
                            | Value::SizedInt(_)
                            | Value::BigInt(_)
                    )
                }
                if !items.is_empty() && !other.is_empty() && items.first().is_some_and(set_op_fast_eligible) {
                    let mut sorted_other: Vec<&Value> = other.iter().collect();
                    sorted_other.sort_by(|a, b| sort_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    let out: Vec<Value> = items
                        .iter()
                        .filter(|x| {
                            let found = sorted_other
                                .binary_search_by(|probe| sort_order(probe, x).unwrap_or(std::cmp::Ordering::Equal))
                                .is_ok();
                            found == want_found
                        })
                        .cloned()
                        .collect();
                    Ok(Value::Set(Rc::new(out)))
                } else {
                    let out: Vec<Value> = items
                        .iter()
                        .filter(|x| other.iter().any(|y| value_key_eq(y, x)) == want_found)
                        .cloned()
                        .collect();
                    Ok(Value::Set(Rc::new(out)))
                }
            }
            _ => Err(format!("`{name}` needs a Set")),
        },
        (Value::Set(items), "symmetric_difference") => match args.into_iter().next() {
            Some(Value::Set(ref other)) => {
                // Follow-up to PR-it828/it829's `union`/`intersect`/`difference`
                // fixes (production-hardening PR-it829, the NINTH instance):
                // same O(n*m) shape, needing sorted copies of BOTH sides (one
                // per direction's membership test) since output order is (self
                // items not in other, self order) ++ (other items not in
                // self, other order) -- neither pass alone suffices, unlike
                // `union`'s single sorted copy of `self`.
                fn set_op_fast_eligible(v: &Value) -> bool {
                    matches!(
                        v,
                        Value::Int(_)
                            | Value::Float(_)
                            | Value::F32(_)
                            | Value::Str(_)
                            | Value::SizedInt(_)
                            | Value::BigInt(_)
                    )
                }
                if !items.is_empty() && !other.is_empty() && items.first().is_some_and(set_op_fast_eligible) {
                    let mut sorted_other: Vec<&Value> = other.iter().collect();
                    sorted_other.sort_by(|a, b| sort_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    let mut sorted_self: Vec<&Value> = items.iter().collect();
                    sorted_self.sort_by(|a, b| sort_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    let mut out: Vec<Value> = items
                        .iter()
                        .filter(|x| {
                            !sorted_other
                                .binary_search_by(|probe| sort_order(probe, x).unwrap_or(std::cmp::Ordering::Equal))
                                .is_ok()
                        })
                        .cloned()
                        .collect();
                    for x in other.iter() {
                        let found = sorted_self
                            .binary_search_by(|probe| sort_order(probe, x).unwrap_or(std::cmp::Ordering::Equal))
                            .is_ok();
                        if !found {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Set(Rc::new(out)))
                } else {
                    // (in self, not other) then (in other, not self) — deterministic order
                    let mut out: Vec<Value> =
                        items.iter().filter(|x| !other.iter().any(|y| value_key_eq(y, x))).cloned().collect();
                    for x in other.iter() {
                        if !items.iter().any(|y| value_key_eq(y, x)) {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Set(Rc::new(out)))
                }
            }
            _ => Err("`symmetric_difference` needs a Set".into()),
        },
        (Value::Set(items), "to_list") => Ok(Value::List(Rc::new(items.as_ref().clone()))),
        (Value::Set(items), "is_empty") => Ok(Value::Bool(items.is_empty())),
        // `is_subset`/`is_superset` (production-hardening PR-it829, the TENTH
        // and ELEVENTH instances -- found alongside `intersect`/`difference`/
        // `symmetric_difference` while auditing this SAME match block, not
        // originally flagged by PR-it828's NEXT-note): same O(n*m) membership-
        // scan shape (live-confirmed: 0.23s/3.61s for a 2,000/8,000-element
        // Set tested against itself, ~16x for 4x size). `all`/`any`'s
        // short-circuiting only helps the FALSE case (first miss found
        // early); the TRUE case (genuinely a subset/superset) still scans
        // every element. Combined into one arm; `want_subset` picks which
        // side is iterated vs. which side is sorted for lookup.
        (Value::Set(items), "is_subset") | (Value::Set(items), "is_superset") => match args.into_iter().next() {
            Some(Value::Set(ref other)) => {
                let want_subset = name == "is_subset";
                let (probe_side, lookup_side): (&[Value], &[Value]) =
                    if want_subset { (items, other) } else { (other, items) };
                fn set_op_fast_eligible(v: &Value) -> bool {
                    matches!(
                        v,
                        Value::Int(_)
                            | Value::Float(_)
                            | Value::F32(_)
                            | Value::Str(_)
                            | Value::SizedInt(_)
                            | Value::BigInt(_)
                    )
                }
                if !probe_side.is_empty() && !lookup_side.is_empty() && probe_side.first().is_some_and(set_op_fast_eligible) {
                    let mut sorted_lookup: Vec<&Value> = lookup_side.iter().collect();
                    sorted_lookup.sort_by(|a, b| sort_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    Ok(Value::Bool(probe_side.iter().all(|x| {
                        sorted_lookup
                            .binary_search_by(|probe| sort_order(probe, x).unwrap_or(std::cmp::Ordering::Equal))
                            .is_ok()
                    })))
                } else {
                    Ok(Value::Bool(
                        probe_side.iter().all(|x| lookup_side.iter().any(|y| value_key_eq(y, x))),
                    ))
                }
            }
            _ => Err(format!("`{name}` needs a Set")),
        },
        (Value::Tensor(d), "len") => Ok(Value::Int(d.len() as i64)),
        (Value::Tensor(d), "get") => match args.into_iter().next() {
            Some(Value::Int(i)) if i >= 0 && (i as usize) < d.len() => Ok(Value::Float(d[i as usize])),
            Some(Value::Int(i)) => {
                Err(format!("tensor index {i} out of range for length {}", d.len()))
            }
            _ => Err("`get` needs an Int index".into()),
        },
        // Accumulate from +0.0 (not Rust's `Iterator::sum`, whose f64 identity is
        // -0.0) so an empty tensor sums to +0.0 — matching the native runtime's
        // `double s = 0` byte-for-byte instead of printing "-0.0".
        (Value::Tensor(d), "sum") => Ok(Value::Float(d.iter().fold(0.0_f64, |a, b| a + b))),
        (Value::Tensor(d), "mean") => {
            if d.is_empty() {
                return Err("mean of an empty tensor".into());
            }
            // fold from +0.0 to match native's accumulator (a tensor summing to zero
            // yields +0.0, not Rust `Iterator::sum`'s -0.0 identity) — PR-it101/102.
            Ok(Value::Float(d.iter().fold(0.0_f64, |s, x| s + x) / d.len() as f64))
        }
        (Value::Tensor(d), "max") => d
            .iter()
            .cloned()
            .fold(None::<f64>, |m, x| Some(m.map_or(x, |m| m.max(x))))
            .map(Value::Float)
            .ok_or_else(|| "max of an empty tensor".to_string()),
        (Value::Tensor(d), "min") => d
            .iter()
            .cloned()
            .fold(None::<f64>, |m, x| Some(m.map_or(x, |m| m.min(x))))
            .map(Value::Float)
            .ok_or_else(|| "min of an empty tensor".to_string()),
        (Value::Tensor(a), "dot") => match args.into_iter().next() {
            Some(Value::Tensor(ref b)) => {
                if a.len() != b.len() {
                    return Err(format!("dot: length mismatch ({} vs {})", a.len(), b.len()));
                }
                // fold from +0.0 (not `Iterator::sum`, whose f64 identity is -0.0) so a
                // dot of two empty tensors is +0.0, matching the native runtime (PR-it101).
                Ok(Value::Float(a.iter().zip(b.iter()).map(|(x, y)| x * y).fold(0.0_f64, |s, p| s + p)))
            }
            _ => Err("`dot` needs a Tensor".into()),
        },
        (Value::Tensor(d), "scale") => match args.into_iter().next() {
            Some(Value::Float(k)) => Ok(Value::Tensor(Rc::new(d.iter().map(|x| x * k).collect()))),
            _ => Err("`scale` needs a Float".into()),
        },
        (Value::Tensor(d), "map") => {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let mut out = Vec::with_capacity(d.len());
            for x in d.iter() {
                match call(f.clone(), vec![Value::Float(*x)])? {
                    Value::Float(y) => out.push(y),
                    other => return Err(format!("tensor map must return Float, got {}", other.type_name())),
                }
            }
            Ok(Value::Tensor(Rc::new(out)))
        }
        (Value::Tensor(d), "to_list") => Ok(Value::List(Rc::new(
            d.iter().map(|x| Value::Float(*x)).collect(),
        ))),
        // A REAL, LIVE-CONFIRMED silent-wrong-value bug found+fixed (production-
        // hardening PR-it1053, found via a background close-read survey of this
        // whole function): these five arms -- UNLIKE every other Option/Result
        // combinator immediately below them -- were NOT variant-guarded, so they
        // matched ANY `Value::Ctor` unconditionally, silently intercepting a
        // user-defined UFCS function of the same name on a completely unrelated
        // ADT (e.g. a user's own `unwrap_or(shape: Shape, default: Float) ->
        // Float`) before it ever got a chance to run. `eval_method`/vm.rs's
        // `Op::Method` only fall back to a user's UFCS function when this whole
        // function returns an `Err` containing "has no method" -- these arms
        // always returned `Ok(...)`, so the user's real function was NEVER
        // called, silently replaced by nonsensical built-in behavior (`is_some`/
        // `is_none`/`is_ok`/`is_err` always `false` unless a user variant
        // happens to be literally named "Some"/"None"/"Ok"/"Err"; `unwrap_or`
        // always just returns its own `default` argument unchanged, discarding
        // the receiver entirely). `check.rs`'s own `infer_method` only assigns a
        // builtin signature to these names for `Ty::Option`/`Ty::Result`
        // (confirmed via a live `kupl check` pass) -- for any OTHER ADT it
        // legitimately resolves to a matching top-level UFCS function, so the
        // type checker itself believed the user's function would run. Live-
        // confirmed identically on ALL THREE engines (interp/vm share this exact
        // function; native's cgen.rs has the SAME unguarded shape in its own
        // independently-written C mirror, fixed alongside this): `type Shape =
        // Circle(r: Float) | Rect(w: Float, h: Float)` with a user `fun
        // unwrap_or(s: Shape, default: Float) -> Float { match s { Circle(r) =>
        // r, Rect(w, h) => w * h } }`, calling `Rect(2.0, 3.0).unwrap_or(99.0)`
        // printed `99.0` (the untouched default) instead of the user's own
        // correct `6.0` (`w * h`) on `kupl run`, `kupl run --vm`, AND `kupl
        // native` alike. Fixed by adding the SAME variant guard every sibling
        // arm below already uses.
        (Value::Ctor { variant, .. }, "is_some")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            Ok(Value::Bool(variant.as_str() == "Some"))
        }
        (Value::Ctor { variant, .. }, "is_none")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            Ok(Value::Bool(variant.as_str() == "None"))
        }
        (Value::Ctor { variant, .. }, "is_ok")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            Ok(Value::Bool(variant.as_str() == "Ok"))
        }
        (Value::Ctor { variant, .. }, "is_err")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            Ok(Value::Bool(variant.as_str() == "Err"))
        }
        (Value::Ctor { variant, fields, .. }, "unwrap_or")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            let default = args.into_iter().next().ok_or("`unwrap_or` needs a default")?;
            match variant.as_str() {
                "Some" | "Ok" => Ok(fields.first().cloned().unwrap_or(Value::Unit)),
                _ => Ok(default),
            }
        }
        // ---- Option / Result combinators (variant-guarded so user ADTs with a
        // like-named method still fall through to the UFCS fallback) ----
        (Value::Ctor { variant, fields, .. }, "map")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            let f = args.into_iter().next().ok_or("`map` needs a function")?;
            let x = || fields.first().cloned().unwrap_or(Value::Unit);
            match variant.as_str() {
                "Some" => Ok(Value::some(call(f, vec![x()])?)),
                "Ok" => Ok(Value::ok(call(f, vec![x()])?)),
                _ => Ok(recv.clone()), // None / Err pass through
            }
        }
        (Value::Ctor { variant, fields, .. }, "and_then")
            if matches!(variant.as_str(), "Some" | "None" | "Ok" | "Err") =>
        {
            let f = args.into_iter().next().ok_or("`and_then` needs a function")?;
            match variant.as_str() {
                "Some" | "Ok" => call(f, vec![fields.first().cloned().unwrap_or(Value::Unit)]),
                _ => Ok(recv.clone()),
            }
        }
        (Value::Ctor { variant, fields, .. }, "filter")
            if matches!(variant.as_str(), "Some" | "None") =>
        {
            let f = args.into_iter().next().ok_or("`filter` needs a function")?;
            match variant.as_str() {
                "Some" => {
                    let x = fields.first().cloned().unwrap_or(Value::Unit);
                    if let Value::Bool(true) = call(f, vec![x.clone()])? {
                        Ok(Value::some(x))
                    } else {
                        Ok(Value::none())
                    }
                }
                _ => Ok(Value::none()),
            }
        }
        (Value::Ctor { variant, fields, .. }, "ok_or")
            if matches!(variant.as_str(), "Some" | "None") =>
        {
            let err = args.into_iter().next().ok_or("`ok_or` needs an error value")?;
            match variant.as_str() {
                "Some" => Ok(Value::ok(fields.first().cloned().unwrap_or(Value::Unit))),
                _ => Ok(Value::err(err)),
            }
        }
        (Value::Ctor { variant, fields, .. }, "map_err")
            if matches!(variant.as_str(), "Ok" | "Err") =>
        {
            let f = args.into_iter().next().ok_or("`map_err` needs a function")?;
            match variant.as_str() {
                "Err" => Ok(Value::err(call(f, vec![fields.first().cloned().unwrap_or(Value::Unit)])?)),
                _ => Ok(recv.clone()),
            }
        }
        (Value::Ctor { variant, fields, .. }, "ok")
            if matches!(variant.as_str(), "Ok" | "Err") =>
        {
            match variant.as_str() {
                "Ok" => Ok(Value::some(fields.first().cloned().unwrap_or(Value::Unit))),
                _ => Ok(Value::none()),
            }
        }
        (other, _) => Err(format!("{} has no method `{name}`", other.type_name())),
    }
}

/// Format an i64 in a given base (2..=36) — lowercase digits, a leading `-`
/// on the magnitude for negatives. Shared with the cgen C mirror.
fn int_to_radix(v: i64, base: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = v.unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % base as u64) as usize]);
        n /= base as u64;
    }
    if v < 0 {
        buf.push(b'-');
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Integer square root (floor) of a non-negative i64.
fn int_isqrt(v: i64) -> i64 {
    let n = v as u64;
    if n == 0 {
        return 0;
    }
    let mut x = (n as f64).sqrt() as u64;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x as i64
}

/// Ordering for `List.min`/`max`/`min_by`/`max_by` — Int, Float, or Str
/// elements only. Float/F32 NaN comparisons are "Equal" (never wins against
/// a real value, matching native's `k_cmp`-based fold, PR-it148/it150 --
/// deliberately UNCHANGED by PR-it711 below, which gives `.sort()` its own,
/// stricter comparator instead of touching this one, to avoid breaking this
/// established min/max/min_by/max_by behavior).
fn list_order(a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => Ok(x.partial_cmp(y).unwrap_or(Ordering::Equal)),
        (Value::Str(x), Value::Str(y)) => Ok(x.cmp(y)),
        // A REAL, LIVE-CONFIRMED cross-engine DIVERGENCE found+fixed
        // (it107, discovered while wiring `Decimal` into this exact
        // function): `check.rs`'s K0234 was widened for `Char` at it105,
        // but this hand-written match was never updated to match --
        // `['c','a','b'].sort()` type-checked fine and then panicked
        // "min/max need Int, Float, Str, or another orderable type" on
        // BOTH interp AND the KVM (which shares this function via
        // `crate::interp::shared_method`), while native's `k_list_order`
        // already handled it correctly (it falls through to the generic,
        // type-agnostic `k_cmp` for any non-float tag, so it never needed
        // a dedicated Char arm the way this hand-enumerated match does) --
        // confirmed live via all three `kupl run`/`kupl run --vm`/`kupl
        // native` before this fix, matching the exact shape of PR-it549's
        // own BigInt-keyed divergence just below.
        (Value::Char(x), Value::Char(y)) => Ok(x.cmp(y)),
        // A REAL cross-engine DIVERGENCE found+fixed, PR-it549: min_by/max_by's key type
        // isn't restricted by the checker (any type unifies), and native's comparator
        // (k_cmp, shared with `<`/`<=`/etc) already handled these — so a BigInt-keyed
        // min_by already worked on native while interp/vm panicked on the identical
        // program. Bringing list_order up to k_cmp's coverage closes the divergence AND
        // (via `.min()`/`.max()`, which also call this) extends direct min/max the same way.
        (Value::SizedInt(x), Value::SizedInt(y)) if x.1 == y.1 => Ok(x.0.cmp(&y.0)),
        (Value::F32(x), Value::F32(y)) => Ok(x.partial_cmp(y).unwrap_or(Ordering::Equal)),
        (Value::BigInt(x), Value::BigInt(y)) => Ok(x.cmp(y)),
        (Value::Rational(x), Value::Rational(y)) => {
            // Same PR-it718 pre-check as raw_binary_op's Lt/Le/Gt/Ge arms --
            // this function backs `.min()`/`.max()`/`.min_by()`/`.max_by()`
            // (and, via sort_order's fallthrough, `.sort()`), an entirely
            // separate reachable path to the SAME uncapped Rational::cmp.
            if x.cmp_would_be_too_expensive(y) {
                return Err(format!(
                    "Rational comparison would require a BigInt multiplication too large to compute (limit ~{} limbs, roughly {} decimal digits)",
                    crate::bigint::MAX_BIGINT_LIMBS,
                    crate::bigint::MAX_BIGINT_LIMBS * 9
                ));
            }
            Ok(x.cmp(y))
        }
        (Value::Decimal(x), Value::Decimal(y)) => crate::decimal::Decimal::cmp(x, y),
        _ => Err("`min`/`max` need Int, Float, Str, Char, or another orderable type".into()),
    }
}

/// A total, TRANSITIVE order used ONLY by `List.sort()` (never by
/// `min`/`max`/`min_by`/`max_by`, which keep `list_order`'s established
/// "NaN never wins" fold behavior above, PR-it148/it150, UNCHANGED): real
/// values compare normally, NaN sorts as the greatest value (NaN == NaN,
/// NaN > everything else). Production-hardening PR-it711: `list_order`'s
/// `partial_cmp().unwrap_or(Ordering::Equal)` treats EVERY NaN comparison as
/// "equal" -- not just to other NaNs, but to every real value too, which is
/// NOT transitive (`NaN == 5.0` and `NaN == 3.0` would imply `5.0 == 3.0`,
/// false). A single linear fold (`min`/`max`/`min_by`/`max_by`) never
/// noticed, but `.sort()` -- built on Rust's `slice::sort_by`, which relies
/// on its comparator being a genuine total order to run its optimized
/// algorithm -- hit an internal Rust standard-library panic ("internal
/// compiler error [.../smallsort.rs:...]") on a NaN-containing list of
/// non-trivial size, crashing the WHOLE interpreter process on ordinary user
/// code (sorting a float list that happens to contain NaN, e.g. from missing/
/// invalid data) -- confirmed live with an 81-element NaN-containing list
/// before this fix. `sort_order` is otherwise IDENTICAL to `list_order`
/// (same type coverage), differing only in the Float/F32 arms.
fn sort_order(a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
    use std::cmp::Ordering;
    fn float_order(x: f64, y: f64) -> Ordering {
        match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => x.partial_cmp(&y).expect("neither operand is NaN"),
        }
    }
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => Ok(float_order(*x, *y)),
        (Value::F32(x), Value::F32(y)) => Ok(float_order(*x as f64, *y as f64)),
        _ => list_order(a, b),
    }
}

/// Build a Set from a List, dropping duplicates (shared by all engines).
pub fn set_from_list(v: &Value) -> Result<Value, String> {
    match v {
        Value::List(items) => {
            // A REAL, live-confirmed severe latency divergence found+fixed
            // (production-hardening PR-it826), the Set(list)-conversion
            // analogue of PR-it825's `List.unique()` fix, with an even
            // WORSE observed constant factor (`value_key_eq`'s structural
            // comparison is more expensive per-call than `==`): the naive
            // O(n^2) fallback below took 2.09s/29.30s to convert an
            // 8,000/32,000-element `List[Int]` to a `Set`. FAST PATH:
            // sort-then-adjacent-dedup is O(n log n), reusing the SAME
            // `sort_order`/`KSortListItem`/`k_sort_cmp` machinery PR-it825
            // already established, deliberately type-gated identically
            // (Int/Float/F32/Str/SizedInt/BigInt only, Rational excluded
            // for the SAME cheap-`==`-vs-expensive-`sort_order` asymmetry
            // reason -- `Set(list)` has NO element-type restriction
            // either, so this falls back to the ORIGINAL O(n^2) scan for
            // every other type). UNLIKE PR-it825, the adjacent-duplicate
            // check here uses `value_key_eq`, NOT `==`: Set element
            // identity is intentionally NaN-COLLAPSING (PR-it691,
            // `Set([nan, nan, 1.0]).len() == 2`), the OPPOSITE of
            // `.unique()`'s IEEE-`==`-based, NaN-PRESERVING identity --
            // and `sort_order`'s own NaN-clustering (PR-it711, all NaNs
            // sort adjacent) happens to AGREE with `value_key_eq`'s NaN-
            // collapsing here, unlike PR-it825's case, so no special
            // handling beyond the equality-predicate swap is needed.
            fn set_fast_eligible(v: &Value) -> bool {
                matches!(
                    v,
                    Value::Int(_)
                        | Value::Float(_)
                        | Value::F32(_)
                        | Value::Str(_)
                        | Value::SizedInt(_)
                        | Value::BigInt(_)
                )
            }
            if items.len() > 1 && items.first().is_some_and(set_fast_eligible) {
                let mut indexed: Vec<(usize, &Value)> = items.iter().enumerate().collect();
                indexed.sort_by(|a, b| sort_order(a.1, b.1).unwrap_or(std::cmp::Ordering::Equal));
                let mut kept: Vec<(usize, &Value)> = Vec::with_capacity(indexed.len());
                for pair in indexed {
                    if kept.last().is_none_or(|last: &(usize, &Value)| !value_key_eq(last.1, pair.1)) {
                        kept.push(pair);
                    }
                }
                kept.sort_by_key(|(idx, _)| *idx);
                Ok(Value::Set(Rc::new(kept.into_iter().map(|(_, v)| v.clone()).collect())))
            } else {
                let mut out: Vec<Value> = Vec::new();
                for it in items.iter() {
                    if !out.iter().any(|x| value_key_eq(x, it)) {
                        out.push(it.clone());
                    }
                }
                Ok(Value::Set(Rc::new(out)))
            }
        }
        other => Err(format!("Set(...) needs a List, found {}", other.type_name())),
    }
}

/// Deterministic PRNG (xorshift64*) behind the seeded-random builtins. The
/// exact algorithm — state init, `next`, the `>> 11` float mapping, and the
/// Fisher-Yates order — is mirrored byte-for-byte in cgen.rs so `random_*` and
/// `shuffle` give identical results on the interpreter, KVM, and native.
struct SeedRng(u64);

impl SeedRng {
    fn new(seed: i64) -> Self {
        // xorshift needs a non-zero state
        SeedRng(if seed as u64 == 0 { 1 } else { seed as u64 })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Seeded random builtins — shared by interpreter and KVM. Pure: a given seed
/// always yields the same output, so results are reproducible.
pub fn random_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_int = |v: &Value| match v {
        Value::Int(n) => *n,
        _ => 0,
    };
    match name {
        "random_ints" => {
            let mut r = SeedRng::new(as_int(&args[0]));
            let n = as_int(&args[1]).max(0);
            if n > 100_000_000 {
                return Err("random count too large".into());
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                out.push(Value::Int(r.next_u64() as i64));
            }
            Ok(Value::List(Rc::new(out)))
        }
        "random_floats" => {
            let mut r = SeedRng::new(as_int(&args[0]));
            let n = as_int(&args[1]).max(0);
            if n > 100_000_000 {
                return Err("random count too large".into());
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                // top 53 bits → a double in [0, 1)
                out.push(Value::Float(
                    (r.next_u64() >> 11) as f64 * (1.0 / 9007199254740992.0),
                ));
            }
            Ok(Value::List(Rc::new(out)))
        }
        "shuffle" => {
            let list = match &args[1] {
                Value::List(xs) => xs,
                other => return Err(format!("`shuffle` needs a List, found {}", other.type_name())),
            };
            let mut out = list.as_ref().clone();
            let mut r = SeedRng::new(as_int(&args[0]));
            // Fisher-Yates from the end: swap i with a random j in 0..=i
            let mut i = out.len();
            while i > 1 {
                i -= 1;
                let j = (r.next_u64() % (i as u64 + 1)) as usize;
                out.swap(i, j);
            }
            Ok(Value::List(Rc::new(out)))
        }
        _ => Err(format!("unknown random builtin `{name}`")),
    }
}

/// The program's own command-line arguments. When KUPL is run through the
/// toolchain (`kupl run prog.kupl -- a b c`), the program's args are everything
/// after `--`; with no `--`, there are none. (The native backend reads argv
/// directly.)
pub fn program_args() -> Vec<String> {
    // `std::env::args()` PANICS on any argument that isn't valid Unicode (a raw,
    // non-UTF8 argv element is rare but real — e.g. a filename-derived argument
    // passed through by another tool) — contradicting the "no panics on any
    // input" goal with a bare Rust panic reported as a bogus "internal compiler
    // error". `args_os()` never panics; an unrepresentable argument is replaced
    // WHOLESALE with a placeholder rather than embedded lossily byte-by-byte, so
    // native (which can't cheaply replicate Rust's per-invalid-run lossy
    // algorithm) can match this exactly with a single whole-value check (PR-it578).
    let all: Vec<String> = std::env::args_os()
        .map(|a| a.to_str().map(str::to_string).unwrap_or_else(|| "\u{FFFD}".to_string()))
        .collect();
    match all.iter().position(|a| a == "--") {
        Some(i) => all[i + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// Environment & process builtins that return a value — shared by interpreter
/// and KVM. `env_var`/`args` carry the `io.env` effect; `eprint` carries `io`.
/// (`exit` diverges and is handled inline, like `panic`.)
pub fn proc_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "env_var" => {
            let key = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            };
            Ok(match std::env::var(&key) {
                Ok(v) => Value::some(Value::str(v)),
                Err(_) => Value::none(),
            })
        }
        "args" => Ok(Value::List(Rc::new(
            program_args().into_iter().map(Value::str).collect(),
        ))),
        "read_line" => {
            use std::io::BufRead;
            // Read raw bytes so a NUL or invalid UTF-8 is rejected rather than
            // embedded (interp) or truncated (native) — a KUPL Str is NUL-free UTF-8.
            let mut buf: Vec<u8> = Vec::new();
            let n = std::io::stdin().lock().read_until(b'\n', &mut buf).unwrap_or(0);
            if n == 0 {
                Ok(Value::none()) // EOF
            } else {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                if buf.contains(&0) {
                    return Err("read_line: stdin line contains a NUL byte".into());
                }
                match String::from_utf8(buf) {
                    Ok(s) => Ok(Value::some(Value::str(s))),
                    Err(_) => Err("read_line: stdin line is not valid UTF-8".into()),
                }
            }
        }
        "read_all" => {
            use std::io::Read;
            let mut buf: Vec<u8> = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut buf);
            if buf.contains(&0) {
                return Err("read_all: stdin contains a NUL byte".into());
            }
            match String::from_utf8(buf) {
                Ok(s) => Ok(Value::str(s)),
                Err(_) => Err("read_all: stdin is not valid UTF-8".into()),
            }
        }
        "eprint" => {
            eprintln!("{}", args[0]);
            Ok(Value::Unit)
        }
        _ => Err(format!("unknown process builtin `{name}`")),
    }
}

/// Structured logging — shared by interpreter and KVM. Deliberately minimal
/// (matching this stdlib's own established style): one formatted line to
/// stderr per call (UTC ISO-8601 timestamp + level + the argument's own
/// `Display` form, exactly like `eprint`'s own permissiveness -- any value,
/// not just `Str`), no level filtering, no structured key-value fields, no
/// configurable destination. Carries the `io` effect (a subset of the same
/// capability `eprint` already carries, not a new sub-effect -- see
/// `effects.rs`). Mirrored in `cgen.rs` via `k_date_iso(k_now())` + `k_show`,
/// the same two primitives this function's own `crate::time::iso`/
/// `now_seconds` calls are built from, so the two are byte-identical by
/// construction, not by coincidence.
pub fn log_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let level = match name {
        "log_debug" => "DEBUG",
        "log_info" => "INFO",
        "log_warn" => "WARN",
        "log_error" => "ERROR",
        _ => return Err(format!("unknown log builtin `{name}`")),
    };
    eprintln!("{} [{}] {}", crate::time::iso(now_seconds()), level, args[0]);
    Ok(Value::Unit)
}

/// URL & query-string builtins — shared by interpreter and KVM. Pure.
pub fn url_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| match v {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    use crate::url as u;
    Ok(match name {
        "url_encode" => Value::str(u::url_encode(&as_str(&args[0]))),
        "url_decode" => match u::url_decode(&as_str(&args[0])) {
            Ok(v) => Value::ok(Value::str(v)),
            Err(e) => Value::err(Value::str(e)),
        },
        "query_parse" => {
            let pairs = u::query_parse(&as_str(&args[0]));
            Value::List(Rc::new(
                pairs
                    .into_iter()
                    .map(|p| Value::List(Rc::new(p.into_iter().map(Value::str).collect())))
                    .collect(),
            ))
        }
        "query_build" => {
            let rows = match &args[0] {
                Value::List(rows) => rows,
                other => return Err(format!("`query_build` needs a List, found {}", other.type_name())),
            };
            let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len());
            for row in rows.iter() {
                let fields = match row {
                    Value::List(fs) => fs,
                    other => return Err(format!("`query_build` pairs must be Lists, found {}", other.type_name())),
                };
                grid.push(fields.iter().map(|f| as_str(f)).collect());
            }
            Value::str(u::query_build(&grid))
        }
        _ => return Err(format!("unknown url builtin `{name}`")),
    })
}

/// CSV builtins — shared by interpreter and KVM. Pure.
pub fn csv_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "csv_parse" => {
            let text = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            };
            let rows = crate::csv::parse(&text);
            let out: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    Value::List(Rc::new(row.into_iter().map(Value::str).collect()))
                })
                .collect();
            Ok(Value::List(Rc::new(out)))
        }
        "csv_stringify" => {
            let rows = match &args[0] {
                Value::List(rows) => rows,
                other => return Err(format!("`csv_stringify` needs a List, found {}", other.type_name())),
            };
            let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len());
            for row in rows.iter() {
                let fields = match row {
                    Value::List(fs) => fs,
                    other => return Err(format!("`csv_stringify` rows must be Lists, found {}", other.type_name())),
                };
                // A REAL, LIVE-CONFIRMED silent-data-loss bug found+fixed
                // (production-hardening PR-it963, survey #112's close-read
                // of csv.rs, independently re-verified live with a fresh
                // repro before implementing): `csv::stringify`'s per-row
                // loop iterates over EACH row's own fields to render them,
                // and for a ZERO-FIELD row the loop body never runs at
                // all, silently emitting NOTHING -- byte-for-byte
                // indistinguishable from "no row," the exact same "empty
                // content collapses to nothing on round-trip" bug SHAPE
                // PR-it883 already fixed for a row with exactly ONE empty
                // field (force-quoted to `""` so it survives), but that
                // fix has no field to force-quote when there are ZERO
                // fields to begin with. `csv_parse` itself never PRODUCES
                // a zero-field row (every row it emits has >= 1 field,
                // even a blank line), so this is unreachable from a
                // genuine parse round-trip -- but `csv_stringify` accepts
                // arbitrary caller-constructed `List[List[Str]]` with no
                // validation, e.g. from filtering all columns off a row.
                // Live-confirmed BEFORE this fix: `csv_stringify([["x",
                // "y"], []])` (2 rows) produced `"x,y\n"` (1 line), and
                // `csv_parse` of that back produced only 1 row -- silent
                // row loss, byte-identical (same wrong result) on
                // interp/vm/native, with zero diagnostic of any kind.
                // CSV's own grammar cannot represent "zero fields" as
                // distinct from "no row" at all (unlike a single empty
                // field, which the quoting-based it883 fix can encode) --
                // so rather than silently losing data, reject it with a
                // clean error the same way an already-invalid row shape
                // (a non-List row, the arm just above) is rejected.
                if fields.is_empty() {
                    return Err(
                        "`csv_stringify` cannot represent a row with zero fields -- CSV has no \
                         way to distinguish this from no row at all"
                            .to_string(),
                    );
                }
                grid.push(fields.iter().map(|f| match f {
                    Value::Str(s) => s.as_str().to_string(),
                    other => other.to_string(),
                }).collect());
            }
            Ok(Value::str(crate::csv::stringify(&grid)))
        }
        _ => Err(format!("unknown csv builtin `{name}`")),
    }
}

/// Encoding & hash builtins — shared by interpreter and KVM. All pure.
/// `*_decode` returns a `Result` value; encode/hash always succeed.
pub fn encoding_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let s = match &args[0] {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    use crate::encoding as enc;
    Ok(match name {
        "base64_encode" => Value::str(enc::base64_encode(&s)),
        "hex_encode" => Value::str(enc::hex_encode(&s)),
        "hash_fnv" => Value::Int(enc::hash_fnv(&s)),
        "sha256" => Value::str(enc::sha256_hex(&s)),
        "hmac_sha256" => {
            let msg = match &args[1] {
                Value::Str(m) => m.as_str().to_string(),
                other => other.to_string(),
            };
            Value::str(enc::hmac_sha256_hex(&s, &msg))
        }
        "base64_decode" => match enc::base64_decode(&s) {
            Ok(v) => Value::ok(Value::str(v)),
            Err(e) => Value::err(Value::str(e)),
        },
        "hex_decode" => match enc::hex_decode(&s) {
            Ok(v) => Value::ok(Value::str(v)),
            Err(e) => Value::err(Value::str(e)),
        },
        _ => return Err(format!("unknown encoding builtin `{name}`")),
    })
}

/// Time/date builtins — shared by interpreter and KVM. All PURE (a timestamp
/// in, a string or Int out); `now` is separate (wall clock, `io.time`).
pub fn time_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let t = match &args[0] {
        Value::Int(n) => *n,
        _ => 0,
    };
    use crate::time as tm;
    Ok(match name {
        "format_time" => Value::str(tm::format_time(t)),
        "year_of" => Value::Int(tm::year_of(t)),
        "month_of" => Value::Int(tm::month_of(t)),
        "day_of" => Value::Int(tm::day_of(t)),
        "hour_of" => Value::Int(tm::hour_of(t)),
        "minute_of" => Value::Int(tm::minute_of(t)),
        "second_of" => Value::Int(tm::second_of(t)),
        "weekday_of" => Value::Int(tm::weekday_of(t)),
        "yearday_of" => Value::Int(tm::yearday_of(t)),
        "date_iso" => Value::str(tm::iso(t)),
        "parse_iso" => {
            let s = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            };
            match tm::parse_iso(&s) {
                Ok(e) => Value::ok(Value::Int(e)),
                Err(m) => Value::err(Value::str(m)),
            }
        }
        "date_make" => {
            let n = |i: usize| match args.get(i) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // `date_make` is declared `(Int, Int, Int, Int, Int, Int) -> Int` (no
            // `Result` in its own type signature — check.rs), so an unrepresentable
            // component (PR-it635) surfaces as a panic here, the same way
            // `json_stringify`'s non-finite-number rejection does (PR-it634).
            return tm::make(n(0), n(1), n(2), n(3), n(4), n(5)).map(Value::Int);
        }
        _ => return Err(format!("unknown time builtin `{name}`")),
    })
}

/// Current Unix epoch seconds (wall clock). Effect `io.time`.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Regex builtins — shared by interpreter and KVM. Pure; a malformed pattern
/// panics with a clear message (the pattern is program text, so this is a bug
/// to surface, like a bad format string).
pub fn regex_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| match v {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    let re = crate::regex::compile(&as_str(&args[0]))
        .map_err(|e| format!("invalid regex: {e}"))?;
    let text = as_str(&args[1]);
    let result = match name {
        "re_match" => Value::Bool(re.is_match(&text)),
        "re_find" => re
            .find(&text)
            .map(|m| Value::some(Value::str(m)))
            .unwrap_or_else(Value::none),
        "re_find_all" => Value::List(Rc::new(
            re.find_all(&text).into_iter().map(Value::str).collect(),
        )),
        "re_replace" => Value::str(re.replace_all(&text, &as_str(&args[2]))),
        _ => return Err(format!("unknown regex builtin `{name}`")),
    };
    // A pathological pattern/input that blew the backtracking budget yields a clean
    // error rather than a silently-wrong result (or a hang).
    if crate::regex::budget_exceeded() {
        return Err("regex match budget exceeded (pattern too complex for the input)".into());
    }
    Ok(result)
}

/// A REAL, live-confirmed resource-exhaustion gap found+fixed (production-
/// hardening PR-it751): `http_builtin`'s `curl` invocation had no response-
/// size limit at all -- `run_curl`'s `child.wait_with_output()` buffers the
/// ENTIRE response body into memory before this module gets a chance to
/// look at it, so `http_get`/`http_post` against a URL that happens to
/// return an enormous body (an attacker-controlled or simply misbehaving
/// server -- the KUPL program author writes the URL, but not what the
/// remote host chooses to send back) could exhaust the process's memory.
/// Confirmed live BEFORE this fix: a local test server serving a 10MB file
/// downloaded in full with the pre-fix flag set (no cap at all); mirrors
/// this same file's own existing `MAX_BODY_SIZE` precedent (10MB, chosen
/// for the SERVER-side inbound request body cap, just above) for the
/// OUTBOUND response side.
const MAX_HTTP_RESPONSE_SIZE: u64 = 10 * 1024 * 1024;

/// Build (but don't spawn) the `curl` invocation's shared base flags, split
/// out purely so a unit test can introspect the exact args via
/// `Command::get_args()` without spawning a real `curl` subprocess -- this
/// codebase's http-builtin tests deliberately never invoke real `curl`
/// (unlike `serve_http`'s tests, which exercise the SERVER side via raw
/// `TcpStream`s and need no external process at all), so a network-
/// dependent test here would be the first of its kind. Testing the args a
/// real invocation WOULD use still catches the actual regression this fix
/// guards against (the `--max-filesize` flag being silently dropped in a
/// future edit). `--fail` makes curl return a non-zero status (and thus an
/// `Err`) on HTTP 4xx/5xx; `-sS` silences the progress meter but keeps
/// error messages; `--max-filesize` aborts an oversized transfer (curl
/// exit 63, handled by the SAME existing non-2xx `Err` branch in
/// `run_curl` -- no new panic surface) rather than buffering an unbounded
/// response into memory (production-hardening PR-it751).
fn base_curl_cmd() -> std::process::Command {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "--fail", "--max-time", "30"]);
    cmd.args(["--max-filesize", &MAX_HTTP_RESPONSE_SIZE.to_string()]);
    cmd
}

/// HTTP builtins — shared by interpreter and KVM. Effect `io.net`. Transport is
/// the system `curl` (the same zero-dependency approach the AI runtime uses).
/// Returns a `Result` value: `Ok(body)` on a successful request, `Err(message)`
/// otherwise (unreachable host, non-2xx, curl missing, response too large, …).
/// The `Err` text is a human-readable description and may vary by platform —
/// match `Ok`/`Err`.
pub fn http_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| match v {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    let url = as_str(&args[0]);
    let mut cmd = base_curl_cmd();
    let result = match name {
        "http_get" => {
            cmd.arg(&url);
            run_curl(cmd, None)
        }
        "http_post" => {
            let body = as_str(&args[1]);
            cmd.args(["-X", "POST", "--data-binary", "@-", &url]);
            run_curl(cmd, Some(body))
        }
        _ => return Err(format!("unknown http builtin `{name}`")),
    };
    Ok(match result {
        Ok(body) => Value::ok(Value::str(body)),
        Err(msg) => Value::err(Value::str(msg)),
    })
}

/// Extract the HOST portion of a URL (`scheme://host[:port][/path]`) --
/// shared by `http_get_with`'s own host-scope check and MUST stay
/// byte-identical to its native mirror (`cgen.rs`'s `k_url_host`).
/// Deliberately simple string slicing, not a full URL parser: strips a
/// leading `scheme://` if present, then stops at the first `/`, `?`, `#`,
/// or `:` (port).
pub fn url_host(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let end = after_scheme.find(['/', '?', '#', ':']).unwrap_or(after_scheme.len());
    &after_scheme[..end]
}

/// `http_get_with(cap, url)` (it116) -- like `http_get(url)` but checks
/// `url`'s host against `cap`'s own carried scope FIRST (CAPABILITIES.md
/// §3.4 Option B: additive, `uses io.net` stays unchanged, capabilities
/// narrow what an already-granted effect can reach, they don't replace
/// the effect declaration). An unrestricted (root) capability allows any
/// host, exactly like `http_get` always has. An out-of-scope host is an
/// ordinary `Err` value (matching `http_get`'s own "unreachable host,
/// non-2xx, ... " Result-of-failure style), not a panic.
pub fn http_get_with(cap: &Value, url: &str) -> Result<Value, String> {
    let Value::CapNet(c) = cap else {
        return Err("http_get_with needs a CapNet".into());
    };
    if let Some(allowed) = &c.allowed_host {
        let host = url_host(url);
        if host != allowed {
            return Ok(Value::err(Value::str(format!(
                "capability limited to `{allowed}`, cannot reach `{host}`"
            ))));
        }
    }
    http_builtin("http_get", std::slice::from_ref(&Value::str(url.to_string())))
}

/// `cap_net_root()` (it116) -- the UNRESTRICTED root `CapNet` capability.
///
/// Call-site-restricted since it117: `check.rs`'s
/// `check_capability_root_call_site` rejects (`K0304`) any call to this
/// builtin outside the top-level `fun main`'s own top-level body, so a
/// program reaching this function at RUNTIME is already guaranteed to
/// have come from that one, audited call site -- see
/// `docs/design/CAPABILITIES.md` §3.2.
pub fn cap_net_root() -> Value {
    Value::CapNet(Rc::new(crate::value::CapNetInner { allowed_host: None }))
}

/// `read_file_with(cap, path)` (it118) -- like `read_file(path)` but checks
/// `path` against `cap`'s own carried scope FIRST, mirroring
/// `http_get_with` exactly. An unrestricted (root) capability allows any
/// path; an out-of-scope path is an ordinary `Err` value, not a panic.
/// The prefix check is a plain `str::starts_with`, NOT canonicalized (no
/// `..`-traversal defense) -- the same deliberate-simplicity precedent
/// `http_get_with`'s own `url_host` helper already established.
pub fn read_file_with(cap: &Value, path: &str) -> Result<Value, String> {
    let Value::CapFs(c) = cap else {
        return Err("read_file_with needs a CapFs".into());
    };
    if let Some(allowed) = &c.allowed_prefix {
        if !path.starts_with(allowed.as_str()) {
            return Ok(Value::err(Value::str(format!(
                "capability limited to `{allowed}`, cannot reach `{path}`"
            ))));
        }
    }
    fs_builtin("read_file", std::slice::from_ref(&Value::str(path.to_string())))
}

/// `cap_fs_root()` (it118) -- the UNRESTRICTED root `CapFs` capability.
/// Call-site-restricted the same way as `cap_net_root` (`K0304`).
pub fn cap_fs_root() -> Value {
    Value::CapFs(Rc::new(crate::value::CapFsInner { allowed_prefix: None }))
}

/// Parse an HTTP request line (`METHOD PATH HTTP/1.1`) into (method, path).
pub fn parse_request_line(head: &str) -> (String, String) {
    let line = head.lines().next().unwrap_or("");
    // A raw socket read can legitimately contain an embedded NUL (e.g. a
    // deliberately malformed request); strip it before splitting so `method`/
    // `path` can never violate K0008 (KUPL strings are NUL-free) — mirrors the
    // native runtime's equivalent buffer sanitizing (PR-it577).
    let line: std::borrow::Cow<str> =
        if line.contains('\0') { line.replace('\0', "").into() } else { line.into() };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    (method, path)
}

/// Build a well-formed HTTP/1.1 text response.
pub fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A request body larger than this is truncated rather than fully buffered —
/// mirrors the existing 64KB request-head cap's DoS-prevention rationale, just
/// sized for bodies (JSON payloads, form posts) rather than header lines.
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Find a `Content-Length` header in a raw request head and return its value,
/// capped at `MAX_BODY_SIZE`. Missing/unparsable/negative -> 0 (no body).
///
/// A REAL cross-engine divergence found+fixed (production-hardening
/// PR-it918, deferred at it902): this used to split on the literal
/// `"\r\n"` line terminator -- for a request whose header lines are
/// separated by a BARE `\n` (still ending in the required literal
/// `\r\n\r\n` terminator overall) this treats the WHOLE multi-line
/// bare-LF header block as a SINGLE "line" with no internal `\r\n` to
/// split on, so `split_once(':')` matches the FIRST colon in the blob
/// rather than the actual `Content-Length` header. `cgen.rs`'s native
/// mirror (`k_content_length`, PR-it901) already scans line-by-line on a
/// bare `\n` boundary -- converging onto that SAME boundary here (rather
/// than porting this function's exact semantics into C, the ORIGINAL
/// larger fix it902 judged not worth it) is a minimal, low-risk change:
/// a trailing `\r` left in an ordinary `\r\n`-terminated line's `value`
/// half is already stripped by the existing `.trim()` call below (`\r`
/// is ASCII whitespace), so this does not disturb the normal case at
/// all. Confirmed live before this fix: `POST /echo HTTP/1.1\nHost:
/// x\nContent-Length: 11\n\r\n\r\nhello world` (bare-LF header lines,
/// literal `\r\n\r\n` terminator) returned an EMPTY body on interp/vm
/// while native correctly returned `hello world`.
fn parse_content_length(head: &str) -> usize {
    for line in head.split('\n') {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(n) = value.trim().parse::<usize>() {
                    return n.min(MAX_BODY_SIZE);
                }
            }
        }
    }
    0
}

/// A minimal blocking HTTP server: bind `127.0.0.1:port`, and for each request
/// call `handler(method, path, body)` to produce the response body. The
/// socket + HTTP wire code is shared by both engines (they differ only in how
/// they invoke the handler value), so behavior is identical. `Err` on bind
/// failure; otherwise this never returns (it serves forever).
pub fn serve_http(
    port: i64,
    handler: &mut dyn FnMut(String, String, String) -> Result<String, String>,
) -> Result<(), String> {
    serve_http_with_read_timeout(port, handler, Some(std::time::Duration::from_secs(30)))
}

/// `serve_http`, but the per-connection read timeout is injectable — lets a
/// test exercise the timeout mechanism itself without waiting out the real
/// 30s production value (`serve_http` above always uses 30s; only tests call
/// this directly). `None` disables the timeout entirely (the pre-fix,
/// blocks-forever behavior), kept only so a test could pin that shape too if
/// ever needed.
fn serve_http_with_read_timeout(
    port: i64,
    handler: &mut dyn FnMut(String, String, String) -> Result<String, String>,
    read_timeout: Option<std::time::Duration>,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", port as u16))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // A REAL, SEVERE availability bug found+fixed (production-hardening
        // PR-it623): no read timeout was ever set on the accepted connection.
        // This is a single-threaded, sequential accept-then-read loop -- the
        // SAME class of bug as PR-it559 (a panicking handler took down the
        // whole server) and PR-it577 (a NUL byte broke the terminator search
        // and hung the read loop forever), both on this exact function. A
        // client that opens a connection and simply never finishes sending
        // its request head (a classic "slowloris" attack, or just a stalled/
        // dead network peer) blocked `stream.read()` INDEFINITELY -- and
        // since the loop can't reach its next `accept()` until the CURRENT
        // connection's read/handle/write cycle finishes, one stalled
        // connection wedged the ENTIRE server, refusing every other client
        // forever. Fixed by bounding the read with a timeout matching this
        // codebase's existing `curl --max-time 30` convention for outbound
        // calls (interp.rs's own http_get, line ~3749). A timed-out read is
        // just another `Err` to the loop below (`Err(_) => break`), so the
        // server falls through to respond to whatever partial/empty head it
        // received (`parse_request_line` already defensively defaults an
        // incomplete line to `GET /`) and moves on to the next connection,
        // rather than hanging forever.
        let _ = stream.set_read_timeout(read_timeout);
        // A second, narrower availability gap found+fixed in the SAME
        // iteration (production-hardening PR-it624), per it623's own lesson
        // ("the same vulnerability class can have MULTIPLE independent
        // trigger mechanisms — always ask if there's a third"): the read
        // timeout above resets on EVERY successful read, so it only bounds
        // how long the server waits for the NEXT byte, not the connection's
        // TOTAL duration. A "trickle" client sending one byte every ~29
        // seconds (just under the 30s per-read window) never trips that
        // timeout at all, since each individual read succeeds -- and could
        // hold the connection (and thus the whole single-threaded server)
        // open for as long as it likes, up to the ~19 days it would take to
        // accumulate the 64KB cap one byte at a time. Fixed with a total
        // elapsed-time deadline (the SAME `read_timeout` duration, checked
        // once per loop iteration) independent of the per-read timeout --
        // closing the trickle variant while leaving the "sends nothing at
        // all" case (the one PR-it623 fixed) covered exactly as before.
        let deadline = read_timeout.map(|d| std::time::Instant::now() + d);
        // read the request head (until the blank line ending the headers)
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut head_end = None;
        loop {
            if deadline.is_some_and(|dl| std::time::Instant::now() >= dl) {
                break;
            }
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = Some(pos + 4);
                        break;
                    }
                    if buf.len() > 64 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let head_end = head_end.unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..head_end]);
        let (method, path) = parse_request_line(&head);
        let content_length = parse_content_length(&head);
        // A `read()` past the head/body terminator can already have pulled in
        // some (or all) of the body in the SAME chunk; only read MORE if the
        // terminator-adjacent bytes don't already satisfy Content-Length.
        let mut body: Vec<u8> = buf[head_end..].to_vec();
        if body.len() > content_length {
            body.truncate(content_length);
        } else {
            while body.len() < content_length {
                if deadline.is_some_and(|dl| std::time::Instant::now() >= dl) {
                    break;
                }
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        let take = n.min(content_length - body.len());
                        body.extend_from_slice(&tmp[..take]);
                    }
                    Err(_) => break,
                }
            }
        }
        // Strip any embedded NUL from the body, matching parse_request_line's
        // identical sanitizing of the method/path line (PR-it577) and the
        // K0008 invariant ("Str is NUL-free UTF-8 text") that governs every
        // KUPL string, not just source literals -- otherwise native's `k_str`
        // (a strlen-based C constructor) would silently TRUNCATE the body at
        // the first embedded NUL where this Rust String preserves it in
        // full, a fresh cross-engine divergence in a brand-new feature.
        let mut body = String::from_utf8_lossy(&body).into_owned();
        if body.contains('\0') {
            body = body.replace('\0', "");
        }
        let resp = match handler(method, path, body) {
            Ok(body) => http_response("200 OK", &body),
            Err(msg) => http_response("500 Internal Server Error", &msg),
        };
        // A REAL, SEVERE availability bug found+fixed (production-hardening
        // PR-it867), the SAME single-stalled-connection-wedges-the-whole-
        // server class as it559/it577/it623/it624 above -- all four fixed
        // the READ side of this exact function; the response WRITE side had
        // NO timeout of any kind, a plain `stream.write_all(...)` that could
        // block forever. A client that sends a valid request and then simply
        // never reads the response (or reads it one byte at a time, slowly
        // enough to keep the TCP send buffer full without ever fully
        // stalling a single `write()` call -- the exact "trickle" shape
        // it624 already fixed on the read side) wedges the single-threaded
        // accept loop exactly as effectively as a stalled READ does.
        // Confirmed live BEFORE this fix: a client that opened a connection,
        // sent a request, and deliberately never read the (large) response
        // caused a SECOND, well-behaved client's request to time out after
        // 8s waiting for `accept()`/a reply -- once the first client closed
        // its socket, a fresh control request was served instantly (0.00s),
        // proving the server was genuinely wedged, not merely slow. Fixed
        // with the IDENTICAL two-layer defense already used for reads: a
        // per-write timeout (mirroring it623) PLUS a total elapsed-time
        // deadline checked every loop iteration (mirroring it624), rather
        // than relying on `set_write_timeout` alone -- a single
        // `set_write_timeout` bounds only each individual `write()` syscall
        // inside `write_all`'s internal retry loop, which resets on every
        // partial write exactly like the per-read timeout did before it624,
        // so it alone would NOT have closed the trickle variant.
        let _ = stream.set_write_timeout(read_timeout);
        let write_deadline = read_timeout.map(|d| std::time::Instant::now() + d);
        let resp_bytes = resp.as_bytes();
        let mut written = 0;
        while written < resp_bytes.len() {
            if write_deadline.is_some_and(|dl| std::time::Instant::now() >= dl) {
                break;
            }
            match stream.write(&resp_bytes[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(_) => break,
            }
        }
    }
    Ok(())
}

/// `exec(program, args)` — run a program (no shell; argv-based) and capture
/// stdout. `Ok(stdout)` on exit 0; else `Err(trimmed stderr)`, or
/// `Err("exited with status N")` if stderr is empty, or
/// `Err("cannot run <program>: <e>")` if it can't be spawned. Same success/
/// failure shape as `http_builtin`, so the two are consistent. Effect `io.proc`.
pub fn exec_builtin(args: &[Value]) -> Result<Value, String> {
    let program = match &args[0] {
        Value::Str(s) => s.as_str().to_string(),
        other => other.to_string(),
    };
    let arglist: Vec<String> = match &args[1] {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Str(s) => s.as_str().to_string(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&arglist);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => return Ok(Value::err(Value::str(format!("cannot run {program}: {e}")))),
    };
    if out.status.success() {
        // A KUPL string must be valid UTF-8 and NUL-free (K0008). Reject rather than
        // embed a NUL (which the native C runtime would truncate at) or lossily
        // replace invalid bytes (which native would pass through raw) — either would
        // diverge across engines.
        match String::from_utf8(out.stdout) {
            Ok(s) if !s.as_bytes().contains(&0) => Ok(Value::ok(Value::str(s))),
            Ok(_) => Ok(Value::err(Value::str("command output contains a NUL byte".to_string()))),
            Err(_) => Ok(Value::err(Value::str("command output is not valid UTF-8".to_string()))),
        }
    } else {
        // Same K0008 rule as the stdout success path above: a NUL byte or invalid
        // UTF-8 in stderr can't become a valid KUPL Str. Rather than truncate at
        // the NUL (a native-only divergence like the stdout case would have) or
        // lossily replace invalid bytes (which native can't cheaply mirror byte-
        // for-byte here), fall back to the SAME generic exit-status message the
        // "stderr is empty" branch below already uses — the process genuinely
        // did fail; only the diagnostic TEXT is unrepresentable (PR-it577).
        let err = match std::str::from_utf8(&out.stderr) {
            Ok(s) if !s.as_bytes().contains(&0) => s.trim().to_string(),
            _ => String::new(),
        };
        let msg = if err.is_empty() {
            format!("exited with status {}", out.status.code().unwrap_or(-1))
        } else {
            err
        };
        Ok(Value::err(Value::str(msg)))
    }
}

fn run_curl(mut cmd: std::process::Command, body: Option<String>) -> Result<String, String> {
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if body.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(b) = body {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b.as_bytes()).map_err(|e| format!("curl stdin: {e}"))?;
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        // Same fallback-on-unrepresentable-message strategy as exec_builtin's
        // equivalent error path (PR-it577): a NUL byte or invalid UTF-8 in
        // curl's stderr can't become a valid KUPL Str (K0008).
        let err = match std::str::from_utf8(&out.stderr) {
            Ok(s) if !s.as_bytes().contains(&0) => s.trim().to_string(),
            _ => String::new(),
        };
        return Err(if err.is_empty() {
            format!("request failed (curl exit {})", out.status.code().unwrap_or(-1))
        } else {
            err
        });
    }
    // A KUPL string must be valid UTF-8 and NUL-free (K0008) — reject a binary/
    // invalid response body rather than pass it through raw (native's C-string
    // Str representation can't do so safely either); this success path
    // previously had NO such check at all, unlike exec_builtin's stdout guard
    // (PR-it577) — a real, non-adversarial gap: any http_get/http_post against
    // a binary resource (an image, say) used to silently smuggle a K0008-
    // violating Str into the program instead of a clean Err.
    match String::from_utf8(out.stdout) {
        Ok(s) if !s.as_bytes().contains(&0) => Ok(s),
        Ok(_) => Err("response body contains a NUL byte".to_string()),
        Err(_) => Err("response body is not valid UTF-8".to_string()),
    }
}

/// File I/O builtins — shared by interpreter and KVM. Effect `io.fs`.
///
/// All return a `Result` value (KUPL has no exceptions): read/write/append/
/// delete give `Result[Str|Unit, Str]` (the `Err` carries the OS message);
/// `file_exists` gives a plain `Bool`. A wrong argument *type* is a checker
/// error, so here we assume the types the checker guaranteed.
pub fn fs_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| -> String {
        match v {
            Value::Str(s) => s.as_str().to_string(),
            other => other.to_string(),
        }
    };
    match name {
        "read_file" => Ok(match std::fs::read_to_string(as_str(&args[0])) {
            // read_to_string already rejects invalid UTF-8; also reject an embedded
            // NUL (valid UTF-8 but not allowed in a KUPL Str, K0008 — the native
            // runtime would truncate at it: a cross-engine divergence).
            Ok(contents) if contents.as_bytes().contains(&0) => {
                Value::err(Value::str("file contains a NUL byte".to_string()))
            }
            Ok(contents) => Value::ok(Value::str(contents)),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "write_file" => Ok(match std::fs::write(as_str(&args[0]), as_str(&args[1])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "append_file" => {
            use std::io::Write;
            let result = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(as_str(&args[0]))
                .and_then(|mut f| f.write_all(as_str(&args[1]).as_bytes()));
            Ok(match result {
                Ok(()) => Value::ok(Value::Unit),
                Err(e) => Value::err(Value::str(e.to_string())),
            })
        }
        "delete_file" => Ok(match std::fs::remove_file(as_str(&args[0])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "file_exists" => Ok(Value::Bool(std::path::Path::new(&as_str(&args[0])).exists())),
        "list_dir" => Ok(match std::fs::read_dir(as_str(&args[0])) {
            Ok(rd) => {
                // names only, "."/".." excluded by read_dir; SORTED for determinism
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                    .collect();
                names.sort();
                Value::ok(Value::List(Rc::new(names.into_iter().map(Value::str).collect())))
            }
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "make_dir" => Ok(match std::fs::create_dir_all(as_str(&args[0])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        "remove_dir" => Ok(match std::fs::remove_dir_all(as_str(&args[0])) {
            Ok(()) => Value::ok(Value::Unit),
            Err(e) => Value::err(Value::str(e.to_string())),
        }),
        _ => Err(format!("unknown file builtin `{name}`")),
    }
}

/// `big(x)` — an arbitrary-precision integer from an `Int` or a decimal `Str`.
pub fn big_builtin(v: &Value) -> Result<Value, String> {
    use std::rc::Rc;
    match v {
        Value::Int(n) => Ok(Value::BigInt(Rc::new(crate::bigint::BigInt::from_i64(*n)))),
        Value::BigInt(b) => Ok(Value::BigInt(b.clone())),
        Value::Str(s) => match crate::bigint::BigInt::from_str(s) {
            Some(b) => Ok(Value::BigInt(Rc::new(b))),
            // A string long enough to be rejected by from_str's own size cap
            // (PR-it638) shouldn't be echoed into the error text -- report the
            // length instead of dumping a potentially enormous string.
            None if s.len() as u64 > crate::bigint::MAX_BIGINT_LIMBS * 9 => Err(format!(
                "invalid BigInt: input is {} characters long, exceeding the {}-digit limit",
                s.len(),
                crate::bigint::MAX_BIGINT_LIMBS * 9
            )),
            None => Err(format!("invalid BigInt: {s}")),
        },
        other => Err(format!("`big` needs an Int or a Str, found {}", other.type_name())),
    }
}

/// `rat(n, d)` — an exact rational number `n/d` (reduced; denominator 0 errors).
/// Accepts `Int` or `BigInt` numerator/denominator.
pub fn rat_builtin(n: &Value, d: &Value) -> Result<Value, String> {
    use crate::bigint::BigInt;
    use std::rc::Rc;
    let to_big = |v: &Value| -> Result<BigInt, String> {
        match v {
            Value::Int(x) => Ok(BigInt::from_i64(*x)),
            Value::BigInt(b) => Ok((**b).clone()),
            other => Err(format!("`rat` needs Int or BigInt, found {}", other.type_name())),
        }
    };
    let r = crate::rational::Rational::new(to_big(n)?, to_big(d)?)?;
    Ok(Value::Rational(Rc::new(r)))
}

/// `dec(x)` — an exact base-10 decimal (`it107`). Accepts an `Int` (scale
/// 0) or a `Str` (parsed exactly, e.g. `"3.14"`/`"-0.005"`) -- mirrors
/// `big`'s own accepted-input shape exactly.
pub fn dec_builtin(v: &Value) -> Result<Value, String> {
    use std::rc::Rc;
    match v {
        Value::Int(n) => Ok(Value::Decimal(Rc::new(crate::decimal::Decimal::from_i64(*n)))),
        Value::Decimal(d) => Ok(Value::Decimal(d.clone())),
        Value::Str(s) => crate::decimal::Decimal::from_str(s).map(|d| Value::Decimal(Rc::new(d))),
        other => Err(format!("`dec` needs an Int or a Str, found {}", other.type_name())),
    }
}

/// `text_embed(s, dims)` — a from-scratch, zero-dependency bag-of-words
/// hash embedding (`it109`); see `embed.rs`'s own doc comment for the
/// technique. `s`/`dims` are checked as `Str`/`Int` statically, so the
/// `other =>` arms below are defensive, not reachable through ordinary
/// KUPL source.
pub fn text_embed_builtin(s: &Value, dims: &Value) -> Result<Value, String> {
    let s = match s {
        Value::Str(s) => s.as_str(),
        other => return Err(format!("`text_embed` needs a Str, found {}", other.type_name())),
    };
    let dims = match dims {
        Value::Int(n) => *n,
        other => return Err(format!("`text_embed` needs an Int dims, found {}", other.type_name())),
    };
    let v = crate::embed::text_embed(s, dims)?;
    Ok(Value::List(Rc::new(v.into_iter().map(Value::Float).collect())))
}

/// `cosine_similarity(a, b)` — see `embed.rs`. `a`/`b` are checked as
/// `List[Float]` statically; the per-element `other =>` arm below is
/// defensive, not reachable through ordinary KUPL source.
pub fn cosine_similarity_builtin(a: &Value, b: &Value) -> Result<Value, String> {
    fn to_vec(v: &Value) -> Result<Vec<f64>, String> {
        match v {
            Value::List(items) => items
                .iter()
                .map(|it| match it {
                    Value::Float(f) => Ok(*f),
                    other => Err(format!("`cosine_similarity` needs List[Float], found an element of type {}", other.type_name())),
                })
                .collect(),
            other => Err(format!("`cosine_similarity` needs a List[Float], found {}", other.type_name())),
        }
    }
    let a = to_vec(a)?;
    let b = to_vec(b)?;
    Ok(Value::Float(crate::embed::cosine_similarity(&a, &b)?))
}

/// Pure `/`-path helpers (no effect). They operate lexically on forward-slash
/// paths — no filesystem access.
pub fn path_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let as_str = |v: &Value| -> String {
        match v {
            Value::Str(s) => s.as_str().to_string(),
            other => other.to_string(),
        }
    };
    let p = as_str(&args[0]);
    match name {
        "path_join" => {
            let b = as_str(&args[1]);
            let joined = if p.is_empty() {
                b
            } else if b.starts_with('/') {
                b
            } else {
                format!("{}/{}", p.trim_end_matches('/'), b)
            };
            Ok(Value::str(joined))
        }
        "path_base" => Ok(Value::str(p.rsplit('/').next().unwrap_or("").to_string())),
        "path_dir" => Ok(Value::str(match p.rfind('/') {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        })),
        "path_ext" => {
            let base = p.rsplit('/').next().unwrap_or("");
            // the ext is the last `.` onward in the base name; a leading-dot
            // dotfile (".bashrc") or a name with no dot has no ext
            Ok(Value::str(match base.rfind('.') {
                Some(i) if i > 0 => base[i..].to_string(),
                _ => String::new(),
            }))
        }
        _ => Err(format!("unknown path builtin `{name}`")),
    }
}

/// tensor / zeros / arange — shared by interpreter and KVM.
pub fn tensor_builtin(name: &str, arg: &Value) -> Result<Value, String> {
    match (name, arg) {
        ("tensor", Value::List(items)) => {
            let mut data = Vec::with_capacity(items.len());
            for it in items.iter() {
                match it {
                    Value::Float(f) => data.push(*f),
                    Value::Int(i) => data.push(*i as f64),
                    other => return Err(format!("tensor() needs Float elements, found {}", other.type_name())),
                }
            }
            Ok(Value::Tensor(Rc::new(data)))
        }
        ("zeros", Value::Int(n)) => {
            if *n < 0 {
                return Err("zeros() needs a non-negative size".into());
            }
            if *n as u64 > MAX_TENSOR_LEN {
                return Err("zeros() size too large".into());
            }
            Ok(Value::Tensor(Rc::new(vec![0.0; *n as usize])))
        }
        ("arange", Value::Int(n)) => {
            if *n < 0 {
                return Err("arange() needs a non-negative size".into());
            }
            if *n as u64 > MAX_TENSOR_LEN {
                return Err("arange() size too large".into());
            }
            Ok(Value::Tensor(Rc::new((0..*n).map(|i| i as f64).collect())))
        }
        _ => Err(format!("invalid argument for {name}()")),
    }
}

/// `callback_invoked` is set to `true` the moment ANY user callback runs for
/// this specific dispatch attempt (production-hardening PR-it1193, see
/// `eval_method`'s own UFCS-retry gate for the full writeup of the bug this
/// closes) -- lets the caller distinguish a genuine "no such builtin method"
/// rejection (`shared_method`'s own match falls through WITHOUT ever
/// running user code) from a callback's own panic message merely
/// CONTAINING the same wording. The real-thread `par_map`/`par_filter` fast
/// path below ALSO marks it unconditionally on any `Some(res)` outcome
/// (success or panic) -- reaching that branch at all already means `name`
/// matched `"par_map"`/`"par_filter"` exactly (each helper's own internal
/// `if name != ... { return None }` gate), so a panic from THAT path can
/// never legitimately be a "no such method" case either, regardless of its
/// own message text.
fn builtin_method(
    recv: Value,
    name: &str,
    args: Vec<Value>,
    span: Span,
    interp: &mut Interp,
    callback_invoked: &std::cell::Cell<bool>,
) -> EvalResult {
    // real-thread fast path: `xs.par_map(pure_fn)` over a large list. Falls
    // through to the sequential shared_method on any non-qualifying call.
    if let Some(image) = interp.image.clone() {
        if let Some(res) = crate::parallel::try_par_map(&recv, name, &args, &image)
            .or_else(|| crate::parallel::try_par_filter(&recv, name, &args, &image))
        {
            callback_invoked.set(true);
            return res.map_err(|msg| Flow::Panic { msg, span, already_reported: false });
        }
    }
    let mut call = |f: Value, args: Vec<Value>| -> Result<Value, String> {
        callback_invoked.set(true);
        match interp.call_value(f, args, span) {
            Ok(v) => Ok(v),
            Err(Flow::Panic { msg, .. }) => Err(msg),
            Err(_) => Err("invalid control flow in callback".into()),
        }
    };
    match shared_method(&recv, name, args, &mut call) {
        Ok(v) => Ok(v),
        Err(msg) => Err(Flow::Panic { msg, span, already_reported: false }),
    }
}

#[cfg(test)]
mod server_tests {
    use super::{
        http_response, parse_content_length, parse_request_line, serve_http, serve_http_with_read_timeout, Interp,
        ProgramDb, Value,
    };
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// A REAL cross-engine divergence found+fixed (production-hardening
    /// PR-it918, deferred at it902): `parse_content_length` used to split
    /// strictly on `"\r\n"`, so a request whose header lines are separated
    /// by a bare `\n` (still ending in the required literal `\r\n\r\n`
    /// terminator overall) was treated as ONE single "line" with no
    /// internal `\r\n` to split on -- `split_once(':')` then matched the
    /// FIRST colon in the whole blob (the one in `Host:`, not
    /// `Content-Length:`), silently missing the real header. Now split on
    /// a bare `\n`, matching `cgen.rs`'s native `k_content_length`
    /// (PR-it901) exactly. Also confirms the ORDINARY `\r\n`-terminated
    /// case (the overwhelming common case) is completely unaffected --
    /// a trailing `\r` left in the value half of an ordinary line is
    /// already stripped by the pre-existing `.trim()` call.
    #[test]
    fn parse_content_length_finds_the_header_even_with_bare_lf_line_boundaries() {
        let mixed = "POST /echo HTTP/1.1\nHost: x\nContent-Length: 11\n\r\n";
        assert_eq!(parse_content_length(mixed), 11, "bare-LF header lines must still find Content-Length");

        let ordinary = "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\n";
        assert_eq!(parse_content_length(ordinary), 11, "ordinary \\r\\n-terminated headers must be unaffected");

        let none = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_content_length(none), 0, "no Content-Length header still means no body");
    }

    /// Send one GET and return the response body (everything after the headers).
    fn get_body(port: u16, path: &str) -> String {
        let mut stream = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(30));
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                stream = Some(s);
                break;
            }
        }
        let mut stream = stream.expect("server should be listening");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .unwrap();
        let mut resp = String::new();
        let _ = stream.read_to_string(&mut resp);
        resp.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or(resp)
    }

    /// A real JSON REST API (the shape of examples/demos/api.kupl) answers live
    /// requests through the interpreter — routing + json_stringify end to end.
    #[test]
    fn json_api_routes() {
        let src = r#"
fun handle(method: Str, path: Str, body: Str) -> Str {
    let parts = path.split("/")
    if path == "/health" {
        json_stringify(JObj(Map().insert("status", JStr("ok"))))
    } else if parts.len() == 4 && parts.get(1) == Some("add") {
        let x = parts.get(2).unwrap_or("").parse_int().unwrap_or(0)
        let y = parts.get(3).unwrap_or("").parse_int().unwrap_or(0)
        json_stringify(JObj(Map().insert("sum", JNum((x + y).to_float()))))
    } else {
        json_stringify(JObj(Map().insert("error", JStr("not found"))))
    }
}
fun main() uses io { let _ = http_serve(38131, handle) }
"#;
        let compiled = crate::run::compile(src).expect("api compiles");
        std::thread::spawn(move || {
            let db = ProgramDb::build(&compiled.program, &compiled.checked);
            let mut interp = Interp::new(db);
            let f = Value::Fun(std::rc::Rc::new("main".to_string()));
            let _ = interp.call_value(f, vec![], crate::diag::Span::default());
        });
        assert_eq!(get_body(38131, "/health"), "{\"status\":\"ok\"}");
        assert_eq!(get_body(38131, "/add/2/3"), "{\"sum\":5}");
        assert_eq!(get_body(38131, "/nope"), "{\"error\":\"not found\"}");
    }

    #[test]
    fn request_line_and_response() {
        assert_eq!(parse_request_line("GET /world HTTP/1.1\r\nHost: x\r\n\r\n"),
                   ("GET".to_string(), "/world".to_string()));
        assert_eq!(parse_request_line("POST /a/b?x=1 HTTP/1.1"),
                   ("POST".to_string(), "/a/b?x=1".to_string()));
        assert_eq!(parse_request_line(""), ("GET".to_string(), "/".to_string()));
        let r = http_response("200 OK", "hi");
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains("Content-Length: 2\r\n"));
        assert!(r.ends_with("\r\n\r\nhi"));
    }

    /// End-to-end: a live server on a background thread answers a real request.
    #[test]
    fn serves_a_request() {
        let port: u16 = 38111;
        std::thread::spawn(move || {
            let mut h = |m: String, p: String, _b: String| -> Result<String, String> { Ok(format!("{m} {p}")) };
            let _ = serve_http(port as i64, &mut h);
        });
        let mut stream = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                stream = Some(s);
                break;
            }
        }
        let mut stream = stream.expect("server should be listening");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        stream.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp = String::new();
        let _ = stream.read_to_string(&mut resp);
        assert!(resp.contains("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.ends_with("GET /world"), "resp: {resp}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it721): `http_serve`'s
    /// handler was only ever given `(method, path)` -- the request BODY was
    /// read off the wire (to find the head/body terminator) and then simply
    /// discarded, making it impossible to implement a real POST/PUT JSON API
    /// endpoint (the flagship `examples/demos/api.kupl` worked around this by
    /// encoding all data in the URL path instead of a real request body).
    /// Confirms the handler now receives the body as its 3rd argument, in
    /// TWO shapes that previously required different code paths internally:
    /// (1) the whole body arrives in the SAME `read()` as the head/terminator
    /// (the common case for a short body), and (2) the body arrives in a
    /// LATER, separate `write_all` (proving the follow-up read loop -- not
    /// just the terminator-adjacent bytes -- is exercised too).
    #[test]
    fn serve_http_exposes_the_request_body_via_content_length() {
        let port: u16 = 38112;
        std::thread::spawn(move || {
            let mut h = |m: String, p: String, b: String| -> Result<String, String> {
                Ok(format!("{m} {p} [{b}]"))
            };
            let _ = serve_http(port as i64, &mut h);
        });
        let connect = || {
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                    return Some(s);
                }
            }
            None
        };
        // shape 1: head + full body land in a single write (and thus, very
        // likely, a single `read()` on the server side).
        let mut s1 = connect().expect("server should be listening");
        s1.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        s1.write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\nhello world")
            .unwrap();
        let mut resp1 = String::new();
        let _ = s1.read_to_string(&mut resp1);
        assert!(resp1.ends_with("POST /echo [hello world]"), "resp1: {resp1}");
        // shape 2: the head arrives first, then the body trickles in via a
        // SEPARATE write shortly after -- proves the post-terminator read
        // loop (not just bytes already sitting in the head's own read) works.
        let mut s2 = connect().expect("server should be listening");
        s2.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        s2.write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        s2.write_all(b"later").unwrap();
        let mut resp2 = String::new();
        let _ = s2.read_to_string(&mut resp2);
        assert!(resp2.ends_with("POST /echo [later]"), "resp2: {resp2}");
        // no Content-Length -> empty body, unchanged from the pre-fix shape.
        let mut s3 = connect().expect("server should be listening");
        s3.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        s3.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp3 = String::new();
        let _ = s3.read_to_string(&mut resp3);
        assert!(resp3.ends_with("GET /world []"), "resp3: {resp3}");
    }

    /// A REAL, SEVERE availability bug found+fixed (production-hardening
    /// PR-it623): confirms `serve_http` no longer hangs forever on a stalled
    /// connection, the same class of single-connection-wedges-the-whole-
    /// server bug as PR-it559 (panic) and PR-it577 (NUL byte), both on this
    /// exact function -- but previously unaddressed for a client that simply
    /// never finishes sending its request head at all (a slowloris attack).
    /// Opens a connection, sends a PARTIAL request line with no terminating
    /// blank line, and deliberately never sends more or closes it. Uses
    /// `serve_http_with_read_timeout` directly with a SHORT injected timeout
    /// (not the real 30s production value `serve_http` uses) so this test
    /// stays fast while still proving the exact mechanism end to end: the
    /// timeout unblocks the read, and -- critically -- the server remains
    /// alive and promptly serves a SECOND, well-formed request on a fresh
    /// connection right after, proving the whole server wasn't wedged by the
    /// one stalled connection (before the fix, this second request would
    /// never have been reached at all).
    #[test]
    fn serve_http_recovers_from_a_stalled_slow_client() {
        let port: u16 = 38113;
        std::thread::spawn(move || {
            let mut h = |m: String, p: String, _b: String| -> Result<String, String> { Ok(format!("{m} {p}")) };
            let _ = serve_http_with_read_timeout(
                port as i64,
                &mut h,
                Some(std::time::Duration::from_millis(200)),
            );
        });
        let connect = || {
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                    return Some(s);
                }
            }
            None
        };
        // connection 1: a partial request line, no terminator -- held open,
        // never closed, never completed. Before the fix, `serve_http` would
        // block on this forever and connection 2 below would never even be
        // accepted, let alone answered.
        let mut stalled = connect().expect("server should be listening");
        stalled.write_all(b"GET /stalled HTTP/1.1\r\nHost: x").unwrap();
        // connection 2: retried for up to 2s (well past the 200ms injected
        // timeout) -- proves the server recovers and serves a fresh request
        // rather than staying wedged on connection 1.
        let mut recovered = None;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                s.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                if s.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").is_err() {
                    continue;
                }
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                if resp.contains("HTTP/1.1 200 OK") {
                    recovered = Some(resp);
                    break;
                }
            }
        }
        let resp = recovered
            .expect("server should recover and serve a fresh request after the stalled one times out");
        assert!(resp.ends_with("GET /world"), "resp: {resp}");
        drop(stalled);
    }

    /// A second, narrower availability gap found+fixed in the SAME iteration
    /// (production-hardening PR-it624), applying it623's own lesson ("the
    /// same vulnerability class can have MULTIPLE independent trigger
    /// mechanisms — always ask if there's a third"): PR-it623's per-read
    /// timeout resets on every successful read, so it bounds the wait for
    /// the NEXT byte, not the connection's TOTAL duration. A "trickle" client
    /// that sends a byte every so often -- always comfortably within a
    /// single read's timeout window -- never trips that timeout at all, and
    /// could hold the connection (and thus the single-threaded server) open
    /// indefinitely. Trickles one byte every 60ms (well under the 300ms
    /// per-read timeout injected here, so the per-read mechanism alone would
    /// never fire) for long enough that the CUMULATIVE elapsed time exceeds
    /// the 300ms total deadline, and confirms the server gives up and serves
    /// a fresh connection promptly afterward — proving the fix is the total-
    /// duration deadline, not a lucky per-read timeout.
    #[test]
    fn serve_http_closes_a_trickle_connection_that_never_finishes() {
        let port: u16 = 38114;
        std::thread::spawn(move || {
            let mut h = |m: String, p: String, _b: String| -> Result<String, String> { Ok(format!("{m} {p}")) };
            let _ = serve_http_with_read_timeout(
                port as i64,
                &mut h,
                Some(std::time::Duration::from_millis(1000)),
            );
        });
        fn connect(port: u16) -> Option<TcpStream> {
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                    return Some(s);
                }
            }
            None
        }
        // connection 1: connected SYNCHRONOUSLY first (so it's guaranteed to
        // be accept()ed by the server before connection 2 below ever tries
        // -- otherwise a scheduling race could let connection 2 reach the
        // server FIRST and get served immediately, making the test pass
        // without ever exercising the trickle scenario at all). Once
        // connected, trickles one byte every 150ms for 200 rounds (30s
        // total -- LONG past the 1s deadline, and deliberately longer than
        // connection 2's own observation budget below, so the trickle
        // thread is STILL actively holding the socket open throughout that
        // entire window; a trickle that finished WITHIN the window would
        // drop its `TcpStream` when the thread ends, sending the server an
        // EOF that resolves the read loop on its own -- an unintended
        // shortcut that doesn't actually exercise the deadline fix at all
        // (confirmed: this exact failure mode hit the analogous native C
        // test in the SAME iteration, fixed there the same way). Each
        // individual gap (150ms) is comfortably inside the 1s per-read
        // timeout, so that mechanism ALONE would never fire while the
        // trickle is ongoing. This is what isolates the deadline check from
        // the per-read timeout: a trickle that stops early would ALSO
        // eventually be closed via the ordinary per-read timeout once the
        // client goes idle, without ever exercising the deadline path at all
        // (confirmed earlier, with a since-widened set of margins: an
        // 8-byte trickle that stopped well before its own per-read timeout
        // window elapsed still passed even with the deadline check
        // disabled). Never sends a terminator, never closes on its own
        // within the window; write errors after the server eventually
        // closes its end are ignored, harmless.
        let mut trickle = connect(port).expect("server should be listening");
        std::thread::spawn(move || {
            for _ in 0..200 {
                let _ = trickle.write_all(b"x");
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
        // connection 2: retried for up to ~24s total -- comfortably past the
        // fixed case's recovery (typically well under 1.5s, but given
        // generous headroom here since this test runs alongside SEVERAL
        // other HTTP tests -- including two ~30s native ones -- spawning
        // their own servers/threads/processes in parallel; observed CI
        // scheduling jitter under that FULL combined load occasionally
        // pushed recovery past earlier, tighter budgets that worked fine in
        // isolation), but well short of the 30s trickle's natural end. Each
        // ATTEMPT's own read is bounded to a SHORT 200ms (not a generous
        // multi-second one) -- while the server is still busy with
        // connection 1, a fresh probe connection here gets queued in the OS
        // backlog but never actually served, so its read would otherwise
        // block for whatever timeout IT was given; a short per-attempt
        // bound is what keeps the outer loop's total budget properly
        // bounded rather than able to balloon to Nx a multi-second
        // per-attempt wait. If the deadline fix is missing, this loop
        // exhausts and `recovered` stays `None` (confirmed via temporarily
        // disabling the deadline check and re-running this exact test: it
        // failed cleanly, not hanging, well within this budget).
        let mut recovered = None;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                s.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
                if s.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").is_err() {
                    continue;
                }
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                if resp.contains("HTTP/1.1 200 OK") {
                    recovered = Some(resp);
                    break;
                }
            }
        }
        let resp = recovered
            .expect("server should recover after the trickle connection's total deadline expires");
        assert!(resp.ends_with("GET /world"), "resp: {resp}");
    }

    /// A REAL, SEVERE availability bug found+fixed (production-hardening
    /// PR-it867), the SAME single-stalled-connection-wedges-the-whole-server
    /// class as PR-it559/it577/it623/it624 above -- all four fixed the READ
    /// side of this exact function; the response WRITE side had no timeout
    /// of any kind at all. A client that sends a valid, complete request and
    /// then simply never reads the response wedges the single-threaded
    /// accept loop just as effectively as a stalled read does, since
    /// `stream.write_all(...)` blocks forever once the OS's TCP send buffer
    /// fills. Mirrors `serve_http_recovers_from_a_stalled_slow_client`'s
    /// (it623) exact structure: connection 1 sends a complete request but
    /// never reads the (deliberately large) response, held open well past
    /// the injected timeout; connection 2 then proves the server recovers
    /// and serves a fresh, small request promptly rather than staying
    /// wedged.
    #[test]
    fn serve_http_recovers_from_a_client_that_never_reads_the_response() {
        let port: u16 = 38115;
        std::thread::spawn(move || {
            let mut h = |m: String, p: String, _b: String| -> Result<String, String> {
                if p == "/big" {
                    Ok("x".repeat(5_000_000))
                } else {
                    Ok(format!("{m} {p}"))
                }
            };
            let _ = serve_http_with_read_timeout(
                port as i64,
                &mut h,
                Some(std::time::Duration::from_millis(200)),
            );
        });
        let connect = || {
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                    return Some(s);
                }
            }
            None
        };
        // connection 1: a COMPLETE request for the large response, held open
        // WITHOUT reading anything back. Before the fix, `write_all` would
        // block on this forever (once the OS send buffer fills) and
        // connection 2 below would never even be accepted.
        let mut stalled = connect().expect("server should be listening");
        stalled.write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        // connection 2: retried for up to 2s (well past the 200ms injected
        // timeout) -- proves the server recovers and serves a fresh request
        // rather than staying wedged on connection 1's unread response.
        let mut recovered = None;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                s.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                if s.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").is_err() {
                    continue;
                }
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                if resp.contains("HTTP/1.1 200 OK") {
                    recovered = Some(resp);
                    break;
                }
            }
        }
        let resp = recovered
            .expect("server should recover and serve a fresh request after the unread response times out");
        assert!(resp.ends_with("GET /world"), "resp: {resp}");
        drop(stalled);
    }
}

#[cfg(test)]
mod http_client_tests {
    use super::{base_curl_cmd, MAX_HTTP_RESPONSE_SIZE};

    #[test]
    fn base_curl_cmd_caps_the_response_size_it_will_buffer_into_memory() {
        // A REAL, live-confirmed resource-exhaustion gap found+fixed
        // (production-hardening PR-it751): `http_builtin`'s `curl`
        // invocation had no response-size limit at all --
        // `run_curl`'s `child.wait_with_output()` buffers the ENTIRE
        // response body into memory before this module gets a chance to
        // look at it, so `http_get`/`http_post` against a URL that happens
        // to return an enormous body (the KUPL program author writes the
        // URL, but not what the remote host chooses to send back) could
        // exhaust the process's memory. Live-confirmed BEFORE this fix,
        // outside this test (a local test HTTP server serving a 10MB file,
        // run via a real `curl` subprocess with and without
        // `--max-filesize`): without the flag, curl downloaded the full
        // 10MB; with `--max-filesize 1000000` (1MB) set against the SAME
        // 10MB file, curl aborted with exit 63 ("Maximum file size
        // exceeded") and downloaded nothing.
        //
        // This test does NOT spawn a real `curl` subprocess -- no existing
        // test in this module invokes real `curl` for the CLIENT side
        // (`serve_http`'s tests exercise only the SERVER side, via raw
        // `TcpStream`s, needing no external process), and a network-
        // dependent test here would be the first of its kind. Instead it
        // introspects the ACTUAL `Command` `http_builtin` would spawn (via
        // `base_curl_cmd`, the same function `http_builtin` itself calls)
        // using `Command::get_args()` -- this still catches the real
        // regression the fix guards against (the `--max-filesize` flag
        // being silently dropped in a future edit), without any network
        // dependency or flakiness.
        let cmd = base_curl_cmd();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let flag_pos = args.iter().position(|a| a == "--max-filesize");
        assert!(flag_pos.is_some(), "http_builtin must pass --max-filesize: {args:?}");
        let limit: u64 = args[flag_pos.unwrap() + 1].parse().expect("--max-filesize value must be numeric");
        assert_eq!(limit, MAX_HTTP_RESPONSE_SIZE, "{args:?}");
        assert!(limit > 0, "a zero cap would reject every legitimate response too: {args:?}");
    }
}

#[cfg(test)]
mod format_tests {
    use super::{format_float, int_to_radix};
    #[test]
    fn fixed_precision_rounds_half_away() {
        assert_eq!(format_float(3.14159, 2), "3.14");
        assert_eq!(format_float(2.5, 0), "3");
        assert_eq!(format_float(2.4, 0), "2");
        assert_eq!(format_float(0.0, 2), "0.00");
        assert_eq!(format_float(-1.5, 1), "-1.5");
        assert_eq!(format_float(100.0, 2), "100.00");
        assert_eq!(format_float(-0.001, 2), "0.00"); // sign suppressed when zero
        assert_eq!(format_float(f64::NAN, 2), "nan");
        assert_eq!(format_float(f64::INFINITY, 2), "inf");
        assert_eq!(format_float(f64::NEG_INFINITY, 2), "-inf");
    }
    #[test]
    fn radix_lowercase_no_prefix() {
        assert_eq!(int_to_radix(255, 16), "ff");
        assert_eq!(int_to_radix(5, 2), "101");
        assert_eq!(int_to_radix(-255, 16), "-ff");
        assert_eq!(int_to_radix(0, 16), "0");
    }
}

#[cfg(test)]
mod capnet_tests {
    use super::{cap_net_root, http_get_with, shared_method, url_host};
    use crate::value::Value;

    /// `url_host` MUST stay byte-identical to `cgen.rs`'s own `k_url_host`
    /// C mirror -- it116's own host-scope check depends on both agreeing.
    #[test]
    fn url_host_strips_scheme_port_path_and_query() {
        assert_eq!(url_host("https://api.example.com/todos"), "api.example.com");
        assert_eq!(url_host("http://api.example.com:8080/path"), "api.example.com");
        assert_eq!(url_host("http://example.com"), "example.com");
        assert_eq!(url_host("example.com/no-scheme"), "example.com");
        assert_eq!(url_host("http://example.com?q=1"), "example.com");
        assert_eq!(url_host("http://example.com#frag"), "example.com");
    }

    #[test]
    fn limited_to_narrows_a_root_capability_and_is_idempotent_on_the_same_host() {
        let root = cap_net_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited = shared_method(&root, "limited_to", vec![Value::str("example.com")], &mut call)
            .expect("limiting a root capability must succeed");
        let Value::CapNet(c) = &limited else { panic!("must stay a CapNet") };
        assert_eq!(c.allowed_host.as_deref(), Some("example.com"));

        // re-limiting to the SAME host is a no-op success, not a widen error.
        let again = shared_method(&limited, "limited_to", vec![Value::str("example.com")], &mut call)
            .expect("re-limiting to the same host must succeed");
        assert_eq!(again, limited);
    }

    #[test]
    fn limited_to_refuses_to_widen_to_a_different_host() {
        let root = cap_net_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited = shared_method(&root, "limited_to", vec![Value::str("example.com")], &mut call).unwrap();
        let err = shared_method(&limited, "limited_to", vec![Value::str("other.com")], &mut call)
            .expect_err("narrowing an already-limited capability to a DIFFERENT host must be refused");
        assert!(err.contains("cannot widen"), "{err}");
    }

    #[test]
    fn http_get_with_rejects_an_out_of_scope_host_without_ever_shelling_out() {
        let root = cap_net_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited =
            shared_method(&root, "limited_to", vec![Value::str("127.0.0.1")], &mut call).unwrap();
        // A host mismatch must be caught BEFORE ever invoking `curl` -- this
        // deliberately targets an address `curl` would refuse to connect to
        // in this sandbox, so a passing result here proves the host-check
        // short-circuited rather than merely happening to fail for a
        // network reason.
        let result = http_get_with(&limited, "http://example.invalid.test/").unwrap();
        let Value::Ctor { ref variant, ref fields, .. } = result else { panic!("must be a Result value") };
        assert_eq!(&**variant, "Err");
        let Value::Str(msg) = &fields[0] else { panic!("Err payload must be a Str") };
        assert!(msg.contains("capability limited to `127.0.0.1`"), "{msg}");
    }
}

#[cfg(test)]
mod capfs_tests {
    // it118: mirrors `capnet_tests` exactly.
    use super::{cap_fs_root, read_file_with, shared_method};
    use crate::value::Value;

    #[test]
    fn limited_to_narrows_a_root_capability_and_is_idempotent_on_the_same_prefix() {
        let root = cap_fs_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited = shared_method(&root, "limited_to", vec![Value::str("/tmp/allowed")], &mut call)
            .expect("limiting a root capability must succeed");
        let Value::CapFs(c) = &limited else { panic!("must stay a CapFs") };
        assert_eq!(c.allowed_prefix.as_deref(), Some("/tmp/allowed"));

        let again = shared_method(&limited, "limited_to", vec![Value::str("/tmp/allowed")], &mut call)
            .expect("re-limiting to the same prefix must succeed");
        assert_eq!(again, limited);
    }

    #[test]
    fn limited_to_refuses_to_widen_to_a_different_prefix() {
        let root = cap_fs_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited = shared_method(&root, "limited_to", vec![Value::str("/tmp/allowed")], &mut call).unwrap();
        let err = shared_method(&limited, "limited_to", vec![Value::str("/tmp/other")], &mut call)
            .expect_err("narrowing an already-limited capability to a DIFFERENT prefix must be refused");
        assert!(err.contains("cannot widen"), "{err}");
    }

    #[test]
    fn read_file_with_rejects_an_out_of_scope_path_without_ever_touching_disk() {
        let root = cap_fs_root();
        let mut call = |_: Value, _: Vec<Value>| -> Result<Value, String> { panic!("no callback expected") };
        let limited =
            shared_method(&root, "limited_to", vec![Value::str("/tmp/allowed")], &mut call).unwrap();
        // `/etc/passwd` is outside the allowed prefix; a passing result here
        // proves the scope check short-circuited before any filesystem call.
        let result = read_file_with(&limited, "/etc/passwd").unwrap();
        let Value::Ctor { ref variant, ref fields, .. } = result else { panic!("must be a Result value") };
        assert_eq!(&**variant, "Err");
        let Value::Str(msg) = &fields[0] else { panic!("Err payload must be a Str") };
        assert!(msg.contains("capability limited to `/tmp/allowed`"), "{msg}");
    }
}

/// This file's FIRST test module targeting `concurrent component` machinery
/// directly (every OTHER concurrent-component test in this codebase spawns
/// the real compiled binary via `main.rs`'s own subprocess-based tests) --
/// needed here because testing `stop_all`'s own genuine-actor-panic
/// handling precisely requires constructing a synthetic `InstanceSlot::
/// Remote` whose thread panics on demand, which only a test living inside
/// this module (where `ActorHandle`'s fields are private, by design — see
/// its own doc comment) can do at all.
#[cfg(test)]
mod concurrent_tests {
    use super::{ActorHandle, ActorMsg, ActorRoute, Flow, Interp, InstanceSlot, ProgramDb};

    /// Production-hardening 1218: a REAL, live-confirmed-by-code-reading
    /// gap found+fixed -- `stop_all` used to discard `join()`'s `Result`
    /// via `let _ = ...`, silently swallowing ANY genuine Rust-level panic
    /// on an actor thread (distinct from a normal KUPL `panic()` call,
    /// which is already handled correctly elsewhere, and distinct from
    /// PR-it1213's now-fixed stack-overflow class, which is a process
    /// ABORT that never even reaches `join()` as an `Err`). Constructs a
    /// synthetic `Remote` instance whose thread panics on purpose (the
    /// cleanest way to exercise this specific mechanism directly, since a
    /// NATURALLY-occurring internal-invariant-violation trigger inside an
    /// actor thread isn't reliably constructible from ordinary KUPL
    /// source -- matching this campaign's own established "a real gap
    /// found by direct code reading, the fix is unconditionally correct
    /// regardless of how the trigger is naturally reached" precedent, e.g.
    /// PR-it1212/PR-it1215).
    #[test]
    fn stop_all_surfaces_a_genuine_actor_thread_panic_instead_of_silently_swallowing_it() {
        let compiled = crate::run::compile("fun main() {}\n").expect("trivial program must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new_bare(db);

        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
        let (_ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, super::Span)>>();
        let join = std::thread::spawn(|| {
            panic!("deliberate test panic, simulating a genuine internal interpreter bug reached on an actor thread")
        });
        interp.instances.push(InstanceSlot::Remote(ActorHandle {
            route: ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) },
            ready: Some(ready_rx),
            shutdown_panic: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let result = interp.stop_all(1);
        // `Flow` deliberately doesn't derive `Debug` (it holds a `Value`,
        // which doesn't either) -- describe the outcome by hand rather
        // than via `{:?}`.
        let describe = match &result {
            Ok(()) => "Ok(())".to_string(),
            Err(Flow::Panic { msg, .. }) => format!("Err(Flow::Panic {{ msg: {msg:?} }})"),
            Err(_) => "Err(<non-Panic Flow>)".to_string(),
        };
        assert!(
            matches!(result, Err(Flow::Panic { .. })),
            "a genuinely panicked actor thread must surface as Err(Flow::Panic), not {describe}"
        );
    }

    /// Companion: an actor thread that shuts down CLEANLY (no panic) must
    /// still report `Ok(())` -- the fix above must not turn every ordinary
    /// shutdown into a false-positive error.
    #[test]
    fn stop_all_still_returns_ok_when_every_actor_thread_shuts_down_cleanly() {
        let compiled = crate::run::compile("fun main() {}\n").expect("trivial program must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new_bare(db);

        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
        let (_ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, super::Span)>>();
        let join = std::thread::spawn(move || {
            // Exits cleanly once the inbox's Sender is dropped (stop_all's
            // own first loop does this via `handle.inbox.take()`).
            while inbox_rx.recv().is_ok() {}
        });
        interp.instances.push(InstanceSlot::Remote(ActorHandle {
            route: ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) },
            ready: Some(ready_rx),
            shutdown_panic: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let result = interp.stop_all(1);
        assert!(result.is_ok(), "a cleanly-shut-down actor thread must not be reported as a panic");
    }

    /// Production-hardening 1221: a REAL, LIVE-CONFIRMED bug found+fixed --
    /// before this fix, a genuine KUPL `Flow::Panic` raised by a
    /// `concurrent` component's OWN `on stop` handler was reported via
    /// `eprintln!` inside `instantiate_concurrent`'s closure and then
    /// silently discarded: the actor thread still returned normally (no
    /// Rust-level panic, so `join()` reports success), so the whole process
    /// exited 0 -- unlike the IDENTICAL `on stop { panic(…) }` on a plain
    /// (non-`concurrent`) component, live-confirmed to correctly produce
    /// `error[K0900]: panic: …` and exit code 101
    /// (`kupl run` on a real `.kupl` fixture, both variants, before writing
    /// this unit test). Constructs a synthetic `Remote` instance whose
    /// thread shuts down CLEANLY (like the companion test above) but writes
    /// a real `(message, span)` pair into `shutdown_panic` before
    /// returning -- exactly what `instantiate_concurrent`'s own closure now
    /// does when ITS `stop_all` call returns `Err(Flow::Panic { .. })`.
    /// Asserts BOTH that this surfaces as `Err(Flow::Panic)` (matching the
    /// panic-thread test above) AND that the ORIGINAL message survives
    /// verbatim, not a generic "bug in KUPL" placeholder -- distinguishing
    /// this from the Rust-level-thread-panic case above, which correctly
    /// DOES use a generic message (there is no original KUPL panic message
    /// to preserve in that case).
    #[test]
    fn stop_all_surfaces_a_genuine_on_stop_panic_from_an_actor_with_its_own_original_message() {
        let compiled = crate::run::compile("fun main() {}\n").expect("trivial program must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new_bare(db);

        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
        let (_ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, super::Span)>>();
        let shutdown_panic: std::sync::Arc<std::sync::Mutex<Option<(String, super::Span)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let shutdown_panic_writer = shutdown_panic.clone();
        let join = std::thread::spawn(move || {
            while inbox_rx.recv().is_ok() {}
            *shutdown_panic_writer.lock().unwrap() =
                Some(("deliberate on-stop panic message, verbatim".to_string(), super::Span::default()));
        });
        interp.instances.push(InstanceSlot::Remote(ActorHandle {
            route: ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) },
            ready: Some(ready_rx),
            shutdown_panic,
        }));

        let result = interp.stop_all(1);
        match result {
            Err(Flow::Panic { msg, .. }) => {
                assert_eq!(
                    msg, "deliberate on-stop panic message, verbatim",
                    "the actor's own on-stop panic message must survive verbatim, not be replaced by a generic placeholder"
                );
            }
            other => {
                let describe = match &other {
                    Ok(()) => "Ok(())".to_string(),
                    Err(_) => "Err(<non-Panic Flow>)".to_string(),
                };
                panic!("a real on-stop panic must surface as Err(Flow::Panic), not {describe}");
            }
        }
    }

    /// Production-hardening 1222: a REAL, LIVE-CONFIRMED bug found+fixed,
    /// symmetric to PR-it1221's shutdown-panic fix above -- before this
    /// fix, a genuine KUPL `Flow::Panic` raised during a `concurrent`
    /// component's OWN startup (its `on start` handler, or anything else
    /// in `instantiate_local`/`start_all`/`run_timers(100)`) was reported
    /// via `eprintln!` inside `instantiate_concurrent`'s closure and then
    /// silently discarded: `Interp::start_all`'s own `ready.recv()` used to
    /// discard whatever it read, so the whole process exited 0 -- unlike
    /// the IDENTICAL `on start { panic(…) }` on a plain (non-`concurrent`)
    /// component, live-confirmed to correctly produce `error[K0900]: panic:
    /// …` and exit code 101 (`kupl run` on a real `.kupl` fixture, both
    /// variants, before writing this unit test). Sends the panic payload
    /// directly on the `ready` channel BEFORE calling `start_all` (no
    /// thread synchronization needed -- unlike the shutdown-panic test
    /// above, `start_all` never joins the thread, only reads `ready`), then
    /// asserts the ORIGINAL message survives verbatim.
    #[test]
    fn start_all_surfaces_a_genuine_on_start_panic_from_an_actor_with_its_own_original_message() {
        let compiled = crate::run::compile("fun main() {}\n").expect("trivial program must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new_bare(db);

        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, super::Span)>>();
        let _ = ready_tx.send(Some(("deliberate on-start panic message, verbatim".to_string(), super::Span::default())));
        let join = std::thread::spawn(|| {});
        interp.instances.push(InstanceSlot::Remote(ActorHandle {
            route: ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) },
            ready: Some(ready_rx),
            shutdown_panic: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let result = interp.start_all();
        match result {
            Err(Flow::Panic { msg, .. }) => {
                assert_eq!(
                    msg, "deliberate on-start panic message, verbatim",
                    "the actor's own on-start panic message must survive verbatim, not be replaced by a generic placeholder"
                );
            }
            other => {
                let describe = match &other {
                    Ok(()) => "Ok(())".to_string(),
                    Err(_) => "Err(<non-Panic Flow>)".to_string(),
                };
                panic!("a real on-start panic must surface as Err(Flow::Panic), not {describe}");
            }
        }
    }

    /// Companion: an actor whose startup succeeds (a `None` on the `ready`
    /// channel, exactly what `instantiate_concurrent`'s closure sends on
    /// `Ok(())`) must still report `Ok(())` -- the fix above must not turn
    /// every ordinary startup into a false-positive error.
    #[test]
    fn start_all_still_returns_ok_when_every_actor_starts_cleanly() {
        let compiled = crate::run::compile("fun main() {}\n").expect("trivial program must compile");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new_bare(db);

        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel::<ActorMsg>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<(String, super::Span)>>();
        let _ = ready_tx.send(None);
        let join = std::thread::spawn(|| {});
        interp.instances.push(InstanceSlot::Remote(ActorHandle {
            route: ActorRoute::Dedicated { join: Some(join), inbox: Some(inbox_tx) },
            ready: Some(ready_rx),
            shutdown_panic: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let result = interp.start_all();
        assert!(result.is_ok(), "a cleanly-started actor must not be reported as a panic");
    }
}
