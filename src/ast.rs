//! Abstract syntax tree for KUPL v0.1.

use crate::diag::Span;

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub items: Vec<Item>,
    /// `use` declarations: dotted module path + span (resolved by the loader).
    pub uses: Vec<(String, Span)>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Fun(FunDecl),
    Type(TypeDecl),
    Component(ComponentDecl),
    Contract(ContractDecl),
    /// A top-level `law "name" { … }` — a free-standing test (property or
    /// concrete) run by `kupl test`.
    Law(Law),
    /// `protocol Foo { forbids io.net }` (`docs/design/AGENTS.md` §3) — a
    /// rule set an `agent` may `follows`. v1 is structural-only: each
    /// `forbids` names an effect (the SAME dotted vocabulary `uses`/`add
    /// uses` already use, e.g. `io`, `io.net`, `io.fs`) that none of a
    /// following agent's own exposed funs may (transitively) perform.
    Protocol(ProtocolDecl),
}

/// See `Item::Protocol`'s own doc comment.
#[derive(Debug, Clone)]
pub struct ProtocolDecl {
    pub name: String,
    pub intent: Option<String>,
    /// Each entry is a forbidden effect name (e.g. `"io.net"`) and the
    /// span of that specific `forbids` clause, for a diagnostic that
    /// points at the RULE, not just the whole `protocol` declaration.
    pub forbids: Vec<(String, Span)>,
    /// `guard Name: Type { expect result ... }` (`docs/design/AGENTS.md`
    /// §3, the behavioral/runtime-checked slice) — a NAMED, concretely
    /// typed postcondition an agent's own exposed fun opts into via
    /// `guards Name` (`FunDecl.guards`). `Type` is fixed HERE, at the
    /// protocol's own declaration site — mirroring `ContractDecl.laws`'
    /// own body, which type-checks against the CONTRACT's own `sigs`,
    /// not any fulfilling component's types (see `check_contract`) — so
    /// a guard's body is well-typed regardless of which agent, or which
    /// of an agent's own exposed funs, ultimately opts in.
    pub guards: Vec<GuardDecl>,
    pub span: Span,
}

/// See `ProtocolDecl.guards`'s own doc comment.
#[derive(Debug, Clone)]
pub struct GuardDecl {
    pub name: String,
    /// The FIXED type `result` is bound to inside `body` — chosen by the
    /// protocol author, independent of any following agent.
    pub ty: TyExpr,
    pub body: Block,
    pub span: Span,
}

/// `contract Store { expose fun get(...) -> ...  law "..." { ... } }`
#[derive(Debug, Clone)]
pub struct ContractDecl {
    pub name: String,
    pub intent: Option<String>,
    pub sigs: Vec<FunSig>,
    pub laws: Vec<Law>,
    pub span: Span,
}

/// A body-less function signature inside a contract.
#[derive(Debug, Clone)]
pub struct FunSig {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<TyExpr>,
    pub effects: Vec<String>,
    pub span: Span,
}

