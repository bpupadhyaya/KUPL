//! The KUPL REPL: define functions/types/components live, evaluate expressions.

use std::io::{self, BufRead, Write};

use crate::interp::{Flow, Interp, ProgramDb};
use crate::parser;
use crate::run;
use crate::value::Value;

const BANNER: &str = "KUPL v0.1 — K Universal Programming Language
Type declarations (fun/type/component/app), statements, or expressions.
Commands: :help  :defs  :upgrade <Component>  :quit";

pub fn repl() -> i32 {
    println!("{BANNER}");
    let stdin = io::stdin();
    // Each entry is (this ITEM's own (kind, name) key, its OWN source text).
    // Kept as separate units rather than one flat string so a re-typed
    // `fun`/`type`/`component`/`contract` can REPLACE its prior declaration
    // instead of appending a same-named duplicate (production-hardening
    // PR-it703): before this, only components could be "redefined" in the
    // REPL, and only because `check.rs` had no duplicate-component-name
    // check at all (a real bug, now fixed with K0278) -- redefining a
    // `fun`/`type`/`contract` already correctly errored (K0203/K0201/K0260)
    // on the accidental last-write-wins concatenation this REPL used to do.
    // Replacing by name makes redefinition an intentional, consistent
    // operation for every item kind, rather than a side effect of one item
    // kind's checker gap. `key` is `None` for a `law` (matching this same
    // exemption below: duplicate top-level law names are legitimately
    // allowed, so a re-typed law always ADDS rather than REPLACES).
    //
    // A REAL, live-confirmed silent-STATE-corruption bug found+fixed
    // (production-hardening PR-it992, an Explore survey finding): this used
    // to track keys per-SUBMISSION (one entry held EVERY key a single REPL
    // input declared, sharing ONE text blob) rather than per-ITEM. `;`
    // lexes to the SAME `Newline` statement-terminator token the parser
    // uses (`lexer.rs:788`), so `type A = X(v: Int); type B = Y(v: Int)` on
    // ONE line is legal KUPL and produced ONE entry with `keys = [(type,A),
    // (type,B)]`. Later redefining ONLY `A` computed `new_keys = [(type,A)]`,
    // and the retain-filter `!keys.iter().any(|k| new_keys.contains(k))`
    // dropped the WHOLE original entry -- including `type B`, which was
    // NEVER touched -- because it merely shared ONE key with the new
    // submission. The recompile then succeeded trivially (the remaining
    // source doesn't need `B`), so `"defined."` printed with ZERO error,
    // and `type B`/its constructor `Y` silently vanished from the session:
    // `:defs` stopped listing them, and a later `Y(...)` panicked `unknown
    // name`. Live-confirmed via a real `kupl repl` subprocess BEFORE this
    // fix. Fixed by tracking ONE key per entry (splitting a multi-item
    // submission into one entry per item, each sliced from its OWN span
    // via `sdiff::item_span` through the NEXT item's span start -- or the
    // end of input for the last item -- so any separator/whitespace
    // between items naturally stays attached to the PRECEDING item's own
    // text, needing no new parsing/formatting logic).
    let mut defs_items: Vec<(Option<(&'static str, String)>, String)> = Vec::new();
    let mut interp = Interp::new(ProgramDb::build(&Default::default(), &Default::default()));

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "kupl> " } else { "  ..> " };
        print!("{prompt}");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                return 0; // EOF
            }
            Ok(_) => {}
            Err(_) => return 1,
        }

        if buffer.is_empty() {
            let cmd = line.trim();
            match cmd {
                ":quit" | ":q" | ":exit" => return 0,
                ":help" | ":h" => {
                    println!("{BANNER}");
                    continue;
                }
                ":defs" => {
                    if defs_items.is_empty() {
                        println!("(no definitions yet)");
                    } else {
                        for (_, text) in &defs_items {
                            print!("{text}");
                        }
                    }
                    continue;
                }
                // `:upgrade <Component>` (it111): hot-swap state migration
                // for existing LIVE instances -- the design-open Q4 gap
                // (`docs/design/LANGUAGE.md` §12, Erlang's `code_change`
                // equivalent). Without this, redefining a component leaves
                // every ALREADY-spawned instance permanently frozen to its
                // OLD shape (`Instance.comp` is a snapshot `Rc`, confirmed
                // by this file's own `defs_items`-driven recompile never
                // touching `interp.instances`) -- correct and safe as a
                // DEFAULT (an already-tested, deliberate design this command
                // does not change), but it means a visual-live-editing tool
                // can never bring a running canvas's existing nodes forward
                // to a just-edited definition without restarting the whole
                // session. See `upgrade_instances`'s own doc comment for the
                // exact migration semantics and its deliberate v1 scope
                // limits.
                other if other == ":upgrade" || other.starts_with(":upgrade ") => {
                    let name = other.strip_prefix(":upgrade").unwrap().trim();
                    if name.is_empty() {
                        println!("usage: :upgrade <ComponentName>");
                        continue;
                    }
                    match upgrade_instances(&mut interp, name) {
                        Ok(0) => println!("no live instances of `{name}` to upgrade"),
                        Ok(n) => println!("upgraded {n} instance(s) of `{name}` to its current definition"),
                        Err(e) => println!("cannot upgrade `{name}`: {e}"),
                    }
                    continue;
                }
                "" => continue,
                // A `:`-prefixed line is a REPL command, not KUPL source — an
                // unknown one gets a helpful message instead of a cryptic
                // "expected an expression, found `:`" parse error.
                other if other.starts_with(':') => {
                    println!("unknown command `{other}` — type :help for the list");
                    continue;
                }
                _ => {}
            }
        }

        buffer.push_str(&line);
        if !braces_balanced(&buffer) {
            continue; // keep reading a multi-line form
        }
        let input = std::mem::take(&mut buffer);
        let trimmed = input.trim();

        // A REAL bug found+fixed (production-hardening PR-it1150, a fresh
        // Explore survey's finding): a submission consisting ONLY of a `//`
        // or `/* .. */` comment (e.g. a doc-comment line typed on its own,
        // immediately before pasting the function it documents) is
        // completely valid, inert KUPL syntax everywhere else -- a whole
        // source file containing nothing but a comment passes `kupl check`
        // cleanly -- but here it fell through both the item-declaration path
        // (`is_item` looks at `split_whitespace().next()`, which for
        // `"// note"` yields the literal token `"//"`, matching no keyword)
        // and straight into `parser::parse_stmt_fragment`, which correctly
        // reports "expected an expression, found end of file" for its own
        // (comment-stripped, now-empty) input -- the right diagnosis for
        // THAT function given empty input, but the wrong path to have
        // reached from a comment. Confirmed live before this fix:
        // `printf '// a helper\n:quit\n' | kupl repl` printed a spurious
        // `error[K0110]: expected an expression, found end of file` instead
        // of silently accepting the comment, exactly like a blank line
        // already does two branches above. Fixed by treating a submission
        // that is comment-and-whitespace-only as the SAME no-op a blank
        // line already is, checked here (after `braces_balanced` has
        // already confirmed any block comment is fully closed, not
        // mid-continuation) rather than in the `buffer.is_empty()` dispatch
        // above, since a comment can itself span multiple lines.
        if is_comment_and_whitespace_only(trimmed) {
            continue;
        }

        if is_item(trimmed) {
            // This input's own top-level items, parsed in isolation --
            // purely syntactic, so it doesn't need the rest of `defs_items`
            // to resolve. Each item becomes its OWN (key, text) entry --
            // see the `defs_items` doc comment above (PR-it992) for why
            // per-ITEM tracking, not per-SUBMISSION, is required. A key
            // collision with an OLD entry drops JUST that old entry, so a
            // re-typed declaration REPLACES rather than duplicates it,
            // without disturbing any UNRELATED item that merely happened to
            // share this input's original submission. If parsing `input`
            // alone fails (no items at all), fall back to one unkeyed entry
            // for the WHOLE input, exactly as before this fix -- it can't
            // collide with (or displace) anything, but still participates
            // in `candidate` so `run::compile` below reports the real error.
            let parsed_items = parser::parse(&input).0.items;
            let new_entries: Vec<(Option<(&'static str, String)>, String)> = if parsed_items.is_empty() {
                vec![(None, format!("{input}\n"))]
            } else {
                parsed_items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let key = if matches!(it, crate::ast::Item::Law(_)) {
                            None
                        } else {
                            Some((crate::sdiff::kind_tag(it), crate::sdiff::item_name(it).to_string()))
                        };
                        let start = crate::sdiff::item_span(it).start as usize;
                        let end = parsed_items
                            .get(i + 1)
                            .map(|next| crate::sdiff::item_span(next).start as usize)
                            .unwrap_or(input.len());
                        let mut text = input[start..end].to_string();
                        if !text.ends_with('\n') {
                            text.push('\n');
                        }
                        (key, text)
                    })
                    .collect()
            };
            let new_keys: Vec<&(&'static str, String)> =
                new_entries.iter().filter_map(|(k, _)| k.as_ref()).collect();
            let mut candidate = String::new();
            for (key, text) in &defs_items {
                if key.as_ref().is_none_or(|k| !new_keys.contains(&k)) {
                    candidate.push_str(text);
                }
            }
            for (_, text) in &new_entries {
                candidate.push_str(text);
            }
            // Try committing the new definition against everything defined so far.
            match run::compile(&candidate) {
                Ok(compiled) => {
                    run::print_diags(&compiled.warnings, &candidate, "<repl>");
                    defs_items.retain(|(key, _)| key.as_ref().is_none_or(|k| !new_keys.contains(&k)));
                    defs_items.extend(new_entries);
                    let db = ProgramDb::build(&compiled.program, &compiled.checked);
                    // Keep live values/instances; swap in the new definitions.
                    let old = std::mem::replace(&mut interp, Interp::new(db));
                    interp.instances = old.instances;
                    interp.globals = old.globals;
                    println!("defined.");
                }
                Err(errors) => {
                    run::print_diags(&errors, &candidate, "<repl>");
                }
            }
            continue;
        }

        // Statement / expression: evaluated dynamically against the live session.
        match parser::parse_stmt_fragment(trimmed) {
            Err(d) => {
                eprintln!("error[{}]: {}", d.code, d.message);
            }
            Ok(mut stmt) => {
                // A REAL, live-confirmed silent-WRONG-VALUE bug found+fixed
                // (production-hardening PR-it1181): unlike the item-declaration
                // path above (which routes through `run::compile`, calling
                // `callargs::resolve_call_args` like an ordinary program), a
                // bare statement/expression used to go straight from parsing to
                // execution -- named-argument calls were silently reinterpreted
                // POSITIONALLY, and a trailing default parameter panicked
                // instead of being applied. See `resolve_call_args_in_stmt`'s
                // own doc comment for the full writeup and live repro.
                let call_diags = crate::callargs::resolve_call_args_in_stmt(
                    interp.db.funs.values().map(|f| f.as_ref()),
                    &mut stmt,
                );
                if !call_diags.is_empty() {
                    for d in &call_diags {
                        eprintln!("error[{}]: {}", d.code, d.message);
                    }
                    continue;
                }
                let env = interp.globals.clone();
                match interp.exec_stmt_public(&stmt, &env) {
                    Ok(Value::Unit) => {}
                    Ok(v) => println!("{v}"),
                    Err(Flow::Panic { msg, .. }) => eprintln!("panic: {msg}"),
                    Err(Flow::Return(v)) => println!("{v}"),
                    Err(_) => eprintln!("error: `break`/`continue` outside of a loop"),
                }
            }
        }
    }
}

