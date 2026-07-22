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
    Panic { msg: String, span: Span },
    Return(Value),
    Break,
    Continue,
}

pub type EvalResult = Result<Value, Flow>;

/// Owned, indexed view of the checked program.
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
}

pub struct Interp {
    pub db: ProgramDb,
    pub instances: Vec<Instance>,
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

impl Interp {
    pub fn new(db: ProgramDb) -> Interp {
        let image = Some(crate::parallel::ProgramImage::from_db(&db));
        Interp {
            db,
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
            image,
            call_depth: 0,
        }
    }

    /// A worker interpreter for the parallel fast path: no program image, so its
    /// own `par_map` calls stay sequential (no nested thread explosion).
    pub fn new_bare(db: ProgramDb) -> Interp {
        Interp {
            db,
            instances: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            print_unwired: false,
            globals: Env::new(),
            now: 0,
            image: None,
            call_depth: 0,
        }
    }

    fn panic_flow(msg: impl Into<String>, span: Span) -> Flow {
        Flow::Panic { msg: msg.into(), span }
    }

    // ---------------- component runtime ----------------

    /// Create an instance of `comp_name`; args are already-evaluated prop values.
    pub fn instantiate(
        &mut self,
        comp_name: &str,
        args: &[(Option<String>, Value)],
        span: Span,
    ) -> EvalResult {
        let Some(comp) = self.db.components.get(comp_name).cloned() else {
            return Err(Self::panic_flow(format!("unknown component `{comp_name}`"), span));
        };
        let env = self.globals.child();

        // props: by name or position, else default
        for (i, prop) in comp.props.iter().enumerate() {
            let supplied = args
                .iter()
                .enumerate()
                .find(|(j, (name, _))| match name {
                    Some(n) => n == &prop.name,
                    None => *j == i,
                })
                .map(|(_, (_, v))| v.clone());
            let value = match (supplied, &prop.default) {
                (Some(v), _) => v,
                (None, Some(d)) => self.eval(d, &env)?,
                (None, None) => {
                    return Err(Self::panic_flow(
                        format!("missing required prop `{}` for `{comp_name}`", prop.name),
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
        self.instances.push(Instance {
            comp: comp.clone(),
            env: env.clone(),
            wires: HashMap::new(),
            last_emit: HashMap::new(),
            restart_on_failure: false,
            timers: Vec::new(),
        });

        // children (constructed after the parent exists, in declaration order)
        let mut child_ids: HashMap<String, usize> = HashMap::new();
        for child in &comp.children {
            let mut child_args = Vec::new();
            for a in &child.args {
                let v = self.eval(&a.value, &env)?;
                child_args.push((a.name.clone(), v));
            }
            let v = self.instantiate(&child.component, &child_args, child.span)?;
            if let Value::Component(cid) = v {
                child_ids.insert(child.name.clone(), cid);
                let supervised = comp
                    .supervises
                    .iter()
                    .any(|s| s.child == child.name && s.policy == SupervisePolicy::RestartOnFailure);
                if supervised {
                    self.instances[cid].restart_on_failure = true;
                }
            }
            env.define(&child.name, v);
        }

        // wires: registered on the source child instance
        for wire in &comp.wires {
            let (from_child, from_port) = &wire.from;
            let (to_child, to_port) = &wire.to;
            let (Some(&src), Some(&dst)) = (child_ids.get(from_child), child_ids.get(to_child)) else {
                return Err(Self::panic_flow("wire references unknown child", wire.span));
            };
            self.instances[src]
                .wires
                .entry(from_port.clone())
                .or_default()
                .push((dst, to_port.clone()));
        }

        Ok(Value::Component(id))
    }

    /// Deliver `on start` to instance `id` and all its descendants (creation order).
    pub fn start_all(&mut self) -> Result<(), Flow> {
        for id in 0..self.instances.len() {
            self.run_lifecycle(id, &Trigger::Start)?;
            self.arm_timers(id);
        }
        self.drain()?;
        Ok(())
    }

    /// Arm the instance's timers relative to the current virtual time.
    fn arm_timers(&mut self, id: usize) {
        let comp = self.instances[id].comp.clone();
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
        self.instances[id].timers = timers;
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
            for (iid, inst) in self.instances.iter().enumerate() {
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
            let handler_idx = self.instances[iid].timers[ti].handler_idx;
            let comp = self.instances[iid].comp.clone();
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
                Err(Flow::Panic { msg, .. }) if self.instances[iid].restart_on_failure => {
                    self.restart(iid, &msg)?;
                    true
                }
                Err(other) => return Err(other),
            };
            self.drain()?;
            if !restarted {
                let t = &mut self.instances[iid].timers[ti];
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
            for (iid, inst) in self.instances.iter().enumerate() {
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

    fn run_lifecycle(&mut self, id: usize, trigger: &Trigger) -> Result<(), Flow> {
        let comp = self.instances[id].comp.clone();
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
            let comp = self.instances[id].comp.clone();
            for h in &comp.handlers {
                if matches!(&h.trigger, Trigger::Port(p) if p == &port) {
                    match self.run_handler(id, h, value.clone()) {
                        Ok(()) => {}
                        Err(Flow::Panic { msg, .. }) if self.instances[id].restart_on_failure => {
                            self.restart(id, &msg)?;
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
        let comp = self.instances[id].comp.clone();
        let env = self.instances[id].env.clone();
        for s in &comp.state {
            let v = self.eval(&s.init, &env)?;
            env.define(&s.name, v);
        }
        Ok(())
    }

    /// Supervision restart: reset state fields to their initial values, keep
    /// props/children/wires, re-run `on start`.
    fn restart(&mut self, id: usize, panic_msg: &str) -> Result<(), Flow> {
        let comp = self.instances[id].comp.clone();
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

    fn run_handler(&mut self, id: usize, h: &Handler, payload: Value) -> Result<(), Flow> {
        let env = self.instances[id].env.child();
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
        self.instances[id].last_emit.insert(port.to_string(), value.clone());
        let targets = self.instances[id].wires.get(port).cloned().unwrap_or_default();
        if targets.is_empty() {
            if self.print_unwired {
                let comp = self.instances[id].comp.name.clone();
                println!("{comp}.{port} = {value}");
            }
        } else {
            for (dst, dport) in targets {
                self.queue.push_back((dst, dport, value.clone()));
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
            let mut last = Value::Unit;
            for stmt in &block.stmts {
                last = self.exec_stmt(stmt, &scope)?;
            }
            Ok(last)
        } else {
            let mut last = Value::Unit;
            for stmt in &block.stmts {
                last = self.exec_stmt(stmt, env)?;
            }
            Ok(last)
        }
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
                let rhs = self.eval(value, env)?;
                let ExprKind::Ident(name) = &target.kind else {
                    return Err(Self::panic_flow("unsupported assignment target", *span));
                };
                let new_value = if *op == AssignOp::Set {
                    rhs
                } else {
                    let old = env.get(name).ok_or_else(|| {
                        Self::panic_flow(format!("unknown variable `{name}`"), *span)
                    })?;
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
                return Err(Self::panic_flow(
                    format!("property failed for {}{}", binding.join(", "), detail),
                    span,
                ));
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
            let child_env = self.instances[id].env.clone();
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
                // Fork-join: evaluate each independent branch and collect the
                // results into a list. Branches share only the (immutable)
                // enclosing scope, so evaluation order does not affect results;
                // v1.0-alpha runs them in deterministic branch order (a
                // multi-threaded scheduler is a later, semantics-preserving step).
                let mut results = Vec::with_capacity(branches.len());
                for b in branches {
                    results.push(self.eval(b, env)?);
                }
                Ok(Value::List(Rc::new(results)))
            }
        }
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
            let comp = self.instances[id].comp.clone();
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
                | ("hex_decode", 1) | ("hash_fnv", 1) => {
                    let v = self.eval(&args[0].value, env)?;
                    return encoding_builtin(name, &[v]).map_err(|m| Self::panic_flow(m, span));
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
                    let comp = self.instances[id].comp.clone();
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
                        let base = self.instances[id].env.clone();
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
                    for (i, a) in args.iter().enumerate() {
                        let v = self.eval(&a.value, env)?;
                        let idx = match &a.name {
                            Some(n) => field_names.iter().position(|f| f == n).ok_or_else(|| {
                                Self::panic_flow(format!("`{name}` has no field `{n}`"), a.value.span)
                            })?,
                            None => i,
                        };
                        if idx < fields.len() {
                            fields[idx] = v;
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
                Err(Flow::Panic { msg, span })
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

    fn call_fun(&mut self, decl: &FunDecl, args: Vec<Value>, base_env: &Env, span: Span) -> EvalResult {
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

    fn eval_method(&mut self, recv: Value, name: &str, args: Vec<Value>, span: Span) -> EvalResult {
        // component expose call
        if let Value::Component(id) = recv {
            let comp = self.instances[id].comp.clone();
            let Some(decl) = comp.exposes.iter().chain(comp.funs.iter()).find(|f| f.name == name) else {
                return Err(Self::panic_flow(
                    format!("component `{}` does not expose `{name}`", comp.name),
                    span,
                ));
            };
            let instance_env = self.instances[id].env.clone();
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
                if self.instances[id].restart_on_failure {
                    self.restart(id, msg)?;
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
        if self.db.funs.contains_key(name) {
            match builtin_method(recv.clone(), name, args.clone(), span, self) {
                Err(Flow::Panic { msg, .. }) if msg.contains("has no method") => {
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
        builtin_method(recv, name, args, span, self)
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
        (Value::List(items), "sum") if matches!(items.first(), Some(Value::SizedInt(_) | Value::F32(_) | Value::BigInt(_) | Value::Rational(_))) => {
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
        (Value::List(items), "product") if matches!(items.first(), Some(Value::SizedInt(_) | Value::F32(_) | Value::BigInt(_) | Value::Rational(_))) => {
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
        (Value::Float(v), "to_str") => Ok(Value::str(v.to_string())),
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
        _ => Err("`min`/`max` need Int, Float, Str, or another orderable type".into()),
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

fn builtin_method(
    recv: Value,
    name: &str,
    args: Vec<Value>,
    span: Span,
    interp: &mut Interp,
) -> EvalResult {
    // real-thread fast path: `xs.par_map(pure_fn)` over a large list. Falls
    // through to the sequential shared_method on any non-qualifying call.
    if let Some(image) = interp.image.clone() {
        if let Some(res) = crate::parallel::try_par_map(&recv, name, &args, &image)
            .or_else(|| crate::parallel::try_par_filter(&recv, name, &args, &image))
        {
            return res.map_err(|msg| Flow::Panic { msg, span });
        }
    }
    let mut call = |f: Value, args: Vec<Value>| -> Result<Value, String> {
        match interp.call_value(f, args, span) {
            Ok(v) => Ok(v),
            Err(Flow::Panic { msg, .. }) => Err(msg),
            Err(_) => Err("invalid control flow in callback".into()),
        }
    };
    match shared_method(&recv, name, args, &mut call) {
        Ok(v) => Ok(v),
        Err(msg) => Err(Flow::Panic { msg, span }),
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
