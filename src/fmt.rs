//! `kupl fmt` — the normative canonical formatter.
//!
//! Zero configuration. Any two programs with the same AST render identically:
//! fixed member order inside components (intent → props → in ports → out ports →
//! state → children → wires → handlers → expose → funs → examples), 4-space
//! indent, one statement per line. Note: `x |> f` is canonicalized to `f(x)`
//! at parse time, so the formatter emits the desugared call.

use crate::ast::*;

pub fn format_program(p: &Program) -> String {
    let mut out = String::new();
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match item {
            Item::Fun(f) => fmt_fun(&mut out, f, 0),
            Item::Type(t) => fmt_type(&mut out, t),
            Item::Component(c) => fmt_component(&mut out, c),
            Item::Contract(ct) => fmt_contract(&mut out, ct),
            Item::Law(l) => {
                out.push_str(&format!("law \"{}\" ", escape_str(&l.name)));
                fmt_block(&mut out, &l.body, 0);
                out.push('\n');
            }
        }
    }
    out
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

fn fmt_type(out: &mut String, t: &TypeDecl) {
    // newtype: single variant named like the type with one field `value`
    let is_record = t.variants.len() == 1 && t.variants[0].name == t.name;
    if is_record {
        let v = &t.variants[0];
        if v.fields.len() == 1 && v.fields[0].name == "value" {
            out.push_str(&format!("type {} = new {}\n", t.name, ty_str(&v.fields[0].ty)));
            return;
        }
        out.push_str(&format!("type {} = {{ ", t.name));
        for (i, f) in v.fields.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{}: {}", f.name, ty_str(&f.ty)));
        }
        out.push_str(" }\n");
        return;
    }
    out.push_str(&format!("type {} = ", t.name));
    for (i, v) in t.variants.iter().enumerate() {
        if i > 0 {
            out.push_str(" | ");
        }
        out.push_str(&v.name);
        if !v.fields.is_empty() {
            out.push('(');
            for (j, f) in v.fields.iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{}: {}", f.name, ty_str(&f.ty)));
            }
            out.push(')');
        }
    }
    out.push('\n');
}

fn fmt_fun(out: &mut String, f: &FunDecl, level: usize) {
    indent(out, level);
    if f.is_pub {
        out.push_str("pub ");
    }
    if f.ai.is_some() {
        out.push_str("ai ");
    }
    out.push_str(&format!("fun {}", f.name));
    if !f.type_params.is_empty() {
        out.push_str(&format!("[{}]", f.type_params.join(", ")));
    }
    out.push('(');
    for (i, p) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{}: {}", p.name, ty_str(&p.ty)));
    }
    out.push(')');
    if !f.effects.is_empty() {
        out.push_str(" uses ");
        out.push_str(&f.effects.join(", "));
    }
    if let Some(r) = &f.ret {
        out.push_str(&format!(" -> {}", ty_str(r)));
    }
    out.push(' ');
    if let Some(ai) = &f.ai {
        if !ai.tools.is_empty() {
            // `tools [...]` renders before the return-type-adjacent brace; the
            // ` ` pushed above sits between `-> T` and `tools`.
            out.pop();
            out.push_str(&format!(" tools [{}] ", ai.tools.join(", ")));
        }
        out.push_str("{\n");
        indent(out, level + 1);
        // render from the expression so interpolation `{...}` round-trips
        out.push_str(&format!("intent {}\n", expr_str(&ai.intent_expr, 0)));
        if let Some(model) = &ai.model {
            indent(out, level + 1);
            out.push_str(&format!("model \"{}\"\n", escape_str(model)));
        }
        indent(out, level);
        out.push_str("}\n");
        return;
    }
    fmt_block(out, &f.body, level);
    out.push('\n');
}

