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
}

fn str_parts_text(parts: &[StrPart]) -> String {
    parts
        .iter()
        .map(|p| match p {
            StrPart::Text(t) => t.clone(),
            StrPart::Expr(e, _) => format!("{{{e}}}"),
        })
        .collect()
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
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new() };
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
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new() };
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
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new() };
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
            Err(Diag::error(
                "K0100",
                format!("expected {}, found {}", tok.describe(), self.peek().describe()),
                self.span(),
            ))
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
                    self.synchronize();
                }
            }
        }
        program.uses = std::mem::take(&mut self.uses);
        program
    }

    fn parse_item(&mut self) -> PResult<Option<Item>> {
        let is_pub = self.eat(&Tok::KwPub);
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
        Ok(FunDecl { name, type_params, params, ret, effects, body, is_pub, span })
    }

    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            let (name, nspan) = self.expect_ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.parse_ty()?;
            let span = nspan.merge(ty.span);
            params.push(Param { name, ty, span });
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
        self.expect(Tok::Eq)?;
        // `type UserId = new Str` — newtype: single variant wrapping one field.
        if self.eat(&Tok::KwNew) {
            let inner = self.parse_ty()?;
            let span = start.merge(inner.span);
            let field = Param { name: "value".into(), ty: inner, span };
            let variants = vec![Variant { name: name.clone(), fields: vec![field], span }];
            self.expect_terminator()?;
            return Ok(TypeDecl { name, variants, span });
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
                fields.push(Param { name: fname, ty, span: fspan });
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
            return Ok(TypeDecl { name, variants, span });
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
        Ok(TypeDecl { name, variants, span })
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
            Tok::KwIn | Tok::KwOut => {
                let dir = if matches!(self.bump(), Tok::KwIn) { PortDir::In } else { PortDir::Out };
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
            Tok::KwState => {
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
                    Tok::KwStart => {
                        self.bump();
                        Trigger::Start
                    }
                    Tok::KwStop => {
                        self.bump();
                        Trigger::Stop
                    }
                    Tok::Ident(name) => {
                        self.bump();
                        Trigger::Port(name)
                    }
                    other => {
                        return Err(Diag::error(
                            "K0105",
                            format!("`on` expects a port name, `start`, or `stop`; found {}", other.describe()),
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
                        other => {
                            return Err(Diag::error(
                                "K0106",
                                format!("example blocks contain `send` and `expect` steps; found {}", other.describe()),
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
        self.parse_pipeline()
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
                Ok(Expr { kind: ExprKind::List(items), span: span.merge(end) })
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
                    self.expect(Tok::FatArrow)?;
                    let body = if self.at(&Tok::LBrace) {
                        let b = self.parse_block()?;
                        let bspan = b.span;
                        Expr { kind: ExprKind::BlockExpr(b), span: bspan }
                    } else {
                        self.parse_expr()?
                    };
                    let aspan = pattern.span.merge(body.span);
                    arms.push(MatchArm { pattern, body, span: aspan });
                    if !self.eat(&Tok::Comma) {
                        if !matches!(self.peek(), Tok::Newline | Tok::RBrace) {
                            return Err(Diag::error(
                                "K0109",
                                format!("expected `,` or newline between match arms, found {}", self.peek().describe()),
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

    fn parse_if(&mut self) -> PResult<Expr> {
        let span = self.expect(Tok::KwIf)?;
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let mut else_block = None;
        if self.eat(&Tok::KwElse) {
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
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(Pattern { kind: PatternKind::Int(v), span })
            }
            Tok::Minus => {
                self.bump();
                match self.bump() {
                    Tok::Int(v) => Ok(Pattern { kind: PatternKind::Int(-v), span: span.merge(self.prev_span()) }),
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

    // ---- types --------------------------------------------------------------

    fn parse_ty(&mut self) -> PResult<TyExpr> {
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
}
