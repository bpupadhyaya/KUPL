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

/// A duration literal (`on every`/`on after`, and an `example` block's
/// `advance` step) is capped at 100 years in milliseconds.
///
/// A REAL, uncatchable/UB-inducing bug found+fixed (production-hardening
/// PR-it728, found via a scoped Explore survey): `parse_duration` already
/// guarded the UNIT-CONVERSION multiplication (`n.checked_mul(per)`, below)
/// against overflow, but placed NO cap on the resulting millisecond value
/// itself — so `9223372036854775807ms` (using the `ms` unit directly, where
/// `per == 1` and the multiplication trivially never overflows) sailed
/// straight through as a legal duration. This value then reached THREE
/// independent unchecked-arithmetic sites, none of which cross-validated it:
/// `interp.rs`/`vm.rs`'s `next_fire: now + interval` (timer arming) and
/// `next_fire += interval` (timer rescheduling), and `interp.rs`/`vm.rs`'s
/// `advance()`'s own `self.now + dur` (used by BOTH automatic timer
/// advancing AND an `example` block's explicit `advance <duration>` step,
/// confirmed as a SEPARATE, independently-reachable crash site — an
/// `advance` step needs no timer at all). Confirmed LIVE on all three
/// mechanisms: `on every 9223372036854775807ms { ... }` crashed BOTH
/// `kupl run` (a raw "attempt to add with overflow" Rust panic reported as
/// a bogus "internal compiler error", interp.rs:363) and `kupl run --vm`
/// (the identical ICE, vm.rs:254) — and, worse, did NOT crash on
/// `kupl native` at all: C signed-integer overflow is UNDEFINED BEHAVIOR,
/// which in practice wraps `next_fire` to a hugely NEGATIVE value, so the
/// timer re-fires FOREVER inside a single `k_run_timers` call (observed
/// multiple GB of output before being killed) — a genuine three-way engine
/// DIVERGENCE (two clean-ish ICEs vs. one silent resource-exhaustion hang),
/// on a trivial, single-line, checker-accepted program. `advance
/// 9223372036854775807ms` in an example block crashed at interp.rs:320
/// (`self.now + dur`) with NO timer involved at all — `check.rs`'s
/// `ExampleStep::Advance` arm does no validation whatsoever. Rather than add
/// checked-arithmetic to every one of these runtime sites, independently,
/// in three separately-maintained engines, this rejects the PATHOLOGICAL
/// duration LITERAL once, here, in the single parser all three engines
/// share (matching the EXISTING `checked_mul` overflow guard's own
/// check-time, not runtime-checked-arithmetic, philosophy) — closing all
/// three crash sites and the cross-engine divergence in one place. 100
/// years is generous for any legitimate `on every`/`on after`/`advance`
/// duration while leaving enormous headroom against the worst case:
/// `run_timers`'s own bounded 100-fire loop repeatedly rescheduling a timer
/// at the cap totals only ~100 * 100 years, over 29,000x below `i64::MAX`.
const MAX_DURATION_MS: i64 = 100 * 365 * 24 * 60 * 60 * 1000;

fn str_parts_text(parts: &[StrPart]) -> String {
    parts
        .iter()
        .map(|p| match p {
            StrPart::Text(t) => t.clone(),
            StrPart::Expr(e, _) => format!("{{{e}}}"),
        })
        .collect()
}