fn fmt_contract(out: &mut String, ct: &ContractDecl) {
    out.push_str(&format!("contract {} {{\n", ct.name));
    let mut first = true;
    if let Some(intent) = &ct.intent {
        indent(out, 1);
        out.push_str(&format!("intent \"{}\"\n", escape_str(intent)));
        first = false;
    }
    if !ct.sigs.is_empty() {
        if !first {
            out.push('\n');
        }
        first = false;
        for s in &ct.sigs {
            indent(out, 1);
            out.push_str(&format!("expose fun {}(", s.name));
            for (i, p) in s.params.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{}: {}", p.name, ty_str(&p.ty)));
            }
            out.push(')');
            if !s.effects.is_empty() {
                out.push_str(&format!(" uses {}", s.effects.join(", ")));
            }
            if let Some(r) = &s.ret {
                out.push_str(&format!(" -> {}", ty_str(r)));
            }
            out.push('\n');
        }
    }
    for law in &ct.laws {
        if !first {
            out.push('\n');
        }
        first = false;
        indent(out, 1);
        out.push_str(&format!("law \"{}\" ", escape_str(&law.name)));
        fmt_block(out, &law.body, 1);
        out.push('\n');
    }
    out.push_str("}\n");
}

fn fmt_component(out: &mut String, c: &ComponentDecl) {
    out.push_str(if c.is_app { "app " } else { "component " });
    out.push_str(&c.name);
    if !c.fulfills.is_empty() {
        out.push_str(&format!(" fulfills {}", c.fulfills.join(", ")));
    }
    out.push_str(" {\n");
    let mut first_group = true;
    let mut sep = |out: &mut String, has_items: bool| {
        if has_items {
            if !first_group {
                out.push('\n');
            }
            first_group = false;
        }
    };

    if let Some(intent) = &c.intent {
        sep(out, true);
        indent(out, 1);
        out.push_str(&format!("intent \"{}\"\n", escape_str(intent)));
    }
    sep(out, !c.props.is_empty());
    for p in &c.props {
        indent(out, 1);
        out.push_str(&format!("prop {}: {}", p.name, ty_str(&p.ty)));
        if let Some(d) = &p.default {
            out.push_str(&format!(" = {}", expr_str(d, 0)));
        }
        out.push('\n');
    }
    let ins: Vec<&Port> = c.ports.iter().filter(|p| p.dir == PortDir::In).collect();
    let outs: Vec<&Port> = c.ports.iter().filter(|p| p.dir == PortDir::Out).collect();
    sep(out, !ins.is_empty() || !outs.is_empty());
    for p in &ins {
        indent(out, 1);
        out.push_str(&format!("in {}: {}\n", p.name, ty_str(&p.ty)));
    }
    for p in &outs {
        indent(out, 1);
        out.push_str(&format!("out {}: {}\n", p.name, ty_str(&p.ty)));
    }
    sep(out, !c.state.is_empty());
    for s in &c.state {
        indent(out, 1);
        out.push_str(&format!("state {}", s.name));
        if let Some(t) = &s.ty {
            out.push_str(&format!(": {}", ty_str(t)));
        }
        out.push_str(&format!(" = {}\n", expr_str(&s.init, 0)));
    }
    sep(out, !c.children.is_empty());
    for child in &c.children {
        indent(out, 1);
        out.push_str(&format!("let {} = {}(", child.name, child.component));
        for (i, a) in child.args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if let Some(n) = &a.name {
                out.push_str(&format!("{n}: "));
            }
            out.push_str(&expr_str(&a.value, 0));
        }
        out.push_str(")\n");
    }
    sep(out, !c.wires.is_empty());
    for w in &c.wires {
        indent(out, 1);
        out.push_str(&format!("wire {}.{} -> {}.{}\n", w.from.0, w.from.1, w.to.0, w.to.1));
    }
    sep(out, !c.supervises.is_empty());
    for s in &c.supervises {
        indent(out, 1);
        let policy = match s.policy {
            SupervisePolicy::RestartOnFailure => "on_failure",
            SupervisePolicy::Never => "never",
        };
        out.push_str(&format!("supervise {} restart {policy}\n", s.child));
    }
    // handlers: on start, timers, port handlers (in in-port order), on stop
    let mut handlers: Vec<&Handler> = Vec::new();
    for h in &c.handlers {
        if matches!(h.trigger, Trigger::Start) {
            handlers.push(h);
        }
    }
    for h in &c.handlers {
        if matches!(h.trigger, Trigger::Every(_) | Trigger::After(_)) {
            handlers.push(h);
        }
    }
    for p in &ins {
        for h in &c.handlers {
            if matches!(&h.trigger, Trigger::Port(name) if name == &p.name) {
                handlers.push(h);
            }
        }
    }
    for h in &c.handlers {
        // port handlers whose port isn't declared (checker errors anyway) keep source order
        if matches!(&h.trigger, Trigger::Port(name) if !ins.iter().any(|p| &p.name == name)) {
            handlers.push(h);
        }
    }
    for h in &c.handlers {
        if matches!(h.trigger, Trigger::Stop) {
            handlers.push(h);
        }
    }
    for h in handlers {
        sep(out, true);
        indent(out, 1);
        out.push_str("on ");
        match &h.trigger {
            Trigger::Start => out.push_str("start"),
            Trigger::Stop => out.push_str("stop"),
            Trigger::Port(p) => out.push_str(p),
            Trigger::Every(ms) => out.push_str(&format!("every {}", fmt_duration(*ms))),
            Trigger::After(ms) => out.push_str(&format!("after {}", fmt_duration(*ms))),
        }
        if let Some(param) = &h.param {
            out.push_str(&format!("({param})"));
        }
        out.push(' ');
        fmt_block(out, &h.body, 1);
        out.push('\n');
    }
    for f in &c.exposes {
        sep(out, true);
        indent(out, 1);
        out.push_str("expose ");
        // exposes are implicitly public — never print `pub`
        let mut plain = f.clone();
        plain.is_pub = false;
        let mut tmp = String::new();
        fmt_fun(&mut tmp, &plain, 1);
        out.push_str(tmp.trim_start());
    }
    for f in &c.funs {
        sep(out, true);
        fmt_fun(out, f, 1);
    }
    for ex in &c.examples {
        sep(out, true);
        indent(out, 1);
        out.push_str("example {\n");
        for step in &ex.steps {
            indent(out, 2);
            match step {
                ExampleStep::Send { port, arg, .. } => {
                    out.push_str(&format!("send {port}"));
                    if let Some(a) = arg {
                        out.push_str(&format!("({})", expr_str(a, 0)));
                    }
                }
                ExampleStep::Expect { expr, .. } => {
                    out.push_str(&format!("expect {}", expr_str(expr, 0)));
                }
                ExampleStep::Advance { ms, .. } => {
                    out.push_str(&format!("advance {}", fmt_duration(*ms)));
                }
            }
            out.push('\n');
        }
        indent(out, 1);
        out.push_str("}\n");
    }
    out.push_str("}\n");
}