/// `law "put then get returns the value" { ... }` — an executable property
/// run by `kupl test` against every component that fulfills the contract.
#[derive(Debug, Clone)]
pub struct Law {
    pub name: String,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunDecl {
    pub name: String,
    /// Type parameters: `fun first[T](xs: List[T]) -> Option[T]`
    pub type_params: Vec<String>,
    /// Bounded generics (it103, universal-language enrichment campaign):
    /// type parameter name -> bound name, e.g. `fun sort[T: Ord](...)` ->
    /// `{"T": "Ord"}`. Empty for ordinary, unbounded generics (the
    /// overwhelming common case) -- a SEPARATE, additive field rather than
    /// widening `type_params` itself, to avoid touching that field's own
    /// 28 existing call sites across check.rs/fmt.rs/lsp.rs/resolve.rs/
    /// sdiff.rs, none of which need to know about bounds. `"Ord"` is
    /// currently the ONLY supported bound (checked at parse time, K0123) --
    /// a built-in, compiler-recognized bound rather than routed through the
    /// existing `contract`/`fulfills` system, which today only models a
    /// COMPONENT exposing certain methods, not a primitive/user TYPE
    /// supporting certain operators; generalizing contracts to cover types
    /// is a separate, larger future step (LANGUAGE.md's own "Generics with
    /// contract bounds" framing anticipates it, but doesn't require this
    /// increment to build the general mechanism first).
    pub type_param_bounds: std::collections::HashMap<String, String>,
    pub params: Vec<Param>,
    pub ret: Option<TyExpr>,
    pub effects: Vec<String>,
    pub body: Block,
    pub is_pub: bool,
    /// `ai fun` — a typed prompt function; the body is the AiDecl, not code.
    pub ai: Option<AiDecl>,
    /// `guards Name1, Name2` (`docs/design/AGENTS.md` §3, behavioral
    /// protocol rules) — names of `GuardDecl`s this fun opts into, each
    /// with the span of that specific reference (for a diagnostic that
    /// points at the RIGHT name, not just the whole fun). Parses on ANY
    /// `fun` (mirrors `weight`/`follows`'s own "parser accepts broadly,
    /// checker narrows" precedent) but is checker-restricted to an
    /// `expose fun` on an `agent` that `follows` the guard's own owning
    /// protocol (new K10xx). A separate desugaring pass (`guards.rs`,
    /// run from `run::compile`'s own pipeline, mirroring `callargs::
    /// resolve_call_args`'s position) rewrites a validated `guards`-
    /// bearing fun's body BEFORE type-checking proper -- `fmt`/`lsp`
    /// never see the desugared form, only the original source shape.
    pub guards: Vec<(String, Span)>,
    pub span: Span,
}

/// Body of an `ai fun`: `{ intent "..."  model "..." }` (model optional).
/// `tools` names top-level functions the model may call while answering.
#[derive(Debug, Clone)]
pub struct AiDecl {
    /// Flattened source form (`{expr}` kept literal) — for fmt/manifest/diff.
    pub intent: String,
    /// The intent as an interpolated string expression, evaluated in the ai
    /// fun's parameter scope at call time so `{param}` substitutes real values.
    pub intent_expr: Expr,
    pub model: Option<String>,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: TyExpr,
    /// Optional default value (`x: Int = EXPR`). Defaults must be trailing.
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
    /// Type parameters (`type Box[T]` -> `["T"]`); empty for monomorphic types.
    pub type_params: Vec<String>,
    pub variants: Vec<Variant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Param>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ComponentDecl {
    pub name: String,
    pub is_app: bool,
    /// `concurrent component Foo { ... }` — opts this component into real
    /// multi-threaded execution (interp-only; VM/native ignore this flag
    /// entirely and always run sequentially, per
    /// `docs/design/ASYNC.md` §8.1/§8.8). A soft keyword, matching `ai`/
    /// `state`/`out`'s own contextual-keyword precedent (token.rs) rather
    /// than a globally reserved word.
    pub concurrent: bool,
    /// `agent Foo { .. }` (`docs/design/AGENTS.md`) — `true` for an
    /// `agent` declaration (a hard keyword, `parser.rs`'s own `KwAgent`),
    /// always implies `concurrent: true` (an agent is inherently its own
    /// actor -- there is no "sequential agent"). Kept as a SEPARATE flag
    /// from `concurrent` (rather than folding agents into plain
    /// `concurrent component`s) so diagnostics/tooling can distinguish
    /// "this is an agent" from "this is a concurrent component," and so
    /// future agent-only fields (`weight` below, later `protocol`/
    /// `follows`) have an unambiguous place to attach without touching
    /// every existing `concurrent component` construction site.
    pub is_agent: bool,
    /// `agent`-only: `weight <lightweight|heavyweight|distributed>` picks
    /// which of KUPL's existing concurrency tiers backs this agent (see
    /// `docs/design/AGENTS.md` §4's own mapping table). `None` (the
    /// default, and the only legal value for a plain `concurrent
    /// component`) means `Lightweight`. Always `None` when `is_agent` is
    /// `false`.
    pub weight: Option<AgentWeight>,
    /// `agent Foo follows Protocol1, Protocol2 { .. }` (`docs/design/
    /// AGENTS.md` §3) — protocol names this agent commits to. Parsed
    /// the SAME way as `fulfills` (right after the component name),
    /// but semantically agent-only (K1003) -- a plain `component`
    /// parses the clause too (one uniform grammar rule) but is always
    /// rejected by the checker. Always empty when `is_agent` is `false`.
    pub follows: Vec<String>,
    pub fulfills: Vec<String>,
    pub intent: Option<String>,
    pub ports: Vec<Port>,
    pub props: Vec<PropDecl>,
    pub state: Vec<StateField>,
    pub children: Vec<ChildDecl>,
    pub wires: Vec<WireDecl>,
    pub supervises: Vec<SuperviseDecl>,
    pub handlers: Vec<Handler>,
    pub exposes: Vec<FunDecl>,
    pub funs: Vec<FunDecl>,
    pub examples: Vec<Example>,
    pub span: Span,
}

/// `docs/design/AGENTS.md` §4's own table: which existing KUPL
/// concurrency tier backs an `agent`. `Distributed` parses (K0316
/// rejects it at check time, mirroring K0309's own "distributed
/// placement parses but isn't implemented yet" precedent) -- real
/// transport doesn't exist yet, see `docs/design/DISTRIBUTION.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWeight {
    /// Go-goroutine-style: multiplexed onto a shared `ActorPool` worker
    /// thread (`interp.rs::ActorRoute::Pooled`) -- the default.
    Lightweight,
    /// Java-thread-style: a dedicated, always-resident OS thread
    /// (`interp.rs::ActorRoute::Dedicated`), even at the top level (not
    /// just the pre-existing nested-spawn fallback case).
    Heavyweight,
    /// Erlang-distribution-style: a remote node. Not yet implemented --
    /// see `docs/design/DISTRIBUTION.md`'s own Phase 6+ note.
    Distributed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
    In,
    Out,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub dir: PortDir,
    pub name: String,
    pub ty: TyExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PropDecl {
    pub name: String,
    pub ty: TyExpr,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StateField {
    pub name: String,
    pub ty: Option<TyExpr>,
    pub init: Expr,
    pub span: Span,
}

/// `let child = Component(args)` inside a component body.
#[derive(Debug, Clone)]
pub struct ChildDecl {
    pub name: String,
    pub component: String,
    pub args: Vec<Arg>,
    /// `at <expr>` (e.g. `at node("api.shop.internal")`, `at node(local)`) —
    /// `docs/design/DISTRIBUTION.md`'s own placement syntax, reusing the
    /// SAME `at` prefix `docs/design/LANGUAGE.md`/`VISION.md` already
    /// document (aspirationally, never implemented before now) for
    /// hardware placement (`at(gpu) f(x)`). Kept as a general `Expr`
    /// here, not a dedicated `Placement` enum — the CHECKER is what
    /// currently understands (and rejects, K0309) the one recognized
    /// shape, `node(...)`; keeping the AST general avoids over-
    /// committing this field's own shape before runtime distribution
    /// semantics are actually implemented. `None` for every child that
    /// doesn't use it — the overwhelming majority, and the only shape
    /// any runtime engine currently executes. Boxed (not a bare
    /// `Option<Expr>`) so the common `None` case only costs one pointer
    /// (8 bytes) on `ChildDecl` instead of `size_of::<Expr>()` (80 bytes,
    /// since `Expr` has no niche for `Option` to reuse) -- found via a
    /// live `perf_guard_100000_concurrent_actors_stay_under_a_memory_bound_between_shared_and_cloned_db`
    /// regression: 100,000 sibling `ChildDecl`s x 80 extra bytes tipped a
    /// tightly-calibrated 650MB `--max-memory` guard test over its bound.
    pub placement: Option<Box<Expr>>,
    pub span: Span,
}

/// A constructor argument: positional or named (`prop title: "..."` style is
/// written `title: "..."`).
#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct WireDecl {
    pub from: (String, String),
    pub to: (String, String),
    pub span: Span,
}

/// `supervise child restart on_failure` / `supervise child restart never` /
/// `supervise child restart on_failure max 5 in 10s`
#[derive(Debug, Clone)]
pub struct SuperviseDecl {
    pub child: String,
    pub policy: SupervisePolicy,
    /// Restart-intensity limit (BEAM/Erlang-inspired `max_restarts`/
    /// `max_seconds`): `Some((n, window_ms))` means more than `n` restarts
    /// within any sliding `window_ms` virtual-time window escalates the
    /// panic instead of restarting again (matching what an unsupervised
    /// child's failure already does) -- a safety valve against an
    /// unbounded panic/restart crash loop. `None` (the syntax is omitted)
    /// preserves today's exact unlimited-restart behavior; only meaningful
    /// alongside `SupervisePolicy::RestartOnFailure`.
    pub max_restarts: Option<(u32, i64)>,
    /// Concurrency-v2 PR-cv2-1 (`docs/design/CONCURRENCY_V2.md` §4.1,
    /// Erlang-inspired): which OTHER supervised siblings, if any, restart
    /// alongside this child when IT fails. `OneForOne` (the default,
    /// omitted syntax) preserves today's exact single-child-only restart
    /// behavior. Only meaningful alongside `SupervisePolicy::
    /// RestartOnFailure` -- deliberately a PER-CHILD setting, not a
    /// per-parent/per-supervisor one the way real Erlang/OTP scopes it,
    /// since KUPL's own `supervise` syntax is already per-child-
    /// declared, not a separate supervisor entity -- see that design
    /// doc section for the full reasoning behind this deliberate
    /// adaptation rather than a literal port.
    pub strategy: RestartStrategy,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisePolicy {
    /// A panic resets the child's state and re-runs `on start`; the app lives.
    RestartOnFailure,
    /// A panic escalates (default behavior when unsupervised).
    Never,
}

/// Concurrency-v2 PR-cv2-1: which siblings restart alongside a failed,
/// supervised child -- Erlang/OTP's own three-strategy vocabulary,
/// adapted to KUPL's per-child (not per-supervisor) `supervise` syntax.
/// See `SuperviseDecl::strategy`'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartStrategy {
    /// Only the failed child itself restarts. Default (the syntax is
    /// omitted) -- today's exact, unchanged behavior.
    #[default]
    OneForOne,
    /// Every OTHER sibling that also declares `restart on_failure` (of
    /// any strategy) restarts too, not just the one that failed.
    OneForAll,
    /// Every `restart on_failure`-declared sibling declared AFTER this
    /// one, in the parent's own `children` declaration order, restarts
    /// too; siblings declared before it are untouched.
    RestForOne,
}

#[derive(Debug, Clone)]
pub enum Trigger {
    Port(String),
    Start,
    Stop,
    /// `on every 5s { … }` — recurring timer; interval in virtual milliseconds.
    Every(i64),
    /// `on after 2s { … }` — one-shot timer; delay in virtual milliseconds.
    After(i64),
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub trigger: Trigger,
    /// Binder for the message payload: `on filter(q) { … }`.
    pub param: Option<String>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Example {
    pub steps: Vec<ExampleStep>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExampleStep {
    /// `send click` or `send filter("milk")`
    Send { port: String, arg: Option<Expr>, span: Span },
    /// `expect value == 2` — any Bool expression; out-port names are bound to
    /// the last value emitted on that port.
    Expect { expr: Expr, span: Span },
    /// `advance 5s` — move the virtual clock forward, firing due timers.
    Advance { ms: i64, span: Span },
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        name: String,
        ty: Option<TyExpr>,
        init: Expr,
        mutable: bool,
        span: Span,
    },
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
        span: Span,
    },
    Expr(Expr),
    Return(Option<Expr>, Span),
    While {
        cond: Expr,
        body: Block,
        span: Span,
    },
    For {
        var: String,
        iter: Expr,
        body: Block,
        span: Span,
    },
    Emit {
        port: String,
        arg: Option<Expr>,
        span: Span,
    },
    /// `expect expr` — runtime assertion (the workhorse of laws and tests).
    Expect(Expr, Span),
    /// `forall x: Int, y: Str { … }` — property-based test: the body runs over
    /// many generated bindings; a failing case is reported (shrunk).
    Forall {
        vars: Vec<(String, TyExpr)>,
        body: Block,
        span: Span,
    },
    Break(Span),
    Continue(Span),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    /// String literal with interpolation parts (already parsed sub-expressions).
    Str(Vec<StrPiece>),
    /// A `Char` literal (`'a'`) -- a single Unicode scalar value.
    Char(char),
    List(Vec<Expr>),
    Ident(String),
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    MethodCall {
        recv: Box<Expr>,
        name: String,
        args: Vec<Arg>,
    },
    Field {
        recv: Box<Expr>,
        name: String,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then_block: Block,
        else_block: Option<Box<Expr>>, // Block-expr or another If
    },
    BlockExpr(Block),
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Lambda {
        params: Vec<LambdaParam>,
        body: Block,
    },
    Range {
        lo: Box<Expr>,
        hi: Box<Expr>,
        inclusive: bool,
    },
    /// `expr with field: value, …` — record update (new value, fields replaced).
    With {
        recv: Box<Expr>,
        updates: Vec<(String, Expr)>,
    },
    /// A width-suffixed integer literal (`255u8`, `1000i16`).
    SizedInt(i128, crate::value::IntW),
    /// An `f32`-suffixed float literal (`1.5f32`).
    F32(f32),
    /// `expr?` — Result propagation.
    Try(Box<Expr>),
    Await(Box<Expr>),
    /// `par { e1  e2  … }` — structured fork-join over independent branches;
    /// evaluates to `List[T]` of the branch results (all branches same type).
    /// Branches are independent (no data flows between them), so they are safe
    /// to run in parallel; execution is deterministic (results in branch order).
    Par(Vec<Expr>),
    /// `receive { port1(pat1) [if guard1] => { .. }  ... }` — Erlang-style
    /// selective receive over a `concurrent component`'s own mailbox: waits
    /// for the next message addressed to one of the named in-ports whose
    /// payload matches that arm's pattern (and guard, if any), SKIPPING
    /// OVER (not discarding) any earlier-arrived non-matching message so
    /// it remains available for a later handler/receive to consume, in its
    /// original relative order — see `docs/design/ASYNC.md`'s own
    /// "Selective receive" section. `concurrent`-only (K0312); may only
    /// appear as the ENTIRE right-hand side of a top-level `let` in an
    /// `on <port>` handler or exposed-fun body (K0310) — NOT a bare
    /// top-level statement (unlike the wording a blocking builtin's own
    /// K0295 restriction might suggest by analogy — `interp.rs::
    /// exec_stmts_checked`'s own frame-push logic only knows how to
    /// capture a continuation from a `Stmt::Let`'s own bind name, and not
    /// `on start`/`on stop`/`on every`/`on after` either, since those run
    /// via a path that was never built to expect a suspend escaping it).
    /// Deliberately does NOT extend to nested private funs in this v1
    /// (unlike K0296's blocking-builtin transitive closure) — a `receive`
    /// must appear directly in the handler/exposed-fun's own body, not
    /// several call frames deep.
    Receive { arms: Vec<ReceiveArm> },
    /// `<method-call> timeout <duration>` (e.g. `p.wait_for_go() timeout
    /// 2s`) — bounds how long a `Call` to a `concurrent component`'s own
    /// exposed fun may block before giving up with a clean panic instead
    /// of waiting forever, see `docs/design/ASYNC.md`'s own "Call
    /// timeout" section. `call` is always a `MethodCall` (checker-
    /// enforced, K0313, mirroring `receive`'s own shape-restriction
    /// style) whose receiver's static type must be a `concurrent`
    /// component — a timeout on an ordinary (non-blocking, same-thread)
    /// call is meaningless. `timeout_ms` is a plain literal duration
    /// (`parser.rs`'s own `parse_duration`, the SAME parse `on every`/
    /// `on after` already use), not a general expression — consistent
    /// with those, and avoids needing a whole new "is this expression a
    /// valid duration" checker rule for a first slice. On the VM/native
    /// engines (`compile.rs`), this compiles straight through to the
    /// wrapped call itself, unchanged — those engines run every
    /// `concurrent` component sequentially (§8.8), so a call there can
    /// never actually block long enough to time out; the wrapper is
    /// therefore byte-identically a no-op, not a rejected construct like
    /// `receive` (K0809) needed to be.
    CallWithTimeout { call: Box<Expr>, timeout_ms: i64 },
}

/// One arm of a `receive { .. }` expression — see `ExprKind::Receive`'s own
/// doc comment.
#[derive(Debug, Clone)]
pub struct ReceiveArm {
    pub port: String,
    pub pattern: Pattern,
    /// Optional `if COND` guard, checked with the pattern's own bindings in
    /// scope — a false guard is treated as "this arm doesn't match" (the
    /// message is left in the mailbox, not consumed), exactly like `match`.
    pub guard: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StrPiece {
    Text(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub struct LambdaParam {
    pub name: String,
    pub ty: Option<TyExpr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// Optional `if COND` guard: the arm matches only when the pattern binds and
    /// the guard is true; a failed guard falls through to the next arm.
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum PatternKind {
    Wildcard,
    Bind(String),
    Int(i64),
    Bool(bool),
    Str(String),
    /// `Circle(r)`, `Some(x)`, `None`
    Ctor { name: String, args: Vec<Pattern> },
    /// `A | B | C` — matches if any alternative matches. Alternatives may not
    /// bind variables (checked), so no binding-merge is needed.
    Or(Vec<Pattern>),
    /// `name @ SUBPATTERN` — binds `name` to the whole value and matches inner.
    At { name: String, inner: Box<Pattern> },
    /// `lo..hi` (half-open) / `lo..=hi` (inclusive) — Int range pattern.
    Range { lo: i64, hi: i64, inclusive: bool },
}

/// Type syntax as written in source.
#[derive(Debug, Clone)]
pub struct TyExpr {
    pub kind: TyExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TyExprKind {
    /// `Int`, `Str`, `Shape`, `Counter` …
    Name(String),
    /// `List[Int]`, `Option[Str]`, `Result[Int, Str]`
    Generic(String, Vec<TyExpr>),
    /// `fn(Int, Str) -> Bool`
    Fun(Vec<TyExpr>, Box<TyExpr>),
}
