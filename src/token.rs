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

    // Keywords (reserved everywhere, matched by `keyword()` below). `out`,
    // `state`, `start`, and `stop` are deliberately NOT here (and NOT in
    // `keyword()`) -- they're CONTEXTUAL/soft keywords, recognized only in
    // specific syntactic positions via `Tok::Ident(s) if s == "out"`-style
    // matching directly in `parser.rs` (see `at_port_direction`/the `state`/
    // `on start`/`on stop` handling there), so a variable or function named
    // `state`/`start`/etc. stays legal everywhere else. This file used to
    // ALSO declare `KwOut`/`KwState`/`KwStart`/`KwStop` variants for these --
    // dead code, since `keyword()` never produced them and nothing else in
    // the codebase ever constructed or matched on them (confirmed via a
    // full-codebase grep before removing them, production-hardening
    // PR-it656).
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
                | Tok::DotDot
                | Tok::DotDotEq
                | Tok::Bang
                | Tok::At
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_recognizes_reserved_words() {
        assert_eq!(keyword("fun"), Some(Tok::KwFun));
        assert_eq!(keyword("component"), Some(Tok::KwComponent));
        assert_eq!(keyword("match"), Some(Tok::KwMatch));
        assert_eq!(keyword("uses"), Some(Tok::KwUses));
        assert_eq!(keyword("notakeyword"), None);
    }

    /// A REAL dead-code finding fixed (production-hardening PR-it656): `Tok`
    /// used to ALSO declare `KwOut`/`KwState`/`KwStart`/`KwStop` variants,
    /// but `keyword()` never produced them and a full-codebase grep found
    /// nothing else that ever constructed or matched on them -- genuinely
    /// dead code, since `out`/`state`/`start`/`stop` are all CONTEXTUAL/soft
    /// keywords, recognized only in specific syntactic positions via
    /// `Tok::Ident(s) if s == "out"`-style matching directly in `parser.rs`
    /// (so a variable or function named `state`/`start`/etc. stays legal
    /// everywhere else in the language) -- removed the 4 dead variants.
    /// This test locks in the DESIGN this depends on: `keyword()` must never
    /// start treating these words as hard/reserved, or the contextual
    /// pattern (and every plain identifier that happens to be named one of
    /// these words) would silently break.
    #[test]
    fn keyword_does_not_reserve_the_contextual_soft_keywords() {
        for w in ["out", "state", "start", "stop"] {
            assert_eq!(keyword(w), None, "`{w}` must stay usable as a plain identifier");
        }
    }

    #[test]
    fn describe_renders_literals_and_symbols_distinctly() {
        assert_eq!(Tok::Int(5).describe(), "integer `5`");
        assert_eq!(Tok::Ident("x".into()).describe(), "identifier `x`");
        assert_eq!(Tok::Newline.describe(), "end of line");
        assert_eq!(Tok::Eof.describe(), "end of file");
        assert_eq!(Tok::Arrow.describe(), "`->`");
        assert_eq!(Tok::KwFun.describe(), "`fun`");
    }

    #[test]
    fn suppresses_newline_covers_binary_operators_and_open_delimiters() {
        assert!(Tok::Plus.suppresses_newline());
        assert!(Tok::Comma.suppresses_newline());
        assert!(Tok::LParen.suppresses_newline());
        assert!(Tok::Newline.suppresses_newline());
        // a closing delimiter or a literal does NOT suppress -- a newline
        // right after `)` or `5` really does end the statement.
        assert!(!Tok::RParen.suppresses_newline());
        assert!(!Tok::Int(5).suppresses_newline());
    }

    /// A REAL bug found+fixed (production-hardening PR-it966, from survey
    /// #114's token.rs close-read): `DotDot`/`DotDotEq`/`Bang`/`At` were
    /// simply never added to this list, even though `lexer.rs`'s own doc
    /// comment promises newlines are insignificant "after a token that
    /// implies continuation (operator, comma, dot, open bracket)" -- every
    /// OTHER operator (including every other unary/binary op) was already
    /// listed. This silently rejected ordinary, validly-formatted source
    /// that split a range (`0 ..\n    5`), a negation (`!\n    true`), or an
    /// `@`-pattern binding (`whole @\n    Circle(_) => ...`) across a line,
    /// with a spurious "expected an expression/pattern, found end of line"
    /// parse error identical across all four engines (shared front-end).
    #[test]
    fn suppresses_newline_covers_range_bang_and_at() {
        assert!(Tok::DotDot.suppresses_newline());
        assert!(Tok::DotDotEq.suppresses_newline());
        assert!(Tok::Bang.suppresses_newline());
        assert!(Tok::At.suppresses_newline());
    }
}