/// Render virtual milliseconds back to the largest whole-unit duration literal.
fn fmt_duration(ms: i64) -> String {
    for (unit, per) in [("h", 3_600_000i64), ("m", 60_000), ("s", 1000)] {
        if ms % per == 0 {
            return format!("{}{unit}", ms / per);
        }
    }
    format!("{ms}ms")
}

fn fmt_block(out: &mut String, b: &Block, level: usize) {
    out.push_str("{\n");
    for stmt in &b.stmts {
        fmt_stmt(out, stmt, level + 1);
    }
    indent(out, level);
    out.push('}');
}

fn fmt_stmt(out: &mut String, stmt: &Stmt, level: usize) {
    indent(out, level);
    match stmt {
        Stmt::Let { name, ty, init, mutable, .. } => {
            out.push_str(if *mutable { "var " } else { "let " });
            out.push_str(name);
            if let Some(t) = ty {
                out.push_str(&format!(": {}", ty_str(t)));
            }
            out.push_str(&format!(" = {}\n", expr_str(init, 0)));
        }
        Stmt::Assign { target, op, value, .. } => {
            let sym = match op {
                AssignOp::Set => "=",
                AssignOp::Add => "+=",
                AssignOp::Sub => "-=",
                AssignOp::Mul => "*=",
                AssignOp::Div => "/=",
            };
            out.push_str(&format!("{} {} {}\n", expr_str(target, 0), sym, expr_str(value, 0)));
        }
        Stmt::Expr(e) => {
            out.push_str(&expr_str(e, 0));
            out.push('\n');
        }
        Stmt::Return(v, _) => {
            out.push_str("return");
            if let Some(e) = v {
                out.push_str(&format!(" {}", expr_str(e, 0)));
            }
            out.push('\n');
        }
        Stmt::While { cond, body, .. } => {
            out.push_str(&format!("while {} ", expr_str(cond, 0)));
            fmt_block(out, body, level);
            out.push('\n');
        }
        Stmt::For { var, iter, body, .. } => {
            out.push_str(&format!("for {var} in {} ", expr_str(iter, 0)));
            fmt_block(out, body, level);
            out.push('\n');
        }
        Stmt::Emit { port, arg, .. } => {
            out.push_str(&format!("emit {port}"));
            if let Some(a) = arg {
                out.push_str(&format!("({})", expr_str(a, 0)));
            } else {
                out.push_str("()");
            }
            out.push('\n');
        }
        Stmt::Expect(e, _) => {
            out.push_str(&format!("expect {}\n", expr_str(e, 0)));
        }
        Stmt::Forall { vars, body, .. } => {
            let bs: Vec<String> = vars.iter().map(|(n, t)| format!("{n}: {}", ty_str(t))).collect();
            out.push_str(&format!("forall {} ", bs.join(", ")));
            fmt_block(out, body, level);
            out.push('\n');
        }
        Stmt::Break(_) => out.push_str("break\n"),
        Stmt::Continue(_) => out.push_str("continue\n"),
    }
}

