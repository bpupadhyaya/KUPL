//! Token definitions for the KUPL lexer.

use crate::diag::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Literals
    Int(i64),
    /// A width-suffixed integer literal (`255u8`, `1000i16`), value in i128.
    SizedInt(i128, crate::value::IntW),
    /// An `f32`-suffixed float literal (`1.5f32`).
    F32Lit(f32),
    Float(f64),
    /// String literal, decomposed into literal text and `{expr}` interpolation parts.
    Str(Vec<StrPart>),
    Ident(String),

    // Keywords (reserved)
    KwComponent,
    KwApp,
    KwContract,
    KwType,
    KwFun,
    KwLet,
    KwVar,
    KwPub,
    KwIntent,
    KwRequires,
    KwProp,
    KwIn,
    KwOut,
    KwState,
    KwOn,
    KwExpose,
    KwEmit,
    KwSend,
    KwWire,
    KwSupervise,
    KwExample,
    KwTest,
    KwExpect,
    KwIf,
    KwElse,
    KwMatch,
    KwFor,
    KwWhile,
    KwBreak,
    KwContinue,
    KwReturn,
    KwUses,
    KwAsync,
    KwAwait,
    KwPar,
    KwTrue,
    KwFalse,
    KwFn,
    KwNew,
    KwUse,
    KwModule,
    KwStart,
    KwStop,

    // Operators & punctuation
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,
    OrOr,
    Bang,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    DotDot,
    DotDotEq,
    Arrow,     // ->
    FatArrow,  // =>
    Question,
    At, // @
    Dot,
    Comma,
    Colon,
    Pipe,
    PipeGt, // |>
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,

    Newline,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// Literal text.
    Text(String),
    /// An interpolated `{expr}`: raw source of the expression plus its byte offset
    /// in the file (so the parser can re-lex it with correct spans).
    Expr(String, u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "component" => Tok::KwComponent,
        "app" => Tok::KwApp,
        "contract" => Tok::KwContract,
        "type" => Tok::KwType,
        "fun" => Tok::KwFun,
        "let" => Tok::KwLet,
        "var" => Tok::KwVar,
        "pub" => Tok::KwPub,
        "intent" => Tok::KwIntent,
        "requires" => Tok::KwRequires,
        "prop" => Tok::KwProp,
        "in" => Tok::KwIn,
        "on" => Tok::KwOn,
        "expose" => Tok::KwExpose,
        "emit" => Tok::KwEmit,
        "send" => Tok::KwSend,
        "wire" => Tok::KwWire,
        "supervise" => Tok::KwSupervise,
        "example" => Tok::KwExample,
        "test" => Tok::KwTest,
        "expect" => Tok::KwExpect,
        "if" => Tok::KwIf,
        "else" => Tok::KwElse,
        "match" => Tok::KwMatch,
        "for" => Tok::KwFor,
        "while" => Tok::KwWhile,
        "break" => Tok::KwBreak,
        "continue" => Tok::KwContinue,
        "return" => Tok::KwReturn,
        "uses" => Tok::KwUses,
        "async" => Tok::KwAsync,
        "await" => Tok::KwAwait,
        "par" => Tok::KwPar,
        "true" => Tok::KwTrue,
        "false" => Tok::KwFalse,
        "fn" => Tok::KwFn,
        "new" => Tok::KwNew,
        "use" => Tok::KwUse,
        "module" => Tok::KwModule,
        _ => return None,
    })
}

impl Tok {
    /// Tokens after which a newline does NOT terminate a statement (continuation).
    pub fn suppresses_newline(&self) -> bool {
        matches!(
            self,
            Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Slash
                | Tok::Percent
                | Tok::EqEq
                | Tok::NotEq
                | Tok::Lt
                | Tok::LtEq
                | Tok::Gt
                | Tok::GtEq
                | Tok::AndAnd
                | Tok::OrOr
                | Tok::Eq
                | Tok::PlusEq
                | Tok::MinusEq
                | Tok::StarEq
                | Tok::SlashEq
                | Tok::Arrow
                | Tok::FatArrow
                | Tok::Comma
                | Tok::Dot
                | Tok::Colon
                | Tok::Pipe
                | Tok::PipeGt
                | Tok::LParen
                | Tok::LBracket
                | Tok::Newline
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Tok::Int(v) => format!("integer `{v}`"),
            Tok::SizedInt(v, w) => format!("integer `{v}{}`", w.name()),
            Tok::F32Lit(v) => format!("float `{v}f32`"),
            Tok::Float(v) => format!("float `{v}`"),
            Tok::Str(_) => "string literal".to_string(),
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Newline => "end of line".to_string(),
            Tok::Eof => "end of file".to_string(),
            other => format!("`{}`", other.symbol()),
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            Tok::KwComponent => "component",
            Tok::KwApp => "app",
            Tok::KwContract => "contract",
            Tok::KwType => "type",
            Tok::KwFun => "fun",
            Tok::KwLet => "let",
            Tok::KwVar => "var",
            Tok::KwPub => "pub",
            Tok::KwIntent => "intent",
            Tok::KwRequires => "requires",
            Tok::KwProp => "prop",
            Tok::KwIn => "in",
            Tok::KwOut => "out",
            Tok::KwState => "state",
            Tok::KwOn => "on",
            Tok::KwExpose => "expose",
            Tok::KwEmit => "emit",
            Tok::KwSend => "send",
            Tok::KwWire => "wire",
            Tok::KwSupervise => "supervise",
            Tok::KwExample => "example",
            Tok::KwTest => "test",
            Tok::KwExpect => "expect",
            Tok::KwIf => "if",
            Tok::KwElse => "else",
            Tok::KwMatch => "match",
            Tok::KwFor => "for",
            Tok::KwWhile => "while",
            Tok::KwBreak => "break",
            Tok::KwContinue => "continue",
            Tok::KwReturn => "return",
            Tok::KwUses => "uses",
            Tok::KwAsync => "async",
            Tok::KwAwait => "await",
            Tok::KwPar => "par",
            Tok::KwTrue => "true",
            Tok::KwFalse => "false",
            Tok::KwFn => "fn",
            Tok::KwNew => "new",
            Tok::KwUse => "use",
            Tok::KwModule => "module",
            Tok::KwStart => "start",
            Tok::KwStop => "stop",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Lt => "<",
            Tok::LtEq => "<=",
            Tok::Gt => ">",
            Tok::GtEq => ">=",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::Bang => "!",
            Tok::Eq => "=",
            Tok::PlusEq => "+=",
            Tok::MinusEq => "-=",
            Tok::StarEq => "*=",
            Tok::SlashEq => "/=",
            Tok::DotDot => "..",
            Tok::DotDotEq => "..=",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::Question => "?",
            Tok::At => "@",
            Tok::Dot => ".",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::Pipe => "|",
            Tok::PipeGt => "|>",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            _ => "?",
        }
    }
}