/// `:upgrade <name>` (it111) — hot-swap every LIVE instance of the
/// component currently named `name` to `interp.db`'s CURRENT (just
/// redefined) declaration, migrating `state` field values by NAME:
/// a field present in both the old and new declaration keeps its
/// CURRENT runtime value; a field only in the new declaration is
/// initialized fresh via its own `init` expression (evaluated in the
/// instance's OWN migrated env, so a later field's init can reference an
/// earlier one — the same left-to-right evaluation order `instantiate`
/// itself already uses for a brand-new instance). A field only in the OLD
/// declaration is simply dropped.
///
/// Deliberately narrow v1 scope, matching Erlang's own `code_change`
/// (which migrates a process's STATE term, not its supervision tree):
/// `children` must be STRUCTURALLY UNCHANGED (by `name:component` pair) --
/// refuses entirely (no instances touched) otherwise, since a changed
/// `children`/`wires` list would need re-spawning/re-routing, a materially
/// different and larger problem than a `state`/`prop` value copy, still out
/// of scope for this iteration.
///
/// `props` may change (it112->it113 follow-up, relaxing the original
/// props-frozen guard): a prop present under the SAME name in both old and
/// new keeps its current runtime value (a prop's own TYPE or default value
/// changing is not checked, since an existing instance's already-supplied
/// value keeps working under its own actual runtime type regardless); a
/// prop only in the OLD declaration is simply dropped; a prop only in the
/// NEW declaration is migrated by evaluating its own `default` expression
/// (against the instance's own migrated env, so it can see already-migrated
/// props) -- mirroring exactly how a genuinely new `state` field already
/// gets its own fresh `init` below. A newly required prop with **no**
/// default still has nothing to migrate FROM, so that case alone still
/// refuses the whole upgrade (no instances touched, not a partial one).
/// Matching is by OLD-PROP-NAME-SET membership, not a blind `old_env.get`
/// lookup -- an instance's env holds props AND state together by name, so a
/// prop renamed from a same-named OLD state field must still be treated as
/// genuinely new (evaluate its own default) rather than accidentally
/// inheriting the old state field's stale value.
///
/// Handler/method bodies always take effect immediately (methods are looked
/// up via `Instance.comp` at CALL time, never cached per-instance), so
/// swapping `instance.comp` alone is what makes logic-only redefinitions
/// (the common case) instantly live — this function's own job is entirely
/// about the STATE (and now PROPS) the new logic runs against.
///
/// Returns the number of instances upgraded (`Ok(0)` if none exist yet —
/// not an error, since redefining a component with no live instances is
/// the ordinary case `:upgrade` is a no-op safety net for).
fn upgrade_instances(interp: &mut crate::interp::Interp, name: &str) -> Result<usize, String> {
    let Some(new_comp) = interp.db.components.get(name).cloned() else {
        return Err(format!("no component named `{name}`"));
    };
    let target_ids: Vec<usize> =
        (0..interp.instances.len()).filter(|&i| interp.instances[i].comp.name == name).collect();
    if target_ids.is_empty() {
        return Ok(0);
    }
    // Every targeted instance currently shares the SAME (old) `Rc<ComponentDecl>`
    // (they were all spawned from the same prior `:name` definition) — reading
    // the first one's own `comp` is representative for the structural guard below.
    let old_comp = interp.instances[target_ids[0]].comp.clone();

    let prop_names = |c: &crate::ast::ComponentDecl| -> std::collections::BTreeSet<String> {
        c.props.iter().map(|p| p.name.clone()).collect()
    };
    let old_prop_names = prop_names(&old_comp);
    for p in &new_comp.props {
        if !old_prop_names.contains(&p.name) && p.default.is_none() {
            return Err(format!(
                "prop `{}` was added without a default value — :upgrade has no old value to migrate it from (see its own doc comment)",
                p.name
            ));
        }
    }
    let child_sig = |c: &crate::ast::ComponentDecl| -> std::collections::BTreeSet<String> {
        c.children.iter().map(|ch| format!("{}:{}", ch.name, ch.component)).collect()
    };
    if child_sig(&old_comp) != child_sig(&new_comp) {
        return Err("children changed — :upgrade only migrates `state` field changes for now (see its own doc comment)".into());
    }

    for id in &target_ids {
        let old_env = interp.instances[*id].env.clone();
        let new_env = interp.globals.child();
        // props: a name present in OLD-PROP-NAMES keeps its current value
        // (guaranteed present in `old_env`'s own LOCAL scope — it was
        // `define`d there at construction, never shadowed); a genuinely NEW
        // prop name (checked above to carry a default) is migrated by
        // evaluating that default fresh, exactly like a new `state` field
        // below. Checked via the NAME-SET, not a blind `old_env.get`, so a
        // prop renamed from a same-named old STATE field can't accidentally
        // inherit that field's stale value (see this function's own doc
        // comment).
        for p in &new_comp.props {
            let v = if old_prop_names.contains(&p.name) {
                old_env.get(&p.name).ok_or_else(|| format!("internal error: prop `{}` unexpectedly missing", p.name))?
            } else {
                let default_expr = p.default.as_ref().expect("checked above: a new prop must carry a default");
                interp.eval(default_expr, &new_env).map_err(|_| {
                    format!("evaluating the default for new prop `{}` failed", p.name)
                })?
            };
            new_env.define(&p.name, v);
        }
        // state: migrate by name, evaluating a genuinely NEW field's own
        // init fresh (against `new_env`, so it can see already-migrated
        // props/earlier state fields, exactly like `instantiate` does).
        for s in &new_comp.state {
            let was_present = old_comp.state.iter().any(|os| os.name == s.name);
            let v = if was_present {
                old_env
                    .get(&s.name)
                    .ok_or_else(|| format!("internal error: state field `{}` unexpectedly missing", s.name))?
            } else {
                interp.eval(&s.init, &new_env).map_err(|_| {
                    format!("evaluating the new default for state field `{}` failed", s.name)
                })?
            };
            new_env.define(&s.name, v);
        }
        interp.instances[*id].env = new_env;
        interp.instances[*id].comp = new_comp.clone();
    }
    Ok(target_ids.len())
}