// Precedence levels (higher binds tighter).
const P_OR: u8 = 1;
const P_AND: u8 = 2;
const P_EQ: u8 = 3;
const P_CMP: u8 = 4;
const P_WITH: u8 = 5;
const P_RANGE: u8 = 6;
const P_ADD: u8 = 7;
const P_MUL: u8 = 8;
const P_UNARY: u8 = 9;

fn bin_prec(op: BinOp) -> u8 {
    use BinOp::*;
    match op {
        Or => P_OR,
        And => P_AND,
        Eq | Ne => P_EQ,
        Lt | Le | Gt | Ge => P_CMP,
        Add | Sub => P_ADD,
        Mul | Div | Rem => P_MUL,
    }
}

fn bin_sym(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
    }
}

pub fn expr_str(e: &Expr, min_prec: u8) -> String {
    let (s, prec) = expr_str_prec(e);
    if prec < min_prec {
        format!("({s})")
    } else {
        s
    }
}

fn expr_str_prec(e: &Expr) -> (String, u8) {
    const ATOM: u8 = 10;
    match &e.kind {
        ExprKind::Int(v) => (v.to_string(), ATOM),
        ExprKind::SizedInt(v, w) => (format!("{v}{}", w.name()), ATOM),
        ExprKind::F32(v) => (format!("{v}f32"), ATOM),
        ExprKind::Float(v) => {
            let s = if v.fract() == 0.0 && v.is_finite() {
                format!("{v:.1}")
            } else {
                v.to_string()
            };
            (s, ATOM)
        }
        ExprKind::Bool(v) => (v.to_string(), ATOM),
        ExprKind::Unit => ("()".into(), ATOM),
        ExprKind::Str(pieces) => {
            let mut s = String::from("\"");
            for p in pieces {
                match p {
                    StrPiece::Text(t) => s.push_str(&escape_str(t)),
                    StrPiece::Expr(inner) => {
                        s.push('{');
                        s.push_str(&expr_str(inner, 0));
                        s.push('}');
                    }
                }
            }
            s.push('"');
            (s, ATOM)
        }
        ExprKind::List(items) => {
            let inner: Vec<String> = items.iter().map(|i| expr_str(i, 0)).collect();
            (format!("[{}]", inner.join(", ")), ATOM)
        }
        ExprKind::Ident(n) => (n.clone(), ATOM),
        ExprKind::Call { callee, args } => {
            let c = expr_str(callee, P_UNARY + 1);
            let a: Vec<String> = args
                .iter()
                .map(|arg| match &arg.name {
                    Some(n) => format!("{n}: {}", expr_str(&arg.value, 0)),
                    None => expr_str(&arg.value, 0),
                })
                .collect();
            (format!("{c}({})", a.join(", ")), ATOM)
        }
        ExprKind::MethodCall { recv, name, args } => {
            let r = expr_str(recv, P_UNARY + 1);
            let a: Vec<String> = args.iter().map(|x| expr_str(x, 0)).collect();
            (format!("{r}.{name}({})", a.join(", ")), ATOM)
        }
        ExprKind::Field { recv, name } => {
            let r = expr_str(recv, P_UNARY + 1);
            (format!("{r}.{name}"), ATOM)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let p = bin_prec(*op);
            let l = expr_str(lhs, p);
            let r = expr_str(rhs, p + 1);
            (format!("{l} {} {r}", bin_sym(*op)), p)
        }
        ExprKind::Unary { op, operand } => {
            let sym = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
            };
            (format!("{sym}{}", expr_str(operand, P_UNARY)), P_UNARY)
        }
        ExprKind::If { cond, then_block, else_block } => {
            let mut s = format!("if {} ", expr_str(cond, 0));
            let mut blk = String::new();
            fmt_block(&mut blk, then_block, 0);
            s.push_str(&reindent_inline(&blk));
            if let Some(e) = else_block {
                s.push_str(" else ");
                match &e.kind {
                    ExprKind::BlockExpr(b) => {
                        let mut eb = String::new();
                        fmt_block(&mut eb, b, 0);
                        s.push_str(&reindent_inline(&eb));
                    }
                    _ => s.push_str(&expr_str(e, 0)),
                }
            }
            (s, ATOM)
        }
        ExprKind::BlockExpr(b) => {
            let mut s = String::new();
            fmt_block(&mut s, b, 0);
            (reindent_inline(&s), ATOM)
        }
        ExprKind::Match { scrutinee, arms } => {
            let mut s = format!("match {} {{ ", expr_str(scrutinee, 0));
            for (i, arm) in arms.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                let guard = match &arm.guard {
                    Some(g) => format!(" if {}", expr_str(g, 0)),
                    None => String::new(),
                };
                s.push_str(&format!("{}{} => {}", pattern_str(&arm.pattern), guard, expr_str(&arm.body, 0)));
            }
            s.push_str(" }");
            (s, ATOM)
        }
        ExprKind::Lambda { params, body } => {
            let ps: Vec<String> = params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => format!("{}: {}", p.name, ty_str(t)),
                    None => p.name.clone(),
                })
                .collect();
            let mut blk = String::new();
            fmt_block(&mut blk, body, 0);
            let head = if ps.is_empty() {
                "fn ".to_string()
            } else {
                format!("fn {} ", ps.join(", "))
            };
            (format!("{head}{}", reindent_inline(&blk)), ATOM)
        }
        ExprKind::Range { lo, hi, inclusive } => {
            let l = expr_str(lo, P_RANGE + 1);
            let h = expr_str(hi, P_RANGE + 1);
            (format!("{l}{}{h}", if *inclusive { "..=" } else { ".." }), P_RANGE)
        }
        ExprKind::With { recv, updates } => {
            let r = expr_str(recv, P_WITH + 1);
            let us: Vec<String> = updates
                .iter()
                .map(|(f, v)| format!("{f}: {}", expr_str(v, P_RANGE)))
                .collect();
            (format!("{r} with {}", us.join(", ")), P_WITH)
        }
        ExprKind::Try(inner) => (format!("{}?", expr_str(inner, P_UNARY + 1)), ATOM),
        ExprKind::Await(inner) => (format!("await {}", expr_str(inner, P_UNARY)), P_UNARY),
        ExprKind::Par(branches) => {
            let inner: Vec<String> = branches.iter().map(|b| expr_str(b, 0)).collect();
            (format!("par {{ {} }}", inner.join(", ")), ATOM)
        }
    }
}