/// A did-you-mean suggestion suffix (`" — did you mean \`X\`?"`, or empty) for a
/// mis-parsed keyword-dispatched fallback: when the unrecognized token is an
/// IDENTIFIER (a typo'd keyword lexes as a plain `Tok::Ident`, not as the
/// keyword's own token) that's close to one of `candidates`, name the fix --
/// otherwise (a non-identifier token, or nothing close enough) stays silent,
/// same calibration as `check::suggest`'s own edit-distance threshold. Reuses
/// `check::suggest` (PR-it603) rather than a second, independently-written
/// fuzzy-match implementation -- this file had ZERO did-you-mean coverage
/// before this, unlike check.rs's ~95 `self.err(...)` sites (it581/582).
fn keyword_suggestion(tok: &Tok, candidates: &[&str]) -> String {
    match tok {
        Tok::Ident(name) => crate::check::suggest(name, candidates.iter().copied())
            .map(|s| format!(" — did you mean `{s}`?"))
            .unwrap_or_default(),
        _ => String::new(),
    }
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
    // A REAL bug found+fixed (production-hardening PR-it753): the fragment's
    // TOKEN spans are shifted by `offset` (above) so they read as absolute
    // file coordinates once merged back into the enclosing program, but its
    // own lexer DIAGNOSTICS were returned raw, still relative to this tiny
    // fragment's own 0-based coordinates. A lexer error inside a string-
    // interpolation sub-expression (most naturally: an unterminated nested
    // string literal) was reported pointing at an essentially arbitrary,
    // unrelated location elsewhere in the real file. Live-confirmed BEFORE
    // this fix: an unterminated string nested inside `"...${"..."..."` at
    // line 6 of a file reported its K0005 error at line 1:1 instead.
    if let Some(mut d) = diags.into_iter().next() {
        d.span = Span::new(d.span.start + offset, d.span.end + offset);
        return Err(d);
    }
    let mut p = Parser { toks, pos: 0, diags: Vec::new(), uses: Vec::new(), depth: 0 };
    p.skip_newlines();
    let expr = p.parse_expr()?;
    // A REAL, live-confirmed SILENT MISPARSE found+fixed (production-
    // hardening PR-it892, an Explore survey finding, independently
    // re-verified live before implementing): unlike every other grammar
    // entry point in this file (`parse_program`'s own loop runs to `Eof`;
    // a parenthesized expression's `Tok::LParen` case explicitly `expect`s
    // its closing `)`), this function never checked that `parse_expr`
    // actually consumed the WHOLE fragment -- `parse_expr` returns as soon
    // as it finds one complete expression and simply stops, so any tokens
    // left over in a `{...}` string-interpolation slot were silently
    // dropped with NO diagnostic. Confirmed live before this fix:
    // `print("sum: {a b}")` (two adjacent identifiers, no operator between
    // them -- most plausibly a forgotten `+`/`*`) printed `sum: 1`,
    // silently discarding `b` from the AST entirely -- identical, wrong
    // output across interp, `--vm`, `.kx`, AND native (this is a shared
    // parser bug, not a cross-engine divergence). Worse: `kupl fmt`
    // CANONICALIZES the bug away, silently rewriting the source text
    // itself from `"sum: {a b}"` to `"sum: {a}"` -- with `--write`, this
    // is genuine, silent, permanent data loss in the user's own file, not
    // just a bad runtime result. Fixed by asserting the fragment is fully
    // consumed, mirroring `self.expect`'s own "expected X, found Y"
    // wording convention. (Checked the sibling `parse_stmt_fragment` above
    // for the SAME gap -- it does NOT need this fix: `parse_stmt` already
    // internally calls `expect_terminator` on every statement variant,
    // which only accepts `Newline`/`RBrace`/`Eof`, so trailing garbage
    // after a REPL statement is already a clean K0102 error, live-
    // reconfirmed before ruling it out.)
    p.skip_newlines();
    if !matches!(p.peek(), Tok::Eof) {
        return Err(Diag::error(
            "K0100",
            format!(
                "expected end of expression, found {} — a string interpolation `{{...}}` must contain a single expression",
                p.peek().describe()
            ),
            p.span(),
        ));
    }
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
                    "unexpected {} at the top level (expected `fun`, `type`, `component`, `app`){}",
                    other.describe(),
                    keyword_suggestion(other, &["fun", "type", "component", "app", "contract", "use", "module"])
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
        let ms = n.checked_mul(per).ok_or_else(|| {
            Diag::error(
                "K0120",
                format!("`{n}{unit}` is too large — durations are stored as milliseconds in a 64-bit integer, and this overflows it"),
                span.merge(self.prev_span()),
            )
        })?;
        if ms > MAX_DURATION_MS {
            return Err(Diag::error(
                "K0120",
                format!(
                    "`{n}{unit}` is too large — durations are capped at 100 years, since an `on every`/`on after` timer that reschedules (or an `advance` step) could otherwise overflow the virtual clock's 64-bit millisecond counter"
                ),
                span.merge(self.prev_span()),
            ));
        }
        Ok(ms)
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
                            "unexpected {} in contract body (expected `intent`, `expose fun`, or `law`){}",
                            other.describe(),
                            keyword_suggestion(&other, &["intent", "expose", "law"])
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
                                format!(
                                    "unexpected {} in example body (expected `send`, `expect`, or `advance`){}",
                                    other.describe(),
                                    keyword_suggestion(&other, &["send", "expect", "advance"])
                                ),
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
                    "unexpected {} in component body (expected `intent`, ports, `state`, `on`, `fun`, `wire`, `example`, …){}",
                    other.describe(),
                    // The did-you-mean candidate list this campaign's it603 memory
                    // entry deferred (K0107 was left out of it603's first pass since
                    // its valid-construct set spans MANY match arms, not one small
                    // literal list like K0103/K0116/K0106's did) -- traced directly
                    // from every arm THIS match actually accepts (not copied from the
                    // message text above, which is itself abbreviated with "…" and
                    // would have given an incomplete candidate set): the two
                    // soft/contextual keywords `out`/`state` (matched by string
                    // comparison, not `token::keyword`) plus every hard keyword this
                    // function's own match handles.
                    keyword_suggestion(
                        &other,
                        &[
                            "intent", "in", "out", "prop", "requires", "state", "let", "wire",
                            "supervise", "on", "expose", "pub", "fun", "async", "example", "test",
                        ]
                    )
                ),
                self.span(),
            )),
        }
    }

    // ---- blocks & statements --------------------------------------------

    /// PR-it715: found during the systematic sweep it714 called for. `while`/
    /// `for`/`forall` bodies recurse into `parse_block` (-> `parse_stmt` ->
    /// the SAME statement kind's body -> `parse_block` -> ...) WITHOUT ever
    /// going through `parse_expr`'s guarded entry point -- `if`/lambda/match-
    /// arm bodies happen to be safe only because `if`/lambda/match are
    /// EXPRESSIONS reached via `parse_expr`, not because `parse_block` itself
    /// is guarded. A deeply nested `while true { while true { ... } }` (or
    /// `for`/`forall`) chain confirmed live to crash the parser with the same
    /// uncatchable "fatal runtime error: stack overflow" it713/it714 already
    /// fixed for expressions and patterns. Guarded here, at `parse_block`
    /// itself -- the ONE shared function every block body funnels through
    /// (`if`/lambda/match arms/fun bodies/component members all included) --
    /// rather than at each of `while`/`for`/`forall`'s call sites individually
    /// (narrower shared boundary point, matching PR-it638/it639/it692's
    /// pattern). `if`/lambda/match bodies now get counted TWICE (once by
    /// `parse_expr`'s own guard, once here) -- harmless: it only makes the
    /// limit marginally more conservative, never less safe, and real code
    /// nests far below 128 either way.
    fn parse_block(&mut self) -> PResult<Block> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(self.block_too_deep());
        }
        let r = self.parse_block_inner();
        self.depth -= 1;
        r
    }

    fn block_too_deep(&self) -> Diag {
        Diag::error(
            "K0121",
            format!(
                "block nesting too deep (limit is {MAX_EXPR_DEPTH}) — break it into a helper `fun`"
            ),
            self.span(),
        )
    }

    fn parse_block_inner(&mut self) -> PResult<Block> {
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
            return Err(self.expr_too_deep());
        }
        let r = self.parse_pipeline();
        self.depth -= 1;
        r
    }

    /// Shared K0121 diagnostic for `MAX_EXPR_DEPTH` overflow, used both by
    /// `parse_expr`'s own recursive-descent counter (`self.depth`, genuine
    /// bracketed/parenthesized nesting) and by every LEFT-ASSOCIATIVE chain
    /// loop below (`parse_pipeline`/`parse_or`/.../`parse_postfix`, PR-it713).
    /// Those loops build their `lhs` via straight-line iteration, never
    /// re-entering `parse_expr`, so a long flat chain of the SAME operator
    /// (e.g. `1 + 1 + 1 + ... ` a few hundred thousand times) used to bypass
    /// `self.depth` entirely and build an AST just as deep as an equivalent
    /// pile of nested parens would -- deep enough to STACK-OVERFLOW every
    /// downstream recursive consumer of that tree (fmt, check, interp, and
    /// the bytecode compiler all recurse over it) with an uncatchable
    /// process abort, not a diagnostic. Each such loop now tracks its OWN
    /// local chain length against the same `MAX_EXPR_DEPTH` limit.
    fn expr_too_deep(&self) -> Diag {
        Diag::error(
            "K0121",
            format!(
                "expression nesting too deep (limit is {MAX_EXPR_DEPTH}) — break it into intermediate `let` bindings"
            ),
            self.span(),
        )
    }

    fn parse_pipeline(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_or()?;
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_and()?;
        let mut chain = 0usize;
        while self.at(&Tok::OrOr) {
            self.bump();
            let rhs = self.parse_and()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_equality()?;
        let mut chain = 0usize;
        while self.at(&Tok::AndAnd) {
            self.bump();
            let rhs = self.parse_equality()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::Binary { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_with()?;
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    /// `expr with field: value, field: value` — record update. After a comma,
    /// the update list only continues when `ident :` follows (so `f(t with
    /// x: 1, other)` parses `other` as the next call argument).
    fn parse_with(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_range()?;
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
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
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        let mut chain = 0usize;
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
            chain += 1;
            if chain > MAX_EXPR_DEPTH {
                return Err(self.expr_too_deep());
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        // Unlike the chain loops above, a prefix-operator run (`------x`,
        // `!!!!!!!!x`) recurses directly into `parse_unary` itself rather
        // than looping -- so it shares `parse_expr`'s own `self.depth`
        // counter (genuine call/return-symmetric recursion) instead of a
        // local chain counter. The recursive call's result is captured
        // (not `?`-propagated immediately) so `self.depth` is decremented
        // on EVERY exit path, including an error bubbling up from deeper in
        // the chain -- otherwise a rejected pathologically-deep chain would
        // leave `self.depth` permanently inflated, false-positive-failing
        // unrelated, ordinarily-shallow expressions later in the same file.
        match self.peek() {
            Tok::Minus => {
                self.depth += 1;
                if self.depth > MAX_EXPR_DEPTH {
                    self.depth -= 1;
                    return Err(self.expr_too_deep());
                }
                let span = self.span();
                self.bump();
                let operand = self.parse_unary();
                self.depth -= 1;
                let operand = operand?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Unary { op: UnOp::Neg, operand: Box::new(operand) }, span })
            }
            Tok::Bang => {
                self.depth += 1;
                if self.depth > MAX_EXPR_DEPTH {
                    self.depth -= 1;
                    return Err(self.expr_too_deep());
                }
                let span = self.span();
                self.bump();
                let operand = self.parse_unary();
                self.depth -= 1;
                let operand = operand?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Unary { op: UnOp::Not, operand: Box::new(operand) }, span })
            }
            Tok::KwAwait => {
                self.depth += 1;
                if self.depth > MAX_EXPR_DEPTH {
                    self.depth -= 1;
                    return Err(self.expr_too_deep());
                }
                let span = self.span();
                self.bump();
                let operand = self.parse_unary();
                self.depth -= 1;
                let operand = operand?;
                let span = span.merge(operand.span);
                Ok(Expr { kind: ExprKind::Await(Box::new(operand)), span })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        let mut chain = 0usize;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let args = self.parse_args()?;
                    let end = self.expect(Tok::RParen)?;
                    let span = expr.span.merge(end);
                    expr = Expr { kind: ExprKind::Call { callee: Box::new(expr), args }, span };
                    chain += 1;
                    if chain > MAX_EXPR_DEPTH {
                        return Err(self.expr_too_deep());
                    }
                }
                Tok::Dot => {
                    self.bump();
                    let (name, nspan) = self.expect_ident()?;
                    if self.at(&Tok::LParen) {
                        self.bump();
                        // A REAL, SEVERE bug found+fixed (production-hardening
                        // PR-it915, survey #71): this site used to silently
                        // DISCARD every argument's own `name` here
                        // (`.map(|a| a.value)`), with NO diagnostic anywhere
                        // downstream -- so `recv.method(b: 1, a: 2)` was
                        // accepted by `kupl check` and then executed
                        // POSITIONALLY in WRITTEN order on every engine,
                        // silently reversing the caller's evident intent
                        // whenever two same-typed parameters were swapped, a
                        // genuine SILENT VALUE-CORRUPTION bug. Now keeps the
                        // full `Arg` (name + value) so downstream passes that
                        // know the difference between "this is a genuine
                        // method call on a value" and "this dotted call is
                        // actually a cross-package qualified constructor
                        // call" (only `resolve.rs`, via its own dependency-
                        // alias table, can tell -- the parser cannot) can
                        // handle each correctly: `resolve.rs`'s `is_dep`
                        // rewrite now preserves names into the resulting
                        // `Call` node (a genuine cross-package constructor
                        // call keeps full named-arg support, matching a
                        // same-package constructor call); `check.rs`'s
                        // `infer_method` rejects a named argument on a
                        // GENUINE method call with K0241, mirroring
                        // `ExprKind::Call`'s own identical existing check.
                        let args = self.parse_args()?;
                        let end = self.expect(Tok::RParen)?;
                        let span = expr.span.merge(end);
                        expr = Expr { kind: ExprKind::MethodCall { recv: Box::new(expr), name, args }, span };
                    } else {
                        let span = expr.span.merge(nspan);
                        expr = Expr { kind: ExprKind::Field { recv: Box::new(expr), name }, span };
                    }
                    chain += 1;
                    if chain > MAX_EXPR_DEPTH {
                        return Err(self.expr_too_deep());
                    }
                }
                Tok::Question => {
                    let end = self.span();
                    self.bump();
                    let span = expr.span.merge(end);
                    expr = Expr { kind: ExprKind::Try(Box::new(expr)), span };
                    chain += 1;
                    if chain > MAX_EXPR_DEPTH {
                        return Err(self.expr_too_deep());
                    }
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

    /// A REAL bug found+fixed (production-hardening PR-it890, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// unlike EVERY other recursive-descent path in this file (the chain
    /// loops guarded via PR-it713's local `chain` counters, `parse_unary`/
    /// `parse_pattern_primary`/`parse_block`/`parse_ty` all guarded via
    /// `self.depth`), a chained `else if` used to recurse straight into
    /// `parse_if_inner` again with NO depth accounting at all -- a long
    /// `if x { } else if x { } else if x { } ...` chain grows the native
    /// Rust call stack by one frame per arm, completely bypassing
    /// `MAX_EXPR_DEPTH`. Confirmed live before this fix: a generated file
    /// with ~3,000,000 chained `else if` arms crashed `kupl check` with an
    /// uncatchable native stack overflow (`fatal runtime error: stack
    /// overflow, aborting`, exit 134/SIGABRT) instead of a clean K0121
    /// diagnostic -- ordinary, non-adversarial generated code (e.g. an
    /// auto-generated dispatch/state-machine chain), not deliberately
    /// pathological input. Fixed by wrapping the function in the SAME
    /// depth-guard pattern `parse_expr` itself already uses (increment,
    /// check, decrement around a call to the renamed inner
    /// implementation), so `parse_if`'s own recursive `else if` calls now
    /// count against the shared `MAX_EXPR_DEPTH` budget like every other
    /// nesting construct in the grammar.
    fn parse_if(&mut self) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(self.expr_too_deep());
        }
        let r = self.parse_if_inner();
        self.depth -= 1;
        r
    }

    fn parse_if_inner(&mut self) -> PResult<Expr> {
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

    /// PR-it714: `parse_pattern_primary` recurses into itself TWICE without
    /// going through any other depth-guarded entry point -- once indirectly
    /// via `parse_pattern()` (a constructor pattern's args, `Some(Some(...))`)
    /// and once DIRECTLY via its own `@`-binding arm (`x @ y @ z @ ...`).
    /// Structurally the exact same gap it713 just closed for expressions: a
    /// recursive-descent call chain that never touches `self.depth`, so a
    /// deep nested-constructor or `@`-chain pattern used to bypass K0121
    /// entirely and stack-overflow the parser/checker/interpreter with an
    /// uncatchable crash (confirmed live: `Some(` * 500_000 `+ "1" + `)` *
    /// 500_000` aborted with "fatal runtime error: stack overflow"). Guarded
    /// the same way `parse_expr` guards itself: push/pop `self.depth` around
    /// the entire body, symmetric on every exit path (the body always
    /// returns through `r`, never via an internal early `?` past the pop).
    fn parse_pattern_primary(&mut self) -> PResult<Pattern> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(self.pattern_too_deep());
        }
        let r = self.parse_pattern_primary_inner();
        self.depth -= 1;
        r
    }

    fn pattern_too_deep(&self) -> Diag {
        Diag::error(
            "K0121",
            format!(
                "pattern nesting too deep (limit is {MAX_EXPR_DEPTH}) — break it into a helper `fun` or intermediate `match` arms"
            ),
            self.span(),
        )
    }

    fn parse_pattern_primary_inner(&mut self) -> PResult<Pattern> {
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
                        format!("expected an integer after `-` in pattern, found {}", other.describe()),
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
                        StrPart::Expr(src, off) => {
                            // A REAL bug found+fixed (PR-it602, a parser.rs span-precision
                            // sweep mirroring check.rs's it585): this used to reuse `span`
                            // (the WHOLE string-literal token) instead of the interpolation's
                            // own offset -- `off` is already the file-absolute byte position
                            // of the `{...}` expression's source text, the SAME mechanism
                            // `str_parts_expr`/`parse_expr_fragment` use elsewhere in this
                            // file to build a precise span from an interpolation offset.
                            let interp_span = Span::new(*off, *off + src.len() as u32);
                            return Err(Diag::error(
                                "K0112",
                                "string patterns cannot contain interpolation",
                                interp_span,
                            ));
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
            Tok::LBracket => Err(Diag::error(
                "K0113",
                "expected a pattern, found `[` — KUPL has no list/cons patterns; \
                 recurse over a list with `match xs.first() { None => ... Some(h) => ... }` \
                 and `xs.drop(1)` for the tail"
                    .to_string(),
                span,
            )),
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
                format!(
                    "type nesting too deep (limit is {MAX_EXPR_DEPTH}) — break it into a named `type` alias"
                ),
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
    fn k0121_names_the_limit_and_the_fix() {
        // Error-message round 32 (PR-it495): K0121 (expression/type nesting too deep) said only
        // "expression nesting too deep" / "type nesting too deep" -- no limit, no fix. Now it names
        // the limit (MAX_EXPR_DEPTH = 128) and suggests the concrete remedy: intermediate `let`
        // bindings for deep expressions, a named `type` alias for deep type annotations. Run on a
        // production-sized (8 MiB) stack, matching check.rs's deep_nesting_is_a_clean_error_not_a_hang
        // -- the default 2 MiB test-thread stack is smaller than the real CLI main thread, and the
        // recursive-descent parser recurses (bounded by K0121) while building the pathological input.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let deep_expr = format!("fun main() {{ let x = {}1{} }}\n", "[".repeat(200), "]".repeat(200));
                let (_, diags) = parse(&deep_expr);
                assert!(
                    diags.iter().any(|d| d.code == "K0121" && d.message.contains("limit is 128") && d.message.contains("`let` bindings")),
                    "expr nesting K0121 must name the limit and the let-binding fix: {diags:?}"
                );
                let deep_ty = format!("fun f(x: {}Int{}) -> Int {{ 0 }}\nfun main() {{ 0 }}\n", "List[".repeat(200), "]".repeat(200));
                let (_, diags) = parse(&deep_ty);
                assert!(
                    diags.iter().any(|d| d.code == "K0121" && d.message.contains("limit is 128") && d.message.contains("`type` alias")),
                    "type nesting K0121 must name the limit and the type-alias fix: {diags:?}"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A REAL, LIVE-CRASHING bug (PR-it713): `MAX_EXPR_DEPTH` only guarded
    /// genuine RECURSIVE nesting (`parse_expr` re-entering itself, e.g. via
    /// parens/brackets) via `self.depth`. Every LEFT-ASSOCIATIVE chain loop
    /// (`parse_pipeline`, `parse_or`, `parse_and`, `parse_equality`,
    /// `parse_comparison`, `parse_additive`, `parse_multiplicative`,
    /// `parse_postfix`, `parse_with`) builds its `lhs` via straight-line
    /// iteration, calling `self.parse_expr()` exactly ZERO times per chain
    /// link -- so a long flat chain of the SAME operator (e.g. `1 + 1 + 1 +
    /// ... ` a few hundred thousand times, entirely realistic for generated
    /// or templated source) bypassed `self.depth` completely and built an
    /// AST just as deep as an equivalent pile of nested parens, deep enough
    /// to STACK-OVERFLOW every downstream recursive consumer (`kupl fmt`,
    /// `kupl check`, `kupl run`, `kupl run --vm` all confirmed to crash with
    /// a fatal, uncatchable "stack overflow, aborting" process abort on a
    /// live repro before this fix -- not a graceful diagnostic). A prefix
    /// chain (`------x`) had the same gap via `parse_unary`'s own direct
    /// self-recursion, never touching `self.depth` either. Fixed by giving
    /// every chain loop a local counter (or, for `parse_unary`'s genuine
    /// recursion, symmetric `self.depth` push/pop) checked against the SAME
    /// `MAX_EXPR_DEPTH` limit, turning the crash into the same clean K0121
    /// the recursive-nesting case already produced.
    #[test]
    fn a_long_flat_operator_chain_is_a_clean_k0121_not_a_stack_overflow() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                // `n` TERMS join into `n - 1` operators for the binary-chain
                // cases, so `MAX_EXPR_DEPTH + 2` terms gives one operator over
                // the limit for every vulnerable chain-loop kind -- each used
                // to build unbounded AST depth with ZERO diagnostic before.
                let n = MAX_EXPR_DEPTH + 2;
                let cases: Vec<(&str, String)> = vec![
                    ("additive (`+`) chain", format!("fun main() -> Int {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "1").collect::<Vec<_>>().join(" + "))),
                    ("multiplicative (`*`) chain", format!("fun main() -> Int {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "1").collect::<Vec<_>>().join(" * "))),
                    ("or (`||`) chain", format!("fun main() -> Bool {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "true").collect::<Vec<_>>().join(" || "))),
                    ("and (`&&`) chain", format!("fun main() -> Bool {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "true").collect::<Vec<_>>().join(" && "))),
                    ("equality (`==`) chain", format!("fun main() -> Bool {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "1").collect::<Vec<_>>().join(" == "))),
                    ("comparison (`<`) chain", format!("fun main() -> Bool {{ let x = {}\n    return x\n}}\n", (0..n).map(|_| "1").collect::<Vec<_>>().join(" < "))),
                    ("pipeline (`|>`) chain", format!("fun id(x: Int) -> Int {{ x }}\nfun main() -> Int {{ let x = 1{}\n    return x\n}}\n", " |> id".repeat(n))),
                    ("postfix try (`?`) chain", format!("fun main() -> Int {{ let x = 1{}\n    return x\n}}\n", "?".repeat(n))),
                    ("unary prefix (`-`) chain", format!("fun main() -> Int {{ let x = {}1\n    return x\n}}\n", "-".repeat(n))),
                ];
                for (label, src) in &cases {
                    let (_, diags) = parse(src);
                    assert!(
                        diags.iter().any(|d| d.code == "K0121"),
                        "{label} of length {n} must hit K0121, not silently build an unbounded AST: {diags:?}\nsrc (truncated): {:.200}",
                        src
                    );
                }
                // The SAME chain kinds at an ordinary, well-under-the-limit length
                // must NOT false-positive -- confirms the new counters don't
                // regress normal code (a realistic long-ish `+` chain, a short
                // `.method()` chain, a couple of `|>` pipeline stages).
                let ok_add = format!("fun main() -> Int {{ let x = {}\n    return x\n}}\n", (0..20).map(|_| "1").collect::<Vec<_>>().join(" + "));
                let (_, diags) = parse(&ok_add);
                assert!(diags.is_empty(), "a 20-term `+` chain must parse cleanly: {diags:?}");
                let ok_pipe = "fun id(x: Int) -> Int { x }\nfun main() -> Int { let x = 1 |> id |> id |> id\n    return x\n}\n";
                let (_, diags) = parse(ok_pipe);
                assert!(diags.is_empty(), "a 3-stage pipeline must parse cleanly: {diags:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A REAL, LIVE-CRASHING bug (PR-it714), the direct follow-up it713
    /// flagged: `parse_pattern_primary` recurses into itself TWICE without
    /// ever touching `self.depth` -- indirectly via `parse_pattern()` for a
    /// nested constructor pattern's args (`Some(Some(Some(...)))`), and
    /// directly via its own `@`-binding arm (`a @ b @ c @ ...`). Confirmed
    /// live BEFORE this fix: a 500,000-level-deep `Some(Some(...1...))`
    /// pattern crashed `kupl fmt`/`kupl check`/`kupl run`/`kupl run --vm`/
    /// `kupl native` ALL FIVE with the same uncatchable "fatal runtime
    /// error: stack overflow, aborting" abort it713 fixed for expressions --
    /// this is the pattern-grammar sibling of that exact gap. Fixed by
    /// guarding `parse_pattern_primary` itself the same way `parse_expr`
    /// guards itself (symmetric `self.depth` push/pop around the whole
    /// body, decrement captured via `let r = ...; self.depth -= 1; r` so it
    /// fires on every exit path, not just success).
    #[test]
    fn a_deeply_nested_constructor_or_at_binding_pattern_is_a_clean_k0121_not_a_stack_overflow() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let n = MAX_EXPR_DEPTH + 2;
                let nested_ctor = format!(
                    "fun main() -> Int {{ let x = match 1 {{ {}1{} => 1\n        _ => 0 }}\n    return x\n}}\n",
                    "Some(".repeat(n),
                    ")".repeat(n)
                );
                let (_, diags) = parse(&nested_ctor);
                assert!(
                    diags.iter().any(|d| d.code == "K0121"),
                    "a {n}-deep nested constructor pattern must hit K0121, not silently build an unbounded AST: {diags:?}"
                );
                let at_chain = format!(
                    "fun main() -> Int {{ let x = match 1 {{ {} 1 => 1\n        _ => 0 }}\n    return x\n}}\n",
                    (0..n).map(|i| format!("a{i} @")).collect::<Vec<_>>().join(" ")
                );
                let (_, diags) = parse(&at_chain);
                assert!(
                    diags.iter().any(|d| d.code == "K0121"),
                    "a {n}-deep `@`-binding chain pattern must hit K0121, not silently build an unbounded AST: {diags:?}"
                );
                // An ordinary, well-under-the-limit nested constructor pattern
                // (Option<Option<Option<Option<Option<Int>>>>>-shaped) must NOT
                // false-positive -- confirms the new guard doesn't regress
                // normal pattern matching.
                let ok = "fun main() -> Int {\n    let x = match 1 {\n        Some(Some(Some(Some(Some(v))))) => v\n        _ => 0\n    }\n    return x\n}\n";
                let (_, diags) = parse(ok);
                assert!(diags.is_empty(), "a 5-level nested constructor pattern must parse cleanly: {diags:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A REAL, LIVE-CRASHING bug (PR-it715), found by the systematic sweep
    /// it714 called for after TWO consecutive iterations turned up crashes in
    /// the same vein: `while`/`for`/`forall` statement bodies recurse into
    /// `parse_block` (-> `parse_stmt` -> the SAME statement kind's body ->
    /// `parse_block` -> ...) without EVER going through `parse_expr`'s
    /// guarded entry point. `if`/lambda/match-arm bodies happened to be safe
    /// only because `if`/lambda/match are EXPRESSIONS reached via
    /// `parse_expr`, not because `parse_block` itself was guarded -- a purely
    /// STATEMENT-level nesting chain (`while true { while true { ... } }`,
    /// same for `for`/`forall`) bypassed K0121 entirely. Confirmed live
    /// BEFORE this fix: a 500,000-deep `while true { ... }` chain crashed
    /// `kupl fmt`/`kupl check`/`kupl run`/`kupl run --vm`/`kupl native` ALL
    /// FIVE with the identical uncatchable "fatal runtime error: stack
    /// overflow, aborting" abort it713/it714 already fixed for expressions
    /// and patterns -- the STATEMENT-grammar sibling of that same gap. Fixed
    /// by guarding `parse_block` itself (renamed body to `parse_block_inner`)
    /// -- the ONE shared function every block body funnels through (`if`/
    /// lambda/match arms/fun bodies/component members all included), rather
    /// than each of `while`/`for`/`forall`'s call sites individually (the
    /// narrower-shared-boundary-point pattern, PR-it638/it639/it692).
    #[test]
    fn deeply_nested_while_for_forall_bodies_are_a_clean_k0121_not_a_stack_overflow() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let n = MAX_EXPR_DEPTH + 2;
                let cases: Vec<(&str, String)> = vec![
                    (
                        "while",
                        format!("fun main() {{\n{}break\n{}}}\n", "while true {\n".repeat(n), "}\n".repeat(n)),
                    ),
                    (
                        "for",
                        format!("fun main() {{\n{}break\n{}}}\n", "for i in 0..1 {\n".repeat(n), "}\n".repeat(n)),
                    ),
                    (
                        "forall",
                        format!("fun main() {{\n{}break\n{}}}\n", "forall a: Int {\n".repeat(n), "}\n".repeat(n)),
                    ),
                ];
                for (label, src) in &cases {
                    let (_, diags) = parse(src);
                    assert!(
                        diags.iter().any(|d| d.code == "K0121"),
                        "a {n}-deep nested `{label}` body must hit K0121, not silently build an unbounded AST: {diags:?}"
                    );
                }
                // An ordinary, well-under-the-limit nested `while` (5 levels)
                // must NOT false-positive -- confirms the new guard doesn't
                // regress normal loop nesting.
                let ok = "fun main() {\n    while true {\n        while true {\n            while true {\n                while true {\n                    while true {\n                        break\n                    }\n                    break\n                }\n                break\n            }\n            break\n        }\n        break\n    }\n}\n";
                let (_, diags) = parse(ok);
                assert!(diags.is_empty(), "a 5-level nested `while` must parse cleanly: {diags:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A REAL, LIVE-CRASHING bug (PR-it890, an Explore survey finding,
    /// independently re-verified live before implementing): a chained
    /// `else if` recurses straight into `parse_if_inner` again without
    /// EVER touching `self.depth` -- unlike every other recursive-descent
    /// path in this file (the it713/it714/it715 sweep just above). This
    /// gap slipped past that sweep's own reasoning: PR-it715's doc comment
    /// (right above) asserts "`if`/lambda/match-arm bodies happened to be
    /// safe only because `if`/lambda/match are EXPRESSIONS reached via
    /// `parse_expr`" -- true for the FIRST entry into `parse_if`, but the
    /// `else if` chain's OWN self-recursion never re-enters through
    /// `parse_expr` at all, so it was never actually covered. Confirmed
    /// live BEFORE this fix: a ~3,000,000-arm `if x == 0 {} else if x == 1
    /// {} else if ...` chain crashed `kupl check` with the same
    /// uncatchable "fatal runtime error: stack overflow, aborting" abort
    /// it713-it715 already fixed for expressions/patterns/loop bodies --
    /// the `if`-chain-grammar sibling of that exact gap. Fixed by guarding
    /// `parse_if` itself the same way `parse_expr` guards itself
    /// (symmetric `self.depth` push/pop wrapping a renamed
    /// `parse_if_inner`).
    #[test]
    fn a_deeply_chained_else_if_is_a_clean_k0121_not_a_stack_overflow() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let n = MAX_EXPR_DEPTH + 2;
                let chain: String =
                    (0..n).map(|i| format!("else if x == {i} {{ {i} }}\n")).collect::<Vec<_>>().join("");
                let src = format!("fun f(x: Int) -> Int {{\n    if x == 0 {{ 0 }}\n{chain}    else {{ -1 }}\n}}\n");
                let (_, diags) = parse(&src);
                assert!(
                    diags.iter().any(|d| d.code == "K0121"),
                    "a {n}-deep chained `else if` must hit K0121, not silently build an unbounded AST: {diags:?}"
                );
                // An `if let` chained `else if` (the desugared-to-`match` path,
                // parser.rs's OTHER self-recursive `self.parse_if()?` call site)
                // must ALSO be caught by the same guard.
                let if_let_chain: String = (0..n)
                    .map(|i| format!("else if let Some(y) = opt({i}) {{ y }}\n"))
                    .collect::<Vec<_>>()
                    .join("");
                let if_let_src = format!(
                    "fun opt(x: Int) -> Option[Int] {{ Some(x) }}\nfun f(x: Int) -> Int {{\n    if let Some(y) = opt(x) {{ y }}\n{if_let_chain}    else {{ -1 }}\n}}\n"
                );
                let (_, diags) = parse(&if_let_src);
                assert!(
                    diags.iter().any(|d| d.code == "K0121"),
                    "a {n}-deep chained `if let`/`else if` must hit K0121, not silently build an unbounded AST: {diags:?}"
                );
                // An ordinary, well-under-the-limit `else if` chain (5 arms)
                // must NOT false-positive -- confirms the new guard doesn't
                // regress normal conditional chains.
                let ok = "fun classify(x: Int) -> Str {\n    if x < 0 {\n        \"negative\"\n    } else if x == 0 {\n        \"zero\"\n    } else if x < 10 {\n        \"small\"\n    } else if x < 100 {\n        \"medium\"\n    } else {\n        \"large\"\n    }\n}\n";
                let (_, diags) = parse(ok);
                assert!(diags.is_empty(), "a 5-arm `else if` chain must parse cleanly: {diags:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn k0120_overflowing_duration_names_the_value_and_the_reason() {
        // Error-message round 42 (PR-it535): `on every 99999999999999999h` -- a duration
        // literal whose millisecond value overflows i64 -- was flat "duration is too
        // large", not naming the offending literal or explaining WHY (durations are
        // stored as milliseconds in a 64-bit integer internally, so a huge `h`/`m`
        // value times its per-unit multiplier can overflow even though the source
        // digits themselves parsed fine as an Int).
        let src = "component T {\n    intent \"t\"\n    out tick: Int\n    on every 99999999999999999h {\n        emit tick(1)\n    }\n}\n";
        let (_, diags) = parse(src);
        assert!(
            diags.iter().any(|d| d.code == "K0120" && d.message.contains("`99999999999999999h`") && d.message.contains("overflows")),
            "overflowing duration must name the literal and explain the overflow: {diags:?}"
        );
        // A normal, non-overflowing duration still parses cleanly.
        ok("component T {\n    intent \"t\"\n    out tick: Int\n    on every 10ms {\n        emit tick(1)\n    }\n}\n");
    }

    /// A REAL, uncatchable/UB-inducing bug found+fixed (production-hardening
    /// PR-it728, found via a scoped Explore survey): `parse_duration`'s
    /// existing overflow guard (`checked_mul`, tested above) only protects
    /// the UNIT-CONVERSION multiplication -- a duration expressed directly
    /// in `ms` (where the multiplier is 1, so the multiplication trivially
    /// never overflows) sailed through with NO cap on the resulting
    /// millisecond value at all. Confirmed live: `on every
    /// 9223372036854775807ms` crashed BOTH `kupl run` (interp.rs:363, timer
    /// rescheduling's `next_fire += interval`) and `kupl run --vm`
    /// (vm.rs:254, identical) with a raw "attempt to add with overflow"
    /// panic -- and did NOT crash `kupl native` at all: C signed-overflow is
    /// UB, which in practice silently WRAPS `next_fire`, so the timer
    /// re-fires FOREVER inside one `k_run_timers` call (a genuine
    /// three-way DIVERGENCE, not just a shared crash). A SEPARATE,
    /// independently-reachable crash site needs no timer at all: an
    /// `example` block's `advance` step feeds the SAME unchecked
    /// `self.now + dur` arithmetic (interp.rs:320) with ZERO validation
    /// (`check.rs`'s `ExampleStep::Advance` arm does nothing at all).
    /// `MAX_DURATION_MS` (100 years) closes all of this in the ONE place
    /// every engine's duration literals pass through.
    #[test]
    fn k0120_a_duration_that_would_overflow_the_virtual_clock_on_reschedule_is_capped() {
        let (_, diags) = parse(
            "component T {\n    intent \"t\"\n    out tick: Int\n    on every 9223372036854775807ms {\n        emit tick(1)\n    }\n}\n",
        );
        assert!(
            diags.iter().any(|d| d.code == "K0120"
                && d.message.contains("9223372036854775807ms")
                && d.message.contains("100 years")),
            "{diags:?}"
        );
        // the SAME cap applies to `on after`.
        let (_, after_diags) = parse(
            "component T {\n    intent \"t\"\n    out tick: Int\n    on after 9223372036854775807ms {\n        emit tick(1)\n    }\n}\n",
        );
        assert!(after_diags.iter().any(|d| d.code == "K0120" && d.message.contains("100 years")), "{after_diags:?}");
        // an `example` block's `advance` step is the SEPARATE, independently
        // reachable crash site (no timer involved at all) -- same cap applies.
        let (_, advance_diags) = parse(
            "app A {\n    intent \"a\"\n    example {\n        advance 9223372036854775807ms\n    }\n}\n",
        );
        assert!(
            advance_diags.iter().any(|d| d.code == "K0120" && d.message.contains("100 years")),
            "{advance_diags:?}"
        );
        // exactly at the cap: fine. one millisecond over: rejected.
        ok("component T {\n    intent \"t\"\n    out tick: Int\n    on every 3153600000000ms {\n        emit tick(1)\n    }\n}\n");
        let (_, over_diags) = parse(
            "component T {\n    intent \"t\"\n    out tick: Int\n    on every 3153600000001ms {\n        emit tick(1)\n    }\n}\n",
        );
        assert!(over_diags.iter().any(|d| d.code == "K0120" && d.message.contains("100 years")), "{over_diags:?}");
        // an ordinary, realistic duration is completely unaffected.
        ok("component T {\n    intent \"t\"\n    out tick: Int\n    on every 500ms {\n        emit tick(1)\n    }\n}\n");
    }

    #[test]
    fn k0112_span_points_at_the_interpolation_not_the_whole_string_literal() {
        // A REAL bug found+fixed (PR-it602, a parser.rs span-precision sweep mirroring
        // check.rs's it585): "string patterns cannot contain interpolation" used to
        // underline the WHOLE string-literal token, no matter how long, instead of the
        // `{...}` interpolation that's actually the problem -- even though `StrPart::
        // Expr`'s own byte offset (the SAME mechanism `str_parts_expr`/
        // `parse_expr_fragment` already use elsewhere in this file) was sitting right
        // there, unused.
        let src = "fun f(s: Str) -> Int {\n    match s {\n        \"prefix-aaaaaaaaaa {x} bbbbbbbbbb-suffix\" => 1\n        _ => 2\n    }\n}\n";
        let (_, diags) = parse(src);
        let d = diags.iter().find(|d| d.code == "K0112").expect("K0112 must fire");
        let text = &src[d.span.start as usize..d.span.end as usize];
        assert_eq!(text, "x", "span must cover just the interpolated expression, not the whole string literal: {text:?}");
    }

    #[test]
    fn keyword_typos_suggest_the_closest_valid_keyword() {
        // A NEW capability (PR-it603, part 2 of the parser.rs sweep this campaign
        // started with K0112's span fix): parser.rs had ZERO did-you-mean coverage
        // before this, unlike check.rs's ~95 `self.err(...)` sites (it581/582) -- a
        // typo'd keyword (which lexes as a plain identifier, not the keyword's own
        // token) at one of the three smallest, cleanest keyword-dispatched fallback
        // sites now suggests the closest valid keyword, reusing `check::suggest`
        // rather than a second fuzzy-match implementation.
        let (_, diags) = parse("fnu main() {\n    print(\"hi\")\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0103" && d.message.contains("did you mean `fun`?")),
            "top-level `fnu` should suggest `fun`: {diags:?}"
        );
        let (_, diags) = parse("contract Store {\n    intetn \"kv\"\n    expose fun get(k: Str) -> Int\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0116" && d.message.contains("did you mean `intent`?")),
            "contract-body `intetn` should suggest `intent`: {diags:?}"
        );
        let (_, diags) = parse(
            "component C {\n    intent \"c\"\n    in click: Event\n    state n: Int = 0\n    on click { n = n + 1 }\n    example {\n        sned click\n    }\n}\n",
        );
        assert!(
            diags.iter().any(|d| d.code == "K0106" && d.message.contains("did you mean `send`?")),
            "example-step `sned` should suggest `send`: {diags:?}"
        );
        // no false-positive suggestion when nothing is close enough, or when the
        // offending token isn't even an identifier (e.g. an integer literal).
        let (_, diags) = parse("xyzzyplugh main() {\n    print(\"hi\")\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0103" && !d.message.contains("did you mean")),
            "a genuinely unrelated identifier must not get a false-positive suggestion: {diags:?}"
        );
        let (_, diags) = parse("123 main() {\n    print(\"hi\")\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0103" && !d.message.contains("did you mean")),
            "a non-identifier token must never get a suggestion: {diags:?}"
        );
    }

    #[test]
    fn k0107_component_body_typos_suggest_the_closest_keyword() {
        // Closes the K0107 gap it603's memory entry deliberately deferred: component
        // bodies accept 16 constructs across many separate match arms (a mix of hard
        // keywords like `wire`/`supervise` and two SOFT/contextual keywords, `out`
        // and `state`, matched by string comparison rather than `token::keyword`) --
        // the candidate list was traced from every arm this file's own
        // `parse_component_member` match actually accepts, not copied from the
        // (abbreviated, "…"-truncated) message text.
        let (_, diags) = parse("component C {\n    intent \"c\"\n    wier x.a -> y.b\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0107" && d.message.contains("did you mean `wire`?")),
            "`wier` should suggest `wire`: {diags:?}"
        );
        let (_, diags) = parse("component C {\n    intent \"c\"\n    stat n: Int = 0\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0107" && d.message.contains("did you mean `state`?")),
            "`stat` should suggest the SOFT keyword `state`: {diags:?}"
        );
        let (_, diags) = parse("component C {\n    intent \"c\"\n    supervize child restart never\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0107" && d.message.contains("did you mean `supervise`?")),
            "`supervize` should suggest `supervise`: {diags:?}"
        );
        // an unrelated identifier still gets no false-positive suggestion.
        let (_, diags) = parse("component C {\n    intent \"c\"\n    xyzzyplugh\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0107" && !d.message.contains("did you mean")),
            "an unrelated identifier must not get a false-positive suggestion: {diags:?}"
        );
    }

    #[test]
    fn k0103_k0106_k0107_k0116_body_error_templates_are_consistent() {
        // A REAL wording inconsistency deferred by it602's memory entry (message-
        // wording-consistency sweep #2 of parser.rs, finding #2 of 2): the four
        // "unrecognized construct in a scope" diagnostics (top-level, contract body,
        // component body, example body) used THREE different sentence shapes for the
        // identical situation. K0116 (contract) and K0107 (component) already agreed
        // on "unexpected {found} in <location> (expected LIST)" -- converged K0103
        // (top-level) and K0106 (example) onto that same template rather than
        // inventing a fourth shape, since it was already the majority form.
        let (_, diags) = parse("123\n");
        assert!(
            diags.iter().any(|d| d.code == "K0103" && d.message.starts_with("unexpected ") && d.message.contains("at the top level (expected")),
            "K0103 should use the same \"unexpected X in/at LOCATION (expected LIST)\" template as K0106/K0107/K0116: {diags:?}"
        );
        let (_, diags) = parse("contract Store {\n    intent \"kv\"\n    123\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0116" && d.message.starts_with("unexpected ") && d.message.contains("in contract body (expected")),
            "K0116's existing template, unchanged: {diags:?}"
        );
        let (_, diags) = parse(
            "component C {\n    intent \"c\"\n    in click: Event\n    state n: Int = 0\n    on click { n = n + 1 }\n    example {\n        123\n    }\n}\n",
        );
        assert!(
            diags.iter().any(|d| d.code == "K0106" && d.message.starts_with("unexpected ") && d.message.contains("in example body (expected")),
            "K0106 should use the same template as K0107/K0116, not its old \"blocks contain LIST steps; found X\" shape: {diags:?}"
        );
        let (_, diags) = parse("component C {\n    intent \"c\"\n    123\n}\n");
        assert!(
            diags.iter().any(|d| d.code == "K0107" && d.message.starts_with("unexpected ") && d.message.contains("in component body (expected")),
            "K0107's existing template, unchanged: {diags:?}"
        );
    }

    #[test]
    fn k0111_wording_is_consistent_between_its_two_sites() {
        // The other half of it602's deferred wording finding: K0111 fires from two
        // sibling sites (integer after a unary `-` in a pattern, and an integer upper
        // bound in a range pattern) that disagreed on an indefinite article --
        // "expected integer after..." vs. "expected an integer...". Added the
        // missing "an" to the first site rather than removing it from the second,
        // since "expected an integer" reads more naturally as a full sentence.
        let (_, diags) = parse("fun f(x: Int) -> Int { match x { -a => 1, _ => 0 } }\n");
        assert!(
            diags.iter().any(|d| d.code == "K0111" && d.message.starts_with("expected an integer after `-` in pattern")),
            "expected `expected an integer after ...`, got: {diags:?}"
        );
        let (_, diags) = parse("fun f(x: Int) -> Int { match x { 0..a => 1, _ => 0 } }\n");
        assert!(
            diags.iter().any(|d| d.code == "K0111" && d.message.starts_with("expected an integer upper bound in range pattern")),
            "the range-pattern site's existing wording, unchanged: {diags:?}"
        );
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
    fn a_lexer_error_inside_a_string_interpolation_fragment_points_at_its_real_file_location() {
        // A REAL bug found+fixed (production-hardening PR-it753): `parse_expr_
        // fragment` re-lexes a string-interpolation sub-expression (`"...${
        // EXPR }..."`) independently, then shifts its TOKEN spans by `offset`
        // so they read as absolute file coordinates once merged back into the
        // enclosing program -- but its own lexer DIAGNOSTICS were returned
        // raw, still relative to the fragment's own 0-based coordinates. A
        // lexer error inside the interpolation (most naturally: an
        // unterminated nested string literal) was reported pointing at an
        // essentially arbitrary, unrelated location elsewhere in the real
        // file. Live-confirmed BEFORE this fix: this exact program's
        // unterminated-string error (genuinely on line 6) was reported at
        // line 1, column 1 -- a location with no quote character at all.
        //
        // Line 1 is a syntactically PERFECT function declaration with no
        // error of any kind, so the strongest, unambiguous invariant is
        // "no diagnostic from this parse ever points at line 1" -- rather
        // than picking out one specific diagnostic by code, which is
        // fragile here: the OUTER string (`"start ${...`) is itself also
        // unterminated (since its interpolation never closes), producing a
        // SECOND, always-correctly-positioned `K0005` at line 6 regardless
        // of this fix. An earlier version of this test used
        // `diags.iter().find(|d| d.code == "K0005")`, which nondeterministically
        // picked up that unrelated, already-correct K0005 instead of the
        // buggy fragment-relexed one -- passing identically whether the fix
        // was present or absent, exactly the "zero regression value" trap
        // this campaign's own established lesson warns against. Caught via
        // this iteration's own mandatory revert-and-verify step.
        let src = "fun padding_marker_XXXXXXXXXXXXXXXXXXXX() -> Int {\n  1\n}\n\n\
                   fun main() -> Str {\n  \"start ${\"nested unterminated string right here\n}\n";
        let (_, diags) = parse(src);
        assert!(!diags.is_empty(), "expected at least one diagnostic: {diags:?}");
        for d in &diags {
            let (line, _) = crate::diag::line_col(src, d.span.start);
            assert_ne!(
                line, 1,
                "no diagnostic may point at line 1 -- it is syntactically valid code; \
                 a diagnostic here means a fragment's lexer error was reported at the \
                 fragment's own 0-based coordinates instead of the real file location: {diags:?}"
            );
        }
    }

    /// A REAL, live-confirmed SILENT MISPARSE (production-hardening PR-it892,
    /// an Explore survey finding, independently re-verified live before
    /// implementing): `parse_expr_fragment` never checked that `parse_expr`
    /// consumed the WHOLE `{...}` interpolation fragment -- `parse_expr`
    /// stops as soon as it finds one complete expression, so trailing tokens
    /// (most plausibly a forgotten binary operator, e.g. `{a b}` meaning
    /// `{a + b}`) were silently DROPPED from the AST with no diagnostic at
    /// all. Confirmed live BEFORE this fix: `print("sum: {a b}")` printed
    /// `sum: 1`, silently discarding `b` -- identical wrong output on
    /// interp/`--vm`/native, and `kupl fmt` CANONICALIZED the bug into the
    /// source text itself (rewriting `"sum: {a b}"` to `"sum: {a}"`),
    /// meaning `--write` would have made the data loss permanent.
    #[test]
    fn a_string_interpolation_with_trailing_tokens_is_a_clean_error_not_a_silent_misparse() {
        let src = "fun main() -> Int {\n    let a = 1\n    let b = 2\n    a\n}\nfun g() -> Str {\n    \"sum: {a b}\"\n}\n";
        let (_, diags) = parse(src);
        assert!(
            diags.iter().any(|d| d.code == "K0100" && d.message.contains("expected end of expression")),
            "trailing tokens after a complete interpolation expression must be a clean K0100 error, \
             not silently dropped from the AST: {diags:?}"
        );
        // The error must point at the OFFENDING trailing token (`b`), not the
        // start of the interpolation or the whole string literal.
        let d = diags.iter().find(|d| d.code == "K0100" && d.message.contains("expected end of expression")).unwrap();
        let bad_b = src.find("{a b}").unwrap() as u32 + 3; // offset of `b` within `{a b}`
        assert_eq!(d.span.start, bad_b, "span must point at the trailing `b`, not elsewhere: {d:?}");
        // A normal, single-expression interpolation -- including operators,
        // method calls, and a NESTED if-expression interpolation -- must
        // still parse cleanly with zero regression.
        ok("fun main() uses io {\n    let a = 1\n    let b = 2\n    \
            print(\"sum: {a + b}, product: {a * b}, nested: {if a > 0 { \"pos\" } else { \"neg\" }}\")\n}\n");
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

    /// A REAL dead/wrong-code finding fixed (production-hardening PR-it657):
    /// `ast.rs` used to ALSO declare `impl From<&StrPart> for StrPiece`, but a
    /// full-codebase grep found nothing that ever called it -- genuinely dead
    /// code, and worse, semantically WRONG relative to this file's own
    /// `str_parts_expr`: it turned an `{expr}` interpolation into the LITERAL
    /// text `"{expr}"` (`StrPiece::Text`) instead of actually parsing it into
    /// a sub-expression (`StrPiece::Expr`) -- a landmine for a future
    /// refactor that reached for `.into()` instead of this function. This
    /// test locks in the DESIGN that impl would have broken: an interpolated
    /// `{expr}` must parse into a real `StrPiece::Expr` sub-expression (here,
    /// a `Binary` for `name + "!"`), never survive as literal brace text.
    #[test]
    fn string_interpolation_parses_into_a_real_expression_not_literal_brace_text() {
        let p = ok("fun f() -> Str {\n    \"hi {1 + 2}!\"\n}\n");
        let Item::Fun(f) = &p.items[0] else { panic!("expected fun") };
        let Stmt::Expr(e) = &f.body.stmts[0] else { panic!("expected expr stmt") };
        let ExprKind::Str(pieces) = &e.kind else { panic!("expected a string literal") };
        assert_eq!(pieces.len(), 3, "{pieces:?}");
        assert!(matches!(&pieces[0], StrPiece::Text(t) if t == "hi "));
        match &pieces[1] {
            StrPiece::Expr(inner) => {
                assert!(matches!(&inner.kind, ExprKind::Binary { .. }), "{inner:?}");
            }
            other => panic!("interpolation must parse to StrPiece::Expr, not {other:?}"),
        }
        assert!(matches!(&pieces[2], StrPiece::Text(t) if t == "!"));
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
    fn match_with_newline_arms_parses_inside_call_arg_closure() {
        // A `match` with newline-separated arms inside a call-argument closure used to fail to parse
        // (the lexer suppressed newlines inside the enclosing `(`, so the arm separators vanished and
        // a bogus K0109 fired). A `{` block now resets paren depth so newlines are significant again
        // inside it (PR-it265). This is the everyday `map`/`fold`/`filter(fn x { match x { ... } })`
        // shape — it must parse with newline-separated arms, not only with commas.
        assert!(
            parse("fun f() -> Str {\n    [1, 2].map(fn x {\n        match x {\n            1 => \"one\"\n            _ => \"other\"\n        }\n    }).join(\",\")\n}\n").1.is_empty(),
            "newline-separated match arms inside a call-arg closure should parse cleanly"
        );
        // Nested match (arm body is itself a match) inside a fold closure — the Result-collect shape.
        assert!(
            parse("fun f(items: List[Str]) -> Result[List[Int], Str] {\n    items.fold(Ok([]), fn(acc, s) {\n        match acc {\n            Ok(ns) => match s.parse_int() {\n                Some(n) => Ok(ns.push(n))\n                None => Err(\"bad\")\n            }\n            Err(e) => Err(e)\n        }\n    })\n}\n").1.is_empty(),
            "nested newline-arm match inside a fold closure should parse cleanly"
        );
        // Line continuation inside parens (the reason newlines are suppressed there) still works.
        assert!(parse("fun f() -> Int {\n    let x = (1 +\n        2 +\n        3)\n    x\n}\n").1.is_empty());
        assert!(parse("fun f() -> List[Int] {\n    [1,\n        2,\n        3]\n}\n").1.is_empty());
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
    fn cons_pattern_is_rejected_with_first_drop_hint() {
        // `[h, ..t]` (list/cons destructuring) is a common reflex from Haskell/Rust/Scala; KUPL has no
        // list patterns, so the parser should name the real recursion idiom (xs.first()/xs.drop(1))
        // rather than emit a bare "expected a pattern, found `[`" (PR-it304).
        let (_p, diags) = parse("fun f(xs: List[Int]) -> Int {\n    match xs {\n        [h, ..t] => 1\n    }\n}\n");
        let m = &diags.iter().find(|d| d.code == "K0113").expect("K0113").message;
        assert!(m.contains("no list/cons patterns"), "{m}");
        assert!(m.contains("xs.first()"), "{m}");
        assert!(m.contains("xs.drop(1)"), "{m}");
        // The real idiom (match on xs.first(), recurse on xs.drop(1)) still parses cleanly.
        assert!(parse("fun f(xs: List[Int]) -> Int {\n    match xs.first() {\n        None => 0\n        Some(h) => h + f(xs.drop(1))\n    }\n}\n").1.is_empty());
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