fn is_item(src: &str) -> bool {
    // A REAL bug found+fixed (production-hardening PR-it854, the THIRTY-THIRD
    // survey): a top-level `law "..." { ... }` block is legitimate KUPL syntax
    // (`ast::Item::Law`, used standalone in examples/properties.kupl and
    // several others) but `"law"` was missing from this match arm, so typing
    // one at the REPL prompt got misrouted into `parser::parse_stmt_fragment`
    // (the statement/expression path just below), which can't parse it --
    // producing a generic, misleading `K0102` "expected end of statement"
    // error instead of `"defined."`, and the law was silently never captured
    // (`:defs` stayed empty). The item-definition branch above already
    // contained a `.filter(|it| !matches!(it, ast::Item::Law(_)))` guard --
    // dead code until this fix, since a Law never reached that branch at
    // all -- strong evidence this was an oversight (someone wrote handling
    // for a Law reaching that path, then forgot to add "law" to the gate
    // that lets it get there), not deliberate scoping. That filter itself is
    // CORRECT as written and needs no change: duplicate top-level law names
    // are legitimately allowed by the compiler (confirmed live -- two
    // identically-named top-level laws both run independently under
    // `kupl test`, no "duplicate definition" error), unlike fun/type/
    // component, so a re-typed law should ADD another law rather than
    // REPLACE the prior same-named one the way the dedup-by-name logic does
    // for those.
    let mut words = src.split_whitespace();
    let first = words.next().unwrap_or("");
    if matches!(
        first,
        "fun" | "type" | "component" | "app" | "pub" | "async" | "contract" | "use" | "module"
    ) {
        return true;
    }
    // A REAL bug found+fixed (production-hardening PR-it1063, a background
    // close-read survey finding): `law` is a SOFT keyword too, exactly like
    // `ai` immediately below -- it's lexed as a plain `Tok::Ident("law")`
    // (`parser.rs:378`), not a hard lexer keyword, so matching it
    // unconditionally on the first word alone (as this branch used to,
    // before PR-it1063) wrongly misroutes an ORDINARY statement/expression
    // that happens to start with a variable literally named `law` (a bare
    // `law`, `law + 1`, `law.foo()`) into the item-declaration path, which
    // then fails with a confusing `K0115: 'law' expects a name string`
    // parse error instead of evaluating the expression -- live-confirmed
    // BEFORE this fix via `let law = 42` followed by a bare `law` at the
    // REPL prompt. `parser::parse_item` (parser.rs:378-393) requires the
    // token immediately after `law` to be a STRING LITERAL (the law's own
    // name) before treating it as a law declaration, so mirror that here:
    // peek the second word and require it to look like a string literal
    // (starts with `"`) before claiming the line as an item.
    if first == "law" {
        return words.next().is_some_and(|w| w.starts_with('"'));
    }
    // A REAL bug found+fixed (production-hardening PR-it935): `ai` is ALSO a
    // soft keyword, mirroring `law` above, but with a narrower shape --
    // `parser::parse_item` only special-cases it directly before `fun`
    // (`ai fun name(...) { intent "..." }`, `ast::Item::Fun` via
    // `parse_ai_fun`), so unlike `law`'s unconditional single-token match,
    // this must ALSO peek the second token before claiming the line as an
    // item -- otherwise an ordinary statement/expression that happens to
    // start with a variable literally named `ai` (e.g. `ai + 1`, a bare
    // `ai`, or `ai.summarize()`) would be wrongly misrouted here instead of
    // to the statement/expression path below. Pre-fix, a bare `ai fun ...`
    // typed at the REPL prompt (no `pub` prefix -- `pub ai fun ...` was
    // already safe, since `is_pub` is consumed by the parser BEFORE this
    // check, routing it through the existing `"pub"` arm above) fell through
    // to `parser::parse_stmt_fragment`, which can't parse it, producing a
    // misleading `K0102: expected end of statement, found 'fun'` and
    // silently losing the declaration (`:defs` stayed empty) -- live-
    // confirmed.
    first == "ai" && words.next() == Some("fun")
}

