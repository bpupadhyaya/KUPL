//! Abstract syntax tree for KUPL v0.1.

use crate::diag::Span;
use crate::token::StrPart;

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
    pub params: Vec<Param>,
    pub ret: Option<TyExpr>,
    pub effects: Vec<String>,
    pub body: Block,
    pub is_pub: bool,
    /// `ai fun` — a typed prompt function; the body is the AiDecl, not code.
    pub ai: Option<AiDecl>,
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
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
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

/// `supervise child restart on_failure` / `supervise child restart never`
#[derive(Debug, Clone)]
pub struct SuperviseDecl {
    pub child: String,
    pub policy: SupervisePolicy,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisePolicy {
    /// A panic resets the child's state and re-runs `on start`; the app lives.
    RestartOnFailure,
    /// A panic escalates (default behavior when unsupervised).
    Never,
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
    List(Vec<Expr>),
    Ident(String),
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    MethodCall {
        recv: Box<Expr>,
        name: String,
        args: Vec<Expr>,
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

impl std::convert::From<&StrPart> for StrPiece {
    fn from(part: &StrPart) -> Self {
        match part {
            StrPart::Text(t) => StrPiece::Text(t.clone()),
            StrPart::Expr(src, _) => StrPiece::Text(format!("{{{src}}}")),
        }
    }
}
