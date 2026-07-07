//! Recursive-descent parser: tokens -> AST.
//!
//! Statement-level errors are recorded and the parser re-synchronizes at the
//! next newline / `}` so one mistake yields one diagnostic, not a cascade.

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::lexer;
use crate::token::{StrPart, Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    pub diags: Vec<Diag>,
    uses: Vec<(String, Span)>,
    depth: usize,
}

/// Max expression-nesting depth. Real code never approaches this; the bound turns
/// pathological input (e.g. thousands of nested `[`) into a clean K0121 diagnostic
/// instead of a superlinear hang in the type checker (whose owned `Ty` tree costs
/// O(depth) to clone/resolve at each level). Kept modest so the recursive-descent
/// parser (~11 precedence frames per level) stays well within a small thread stack.
const MAX_EXPR_DEPTH: usize = 128;

fn str_parts_text(parts: &[StrPart]) -> String {
    parts
        .iter()
        .map(|p| match p {
            StrPart::Text(t) => t.clone(),
            StrPart::Expr(e, _) => format!("{{{e}}}"),
        })
        .collect()
}

/// Build an interpolated-string `Expr` from lexed string parts (interpolation
/// sub-expressions are parsed). Used for `ai fun` intents.
fn str_parts_expr(parts: &[StrPart], span: Span) -> PResult<Expr> {
    let mut pieces = Vec::new();
    for part in parts {
        match part {
            StrPart::Text(t) => pieces.push(StrPiece::Text(t.clone())),
            StrPart::Expr(src, off) => {
                pieces.push(StrPiece::Expr(Box::new(parse_expr_fragment(src, *off)?)));
            }
        }
    }
    Ok(Expr { kind: ExprKind::Str(pieces), span })
}

type PResult<T> = Result<T, Diag>;

pub fn parse(src: &str) -> (Program, Vec<Diag>) {
    parse_with_base(src, 0)
}

/// Parse with all spans shifted by `base` — used by the multi-file loader so
/// spans index into the concatenated source map.
pub fn parse_with_base(src: &str, base: u32) -> (Program, Vec<Diag>) {
    let (mut toks, mut diags) = lexer::lex(src);
    if base != 0 {
        for t in &mut toks {
            t.span = Span::new(t.span.start + base, t.span.end + base);
        }
        for d in &mut diags {
            d.span = Span::new(d.span.start + base, d.span.end + base);
        }
    }
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new(), depth: 0 };
    let program = p.parse_program();
    diags.extend(p.diags);
    (program, diags)
}

/// Parse a single statement from a source fragment (used by the REPL).
pub fn parse_stmt_fragment(src: &str) -> Result<Stmt, Diag> {
    let (toks, diags) = lexer::lex(src);
    if let Some(d) = diags.into_iter().next() {
        return Err(d);
    }
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new(), depth: 0 };
    p.skip_newlines();
    let stmt = p.parse_stmt()?;
    if let Some(d) = p.diags.into_iter().next() {
        return Err(d);
    }
    Ok(stmt)
}

/// Parse a single expression from a source fragment (used for string
/// interpolation). `offset` shifts spans to file coordinates.
pub fn parse_expr_fragment(src: &str, offset: u32) -> Result<Expr, Diag> {
    let (mut toks, diags) = lexer::lex(src);
    for t in &mut toks {
        t.span = Span::new(t.span.start + offset, t.span.end + offset);
    }
    if let Some(d) = diags.into_iter().next() {
        return Err(d);
    }
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new(), depth: 0 };
    p.skip_newlines();
    let expr = p.parse_expr()?;
    if let Some(d) = p.diags.into_iter().next() {
        return Err(d);
    }
    Ok(expr)
}