/// A REAL bug found+fixed (production-hardening PR-it768): this used to be
/// completely unaware of `//` line comments and `/* */` block comments --
/// unlike the real lexer (`lexer.rs:90-123`), which supports both (block
/// comments even NESTABLE, mirrored below). Any bracket-class character
/// typed inside what the user intends as a comment (e.g. a `:(` sad-face
/// emoticon in `// ugh this crashed :(`) was counted as genuine unclosed
/// syntax, permanently WEDGING the REPL: `buffer` never balances again, so
/// every subsequent line -- including a bare `:quit` -- gets silently
/// APPENDED to the same dead buffer instead of executing/being recognized
/// as a command (the `:`-command dispatch above only fires when `buffer.
/// is_empty()`), and on EOF the entire unsubmitted buffer is silently
/// discarded with zero diagnostic that anything was lost. Live-confirmed
/// BEFORE this fix via a piped `kupl repl` session: `// ugh this crashed
/// :(` followed by `print("hi")` followed by `:quit` never printed `hi`,
/// never processed `:quit`, and exited cleanly on EOF as if nothing went
/// wrong.
fn braces_balanced(src: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    // Tracked ACROSS the whole scan (not just within one `/* .. */` span) so
    // a block comment left open at the end of the buffer -- e.g. the user
    // just typed `/* start of a` on its own line, intending to close it on a
    // LATER line -- correctly signals "keep reading" (a `..>` continuation),
    // matching how an open `{`/`(`/`[` already does, rather than prematurely
    // submitting a truncated comment as if it were a complete top-level form.
    let mut comment_depth: u32 = 0;
    let mut chars = src.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_str {
            // A REAL bug found+fixed (production-hardening PR-it779, a
            // long-abandoned survey's finding, agentId aaed1d00a40c9e7b6,
            // dispatched at it764, delivered 14 iterations late; independently
            // re-verified live before implementing since this SAME survey's
            // own top finding just turned out to be stale): the OLD escape
            // check, `ch == '"' && prev != '\\'`, only looked at the SINGLE
            // immediately-preceding character -- for a string ending in an
            // escaped backslash, e.g. `"\\"` (source chars: `"`, `\`, `\`,
            // `"` -- ONE escaped backslash, which `lexer.rs` correctly closes),
            // the closing `"` is itself preceded by a `\` (the SECOND half of
            // the `\\` pair), so the old check wrongly treated the close as
            // escaped and never left `in_str` -- permanently wedging the REPL
            // (every subsequent line, including `:quit`, got silently
            // appended to the same never-balanced buffer, since `:`-command
            // dispatch only fires when the buffer is empty). Confirmed live
            // before fixing: `printf 'print("\\\\")\n:quit\nprint(1)\n" |
            // kupl repl` produced FOUR stacked `..>` continuation prompts,
            // never executing `print("\\")`, never processing `:quit`.
            // Fixed by mirroring `lexer.rs::lex_string`'s OWN "consume in
            // pairs" approach exactly (`Some(b'\\') => match self.bump() {
            // ... }`, which unconditionally consumes the character AFTER a
            // backslash as part of the SAME escape unit) instead of a
            // trailing-parity lookback: a `\` while inside a string
            // immediately consumes the NEXT character too, so THAT character
            // (whatever it is -- a quote, another backslash, anything) can
            // never be misread as closing the string on this same pass. This
            // removes the need for `prev` entirely (its only reader was this
            // exact check), so it's dropped rather than left as dead state.
            match ch {
                '\\' => {
                    chars.next();
                }
                '"' => in_str = false,
                // A REAL bug found+fixed (production-hardening PR-it870): a
                // single `{` inside a string opens INTERPOLATION
                // (`lexer.rs::lex_string`), which can itself contain a
                // NESTED string literal (e.g. `"{f("(")}"`, or
                // `"{xs.join(", ")}"`, lexer.rs's own documented example) --
                // the real lexer skips such a nested string's quotes/braces
                // WHOLE, so they never affect the outer string's own
                // boundary. This scan's naive single `in_str` toggle had no
                // such awareness: a `"` inside an interpolation expression
                // was misread as the OUTER string's own closing quote,
                // desyncing this function from the real lexer -- any
                // bracket character that followed (now wrongly outside
                // `in_str`) got counted toward `depth`, permanently
                // unbalancing it and WEDGING the REPL exactly like it768/
                // it779 (every subsequent line, including `:quit`, silently
                // appended to the same dead buffer). Confirmed live before
                // this fix via a piped `kupl repl` session: `"{f("(")}"`
                // followed by `print("done-marker")` followed by `:quit`
                // produced four stacked `..>` continuation prompts, never
                // printed `done-marker`, never processed `:quit`. Fixed by
                // tracking interpolation's OWN nested `{`/`}` depth and
                // skipping any nested string literal whole, mirroring
                // `lexer.rs::lex_string`'s exact algorithm (including its
                // `{{` == literal-`{` priority check, checked FIRST, so a
                // doubled brace never mistakenly opens interpolation).
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                }
                '{' => {
                    let mut interp_depth: u32 = 1;
                    while interp_depth > 0 {
                        match chars.next() {
                            None => break, // buffer ends mid-interpolation -- reported unbalanced below
                            Some('{') => interp_depth += 1,
                            Some('}') => interp_depth -= 1,
                            Some('"') => loop {
                                match chars.next() {
                                    None => break,
                                    Some('\\') => {
                                        chars.next();
                                    }
                                    Some('"') => break,
                                    _ => {}
                                }
                            },
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            // line comment: skip to end of line (or end of input).
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            // block comment: NESTABLE, matching `lexer.rs`'s own algorithm
            // exactly (a `/*` inside an already-open block comment opens
            // ANOTHER level, requiring a matching extra `*/` to close).
            chars.next(); // consume the '*'
            comment_depth += 1;
            while comment_depth > 0 {
                match chars.next() {
                    None => break, // buffer ends mid-comment -- reported unbalanced below
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        comment_depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        comment_depth -= 1;
                    }
                    _ => {}
                }
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    depth <= 0 && comment_depth == 0
}

// True if `src` contains only whitespace and `//`/`/* .. */` comments (no
// real code token) -- see the PR-it1150 doc comment at this function's own
// call site for why a comment-only REPL submission needs this check. Mirrors
// `braces_balanced`'s own comment-skipping algorithm (including nestable
// block comments) but doesn't need string-literal awareness: encountering a
// `"` (or any other non-whitespace, non-comment character) is ALREADY real
// content, so this returns `false` immediately rather than needing to track
// where the string ends.
fn is_comment_and_whitespace_only(src: &str) -> bool {
    let mut chars = src.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume the '*'
            let mut comment_depth: u32 = 1;
            while comment_depth > 0 {
                match chars.next() {
                    None => break, // unterminated; braces_balanced already gates this case
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        comment_depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        comment_depth -= 1;
                    }
                    _ => {}
                }
            }
            continue;
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{braces_balanced, is_comment_and_whitespace_only, is_item, upgrade_instances};
    use crate::interp::{Interp, ProgramDb};

    /// Compile `src`, build a fresh `Interp`, and spawn ONE instance of
    /// `comp_name` with no constructor args -- the shared setup every
    /// `upgrade_instances` test below starts from.
    fn interp_with_one_instance(src: &str, comp_name: &str) -> Interp {
        let compiled = crate::run::compile(src).expect("program compiles");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let mut interp = Interp::new(db);
        interp
            .instantiate(comp_name, &[], crate::diag::Span::default())
            .unwrap_or_else(|_| panic!("{comp_name} must construct with no args"));
        interp
    }

    /// Redefine `interp`'s component set to `new_src`'s, mirroring
    /// `repl()`'s own exact redefinition mechanism (`ProgramDb::build` +
    /// swap-in-a-fresh-`Interp`-carrying-over-`instances`/`globals`) --
    /// see that function's own `defined.` branch for the real code this
    /// copies.
    fn redefine(interp: &mut Interp, new_src: &str) {
        let compiled = crate::run::compile(new_src).expect("redefinition compiles");
        let db = ProgramDb::build(&compiled.program, &compiled.checked);
        let old = std::mem::replace(interp, Interp::new(db));
        interp.instances = old.instances;
        interp.globals = old.globals;
    }

    /// The core migration contract: a state field present in BOTH the old
    /// and new definition keeps its CURRENT (mutated) value; a field only
    /// in the new definition gets its own fresh `init` default; the
    /// instance's `comp` is swapped so new methods are immediately
    /// callable. Live-confirmed via a real `kupl repl` subprocess before
    /// writing this test (not assumed) -- this exercises the SAME
    /// `upgrade_instances` function directly, without the REPL I/O loop.
    #[test]
    fn upgrade_migrates_matching_state_by_name_and_defaults_new_fields() {
        let mut interp = interp_with_one_instance(
            "component Counter {\n    intent \"c\"\n    state n: Int = 0\n    expose fun bump(v: Int) -> Int {\n        n = n + v\n        n\n    }\n}\n",
            "Counter",
        );
        // mutate state before upgrading (directly, via a real exposed call --
        // Value::Bound is exactly how a method call resolves an instance +
        // name to a callable, see interp.rs's own `eval` ExprKind::Field
        // arm), so migration-vs-reset is unambiguous.
        let bump = crate::value::Value::Bound(0, std::rc::Rc::new("bump".to_string()));
        if interp.call_value(bump, vec![crate::value::Value::Int(5)], crate::diag::Span::default()).is_err() {
            panic!("bump(5) must succeed");
        }

        redefine(
            &mut interp,
            "component Counter {\n    intent \"c\"\n    state n: Int = 0\n    state label: Str = \"fresh\"\n    expose fun bump(v: Int) -> Int {\n        n = n + v\n        n\n    }\n    expose fun readLabel() -> Str {\n        label\n    }\n}\n",
        );
        assert_eq!(upgrade_instances(&mut interp, "Counter"), Ok(1));
        assert_eq!(interp.instances[0].env.get("n"), Some(crate::value::Value::Int(5)), "existing state must be MIGRATED, not reset");
        assert_eq!(
            interp.instances[0].env.get("label"),
            Some(crate::value::Value::str("fresh".to_string())),
            "a genuinely new field must get its own fresh default"
        );
        assert_eq!(interp.instances[0].comp.name, "Counter");
        assert!(interp.instances[0].comp.exposes.iter().any(|f| f.name == "readLabel"), "the new method must be immediately callable");
    }

    #[test]
    fn upgrade_reports_zero_for_no_live_instances_and_errors_on_an_unknown_component() {
        let compiled = crate::run::compile("component Foo {\n    intent \"f\"\n    state n: Int = 0\n}\n").unwrap();
        let mut interp = Interp::new(ProgramDb::build(&compiled.program, &compiled.checked));
        assert_eq!(upgrade_instances(&mut interp, "Foo"), Ok(0));
        assert!(upgrade_instances(&mut interp, "DoesNotExist").is_err());
    }

    /// A `children` change always refuses the upgrade entirely (no instance
    /// touched) -- still out of scope this iteration, see
    /// `upgrade_instances`'s own doc comment. A `props` change only refuses
    /// when the genuinely NEW prop carries no default (nothing to migrate
    /// it from); see the tests below for the now-supported success cases.
    #[test]
    fn upgrade_refuses_when_a_new_prop_has_no_default_or_children_changed() {
        let mut interp = interp_with_one_instance(
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    state n: Int = 0\n}\n",
            "P",
        );
        redefine(
            &mut interp,
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    prop y: Int\n    state n: Int = 0\n}\n",
        );
        let err = upgrade_instances(&mut interp, "P").expect_err("a new prop with no default must be refused");
        assert!(err.contains("added without a default"), "{err}");

        let mut interp2 = interp_with_one_instance(
            "component Leaf {\n    intent \"l\"\n}\ncomponent Parent {\n    intent \"p\"\n    let a = Leaf()\n}\n",
            "Parent",
        );
        redefine(
            &mut interp2,
            "component Leaf {\n    intent \"l\"\n}\ncomponent Parent {\n    intent \"p\"\n    let a = Leaf()\n    let b = Leaf()\n}\n",
        );
        let err2 = upgrade_instances(&mut interp2, "Parent").expect_err("a children change must be refused");
        assert!(err2.contains("children changed"), "{err2}");
    }

    /// it112->it113 follow-up: a genuinely NEW prop carrying its own
    /// `default` migrates fine (evaluated fresh, exactly like a new `state`
    /// field), and a REMOVED prop is silently dropped -- no longer a
    /// blanket refusal the moment `props` differs at all.
    #[test]
    fn upgrade_migrates_a_new_prop_with_default_and_drops_a_removed_one() {
        let mut interp = interp_with_one_instance(
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    prop z: Int = 2\n    state n: Int = 0\n}\n",
            "P",
        );
        redefine(
            &mut interp,
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    prop y: Int = 42\n    state n: Int = 0\n}\n",
        );
        assert_eq!(upgrade_instances(&mut interp, "P"), Ok(1));
        assert_eq!(interp.instances[0].env.get("x"), Some(crate::value::Value::Int(1)), "an unchanged prop keeps its value");
        assert_eq!(
            interp.instances[0].env.get("y"),
            Some(crate::value::Value::Int(42)),
            "a genuinely new prop must get its own fresh default"
        );
        assert_eq!(interp.instances[0].env.get("z"), None, "a removed prop must be dropped, not left dangling");
    }

    /// The specific correctness edge case this follow-up must NOT get
    /// wrong: a prop renamed from a same-named OLD *state* field must
    /// evaluate its own new default, never accidentally inherit the old
    /// state field's stale value via a blind by-name env lookup.
    #[test]
    fn upgrade_does_not_confuse_a_new_prop_with_a_same_named_old_state_field() {
        let mut interp = interp_with_one_instance(
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    state y: Str = \"old-state-value\"\n}\n",
            "P",
        );
        redefine(
            &mut interp,
            "component P {\n    intent \"p\"\n    prop x: Int = 1\n    prop y: Str = \"new-prop-default\"\n}\n",
        );
        assert_eq!(upgrade_instances(&mut interp, "P"), Ok(1));
        assert_eq!(
            interp.instances[0].env.get("y"),
            Some(crate::value::Value::str("new-prop-default".to_string())),
            "a prop named like an old STATE field must get its own default, not the stale state value"
        );
    }

    #[test]
    fn braces_balanced_drives_multiline_reads() {
        // balanced forms are ready to evaluate
        assert!(braces_balanced("fun f() -> Int { 1 }"));
        assert!(braces_balanced("2 + 3"));
        assert!(braces_balanced("[1, 2, 3].sum()"));
        // an unclosed brace/paren keeps the REPL reading (a `..>` continuation)
        assert!(!braces_balanced("fun f() -> Int {"));
        assert!(!braces_balanced("foo("));
        // a COMPLETE `{x}` interpolation (a matching `}` before the string's
        // own closing quote) is genuinely valid, complete syntax -- `kupl
        // check`/`kupl run` both accept it and `x` is evaluated as a real
        // expression, so it must NOT keep the REPL waiting for more input.
        assert!(braces_balanced("print(\"val {x}\")"));
        // A REAL, PRE-EXISTING bug in this test itself, corrected as part of
        // production-hardening PR-it870: this used to assert `print("a { b")`
        // (a `{` with NO matching `}` before the string's own closing quote)
        // was "balanced" -- but `kupl check` on this EXACT source reports
        // real K0005/K0007 errors ("unterminated `{` interpolation in
        // string"), confirming it's genuinely INCOMPLETE syntax, not text a
        // user could legitimately finish typing on the same line. The
        // original comment here ("braces INSIDE a string literal... don't
        // count") was simply wrong about how `{` inside a KUPL string
        // behaves -- a single unescaped `{` ALWAYS opens interpolation
        // (`lexer.rs::lex_string`), it is never inert text.
        assert!(!braces_balanced("print(\"a { b\")"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it768): `braces_balanced`
    /// used to be completely unaware of `//` line comments and `/* */` block
    /// comments -- any bracket-class character typed inside a comment was
    /// counted as genuine unclosed syntax, permanently wedging the REPL. Live-
    /// confirmed BEFORE this fix via a real piped `kupl repl` session (see the
    /// subprocess test below for the full end-to-end repro).
    #[test]
    fn braces_balanced_ignores_brackets_inside_comments() {
        // a line comment containing bracket-class characters must not be
        // mistaken for unclosed syntax.
        assert!(braces_balanced("// look at this { unmatched"));
        assert!(braces_balanced("// ugh this crashed :("));
        assert!(braces_balanced("print(1) // trailing { comment"));
        // a block comment, including one spanning what LOOKS like a
        // multi-line unclosed form, is still recognized as fully consumed.
        assert!(braces_balanced("/* { ( [ all unmatched */"));
        assert!(braces_balanced("fun f() -> Int { /* comment { */ 1 }"));
        // NESTED block comments, mirroring `lexer.rs`'s own nestable algorithm.
        assert!(braces_balanced("/* outer /* inner { */ still outer */"));
        // a genuinely UNCLOSED block comment (no closing `*/` at all) must
        // still correctly signal "keep reading" -- otherwise a multi-line
        // comment split across several `read_line` calls (e.g. `/* start`
        // on one line, `continues */` on the next) would be prematurely
        // submitted after just the FIRST line, treating the comment's own
        // closing line as an unrelated new top-level statement instead.
        assert!(!braces_balanced("/* never closed { ["));
        // a REAL unclosed brace OUTSIDE any comment must still correctly
        // signal "keep reading" -- this fix must not over-correct into
        // treating everything after a `/` as inert.
        assert!(!braces_balanced("fun f() -> Int { // trailing comment on an open line"));
        assert!(!braces_balanced("foo(1, 2"));
    }

    #[test]
    fn is_comment_and_whitespace_only_detects_the_no_op_case_and_rejects_real_code() {
        // the actual PR-it1150 repro shapes: no-op.
        assert!(is_comment_and_whitespace_only("// a helper"));
        assert!(is_comment_and_whitespace_only("/* a block comment */"));
        // blank / whitespace-only is also (vacuously) comment-and-whitespace-only.
        assert!(is_comment_and_whitespace_only(""));
        assert!(is_comment_and_whitespace_only("   \n\t  "));
        // multiple comments, and comments spanning several lines, are still a no-op.
        assert!(is_comment_and_whitespace_only("// one\n// two\n"));
        assert!(is_comment_and_whitespace_only("/* start\ncontinues */\n// trailing too"));
        // NESTED block comments, mirroring `braces_balanced`'s own nestable algorithm.
        assert!(is_comment_and_whitespace_only("/* outer /* inner */ still outer */"));
        // the discriminating pair: real code, with or without an accompanying
        // comment, must NOT be treated as a no-op -- this fix must not
        // over-correct into silently swallowing genuine input.
        assert!(!is_comment_and_whitespace_only("1 + 1"));
        assert!(!is_comment_and_whitespace_only("// note\n1 + 1"));
        assert!(!is_comment_and_whitespace_only("fun f() -> Int { 1 }"));
        // a string literal that merely CONTAINS comment-like text is real
        // code, not a comment, even though it starts with `"` rather than a
        // recognizable keyword -- encountering the `"` itself is enough to
        // disqualify it, with no need to track where the string ends.
        assert!(!is_comment_and_whitespace_only("\"// not actually a comment\""));
    }

    /// A REAL bug found+fixed (production-hardening PR-it779, a long-abandoned
    /// survey's finding, agentId aaed1d00a40c9e7b6, dispatched at it764,
    /// delivered 14 iterations late; independently re-verified live before
    /// implementing since this SAME survey's own top finding just turned out
    /// to be stale): the OLD escape check, `ch == '"' && prev != '\\'`, only
    /// looked at the SINGLE immediately-preceding character -- a string
    /// ending in an escaped backslash, `"\\"` (ONE escaped backslash char,
    /// which `lexer.rs` correctly treats as closed), has its closing `"`
    /// itself preceded by a `\` (the second half of the `\\` pair), so the
    /// old check wrongly treated the close as escaped and never left
    /// `in_str` -- permanently wedging the REPL (see the subprocess test
    /// below for the full end-to-end repro).
    #[test]
    fn braces_balanced_handles_a_string_ending_in_an_escaped_backslash() {
        // one escaped backslash, correctly closed -- the exact PR-it779 repro.
        assert!(braces_balanced("print(\"\\\\\")"));
        // two escaped backslashes in a row, still correctly closed.
        assert!(braces_balanced("print(\"\\\\\\\\\")"));
        // an escaped quote followed by more text and a real close still works
        // (guards against over-correcting into "a backslash always closes").
        assert!(braces_balanced("print(\"a\\\"b\")"));
        // a GENUINELY unterminated string (odd trailing backslash with no
        // closing quote at all) must still correctly signal "keep reading".
        assert!(!braces_balanced("print(\"a\\"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it870, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// a `{` inside a string ALWAYS opens interpolation (`lexer.rs::
    /// lex_string`), which can itself contain a NESTED string literal (e.g.
    /// `"{f("(")}"`, or `"{xs.join(", ")}"`, lexer.rs's own documented
    /// example) -- the real lexer skips such a nested string's quotes/
    /// braces WHOLE. This scan's naive single `in_str` toggle had no such
    /// awareness: a `"` inside an interpolation expression was misread as
    /// the OUTER string's own closing quote, desyncing this function from
    /// the real lexer -- any bracket character that followed (now wrongly
    /// treated as outside the string) got counted toward `depth`,
    /// permanently unbalancing it. See the subprocess test below for the
    /// full end-to-end repro.
    #[test]
    fn braces_balanced_handles_a_nested_string_inside_an_interpolation_expression() {
        // a bracket char inside a NESTED string within an interpolation --
        // the EXACT PR-it870 repro.
        assert!(braces_balanced("print(\"{f(\"(\")}\")"));
        // the lexer's OWN documented example: a comma inside a nested string
        // argument to `join`, a completely ordinary, idiomatic use.
        assert!(braces_balanced("print(\"{xs.join(\", \")}\")"));
        // `{{` is a literal brace (not interpolation) -- must NOT be
        // misread as opening interpolation, which would desync this scan
        // against the REAL closing quote that follows.
        assert!(braces_balanced("print(\"a{{b}\")"));
        // a genuinely UNTERMINATED interpolation (no matching `}` at all)
        // must still correctly signal "keep reading".
        assert!(!braces_balanced("print(\"{f(\")"));
    }

    #[test]
    fn is_item_classifies_declarations_vs_expressions() {
        assert!(is_item("fun f() -> Int { 1 }"));
        assert!(is_item("type P = Pt(x: Int)"));
        assert!(is_item("pub fun g() {}"));
        assert!(is_item("component C {}"));
        // a top-level `law` block is a real item too (PR-it854): missing from
        // this match arm before, so it fell through to statement-fragment
        // parsing and produced a misleading K0102 error instead of "defined.".
        assert!(is_item("law \"ok\" { expect 1 == 1 }"));
        // `law` is a soft keyword too (PR-it1063, a background close-read
        // survey finding): like `ai` below, it must ALSO peek the second
        // token before claiming the line as an item -- an ordinary
        // statement/expression that happens to start with a variable
        // literally named `law` must still correctly route to the
        // statement/expression path, not be misrouted into item parsing
        // (which used to produce a misleading K0115 "expects a name
        // string" error instead of evaluating the expression).
        assert!(!is_item("law + 1"));
        assert!(!is_item("law"));
        assert!(!is_item("law.foo()"));
        // a bare `ai fun ...` is a real item too (PR-it935): missing from
        // this match arm before, so it fell through to statement-fragment
        // parsing and produced a misleading K0102 error instead of "defined.",
        // silently losing the declaration (`:defs` stayed empty).
        assert!(is_item("ai fun summarize(text: Str) -> Str { intent \"x\" }"));
        // `ai` is a soft keyword only directly before `fun` -- unlike `law`'s
        // unconditional single-token match, an ordinary statement/expression
        // that happens to start with a variable literally named `ai` must
        // still correctly route to the statement/expression path, not be
        // misrouted here.
        assert!(!is_item("ai + 1"));
        assert!(!is_item("ai"));
        assert!(!is_item("ai.summarize()"));
        // `pub ai fun ...` was already safe pre-fix: `is_pub` is consumed by
        // the parser BEFORE the `ai`-soft-keyword check, so it already
        // routed through the existing `"pub"` arm.
        assert!(is_item("pub ai fun summarize(text: Str) -> Str { intent \"x\" }"));
        // statements and expressions are not items (they run against current state)
        assert!(!is_item("let x = 1"));
        assert!(!is_item("2 + 3"));
        assert!(!is_item("x.to_upper()"));
    }

    /// End-to-end companion to `braces_balanced_ignores_brackets_inside_comments`
    /// above: spawns the REAL `kupl repl` process (this codebase's established
    /// subprocess-test pattern, e.g. `main.rs::wait_with_timeout`) to confirm the
    /// full wedge is fixed, not just the underlying pure function. Live-confirmed
    /// BEFORE this fix: a `// ugh this crashed :(` comment permanently wedged the
    /// session -- `print("hi")` never ran and `:quit` never processed as a
    /// command, with the process only exiting via silent EOF.
    #[test]
    fn a_bracket_character_inside_a_repl_comment_does_not_wedge_the_session() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "// ugh this crashed :(\nprint(\"hi\")\n:quit\nprint(\"should not run\")\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung on a bracket character inside a comment").expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("hi"),
            "print(\"hi\") must actually run -- the comment must not wedge the REPL: {stdout}"
        );
        assert!(
            !stdout.contains("should not run"),
            ":quit must genuinely terminate the session, not get silently appended to a dead buffer: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }

    /// A REAL, live-confirmed silent-WRONG-VALUE bug found+fixed (production-
    /// hardening PR-it1181, a fresh Explore survey finding, independently
    /// re-verified live before implementing -- see `callargs::
    /// resolve_call_args_in_stmt`'s own doc comment for the full writeup): a
    /// bare statement/expression typed at the REPL prompt -- the REPL's OWN
    /// PRIMARY interactive mode -- went straight from parsing to execution,
    /// entirely bypassing `callargs::resolve_call_args`, unlike the item-
    /// declaration path (`fun`/`type`/...) a few lines above, which routes
    /// through `run::compile` like an ordinary program. Live-confirmed BEFORE
    /// this fix via a real `kupl repl` subprocess: `fun sub(a: Int, b: Int)
    /// -> Int { a - b }` then `sub(b: 2, a: 10)` printed `-8` (the named
    /// arguments silently reinterpreted POSITIONALLY) instead of the correct
    /// `8` the IDENTICAL call gives inside a function body; `fun greet(name:
    /// Str, punct: Str = "!") -> Str { "{name}{punct}" }` then `greet("hi")`
    /// panicked instead of correctly applying the trailing default.
    #[test]
    fn a_bare_statement_at_the_repl_prompt_resolves_named_arguments_and_defaults_like_a_real_program() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "fun sub(a: Int, b: Int) -> Int { a - b }\n\
             sub(b: 2, a: 10)\n\
             fun greet(name: Str, punct: Str = \"!\") -> Str { \"{name}{punct}\" }\n\
             greet(\"hi\")\n\
             fun add(a: Int, b: Int) -> Int { a + b }\n\
             add(3, 4)\n\
             sub(a: 1, c: 2)\n\
             :quit\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung on named-arg/default resolution").expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("8\n"),
            "named arguments must resolve correctly (10 - 2 = 8), not positionally (-8): {stdout}"
        );
        assert!(!stdout.contains("-8"), "must not silently compute the WRONG positional answer: {stdout}");
        assert!(
            stdout.contains("hi!"),
            "a trailing default parameter must be applied, not panic: {stdout}"
        );
        assert!(
            !stderr.contains("takes 2 argument"),
            "must not panic on a missing-but-defaulted trailing argument: {stderr}"
        );
        // discriminating pair: an ordinary POSITIONAL call (baseline, no named
        // args/defaults involved at all) must be completely unaffected.
        assert!(stdout.contains("7\n"), "an ordinary positional call must still work: {stdout}");
        // discriminating pair: a genuinely malformed named-arg call (unknown
        // parameter name) must still be a clean error, not silently accepted
        // or a panic/crash.
        assert!(
            stderr.contains("K0273") || stderr.contains("K0274"),
            "an unknown parameter name must still be a clean, reported error: {stderr}"
        );
        assert!(
            !stdout.contains("panicked at") && !stdout.contains("internal compiler error"),
            "kupl repl panicked: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }

    /// End-to-end companion to
    /// `braces_balanced_handles_a_string_ending_in_an_escaped_backslash`
    /// above: spawns the REAL `kupl repl` process to confirm the full wedge
    /// is fixed, not just the underlying pure function. Live-confirmed
    /// BEFORE this fix: `print("\\")` (a string containing one escaped
    /// backslash) permanently wedged the session -- neither it nor any
    /// later line, including `:quit`, ever ran; the process only exited via
    /// silent EOF with the input never fully consumed.
    #[test]
    fn a_string_ending_in_an_escaped_backslash_does_not_wedge_the_session() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "print(\"\\\\\")\nprint(\"done-marker\")\n:quit\nprint(\"should not run\")\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out
            .expect("kupl repl hung on a string ending in an escaped backslash")
            .expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("done-marker"),
            "print(\"done-marker\") must actually run -- the escaped-backslash string must not wedge the REPL: {stdout}"
        );
        assert!(
            !stdout.contains("should not run"),
            ":quit must genuinely terminate the session, not get silently appended to a dead buffer: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }

    /// End-to-end companion to
    /// `braces_balanced_handles_a_nested_string_inside_an_interpolation_
    /// expression` above: spawns the REAL `kupl repl` process to confirm
    /// the full wedge is fixed, not just the underlying pure function.
    /// Live-confirmed BEFORE this fix: `"{f("(")}"` (a bracket character
    /// inside a nested string within an interpolation expression)
    /// permanently wedged the session -- neither it nor any later line,
    /// including `:quit`, ever ran; the process only exited via silent EOF
    /// with the input never fully consumed.
    #[test]
    fn a_bracket_character_inside_a_nested_interpolation_string_does_not_wedge_the_session() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "\"{f(\"(\")}\"\nprint(\"done-marker\")\n:quit\nprint(\"should not run\")\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out
            .expect("kupl repl hung on a bracket character inside a nested interpolation string")
            .expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("done-marker"),
            "print(\"done-marker\") must actually run -- the nested-string interpolation must not wedge the REPL: {stdout}"
        );
        assert!(
            !stdout.contains("should not run"),
            ":quit must genuinely terminate the session, not get silently appended to a dead buffer: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }

    /// End-to-end companion to `is_item_classifies_declarations_vs_expressions`
    /// above: spawns the REAL `kupl repl` process to confirm a top-level `law`
    /// is genuinely captured as a definition, not just that the pure `is_item`
    /// function classifies it correctly. Live-confirmed BEFORE this fix
    /// (production-hardening PR-it854, the THIRTY-THIRD survey): typing a
    /// `law "..." { ... }` block at the REPL prompt produced a misleading
    /// `error[K0102]: expected end of statement, found string literal`
    /// instead of `"defined."`, and `:defs` never showed it. Also confirms
    /// two identically-named laws BOTH get captured (duplicate law names are
    /// legitimately allowed by the compiler, unlike fun/type/component) --
    /// guards against a future "fix" that over-corrects into deduping laws
    /// by name the way the general item-redefinition path does.
    #[test]
    fn a_top_level_law_is_captured_as_a_definition_not_a_parse_error() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "law \"one\" {\n    expect 1 == 1\n}\nlaw \"one\" {\n    expect 2 == 2\n}\n:defs\n:quit\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung on a top-level law").expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            stdout.matches("defined.").count(),
            2,
            "both laws must be captured as definitions, not misrouted to a parse error: stdout={stdout} stderr={stderr}"
        );
        assert!(!stderr.contains("K0102"), "no parse error should fire for a top-level law: stderr={stderr}");
        assert_eq!(
            stdout.matches("law \"one\"").count(),
            2,
            ":defs must list BOTH identically-named laws, not dedupe them by name: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }

    /// End-to-end companion to
    /// `is_comment_and_whitespace_only_detects_the_no_op_case_and_rejects_real_code`
    /// above: spawns the REAL `kupl repl` process to confirm a comment-only
    /// submission (both `//` and `/* .. */` forms) is a silent no-op, not a
    /// spurious `K0110`. Live-confirmed BEFORE this fix: `printf '// a
    /// helper\n:quit\n' | kupl repl` printed `error[K0110]: expected an
    /// expression, found end of file` for the comment line.
    #[test]
    fn a_comment_only_submission_is_a_silent_no_op_not_a_parse_error() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/kupl");
        if !bin.exists() {
            return;
        }
        let input = "// a helper\n/* another one */\nprint(\"hi\")\n:quit\n";
        let mut child = std::process::Command::new(&bin)
            .arg("repl")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("kupl repl spawns");
        let mut stdin = child.stdin.take().unwrap();
        let input_bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input_bytes);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(15));
        let _ = writer.join();
        let out = out.expect("kupl repl hung on a comment-only submission").expect("wait_with_output succeeds");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("K0110"),
            "a comment-only submission must be a silent no-op, not a parse error: stdout={stdout} stderr={stderr}"
        );
        assert!(
            stdout.contains("hi"),
            "print(\"hi\") must still actually run after the comment-only lines: {stdout}"
        );
        assert!(out.status.success(), ":quit must exit cleanly: {out:?}");
    }
}