/// Collapse a formatted block onto one line when short: `{ a, b }`.
fn reindent_inline(block: &str) -> String {
    let inner: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "{" && *l != "}")
        .collect();
    if inner.is_empty() {
        return "{ }".into();
    }
    format!("{{ {} }}", inner.join("; "))
}

fn pattern_str(p: &Pattern) -> String {
    match &p.kind {
        PatternKind::Wildcard => "_".into(),
        PatternKind::Bind(n) => n.clone(),
        PatternKind::Int(v) => v.to_string(),
        PatternKind::Bool(v) => v.to_string(),
        PatternKind::Str(s) => format!("\"{}\"", escape_str(s)),
        PatternKind::Ctor { name, args } => {
            if args.is_empty() {
                name.clone()
            } else {
                let a: Vec<String> = args.iter().map(pattern_str).collect();
                format!("{name}({})", a.join(", "))
            }
        }
        PatternKind::Or(alts) => {
            let a: Vec<String> = alts.iter().map(pattern_str).collect();
            a.join(" | ")
        }
    }
}

pub fn ty_str(t: &TyExpr) -> String {
    match &t.kind {
        TyExprKind::Name(n) => n.clone(),
        TyExprKind::Generic(n, args) => {
            let a: Vec<String> = args.iter().map(ty_str).collect();
            format!("{n}[{}]", a.join(", "))
        }
        TyExprKind::Fun(params, ret) => {
            let p: Vec<String> = params.iter().map(ty_str).collect();
            format!("fn({}) -> {}", p.join(", "), ty_str(ret))
        }
    }
}

fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::parser;

    fn roundtrip(src: &str) {
        let (p1, d1) = parser::parse(src);
        assert!(d1.is_empty(), "input diags: {d1:?}");
        let f1 = super::format_program(&p1);
        let (p2, d2) = parser::parse(&f1);
        assert!(d2.is_empty(), "formatted output failed to reparse: {d2:?}\n---\n{f1}");
        let f2 = super::format_program(&p2);
        assert_eq!(f1, f2, "formatter is not idempotent");
    }

    #[test]
    fn fmt_idempotent_fun() {
        roundtrip("fun add(a:Int,b:Int)->Int{a+b}\n");
    }

    #[test]
    fn fmt_idempotent_component() {
        roundtrip(
            "component C {\n out value: Int\n in click: Event\n intent \"x\"\n state n: Int = 0\n on click { n += 1\n emit value(n) }\n example { send click\n expect value == 1 }\n}\n",
        );
    }

    #[test]
    fn fmt_idempotent_exprs() {
        roundtrip("fun f(x: Int) -> Int {\n    let y = (x + 1) * 2\n    match y { 0 => 1, n => n * 2 }\n}\n");
    }

    #[test]
    fn fmt_idempotent_contract() {
        roundtrip(
            "contract Store {\n intent \"keyed storage\"\n expose fun get(k: Str) -> Option[Str]\n law \"missing is None\" { expect get(\"x\") == None }\n}\ncomponent M fulfills Store {\n intent \"in-memory\"\n expose fun get(k: Str) -> Option[Str] { None }\n}\n",
        );
    }

    #[test]
    fn fmt_idempotent_par() {
        roundtrip("fun f(n: Int) -> Int {\n    n\n}\nfun g() -> List[Int] {\n    par { f(1)  f(2)  f(3) }\n}\n");
    }

    #[test]
    fn fmt_idempotent_timers() {
        let src = "component T {\n out tick: Int\n intent \"t\"\n state n: Int = 0\n on every 5s { n += 1\n emit tick(n) }\n on after 100ms { emit tick(0) }\n example { advance 5s\n expect tick == 1 }\n}\n";
        roundtrip(src);
        // guard against silent handler loss (idempotence alone can't catch it)
        let (p, _) = parser::parse(src);
        let f = super::format_program(&p);
        assert!(f.contains("on every 5s"), "{f}");
        assert!(f.contains("on after 100ms"), "{f}");
        assert!(f.contains("advance 5s"), "{f}");
    }

    #[test]
    fn fmt_idempotent_forall_and_toplevel_law() {
        roundtrip(
            "fun id(xs: List[Int]) -> List[Int] {\n    xs\n}\nlaw \"reverse\" {\n    forall xs: List[Int], n: Int {\n        expect id(xs) == xs\n    }\n}\n",
        );
    }

    #[test]
    fn fmt_idempotent_ai_fun_interpolated_intent() {
        // the intent interpolation braces must round-trip, not get escaped
        roundtrip(
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nai fun reply(msg: Str) -> Str tools [add] {\n    intent \"Reply to {msg} using add.\"\n    model \"claude-opus-4-8\"\n}\n",
        );
    }
}