impl Parser {
    // ---- token helpers -------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1).min(self.toks.len() - 1)].span
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }
    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, tok: Tok) -> PResult<Span> {
        if self.at(&tok) {
            let s = self.span();
            self.bump();
            Ok(s)
        } else {
            let mut msg = format!("expected {}, found {}", tok.describe(), self.peek().describe());
            // A bare `=` where a token was expected is almost always the classic
            // `=` (assignment) vs `==` (comparison) slip — e.g. `if n = 5 { … }`.
            if matches!(self.peek(), Tok::Eq) {
                msg.push_str(" — `=` assigns a value; use `==` to compare");
            }
            Err(Diag::error("K0100", msg, self.span()))
        }
    }
    fn expect_ident(&mut self) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let s = self.span();
                self.bump();
                Ok((name, s))
            }
            other => Err(Diag::error(
                "K0101",
                format!("expected identifier, found {}", other.describe()),
                self.span(),
            )),
        }
    }
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
    }
    fn expect_terminator(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Newline => {
                self.bump();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            // A `{` right after an identifier is almost always an attempted record literal with
            // braces (`Point{x: 1}`); KUPL builds records with parentheses. Name that fix rather
            // than the bare "expected end of statement" (PR-it243, cf. it228 for `with`).
            Tok::LBrace if matches!(&self.toks[self.pos.saturating_sub(1)].tok, Tok::Ident(_)) => {
                Err(Diag::error(
                    "K0102",
                    "records are constructed with parentheses — write `Name(field: value)`, not `Name{field: value}`".to_string(),
                    self.span(),
                ))
            }
            other => Err(Diag::error(
                "K0102",
                format!("expected end of statement, found {}", other.describe()),
                self.span(),
            )),
        }
    }
    /// After an error: skip to the next newline or closing brace.
    fn synchronize(&mut self) {
        loop {
            match self.peek() {
                Tok::Newline => {
                    self.bump();
                    return;
                }
                Tok::RBrace | Tok::Eof => return,
                Tok::LBrace => {
                    // skip a balanced block
                    let mut depth = 0usize;
                    loop {
                        match self.bump() {
                            Tok::LBrace => depth += 1,
                            Tok::RBrace => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Tok::Eof => return,
                            _ => {}
                        }
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    // ---- program & items ----------------------------------------------

    fn parse_program(&mut self) -> Program {
        let mut program = Program::default();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            match self.parse_item() {
                Ok(Some(item)) => program.items.push(item),
                Ok(None) => {}
                Err(d) => {
                    self.diags.push(d);
                    let before = self.pos;
                    self.synchronize();
                    // never loop without progress (e.g. a stray top-level `}`)
                    if self.pos == before && !matches!(self.peek(), Tok::Eof) {
                        self.bump();
                    }
                }
            }
        }
        program.uses = std::mem::take(&mut self.uses);
        program
    }

    fn parse_item(&mut self) -> PResult<Option<Item>> {
        let is_pub = self.eat(&Tok::KwPub);
        // `ai` is a soft keyword: only special directly before `fun`.
        if matches!(self.peek(), Tok::Ident(n) if n == "ai")
            && matches!(self.peek_at(1), Tok::KwFun)
        {
            self.bump();
            return Ok(Some(Item::Fun(self.parse_ai_fun(is_pub)?)));
        }
        // top-level `law "name" { … }` — a free-standing test (soft keyword)
        if matches!(self.peek(), Tok::Ident(n) if n == "law") {
            let lspan = self.span();
            self.bump();
            let name = match self.bump() {
                Tok::Str(parts) => str_parts_text(&parts),
                other => {
                    return Err(Diag::error(
                        "K0115",
                        format!("`law` expects a name string, found {}", other.describe()),
                        self.prev_span(),
                    ))
                }
            };
            let body = self.parse_block()?;
            let span = lspan.merge(body.span);
            return Ok(Some(Item::Law(Law { name, body, span })));
        }
        match self.peek() {
            Tok::KwFun | Tok::KwAsync => Ok(Some(Item::Fun(self.parse_fun(is_pub)?))),
            Tok::KwType => Ok(Some(Item::Type(self.parse_type_decl()?))),
            Tok::KwComponent => Ok(Some(Item::Component(self.parse_component(false)?))),
            Tok::KwApp => Ok(Some(Item::Component(self.parse_component(true)?))),
            Tok::KwContract => Ok(Some(Item::Contract(self.parse_contract()?))),
            Tok::KwUse => {
                let uspan = self.span();
                self.bump();
                let (mut path, _) = self.expect_ident()?;
                while self.eat(&Tok::Dot) {
                    let (part, _) = self.expect_ident()?;
                    path.push('.');
                    path.push_str(&part);
                }
                self.uses.push((path, uspan.merge(self.prev_span())));
                self.expect_terminator()?;
                Ok(None)
            }
            Tok::KwModule => {
                // Accepted; module identity is derived from the file path (v0).
                self.bump();
                while !matches!(self.peek(), Tok::Newline | Tok::Eof) {
                    self.bump();
                }
                Ok(None)
            }
            other => Err(Diag::error(
                "K0103",
                format!(
                    "expected a declaration (`fun`, `type`, `component`, `app`), found {}",
                    other.describe()
                ),
                self.span(),
            )),
        }
    }

    fn parse_fun(&mut self, is_pub: bool) -> PResult<FunDecl> {
        self.eat(&Tok::KwAsync);
        let start = self.expect(Tok::KwFun)?;
        let (name, _) = self.expect_ident()?;
        let mut type_params = Vec::new();
        if self.eat(&Tok::LBracket) {
            loop {
                let (tp, _) = self.expect_ident()?;
                type_params.push(tp);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RBracket)?;
        }
        self.expect(Tok::LParen)?;
        let params = self.parse_params()?;
        self.expect(Tok::RParen)?;
        let mut effects = Vec::new();
        if self.eat(&Tok::KwUses) {
            loop {
                let (mut eff, _) = self.expect_ident()?;
                while self.eat(&Tok::Dot) {
                    let (part, _) = self.expect_ident()?;
                    eff.push('.');
                    eff.push_str(&part);
                }
                effects.push(eff);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.parse_ty()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        let span = start.merge(body.span);
        Ok(FunDecl { name, type_params, params, ret, effects, body, is_pub, ai: None, span })
    }

    /// `ai fun name(params) -> T { intent "..." [model "..."] }`
    fn parse_ai_fun(&mut self, is_pub: bool) -> PResult<FunDecl> {
        let start = self.expect(Tok::KwFun)?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::LParen)?;
        let params = self.parse_params()?;
        self.expect(Tok::RParen)?;
        let mut effects = Vec::new();
        if self.eat(&Tok::KwUses) {
            loop {
                let (eff, _) = self.expect_ident()?;
                effects.push(eff);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let ret = if self.eat(&Tok::Arrow) { Some(self.parse_ty()?) } else { None };
        // `tools [f, g]` — soft keyword, before the body brace.
        let mut tools = Vec::new();
        if matches!(self.peek(), Tok::Ident(n) if n == "tools") {
            self.bump();
            self.expect(Tok::LBracket)?;
            self.skip_newlines();
            while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                let (t, _) = self.expect_ident()?;
                tools.push(t);
                self.skip_newlines();
                if !self.eat(&Tok::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(Tok::RBracket)?;
        }
        self.expect(Tok::LBrace)?;
        self.skip_newlines();
        let bad_body = |span| {
            Diag::error(
                "K0119",
                "an `ai fun` body is `intent \"...\"` optionally followed by `model \"...\"`",
                span,
            )
        };
        if !self.at(&Tok::KwIntent) {
            return Err(bad_body(self.span()));
        }
        self.bump();
        let intent_span = self.span();
        let (intent, intent_expr) = match self.bump() {
            Tok::Str(parts) => (str_parts_text(&parts), str_parts_expr(&parts, intent_span)?),
            _ => return Err(bad_body(self.prev_span())),
        };
        self.skip_newlines();
        let mut model = None;
        if matches!(self.peek(), Tok::Ident(n) if n == "model") {
            self.bump();
            model = match self.bump() {
                Tok::Str(parts) => Some(str_parts_text(&parts)),
                _ => return Err(bad_body(self.prev_span())),
            };
            self.skip_newlines();
        }
        let end = self.expect(Tok::RBrace).map_err(|_| bad_body(self.span()))?;
        let span = start.merge(end);
        Ok(FunDecl {
            name,
            type_params: Vec::new(),
            params,
            ret,
            effects,
            body: Block { stmts: Vec::new(), span },
            is_pub,
            ai: Some(AiDecl { intent, intent_expr, model, tools }),
            span,
        })
    }

    /// Parse a duration literal like `5s`, `2s`, `100ms`, `1m`, `2h` into
    /// virtual milliseconds. Written as an integer immediately followed by a
    /// unit identifier.
    fn parse_duration(&mut self) -> PResult<i64> {
        let span = self.span();
        let n = match self.bump() {
            Tok::Int(n) => n,
            other => {
                return Err(Diag::error(
                    "K0120",
                    format!("expected a duration (e.g. `5s`, `100ms`), found {}", other.describe()),
                    span,
                ))
            }
        };
        let unit = match self.peek().clone() {
            Tok::Ident(u) => {
                self.bump();
                u
            }
            other => {
                return Err(Diag::error(
                    "K0120",
                    format!("expected a duration unit (`ms`, `s`, `m`, `h`), found {}", other.describe()),
                    self.span(),
                ))
            }
        };
        let per = match unit.as_str() {
            "ms" => 1,
            "s" => 1000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => {
                return Err(Diag::error(
                    "K0120",
                    format!("unknown duration unit `{unit}` (use `ms`, `s`, `m`, or `h`)"),
                    self.prev_span(),
                ))
            }
        };
        n.checked_mul(per).ok_or_else(|| {
            Diag::error("K0120", "duration is too large".to_string(), span.merge(self.prev_span()))
        })
    }

    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            let (name, nspan) = self.expect_ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.parse_ty()?;
            let default = if self.eat(&Tok::Eq) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let span = nspan.merge(ty.span);
            params.push(Param { name, ty, default, span });
            self.skip_newlines();
            if !self.eat(&Tok::Comma) {
                break;
            }
            self.skip_newlines();
        }
        Ok(params)
    }

    fn parse_type_decl(&mut self) -> PResult<TypeDecl> {
        let start = self.expect(Tok::KwType)?;
        let (name, _) = self.expect_ident()?;
        let mut type_params = Vec::new();
        if self.eat(&Tok::LBracket) {
            loop {
                let (tp, _) = self.expect_ident()?;
                type_params.push(tp);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RBracket)?;
        }
        self.expect(Tok::Eq)?;
        // `type UserId = new Str` — newtype: single variant wrapping one field.
        if self.eat(&Tok::KwNew) {
            let inner = self.parse_ty()?;
            let span = start.merge(inner.span);
            let field = Param { name: "value".into(), ty: inner, default: None, span };
            let variants = vec![Variant { name: name.clone(), fields: vec![field], span }];
            self.expect_terminator()?;
            return Ok(TypeDecl { name, type_params, variants, span });
        }
        // `type User = { name: Str, age: Int }` — record: single variant named
        // like the type, constructed with named args.
        if self.at(&Tok::LBrace) {
            self.bump();
            self.skip_newlines();
            let mut fields = Vec::new();
            while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                let (fname, fspan) = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let ty = self.parse_ty()?;
                fields.push(Param { name: fname, ty, default: None, span: fspan });
                self.skip_newlines();
                if !self.eat(&Tok::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            let end = self.expect(Tok::RBrace)?;
            let span = start.merge(end);
            let variants = vec![Variant { name: name.clone(), fields, span }];
            self.expect_terminator()?;
            return Ok(TypeDecl { name, type_params, variants, span });
        }
        // Union of variants: `Circle(r: Float) | Rect(w: Float, h: Float)`
        let mut variants = Vec::new();
        loop {
            let (vname, vspan) = self.expect_ident()?;
            let mut fields = Vec::new();
            if self.eat(&Tok::LParen) {
                fields = self.parse_params()?;
                self.expect(Tok::RParen)?;
            }
            variants.push(Variant { name: vname, fields, span: vspan });
            self.skip_newlines_if_pipe_follows();
            if !self.eat(&Tok::Pipe) {
                break;
            }
            self.skip_newlines();
        }
        let span = start.merge(self.prev_span());
        self.expect_terminator()?;
        Ok(TypeDecl { name, type_params, variants, span })
    }

    /// Allow a union to continue on the next line: `= A\n | B`.
    fn skip_newlines_if_pipe_follows(&mut self) {
        let mut n = 0;
        while matches!(self.peek_at(n), Tok::Newline) {
            n += 1;
        }
        if n > 0 && matches!(self.peek_at(n), Tok::Pipe) {
            for _ in 0..n {
                self.bump();
            }
        }
    }

    // ---- contracts --------------------------------------------------------

    fn parse_contract(&mut self) -> PResult<ContractDecl> {
        let start = self.expect(Tok::KwContract)?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::LBrace)?;
        let mut c = ContractDecl {
            name,
            intent: None,
            sigs: Vec::new(),
            laws: Vec::new(),
            span: start,
        };
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            match self.peek().clone() {
                Tok::KwIntent => {
                    self.bump();
                    match self.bump() {
                        Tok::Str(parts) => {
                            c.intent = Some(str_parts_text(&parts));
                        }
                        other => {
                            return Err(Diag::error(
                                "K0104",
                                format!("`intent` expects a string literal, found {}", other.describe()),
                                self.prev_span(),
                            ))
                        }
                    }
                    self.expect_terminator()?;
                }
                Tok::KwExpose => {
                    self.bump();
                    let sig = self.parse_fun_sig()?;
                    c.sigs.push(sig);
                }
                Tok::Ident(ref n) if n == "law" => {
                    let lspan = self.span();
                    self.bump();
                    let name = match self.bump() {
                        Tok::Str(parts) => str_parts_text(&parts),
                        other => {
                            return Err(Diag::error(
                                "K0115",
                                format!("`law` expects a name string, found {}", other.describe()),
                                self.prev_span(),
                            ))
                        }
                    };
                    let body = self.parse_block()?;
                    let span = lspan.merge(body.span);
                    c.laws.push(Law { name, body, span });
                }
                other => {
                    return Err(Diag::error(
                        "K0116",
                        format!(
                            "unexpected {} in contract body (expected `intent`, `expose fun`, or `law`)",
                            other.describe()
                        ),
                        self.span(),
                    ))
                }
            }
        }
        let end = self.expect(Tok::RBrace)?;
        c.span = start.merge(end);
        Ok(c)
    }

    /// A body-less `fun name(params) [uses ...] [-> Ty]` signature.
    fn parse_fun_sig(&mut self) -> PResult<FunSig> {
        let start = self.expect(Tok::KwFun)?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::LParen)?;
        let params = self.parse_params()?;
        self.expect(Tok::RParen)?;
        let mut effects = Vec::new();
        if self.eat(&Tok::KwUses) {
            loop {
                let (mut eff, _) = self.expect_ident()?;
                while self.eat(&Tok::Dot) {
                    let (part, _) = self.expect_ident()?;
                    eff.push('.');
                    eff.push_str(&part);
                }
                effects.push(eff);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let ret = if self.eat(&Tok::Arrow) { Some(self.parse_ty()?) } else { None };
        let span = start.merge(self.prev_span());
        self.expect_terminator()?;
        Ok(FunSig { name, params, ret, effects, span })
    }

    // ---- components -----------------------------------------------------

    fn parse_component(&mut self, is_app: bool) -> PResult<ComponentDecl> {
        let start = self.span();
        self.bump(); // `component` or `app`
        let (name, _) = self.expect_ident()?;
        let mut fulfills = Vec::new();
        if matches!(self.peek(), Tok::Ident(n) if n == "fulfills") {
            self.bump();
            loop {
                let (contract, _) = self.expect_ident()?;
                fulfills.push(contract);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::LBrace)?;
        let mut c = ComponentDecl {
            name,
            is_app,
            fulfills,
            intent: None,
            ports: Vec::new(),
            props: Vec::new(),
            state: Vec::new(),
            children: Vec::new(),
            wires: Vec::new(),
            supervises: Vec::new(),
            handlers: Vec::new(),
            exposes: Vec::new(),
            funs: Vec::new(),
            examples: Vec::new(),
            span: start,
        };
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            if let Err(d) = self.parse_component_member(&mut c) {
                self.diags.push(d);
                self.synchronize();
            }
        }
        let end = self.expect(Tok::RBrace)?;
        c.span = start.merge(end);
        Ok(c)
    }

    /// True at a port declaration: `in …` or the contextual keyword `out …`.
    fn at_port_direction(&self) -> bool {
        matches!(self.peek(), Tok::KwIn) || matches!(self.peek(), Tok::Ident(s) if s == "out")
    }

    fn parse_component_member(&mut self, c: &mut ComponentDecl) -> PResult<()> {
        match self.peek().clone() {
            Tok::KwIntent => {
                self.bump();
                match self.bump() {
                    Tok::Str(parts) => {
                        let text: String = parts
                            .iter()
                            .map(|p| match p {
                                StrPart::Text(t) => t.clone(),
                                StrPart::Expr(e, _) => format!("{{{e}}}"),
                            })
                            .collect();
                        c.intent = Some(text);
                    }
                    other => {
                        return Err(Diag::error(
                            "K0104",
                            format!("`intent` expects a string literal, found {}", other.describe()),
                            self.prev_span(),
                        ))
                    }
                }
                self.expect_terminator()
            }
            // `in`/`out` port declarations. `out` is a contextual keyword (a plain
            // identifier elsewhere), recognized here only in member position.
            Tok::KwIn | Tok::Ident(_) if self.at_port_direction() => {
                let dir = match self.bump() {
                    Tok::KwIn => PortDir::In,
                    _ => PortDir::Out,
                };
                let (name, nspan) = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let ty = self.parse_ty()?;
                let span = nspan.merge(ty.span);
                c.ports.push(Port { dir, name, ty, span });
                self.expect_terminator()
            }
            Tok::KwProp | Tok::KwRequires => {
                self.bump();
                loop {
                    let (name, nspan) = self.expect_ident()?;
                    self.expect(Tok::Colon)?;
                    let ty = self.parse_ty()?;
                    let default = if self.eat(&Tok::Eq) { Some(self.parse_expr()?) } else { None };
                    c.props.push(PropDecl { name, ty, default, span: nspan });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect_terminator()
            }
            // `state` is a contextual keyword — a state field here, a plain
            // identifier elsewhere.
            Tok::Ident(s) if s == "state" => {
                self.bump();
                let (name, nspan) = self.expect_ident()?;
                let ty = if self.eat(&Tok::Colon) { Some(self.parse_ty()?) } else { None };
                self.expect(Tok::Eq)?;
                let init = self.parse_expr()?;
                let span = nspan.merge(init.span);
                c.state.push(StateField { name, ty, init, span });
                self.expect_terminator()
            }
            Tok::KwLet => {
                self.bump();
                let (name, nspan) = self.expect_ident()?;
                self.expect(Tok::Eq)?;
                let (component, _) = self.expect_ident()?;
                let mut args = Vec::new();
                if self.eat(&Tok::LParen) {
                    args = self.parse_args()?;
                    self.expect(Tok::RParen)?;
                }
                let span = nspan.merge(self.prev_span());
                c.children.push(ChildDecl { name, component, args, span });
                self.expect_terminator()
            }
            Tok::KwWire => {
                let wspan = self.span();
                self.bump();
                let (a, _) = self.expect_ident()?;
                self.expect(Tok::Dot)?;
                let (ap, _) = self.expect_ident()?;
                self.expect(Tok::Arrow)?;
                let (b, _) = self.expect_ident()?;
                self.expect(Tok::Dot)?;
                let (bp, _) = self.expect_ident()?;
                c.wires.push(WireDecl { from: (a, ap), to: (b, bp), span: wspan.merge(self.prev_span()) });
                self.expect_terminator()
            }
            Tok::KwSupervise => {
                let sspan = self.span();
                self.bump();
                let (child, _) = self.expect_ident()?;
                // contextual: `restart on_failure` | `restart never`
                let (word1, w1span) = self.expect_ident()?;
                if word1 != "restart" {
                    return Err(Diag::error(
                        "K0117",
                        format!("expected `restart` after the child name, found `{word1}`"),
                        w1span,
                    ));
                }
                let (word2, w2span) = self.expect_ident()?;
                let policy = match word2.as_str() {
                    "on_failure" => SupervisePolicy::RestartOnFailure,
                    "never" => SupervisePolicy::Never,
                    other => {
                        return Err(Diag::error(
                            "K0118",
                            format!("unknown restart policy `{other}` (use `on_failure` or `never`)"),
                            w2span,
                        ))
                    }
                };
                c.supervises.push(SuperviseDecl { child, policy, span: sspan.merge(self.prev_span()) });
                self.expect_terminator()
            }
            Tok::KwOn => {
                let hspan = self.span();
                self.bump();
                let trigger = match self.peek().clone() {
                    // `start`/`stop` are contextual handler names here
                    Tok::Ident(ref kw) if kw == "start" => {
                        self.bump();
                        Trigger::Start
                    }
                    Tok::Ident(ref kw) if kw == "stop" => {
                        self.bump();
                        Trigger::Stop
                    }
                    // `on every 5s` / `on after 2s` — timers (soft keywords)
                    Tok::Ident(ref kw) if kw == "every" || kw == "after" => {
                        let recurring = kw == "every";
                        self.bump();
                        let ms = self.parse_duration()?;
                        if recurring {
                            Trigger::Every(ms)
                        } else {
                            Trigger::After(ms)
                        }
                    }
                    Tok::Ident(name) => {
                        self.bump();
                        Trigger::Port(name)
                    }
                    other => {
                        return Err(Diag::error(
                            "K0105",
                            format!("`on` expects a port name, `start`, `stop`, `every <dur>`, or `after <dur>`; found {}", other.describe()),
                            self.span(),
                        ))
                    }
                };
                let mut param = None;
                if self.eat(&Tok::LParen) {
                    let (p, _) = self.expect_ident()?;
                    param = Some(p);
                    self.expect(Tok::RParen)?;
                }
                let body = self.parse_block()?;
                let span = hspan.merge(body.span);
                c.handlers.push(Handler { trigger, param, body, span });
                Ok(())
            }
            Tok::KwExpose => {
                self.bump();
                let f = self.parse_fun(true)?;
                c.exposes.push(f);
                Ok(())
            }
            Tok::KwPub | Tok::KwFun | Tok::KwAsync => {
                let is_pub = self.eat(&Tok::KwPub);
                let f = self.parse_fun(is_pub)?;
                c.funs.push(f);
                Ok(())
            }
            Tok::KwExample | Tok::KwTest => {
                let espan = self.span();
                self.bump();
                self.expect(Tok::LBrace)?;
                let mut steps = Vec::new();
                loop {
                    self.skip_newlines();
                    if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                        break;
                    }
                    match self.peek().clone() {
                        Tok::KwSend => {
                            let sspan = self.span();
                            self.bump();
                            let (port, _) = self.expect_ident()?;
                            let mut arg = None;
                            if self.eat(&Tok::LParen) {
                                if !self.at(&Tok::RParen) {
                                    arg = Some(self.parse_expr()?);
                                }
                                self.expect(Tok::RParen)?;
                            }
                            steps.push(ExampleStep::Send { port, arg, span: sspan.merge(self.prev_span()) });
                            self.expect_terminator()?;
                        }
                        Tok::KwExpect => {
                            let sspan = self.span();
                            self.bump();
                            let expr = self.parse_expr()?;
                            steps.push(ExampleStep::Expect { expr, span: sspan.merge(self.prev_span()) });
                            self.expect_terminator()?;
                        }
                        // `advance 5s` — move the virtual clock (soft keyword)
                        Tok::Ident(ref kw) if kw == "advance" => {
                            let sspan = self.span();
                            self.bump();
                            let ms = self.parse_duration()?;
                            steps.push(ExampleStep::Advance { ms, span: sspan.merge(self.prev_span()) });
                            self.expect_terminator()?;
                        }
                        other => {
                            return Err(Diag::error(
                                "K0106",
                                format!("example blocks contain `send`, `expect`, and `advance` steps; found {}", other.describe()),
                                self.span(),
                            ))
                        }
                    }
                }
                let end = self.expect(Tok::RBrace)?;
                c.examples.push(Example { steps, span: espan.merge(end) });
                Ok(())
            }
            other => Err(Diag::error(
                "K0107",
                format!(
                    "unexpected {} in component body (expected `intent`, ports, `state`, `on`, `fun`, `wire`, `example`, …)",
                    other.describe()
                ),
                self.span(),
            )),
        }
    }

    // ---- blocks & statements --------------------------------------------

    fn parse_block(&mut self) -> PResult<Block> {
        let start = self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            match self.parse_stmt() {
                Ok(s) => stmts.push(s),
                Err(d) => {
                    self.diags.push(d);
                    self.synchronize();
                }
            }
        }
        let end = self.expect(Tok::RBrace)?;
        Ok(Block { stmts, span: start.merge(end) })
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        match self.peek().clone() {
            Tok::KwLet | Tok::KwVar => {
                let mutable = matches!(self.bump(), Tok::KwVar);
                let (name, nspan) = self.expect_ident()?;
                let ty = if self.eat(&Tok::Colon) { Some(self.parse_ty()?) } else { None };
                self.expect(Tok::Eq)?;
                let init = self.parse_expr()?;
                let span = nspan.merge(init.span);
                self.expect_terminator()?;
                Ok(Stmt::Let { name, ty, init, mutable, span })
            }
            Tok::KwReturn => {
                let span = self.span();
                self.bump();
                let value = if matches!(self.peek(), Tok::Newline | Tok::RBrace | Tok::Eof) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect_terminator()?;
                Ok(Stmt::Return(value, span))
            }
            Tok::KwWhile => {
                let span = self.span();
                self.bump();
                // `while let PATTERN = EXPR { BODY }` desugars to
                // `while true { match EXPR { PATTERN => { BODY; () }  _ => { break } } }`
                if self.at(&Tok::KwLet) {
                    self.bump();
                    let pattern = self.parse_pattern()?;
                    self.expect(Tok::Eq)?;
                    let scrutinee = self.parse_expr()?;
                    let mut body = self.parse_block()?;
                    let bspan = body.span;
                    // append `()` so the matched arm is Unit-typed, unifying with
                    // the break arm (which is also Unit)
                    body.stmts.push(Stmt::Expr(Expr { kind: ExprKind::Unit, span: bspan }));
                    let match_arm = MatchArm {
                        pattern,
                        guard: None,
                        body: Expr { kind: ExprKind::BlockExpr(body), span: bspan },
                        span: bspan,
                    };
                    let break_block = Block { stmts: vec![Stmt::Break(bspan)], span: bspan };
                    let break_arm = MatchArm {
                        pattern: Pattern { kind: PatternKind::Wildcard, span: bspan },
                        guard: None,
                        body: Expr { kind: ExprKind::BlockExpr(break_block), span: bspan },
                        span: bspan,
                    };
                    let m = Expr {
                        kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms: vec![match_arm, break_arm] },
                        span: bspan,
                    };
                    let loop_body = Block { stmts: vec![Stmt::Expr(m)], span: bspan };
                    let cond = Expr { kind: ExprKind::Bool(true), span };
                    return Ok(Stmt::While { cond, body: loop_body, span });
                }
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(Stmt::While { cond, body, span })
            }
            Tok::KwFor => {
                let span = self.span();
                self.bump();
                let (var, _) = self.expect_ident()?;
                self.expect(Tok::KwIn)?;
                let iter = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(Stmt::For { var, iter, body, span })
            }
            Tok::KwEmit => {
                let span = self.span();
                self.bump();
                let (port, _) = self.expect_ident()?;
                let mut arg = None;
                if self.eat(&Tok::LParen) {
                    if !self.at(&Tok::RParen) {
                        arg = Some(self.parse_expr()?);
                    }
                    self.expect(Tok::RParen)?;
                }
                self.expect_terminator()?;
                Ok(Stmt::Emit { port, arg, span: span.merge(self.prev_span()) })
            }
            Tok::KwExpect => {
                let span = self.span();
                self.bump();
                let expr = self.parse_expr()?;
                let span = span.merge(expr.span);
                self.expect_terminator()?;
                Ok(Stmt::Expect(expr, span))
            }
            // `forall` is a soft keyword: a statement only when a binder follows
            Tok::Ident(ref n)
                if n == "forall" && matches!(self.peek_at(1), Tok::Ident(_)) =>
            {
                let span = self.span();
                self.bump();
                let mut vars = Vec::new();
                loop {
                    let (name, _) = self.expect_ident()?;
                    self.expect(Tok::Colon)?;
                    let ty = self.parse_ty()?;
                    vars.push((name, ty));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                let body = self.parse_block()?;
                let full = span.merge(body.span);
                Ok(Stmt::Forall { vars, body, span: full })
            }
            Tok::KwBreak => {
                let span = self.span();
                self.bump();
                self.expect_terminator()?;
                Ok(Stmt::Break(span))
            }
            Tok::KwContinue => {
                let span = self.span();
                self.bump();
                self.expect_terminator()?;
                Ok(Stmt::Continue(span))
            }
            _ => {
                let expr = self.parse_expr()?;
                // assignment?
                let op = match self.peek() {
                    Tok::Eq => Some(AssignOp::Set),
                    Tok::PlusEq => Some(AssignOp::Add),
                    Tok::MinusEq => Some(AssignOp::Sub),
                    Tok::StarEq => Some(AssignOp::Mul),
                    Tok::SlashEq => Some(AssignOp::Div),
                    _ => None,
                };
                if let Some(op) = op {
                    self.bump();
                    let value = self.parse_expr()?;
                    if !matches!(expr.kind, ExprKind::Ident(_) | ExprKind::Field { .. }) {
                        return Err(Diag::error(
                            "K0108",
                            "invalid assignment target (must be a variable or field)",
                            expr.span,
                        ));
                    }
                    let span = expr.span.merge(value.span);
                    self.expect_terminator()?;
                    return Ok(Stmt::Assign { target: expr, op, value, span });
                }
                self.expect_terminator()?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        // Guard against pathologically deep nesting (every nested sub-expression
        // re-enters here). Sequential expressions don't accumulate — depth is
        // decremented on the way out.
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(Diag::error(
                "K0121",
                "expression nesting too deep".to_string(),
                self.span(),
            ));
        }
        let r = self.parse_pipeline();
        self.depth -= 1;
        r
    }

    fn parse_pipeline(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_or()?;
        while self.at(&Tok::PipeGt) {
            self.bump();
            let rhs = self.parse_or()?;
            let span = lhs.span.merge(rhs.span);
            // `x |> f` desugars to `f(x)`; `x |> f(a)` to `f(x, a)`.
            lhs = match rhs.kind {
                ExprKind::Call { callee, mut args } => {
                    args.insert(0, Arg { name: None, value: lhs });
                    Expr { kind: ExprKind::Call { callee, args }, span }
                }
                _ => Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(rhs),
                        args: vec![Arg { name: None, value: lhs }],
                    },
                    span,
                },
            };
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.at(&Tok::OrOr) {
            self.bump();
            let rhs = self.parse_and()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_equality()?;
        while self.at(&Tok::AndAnd) {
            self.bump();
            let rhs = self.parse_equality()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Tok::EqEq => BinOp::Eq,
                Tok::NotEq => BinOp::Ne,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_comparison()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_with()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::LtEq => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::GtEq => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_with()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    /// `expr with field: value, field: value` — record update. After a comma,
    /// the update list only continues when `ident :` follows (so `f(t with
    /// x: 1, other)` parses `other` as the next call argument).
    fn parse_with(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_range()?;
        while matches!(self.peek(), Tok::Ident(n) if n == "with") {
            self.bump();
            // A record update lists fields directly: `p with x: 1, y: 2` — there are no braces.
            // Users coming from other languages often reach for `p with { x: 1 }`; give them the
            // real syntax instead of a bare "expected identifier, found `{`" (PR-it228).
            if self.at(&Tok::LBrace) {
                return Err(Diag::error(
                    "K0101",
                    "record update has no braces — write `x with field: value, field: value`".to_string(),
                    self.span(),
                ));
            }
            let mut updates = Vec::new();
            loop {
                let (field, _) = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let value = self.parse_range()?;
                updates.push((field, value));
                if !(self.at(&Tok::Comma)
                    && matches!(self.peek_at(1), Tok::Ident(_))
                    && matches!(self.peek_at(2), Tok::Colon))
                {
                    break;
                }
                self.bump(); // comma
            }
            let span = lhs.span.merge(self.prev_span());
            lhs = Expr { kind: ExprKind::With { recv: Box::new(lhs), updates }, span };
        }
        Ok(lhs)
    }

    fn parse_range(&mut self) -> PResult<Expr> {
        let lhs = self.parse_additive()?;
        let inclusive = match self.peek() {
            Tok::DotDot => false,
            Tok::DotDotEq => true,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.parse_additive()?;
        let span = lhs.span.merge(rhs.span);
        Ok(Expr { kind: ExprKind::Range { lo: Box::new(lhs), hi: Box::new(rhs), inclusive }, span })
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_multiplicative()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        match self.peek() {
            Tok::Minus => {
                let span = self.span();
                self.bump();
                let operand = self.parse_unary()?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Unary { op: UnOp::Neg, operand: Box::new(operand) }, span })
            }
            Tok::Bang => {
                let span = self.span();
                self.bump();
                let operand = self.parse_unary()?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Unary { op: UnOp::Not, operand: Box::new(operand) }, span })
            }
            Tok::KwAwait => {
                let span = self.span();
                self.bump();
                let operand = self.parse_unary()?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Await(Box::new(operand)), span })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let args = self.parse_args()?;
                    let end = self.expect(Tok::RParen)?;
                    let span = expr.span.merge(end);
                    expr = Expr { kind: ExprKind::Call { callee: Box::new(expr), args }, span };
                }
                Tok::Dot => {
                    self.bump();
                    let (name, nspan) = self.expect_ident()?;
                    if self.at(&Tok::LParen) {
                        self.bump();
                        let args = self
                            .parse_args()?
                            .into_iter()
                            .map(|a| a.value)
                            .collect();
                        let end = self.expect(Tok::RParen)?;
                        let span = expr.span.merge(end);
                        expr = Expr { kind: ExprKind::MethodCall { recv: Box::new(expr), name, args }, span };
                    } else {
                        let span = expr.span.merge(nspan);
                        expr = Expr { kind: ExprKind::Field { recv: Box::new(expr), name }, span };
                    }
                }
                Tok::Question => {
                    let end = self.span();
                    self.bump();
                    let span = expr.span.merge(end);
                    expr = Expr { kind: ExprKind::Try(Box::new(expr)), span };
                }
                // a method chain may continue on the next line when that line
                // starts with `.` (a leading `.` can't begin a statement, so this
                // is unambiguous); otherwise the newline still ends the statement.
                Tok::Newline => {
                    let saved = self.pos;
                    while matches!(self.peek(), Tok::Newline) {
                        self.bump();
                    }
                    if matches!(self.peek(), Tok::Dot) {
                        continue;
                    }
                    self.pos = saved;
                    break;
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_args(&mut self) -> PResult<Vec<Arg>> {
        let mut args = Vec::new();
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            // optional `prop` prefix in constructor calls: `Header(prop title: "x")`
            self.eat(&Tok::KwProp);
            // named argument: `ident: expr`
            let name = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Colon) {
                let (n, _) = self.expect_ident()?;
                self.bump(); // colon
                Some(n)
            } else {
                None
            };
            let value = self.parse_expr()?;
            args.push(Arg { name, value });
            self.skip_newlines();
            if !self.eat(&Tok::Comma) {
                break;
            }
            self.skip_newlines();
        }
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Int(v), span })
            }
            Tok::SizedInt(v, w) => {
                self.bump();
                Ok(Expr { kind: ExprKind::SizedInt(v, w), span })
            }
            Tok::F32Lit(v) => {
                self.bump();
                Ok(Expr { kind: ExprKind::F32(v), span })
            }
            Tok::Float(v) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Float(v), span })
            }
            Tok::KwTrue => {
                self.bump();
                Ok(Expr { kind: ExprKind::Bool(true), span })
            }
            Tok::KwFalse => {
                self.bump();
                Ok(Expr { kind: ExprKind::Bool(false), span })
            }
            Tok::Str(parts) => {
                self.bump();
                let mut pieces = Vec::new();
                for part in &parts {
                    match part {
                        StrPart::Text(t) => pieces.push(StrPiece::Text(t.clone())),
                        StrPart::Expr(src, off) => match parse_expr_fragment(src, *off) {
                            Ok(e) => pieces.push(StrPiece::Expr(Box::new(e))),
                            Err(d) => return Err(d),
                        },
                    }
                }
                Ok(Expr { kind: ExprKind::Str(pieces), span })
            }
            Tok::Ident(name) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Ident(name), span })
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(Expr { kind: ExprKind::Unit, span: span.merge(self.prev_span()) });
                }
                let inner = self.parse_expr()?;
                // `(a, b)` is a common attempt to write a tuple — KUPL has none. Point at the
                // list/record alternatives instead of the bare "expected `)`, found `,`".
                if self.at(&Tok::Comma) {
                    return Err(Diag::error(
                        "K0100",
                        "expected `)`, found `,` — KUPL has no tuples; use a list `[a, b]` or a record".to_string(),
                        self.span(),
                    ));
                }
                self.expect(Tok::RParen)?;
                Ok(inner)
            }
            Tok::LBracket => {
                self.bump();
                self.skip_newlines();
                let mut items = Vec::new();
                while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                    items.push(self.parse_expr()?);
                    self.skip_newlines();
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                let end = self.expect(Tok::RBracket)?;
                // An empty list immediately followed by `:` is the classic attempt to
                // type-annotate the expression itself (`[]: List[Int]`). KUPL has no inline
                // expression type-ascription — the element type comes from the binding — so
                // name the fix: annotate the `let` instead. Without this the user gets a bare
                // "expected `)`, found `:`" (call arg) or "expected end of statement" (let rhs).
                if items.is_empty() && self.at(&Tok::Colon) {
                    return Err(Diag::error(
                        "K0100",
                        "an empty list can't be type-annotated inline — write the type on the binding, e.g. `let xs: List[Int] = []`".to_string(),
                        self.span(),
                    ));
                }
                Ok(Expr { kind: ExprKind::List(items), span: span.merge(end) })
            }
            Tok::KwPar => {
                self.bump();
                self.expect(Tok::LBrace)?;
                self.skip_newlines();
                let mut branches = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    branches.push(self.parse_expr()?);
                    self.skip_newlines();
                    if self.eat(&Tok::Comma) {
                        self.skip_newlines();
                    }
                }
                let end = self.expect(Tok::RBrace)?;
                Ok(Expr { kind: ExprKind::Par(branches), span: span.merge(end) })
            }
            Tok::KwIf => self.parse_if(),
            Tok::KwMatch => {
                self.bump();
                let scrutinee = self.parse_expr()?;
                self.expect(Tok::LBrace)?;
                let mut arms = Vec::new();
                loop {
                    self.skip_newlines();
                    if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                        break;
                    }
                    let pattern = self.parse_pattern()?;
                    // optional `if COND` guard before the `=>`
                    let guard = if self.eat(&Tok::KwIf) {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    self.expect(Tok::FatArrow)?;
                    let body = if self.at(&Tok::LBrace) {
                        let b = self.parse_block()?;
                        let bspan = b.span;
                        Expr { kind: ExprKind::BlockExpr(b), span: bspan }
                    } else {
                        self.parse_expr()?
                    };
                    let aspan = pattern.span.merge(body.span);
                    arms.push(MatchArm { pattern, guard, body, span: aspan });
                    if !self.eat(&Tok::Comma) {
                        if !matches!(self.peek(), Tok::Newline | Tok::RBrace) {
                            return Err(Diag::error(
                                "K0109",
                                // The usual cause is two arms on one line (`Some(v) => v None => 0`);
                                // name that fix rather than just reporting the next token (PR-it239).
                                format!("match arms are separated by a newline or `,` — put each arm on its own line (found {} after an arm body)", self.peek().describe()),
                                self.span(),
                            ));
                        }
                    }
                }
                let end = self.expect(Tok::RBrace)?;
                Ok(Expr {
                    kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms },
                    span: span.merge(end),
                })
            }
            Tok::KwFn => {
                self.bump();
                let mut params = Vec::new();
                let parens = self.eat(&Tok::LParen);
                while matches!(self.peek(), Tok::Ident(_)) {
                    let (pname, pspan) = self.expect_ident()?;
                    let ty = if self.eat(&Tok::Colon) { Some(self.parse_ty()?) } else { None };
                    params.push(LambdaParam { name: pname, ty, span: pspan });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                if parens {
                    self.expect(Tok::RParen)?;
                }
                let body = self.parse_block()?;
                let fspan = span.merge(body.span);
                Ok(Expr { kind: ExprKind::Lambda { params, body }, span: fspan })
            }
            Tok::LBrace => {
                let b = self.parse_block()?;
                let bspan = b.span;
                Ok(Expr { kind: ExprKind::BlockExpr(b), span: bspan })
            }
            other => Err(Diag::error(
                "K0110",
                format!("expected an expression, found {}", other.describe()),
                span,
            )),
        }
    }

    /// Consume an `else` that may follow the then-block across newline(s), so
    /// multi-line `}\nelse { … }` parses like `} else { … }`. `else` cannot begin
    /// a statement, so skipping newlines to find it is unambiguous; if none
    /// follows, the cursor is restored and the newline still separates statements.
    fn eat_else(&mut self) -> bool {
        let saved = self.pos;
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
        if self.eat(&Tok::KwElse) {
            true
        } else {
            self.pos = saved;
            false
        }
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let span = self.expect(Tok::KwIf)?;
        // `if let PATTERN = EXPR { … } else { … }` desugars to a `match` whose
        // wildcard arm is the else branch (or `()` when there is no else — which
        // makes the then-branch required to be Unit, exactly like `if` w/o else).
        if self.at(&Tok::KwLet) {
            self.bump();
            let pattern = self.parse_pattern()?;
            self.expect(Tok::Eq)?;
            let scrutinee = self.parse_expr()?;
            let then = self.parse_block()?;
            let then_span = then.span;
            let then_expr = Expr { kind: ExprKind::BlockExpr(then), span: then_span };
            let else_expr = if self.eat_else() {
                if self.at(&Tok::KwIf) {
                    self.parse_if()?
                } else {
                    let b = self.parse_block()?;
                    let bs = b.span;
                    Expr { kind: ExprKind::BlockExpr(b), span: bs }
                }
            } else {
                Expr { kind: ExprKind::Unit, span: then_span }
            };
            let full = span.merge(self.prev_span());
            let wild = Pattern { kind: PatternKind::Wildcard, span: else_expr.span };
            let arms = vec![
                MatchArm { pattern, guard: None, body: then_expr, span: then_span },
                MatchArm { pattern: wild, guard: None, body: else_expr, span: full },
            ];
            return Ok(Expr { kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, span: full });
        }
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let mut else_block = None;
        if self.eat_else() {
            if self.at(&Tok::KwIf) {
                else_block = Some(Box::new(self.parse_if()?));
            } else {
                let b = self.parse_block()?;
                let bspan = b.span;
                else_block = Some(Box::new(Expr { kind: ExprKind::BlockExpr(b), span: bspan }));
            }
        }
        let full = span.merge(self.prev_span());
        Ok(Expr {
            kind: ExprKind::If { cond: Box::new(cond), then_block, else_block },
            span: full,
        })
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        let first = self.parse_pattern_primary()?;
        if !self.at(&Tok::Pipe) {
            return Ok(first);
        }
        // or-pattern: `P1 | P2 | …`
        let span = first.span;
        let mut alts = vec![first];
        while self.eat(&Tok::Pipe) {
            alts.push(self.parse_pattern_primary()?);
        }
        let end = alts.last().map(|p| p.span).unwrap_or(span);
        Ok(Pattern { kind: PatternKind::Or(alts), span: span.merge(end) })
    }

    fn parse_pattern_primary(&mut self) -> PResult<Pattern> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                self.maybe_range(v, span)
            }
            Tok::Minus => {
                self.bump();
                match self.bump() {
                    Tok::Int(v) => self.maybe_range(-v, span.merge(self.prev_span())),
                    other => Err(Diag::error(
                        "K0111",
                        format!("expected integer after `-` in pattern, found {}", other.describe()),
                        self.prev_span(),
                    )),
                }
            }
            Tok::KwTrue => {
                self.bump();
                Ok(Pattern { kind: PatternKind::Bool(true), span })
            }
            Tok::KwFalse => {
                self.bump();
                Ok(Pattern { kind: PatternKind::Bool(false), span })
            }
            Tok::Str(parts) => {
                self.bump();
                let mut text = String::new();
                for p in &parts {
                    match p {
                        StrPart::Text(t) => text.push_str(t),
                        StrPart::Expr(..) => {
                            return Err(Diag::error(
                                "K0112",
                                "string patterns cannot contain interpolation",
                                span,
                            ))
                        }
                    }
                }
                Ok(Pattern { kind: PatternKind::Str(text), span })
            }
            Tok::Ident(name) => {
                self.bump();
                if name == "_" {
                    return Ok(Pattern { kind: PatternKind::Wildcard, span });
                }
                let is_ctor = name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
                if self.at(&Tok::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
                        args.push(self.parse_pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    let end = self.expect(Tok::RParen)?;
                    Ok(Pattern { kind: PatternKind::Ctor { name, args }, span: span.merge(end) })
                } else if is_ctor {
                    Ok(Pattern { kind: PatternKind::Ctor { name, args: Vec::new() }, span })
                } else if self.eat(&Tok::At) {
                    // `name @ SUBPATTERN` — bind the whole value AND match inner
                    let inner = self.parse_pattern_primary()?;
                    let end = inner.span;
                    Ok(Pattern {
                        kind: PatternKind::At { name, inner: Box::new(inner) },
                        span: span.merge(end),
                    })
                } else {
                    Ok(Pattern { kind: PatternKind::Bind(name), span })
                }
            }
            other => Err(Diag::error(
                "K0113",
                format!("expected a pattern, found {}", other.describe()),
                span,
            )),
        }
    }

    /// After an Int literal in a pattern, an optional `..`/`..=` upper bound
    /// turns it into a range pattern.
    fn maybe_range(&mut self, lo: i64, span: Span) -> PResult<Pattern> {
        let inclusive = match self.peek() {
            Tok::DotDot => false,
            Tok::DotDotEq => true,
            _ => return Ok(Pattern { kind: PatternKind::Int(lo), span }),
        };
        self.bump();
        let neg = self.eat(&Tok::Minus);
        let hi = match self.bump() {
            Tok::Int(v) => {
                if neg {
                    -v
                } else {
                    v
                }
            }
            other => {
                return Err(Diag::error(
                    "K0111",
                    format!("expected an integer upper bound in range pattern, found {}", other.describe()),
                    self.prev_span(),
                ))
            }
        };
        Ok(Pattern { kind: PatternKind::Range { lo, hi, inclusive }, span: span.merge(self.prev_span()) })
    }

    // ---- types --------------------------------------------------------------

    fn parse_ty(&mut self) -> PResult<TyExpr> {
        // Same nesting bound as parse_expr (shared counter) — a deeply nested type
        // annotation (List[List[…]]) builds an O(depth) `Ty` the checker handles
        // superlinearly, so cap it to a clean K0121 instead of a slow blow-up.
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(Diag::error(
                "K0121",
                "type nesting too deep".to_string(),
                self.span(),
            ));
        }
        let r = self.parse_ty_inner();
        self.depth -= 1;
        r
    }

    fn parse_ty_inner(&mut self) -> PResult<TyExpr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::KwFn => {
                self.bump();
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
                    params.push(self.parse_ty()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
                self.expect(Tok::Arrow)?;
                let ret = self.parse_ty()?;
                let full = span.merge(ret.span);
                Ok(TyExpr { kind: TyExprKind::Fun(params, Box::new(ret)), span: full })
            }
            Tok::LParen => {
                // `()` is Unit
                self.bump();
                let end = self.expect(Tok::RParen)?;
                Ok(TyExpr { kind: TyExprKind::Name("Unit".into()), span: span.merge(end) })
            }
            Tok::Ident(name) => {
                self.bump();
                if self.at(&Tok::LBracket) {
                    self.bump();
                    let mut args = Vec::new();
                    while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                        args.push(self.parse_ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    let end = self.expect(Tok::RBracket)?;
                    Ok(TyExpr { kind: TyExprKind::Generic(name, args), span: span.merge(end) })
                } else {
                    Ok(TyExpr { kind: TyExprKind::Name(name), span })
                }
            }
            other => Err(Diag::error(
                "K0114",
                format!("expected a type, found {}", other.describe()),
                span,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Program {
        let (p, diags) = parse(src);
        assert!(diags.is_empty(), "diags: {diags:#?}");
        p
    }

    #[test]
    fn malformed_input_never_panics_only_diagnoses() {
        // A fuzz pass (PR-it146: ~2600 mutation-fuzzed + 31 structured-malformed inputs)
        // found no Rust-level panic/abort/hang — the CLI always degrades to a clean
        // diagnostic. These pin the nastiest structured cases: parse + type-check must
        // RETURN (never panic / unwrap / index-oob), yielding error diagnostics.
        let nasty = [
            "",                              // empty file
            "   \n\t\n  ",                   // only whitespace
            "// just a comment\n",           // only a comment
            "(((((((((((",                   // unbalanced open parens
            "}}}}}}}}}}}",                   // unbalanced close braces
            "[[[[[[[[[[[",                   // unbalanced open brackets
            "\"unterminated string",         // unterminated string literal
            "\"{a + ",                       // unterminated interpolation
            "fun f() { match x { } }\n",     // match with no arms
            "type = = =",                    // garbage type header
            "fun fun fun",                   // repeated keywords
            "app X {",                       // unterminated app
            "contract C {",                  // unterminated contract
            "component",                     // bare keyword
            "1e999999999",                   // huge float literal
            "0xffffffffffffffffffffff",      // huge hex literal
            "fun f[",                        // truncated generic header
            "let let let",                   // repeated let
            "fun main() uses io { print(",   // unterminated call
        ];
        for src in nasty {
            // Neither of these may panic; both must terminate and return.
            let (program, pdiags) = parse(src);
            let (_checked, cdiags) = crate::check::check(&program);
            // Every one of these inputs is invalid, so at least one pass must complain
            // (except the trivially-empty program forms, which are simply empty).
            let has_err = pdiags.iter().chain(cdiags.iter()).any(|d| d.severity == crate::diag::Severity::Error);
            let trivially_empty = matches!(src, "" | "   \n\t\n  " | "// just a comment\n");
            assert!(has_err || trivially_empty, "expected a diagnostic for {src:?}");
        }
    }

    #[test]
    fn eq_in_condition_suggests_double_equals() {
        // `=` (assignment) where `==` (comparison) was meant is a classic slip; the
        // K0100 error must point at the fix, not just say "expected `{`, found `=`".
        for src in ["fun f(n: Int) -> Bool { if n = 5 { true } else { false } }\n",
                    "fun main() uses io { while x = 0 { print(\"x\") } }\n"] {
            let (_, diags) = parse(src);
            let m = &diags.iter().find(|d| d.code == "K0100").expect("K0100").message;
            assert!(m.contains("use `==` to compare"), "missing == hint: {m}");
        }
        // a genuine missing brace (found something other than `=`) gets no such hint
        let (_, diags) = parse("fun f() -> Int 1 }\n");
        let m = &diags.iter().find(|d| d.code == "K0100").expect("K0100").message;
        assert!(!m.contains("use `==`"), "spurious == hint: {m}");
    }

    /// `else` may follow the then-block across a newline; the AST is identical
    /// to the same-line form, and a no-else `if` doesn't swallow the next line.
    /// `out`/`state`/`start`/`stop` are contextual keywords — usable as ordinary
    /// identifiers outside a component, while component syntax still parses.
    #[test]
    fn contextual_component_keywords() {
        ok("fun f() -> Int {\n    let out = 1\n    let state = 2\n    let start = 3\n    let stop = 4\n    out + state + start + stop\n}\n");
        // component with in/out ports, state, and on start still parses
        ok("component C {\n    in click: Event\n    out value: Int\n    state count: Int = 0\n    on start {\n        emit value(count)\n    }\n    on click {\n        count += 1\n        emit value(count)\n    }\n}\n");
    }

    #[test]
    fn newline_before_else() {
        // both the one-line and multi-line forms parse cleanly (the AST differs
        // only in spans, which reflect the different source layout)
        ok("fun f(c: Bool) -> Int {\n    if c { 1 } else { 2 }\n}\n");
        ok("fun f(c: Bool) -> Int {\n    if c {\n        1\n    }\n    else {\n        2\n    }\n}\n");
        // else-if chain across lines
        ok("fun g(n: Int) -> Str {\n    if n > 1 {\n        \"a\"\n    }\n    else if n > 0 {\n        \"b\"\n    }\n    else {\n        \"c\"\n    }\n}\n");
        // a no-else `if` followed by a newline + another statement is TWO statements
        let p = ok("fun h() -> Int {\n    var x = 0\n    if true {\n        x = 5\n    }\n    x = x + 1\n    x\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    /// A method chain continues on a line starting with `.`; a normal statement
    /// sequence (no leading dot) is unaffected.
    #[test]
    fn multiline_method_chain() {
        ok("fun m() -> Int {\n    Some(5).map(fn x { x + 1 })\n        .filter(fn x { x > 3 })\n        .unwrap_or(0)\n}\n");
        // a `.field` continuation across a newline
        ok("type P = { x: Int }\nfun m(p: P) -> Int {\n    p\n        .x\n}\n");
        // two independent statements with NO leading dot -> still two statements
        let p = ok("fun m() -> Int {\n    let a = 1\n    let b = 2\n    a + b\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn parse_fun() {
        let p = ok("fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn parse_adt_and_match() {
        let p = ok("type Shape = Circle(r: Float) | Rect(w: Float, h: Float)\nfun area(s: Shape) -> Float {\n    match s {\n        Circle(r) => 3.14 * r * r\n        Rect(w, h) => w * h\n    }\n}\n");
        assert_eq!(p.items.len(), 2);
    }

    #[test]
    fn parse_component() {
        let src = r#"
component Counter {
    intent "Counts clicks."

    in click: Event
    out value: Int

    state count: Int = 0

    on click {
        count += 1
        emit value(count)
    }

    example {
        send click
        send click
        expect value == 2
    }
}
"#;
        let p = ok(src);
        match &p.items[0] {
            Item::Component(c) => {
                assert_eq!(c.name, "Counter");
                assert_eq!(c.ports.len(), 2);
                assert_eq!(c.state.len(), 1);
                assert_eq!(c.handlers.len(), 1);
                assert_eq!(c.examples.len(), 1);
            }
            other => panic!("expected component, got {other:?}"),
        }
    }

    #[test]
    fn parse_app_with_wiring() {
        let src = r#"
app Main {
    intent "Wire a source to a sink."
    let a = Source()
    let b = Sink()
    wire a.value -> b.input
}
"#;
        let p = ok(src);
        match &p.items[0] {
            Item::Component(c) => {
                assert!(c.is_app);
                assert_eq!(c.children.len(), 2);
                assert_eq!(c.wires.len(), 1);
            }
            other => panic!("expected app, got {other:?}"),
        }
    }

    #[test]
    fn parse_interpolation() {
        let p = ok("fun greet(name: Str) -> Str {\n    \"hello {name}!\"\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn parse_lambda_and_methods() {
        let p = ok("fun f(xs: List[Int]) -> List[Int] {\n    xs.filter(fn x { x > 1 }).map(fn x { x * 2 })\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn parse_ai_fun() {
        let p = ok("ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku.\"\n    model \"claude-opus-4-8\"\n}\n");
        match &p.items[0] {
            Item::Fun(f) => {
                let ai = f.ai.as_ref().expect("is an ai fun");
                assert_eq!(ai.intent, "Write a haiku.");
                assert_eq!(ai.model.as_deref(), Some("claude-opus-4-8"));
                assert_eq!(f.params.len(), 1);
            }
            other => panic!("expected fun, got {other:?}"),
        }
    }

    #[test]
    fn ai_is_still_an_ordinary_identifier() {
        let p = ok("fun f(ai: Int) -> Int {\n    let ai = ai + 1\n    ai\n}\n");
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn record_literal_with_braces_names_the_paren_fix() {
        // `Point{x: 1}` is a common mistake — records use parens. K0102 now names the fix rather
        // than the bare "expected end of statement, found `{`" (PR-it243).
        let (_p, diags) = parse("type Point = { x: Int, y: Int }\nfun f() -> Int {\n    let p = Point{x: 1, y: 2}\n    p.x\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0102" && d.message.contains("records are constructed with parentheses")),
            "{diags:?}"
        );
        // The correct paren form still parses cleanly.
        assert!(parse("type Point = { x: Int, y: Int }\nfun f() -> Int {\n    let p = Point(x: 1, y: 2)\n    p.x\n}\n").1.is_empty());
    }

    #[test]
    fn empty_list_inline_annotation_names_the_binding_fix() {
        // `[]: List[Int]` (annotating the expression) is a common attempt to give an empty list an
        // element type; KUPL has no inline expression ascription, so K0100 now names the fix —
        // annotate the binding — rather than the bare "expected `)`, found `:`" / "expected end of
        // statement" the user got before (PR-it263). Both the call-arg and let-rhs positions hit it.
        let (_p, d1) = parse("fun f() -> Int {\n    let x = [1].zip_with([]: List[Int], fn(a, b) { a + b })\n    x.len()\n}\n");
        assert!(
            d1.iter().any(|d| d.code == "K0100" && d.message.contains("annotated inline") && d.message.contains("let xs: List[Int] = []")),
            "{d1:?}"
        );
        let (_p, d2) = parse("fun f() -> Int {\n    let x = []: List[Int]\n    x.len()\n}\n");
        assert!(
            d2.iter().any(|d| d.code == "K0100" && d.message.contains("annotated inline")),
            "{d2:?}"
        );
        // The correct binding-annotation form and non-empty list literals still parse cleanly.
        assert!(parse("fun f() -> Int {\n    let xs: List[Int] = []\n    xs.len()\n}\n").1.is_empty());
        assert!(parse("fun f() -> Int {\n    let ys = [1, 2, 3]\n    ys.len()\n}\n").1.is_empty());
    }

    #[test]
    fn two_match_arms_on_one_line_names_the_fix() {
        // Putting two arms on one line (`Some(v) => v None => 0`) is a common mistake; K0109 now
        // names the fix — one arm per line — rather than just reporting the next token (PR-it239).
        let (_p, diags) = parse("fun f(o: Option[Int]) -> Int { match o { Some(v) => v None => 0 } }\n");
        assert!(
            diags.iter().any(|d| d.code == "K0109" && d.message.contains("put each arm on its own line")),
            "{diags:?}"
        );
        // Both correct forms still parse: one arm per line, and comma-separated arms.
        assert!(parse("fun f(o: Option[Int]) -> Int {\n    match o {\n        Some(v) => v\n        None => 0\n    }\n}\n").1.is_empty());
        assert!(parse("fun f(n: Int) -> Str { match n { 0 => \"z\", 1 => \"o\", _ => \"m\" } }\n").1.is_empty());
    }

    #[test]
    fn record_update_with_braces_is_rejected_with_a_hint() {
        // `p with { x: 5 }` (braces) is a common mistake from other languages; the parser should
        // name the real brace-free syntax rather than emit a bare "expected identifier" (PR-it228).
        let (_p, diags) = parse("type P = P(x: Int, y: Int)\nfun main() { let p = P(x: 1, y: 2)\n    let q = p with { x: 5 } }\n");
        assert!(
            diags.iter().any(|d| d.code == "K0101" && d.message.contains("record update has no braces")),
            "{diags:?}"
        );
        // The correct brace-free forms still parse cleanly (single and multi-field).
        assert!(parse("type P = P(x: Int, y: Int)\nfun main() { let p = P(x: 1, y: 2)\n    let q = p with x: 5\n    let r = p with x: 1, y: 2 }\n").1.is_empty());
    }

    #[test]
    fn stray_top_level_brace_terminates() {
        // regression: error recovery must always make progress
        let (p, diags) = parse("}\n}\nfun f() -> Int {\n    1\n}\n");
        assert!(!diags.is_empty());
        assert_eq!(p.items.len(), 1);
    }
}
