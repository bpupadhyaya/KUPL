//! `kupl diff` — semantic comparison of two KUPL files.
//!
//! Items are compared by canonical form (the formatter), so whitespace,
//! comments, and layout never register as changes. Changes are classified:
//!   - interface: signature/ports/props/exposes/fulfills changed (breaking)
//!   - implementation: only bodies/state/wiring changed (non-breaking)
//! Exit code 0 = semantically identical, 1 = changes found OR either input
//! failed to load (missing/unreadable file, parse error, duplicate item
//! name) -- mirrors `kupl fmt --check`'s own established convention of
//! reusing exit 1 for "this isn't in a fully clean state," rather than a
//! genuine command-line usage error (exit 2, e.g. a missing/extra
//! argument).

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Item, Program};
use crate::fmt::{expr_str, format_program, ty_str};
use crate::parser;

/// A parameter's fingerprint fragment: `name:Ty` or `name:Ty=default` (PR-it679).
/// A default's PRESENCE and VALUE are both caller-observable interface facts --
/// removing `= EXPR` turns an optional argument into a required one (an
/// existing call site that omits it now fails to compile), and CHANGING an
/// existing default's value changes what a call site that omits the argument
/// actually receives, even though nothing else about the signature changed.
/// Mirrors the exact `= {expr}` rendering `fmt.rs`'s canonical formatter uses.
fn param_fingerprint(p: &crate::ast::Param) -> String {
    match &p.default {
        Some(d) => format!("{}:{}={}", p.name, ty_str(&p.ty), expr_str(d, 0)),
        None => format!("{}:{}", p.name, ty_str(&p.ty)),
    }
}

/// A program's imported module paths, as a SET -- `use` declaration ORDER has
/// no semantic meaning in KUPL, mirroring how items are compared by
/// `(kind, name)` identity rather than list position. Shared between the real
/// diff path and the test module's `diff_lines` helper (mirroring how
/// `items_by_kind_and_name` is ALSO shared, PR-it699/757's own precedent) so a
/// regression here is caught by either.
fn use_paths(program: &Program) -> BTreeSet<String> {
    program.uses.iter().map(|(p, _)| p.clone()).collect()
}

pub fn semantic_diff(old_path: &str, new_path: &str) -> i32 {
    let (old_items, old_uses, ok_a) = load_items(old_path);
    let (new_items, new_uses, ok_b) = load_items(new_path);
    if !ok_a || !ok_b {
        // A REAL bug found+fixed (production-hardening PR-it1147, an
        // Explore-agent survey finding, independently re-verified live
        // before implementing): this used to return 2 (the "malformed
        // command line" class), conflating a genuine USAGE error (missing/
        // extra argument -- main.rs's own `diff` dispatch arm, unaffected
        // by this fix) with a bad INPUT (one of the two files is missing,
        // unreadable, fails to parse, or has a duplicate item name). Every
        // OTHER file-taking subcommand in this binary (`run`/`check`/`dis`/
        // `native`/`manifest`/`test`/`build`/`bundle`/`context`, all routed
        // through `with_path`/`load_compile`) treats this exact condition
        // as a load failure, exit 1 -- established and locked in by
        // PR-it594's own `with_file_reports_missing_entry_as_a_load_
        // failure_not_a_bare_usage_error` test. Live-confirmed before this
        // fix: `kupl check missing.kupl` exits 1, but `kupl diff missing.
        // kupl real.kupl` exited 2 for the IDENTICAL underlying condition
        // -- a script/CI job branching on exit code (a pattern this
        // project's own conventions actively encourage, e.g. `fmt --check`)
        // could never distinguish "you typed the diff command wrong" from
        // "one of the two files you pointed at doesn't exist" for `diff`
        // alone, unlike every sibling subcommand. `sdiff.rs`'s own
        // load_items/semantic_diff pairing was simply never routed through
        // the K0400 convention the rest of the CLI converged on, and slipped
        // past both PR-it594's original sweep (main.rs::with_file only) and
        // PR-it864's later one (diff's own EXTRA-argument handling only,
        // still correctly exit 2, unaffected here).
        return 1;
    }

    let mut changes = 0usize;
    let report = |line: String| {
        println!("{line}");
    };

    // A REAL bug found+fixed (production-hardening PR-it776, an Explore
    // survey finding, agentId ad3c3f6ee2f0cd891, independently re-verified
    // live before implementing): `use` declarations (`Program.uses`) were
    // never compared at all -- only `program.items` fed `load_items`. Two
    // files with textually-identical item bodies but a REMOVED `use` line
    // reported `semantically identical` (exit 0) even when the removal makes
    // the new file fail to even compile (`error[K0240]: unknown name`).
    // Confirmed live: an entry file calling `mathlib.value()` with `use
    // mathlib` present checks cleanly; the identical file with that ONE line
    // deleted fails K0240 -- yet `kupl diff` reported no change at all. A
    // `use` change has no interface/implementation distinction (unlike an
    // item, it has no signature to compare) -- reported the same simple way
    // as an added/removed item.
    for path in old_uses.difference(&new_uses) {
        changes += 1;
        report(format!("removed    use {path}"));
    }
    for path in new_uses.difference(&old_uses) {
        changes += 1;
        report(format!("added      use {path}"));
    }

    for (key, old) in &old_items {
        let name = &key.1;
        match new_items.get(key) {
            None => {
                changes += 1;
                report(format!("removed    {} {name}", kind(old)));
            }
            Some(new) => {
                let old_canon = canonical(old);
                let new_canon = canonical(new);
                if old_canon == new_canon {
                    continue; // identical (formatting/comments ignored by construction)
                }
                changes += 1;
                if interface_of(old) != interface_of(new) {
                    report(format!("changed    {} {name}  [INTERFACE — breaking]", kind(old)));
                } else {
                    report(format!("changed    {} {name}  [implementation only]", kind(old)));
                }
            }
        }
    }
    for (key, new) in &new_items {
        if !old_items.contains_key(key) {
            changes += 1;
            report(format!("added      {} {}", kind(new), key.1));
        }
    }

    if changes == 0 {
        println!("semantically identical");
        0
    } else {
        println!("\n{changes} semantic change(s)");
        1
    }
}

fn load_items(path: &str) -> (BTreeMap<(&'static str, String), Item>, BTreeSet<String>, bool) {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return (BTreeMap::new(), BTreeSet::new(), false);
        }
    };
    let (program, diags) = parser::parse(&src);
    if diags
        .iter()
        .any(|d| d.severity == crate::diag::Severity::Error)
    {
        eprintln!("error: {path} does not parse; fix it before diffing");
        return (BTreeMap::new(), BTreeSet::new(), false);
    }
    let uses = use_paths(&program);
    match items_by_kind_and_name(program.items) {
        Ok(map) => (map, uses, true),
        Err((kind, name)) => {
            eprintln!("error: {path} declares {kind} `{name}` more than once; fix it before diffing");
            (BTreeMap::new(), BTreeSet::new(), false)
        }
    }
}

/// Key a program's items by `(kind, name)`, NOT bare name alone -- a REAL,
/// silent, CI-gate-defeating bug found+fixed (production-hardening PR-it699):
/// KUPL allows a `fun` and a `type` (or any two DIFFERENT item kinds) to share
/// the same name with no diagnostic anywhere (`check.rs`'s `collect` pass
/// keeps `funs`/`types`/`components`/`contracts` in entirely separate maps,
/// with no cross-kind duplicate-name check) -- an ordinary, non-adversarial
/// pattern (e.g. a `Config` type alongside a `Config(...)` smart-constructor
/// function). Keying this map by bare name alone let the later-loaded item
/// silently CLOBBER the earlier one, so `semantic_diff` only ever compared
/// ONE of the two colliding items -- confirmed live before this fix: `kupl
/// diff` reported `semantically identical` (exit 0) for a file pair where a
/// colliding type's shape had genuinely changed (a real, breaking interface
/// change), purely because a same-named function loaded after it in the same
/// file silently overwrote its map entry.
///
/// A SECOND, sibling REAL bug found+fixed (production-hardening PR-it757):
/// keying by `(kind, name)` closes the CROSS-kind collision (above) but does
/// NOTHING for the SAME-kind case -- two `fun add`s (or two `type Config`s)
/// collide on the IDENTICAL key and the plain `map.insert` below still let
/// the second silently clobber the first, with zero diagnostic (this file
/// never runs `check.rs`'s own K0203 "function defined more than once"
/// check -- `load_items` only gates on PARSER errors, above). Confirmed live
/// before this fix: `old.kupl` with two `fun add`s (`a+b` then `a-b`),
/// `new.kupl` changing the FIRST's body to `a*100` but leaving the second
/// `a-b` unchanged -- `kupl diff old.kupl new.kupl` reported `semantically
/// identical` (exit 0), silently hiding the first `add`'s genuinely changed
/// body. Rather than silently keeping either declaration (which would just
/// swap WHICH real change gets hidden depending on iteration order), a
/// same-kind duplicate now makes the WHOLE file un-diffable -- returned as
/// an `Err`, reported by `load_items` exactly like an unparseable file
/// (exit 2, "fix it before diffing") -- since a file with two declarations
/// sharing a key isn't a case this tool can meaningfully compare at all.
fn items_by_kind_and_name(items: Vec<Item>) -> Result<BTreeMap<(&'static str, String), Item>, (&'static str, String)> {
    let mut map = BTreeMap::new();
    for item in items {
        let key = (kind_tag(&item), item_name(&item).to_string());
        if map.contains_key(&key) {
            return Err(key);
        }
        map.insert(key, item);
    }
    Ok(map)
}

/// `pub(crate)`: also used by `repl.rs` to identify which prior top-level
/// declaration a freshly-typed one should replace (production-hardening
/// PR-it703) -- reusing this rather than a parallel name-extraction match.
pub(crate) fn item_name(item: &Item) -> &str {
    match item {
        Item::Fun(f) => &f.name,
        Item::Type(t) => &t.name,
        Item::Component(c) => &c.name,
        Item::Contract(ct) => &ct.name,
        Item::Law(l) => &l.name,
    }
}

/// Unpadded kind discriminant, for the `(kind, name)` map key -- distinct
/// from `kind()` below, whose fixed-width-padded strings are for aligned
/// DISPLAY output, not key uniqueness (though either would work as a key;
/// this one exists so the two concerns stay visibly separate). `pub(crate)`:
/// also used by `repl.rs` (production-hardening PR-it703).
pub(crate) fn kind_tag(item: &Item) -> &'static str {
    match item {
        Item::Fun(_) => "fun",
        Item::Type(_) => "type",
        Item::Component(_) => "component",
        Item::Contract(_) => "contract",
        Item::Law(_) => "law",
    }
}

/// This item's own span in the source it was parsed from -- `pub(crate)`:
/// used by `repl.rs` (production-hardening PR-it992) to slice a multi-item
/// single input into its OWN individually-tracked per-item source text,
/// mirroring `item_name`/`kind_tag`'s exact per-variant match shape.
pub(crate) fn item_span(item: &Item) -> crate::diag::Span {
    match item {
        Item::Fun(f) => f.span,
        Item::Type(t) => t.span,
        Item::Component(c) => c.span,
        Item::Contract(ct) => ct.span,
        Item::Law(l) => l.span,
    }
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::Fun(_) => "fun      ",
        Item::Type(_) => "type     ",
        Item::Component(c) if c.is_app => "app      ",
        Item::Component(_) => "component",
        Item::Contract(_) => "contract ",
        Item::Law(_) => "law      ",
    }
}

/// Canonical text of a single item — the formatter is the equality oracle.
fn canonical(item: &Item) -> String {
    format_program(&Program { items: vec![item.clone()], uses: Vec::new() })
}

/// The public surface of an item: what other code (or a visual tool) can see.
fn interface_of(item: &Item) -> String {
    let mut s = String::new();
    match item {
        Item::Fun(f) => {
            s.push_str(&format!(
                "fun {}[{}] pub={} ai={}",
                f.name,
                f.type_params.join(","),
                f.is_pub,
                f.ai.is_some()
            ));
            for p in &f.params {
                s.push_str(&format!(" {}", param_fingerprint(p)));
            }
            if let Some(r) = &f.ret {
                s.push_str(&format!(" -> {}", ty_str(r)));
            }
            // A REAL false-positive bug found+fixed (production-hardening
            // PR-it1043, the SAME class PR-it646 fixed for `fulfills`/
            // `ports`/`exposes`): a `uses` effect budget is checked via SET
            // membership everywhere it's consumed (`check.rs`'s
            // `effects.iter().any(|budget| covers_effect(budget, e))`, line
            // ~1302), never by position or declaration order -- so `fun f()
            // uses io, net { .. }` reordered to `uses net, io`, with nothing
            // else changed, previously flagged `[INTERFACE — breaking]` on a
            // function with no actual caller-visible change. Sorted before
            // joining, same fix shape as PR-it646.
            let mut effects = f.effects.clone();
            effects.sort();
            s.push_str(&format!(" uses[{}]", effects.join(",")));
            if let Some(ai) = &f.ai {
                // A REAL bug found+fixed (production-hardening PR-it864, an
                // Explore survey finding, independently re-verified live
                // before implementing): the SAME sig-interface gap PR-it580/
                // PR-it643 fixed for contract method effects and a type's
                // own type parameters, found here for an `ai fun`'s `model`
                // field -- `intent`/`tools` are both part of this fingerprint
                // (an `ai fun`'s observable behavior), but `model` (which
                // underlying LLM the call routes to, e.g. `claude-opus-4-8`
                // vs a different vendor's model entirely) was omitted, so a
                // model-only change was misclassified as "[implementation
                // only]" instead of "[INTERFACE — breaking]". Live-confirmed
                // before this fix: an `ai fun` differing ONLY in its `model`
                // field reported `[implementation only]`, while the SAME
                // function differing only in `intent` correctly reported
                // `[INTERFACE — breaking]` -- the two fields control the
                // exact same kind of caller-observable behavior change but
                // were classified inconsistently.
                s.push_str(&format!(
                    " ai[intent={} model={} tools={}]",
                    ai.intent,
                    ai.model.as_deref().unwrap_or(""),
                    ai.tools.join(",")
                ));
            }
        }
        Item::Type(t) => {
            // A type's OWN type parameter list is part of its interface, exactly
            // like a `fun`'s `type_params` above -- both its ARITY (how many type
            // arguments a caller must supply) and its ORDER (which position binds
            // to which field, e.g. `Pair[A, B]` vs `Pair[B, A]`) are caller-
            // observable even when no field's rendered type string changes at all:
            // an UNUSED/phantom type parameter (`Tagged[T] = Tagged(value: Int)`,
            // legal in KUPL -- `T` never appears in a field) being renamed or
            // dropped entirely changes the required instantiation arity with NO
            // visible change to any field's `ty_str`; reordering a USED type
            // parameter list swaps which concrete type binds to which position
            // without changing any field's type text either. This was the SAME
            // sig-interface gap `PR-it580` fixed for contract method effects,
            // just for `Item::Type` this time (production-hardening PR-it643).
            s.push_str(&format!("[{}]", t.type_params.join(",")));
            for v in &t.variants {
                s.push_str(&format!("{}(", v.name));
                for fld in &v.fields {
                    s.push_str(&format!("{}:{},", fld.name, ty_str(&fld.ty)));
                }
                s.push(')');
            }
        }
        Item::Component(c) => {
            // `fulfills`/`ports`/`exposes` are all looked up BY NAME everywhere
            // they're consumed -- `fulfills` via set membership (`check.rs`'s
            // `.contains`/`.any`), `ports` via named wiring (`WireDecl`'s
            // `(component, port)` string tuples, never a position), `exposes`
            // via `sig.exposes.insert(f.name.clone(), ...)` (a HashMap keyed by
            // name). Declaration ORDER therefore carries no interface-observable
            // meaning for any of the three -- confirmed live: reordering each one
            // alone (with no other change) previously flagged `[INTERFACE —
            // breaking]` on a component with no actual caller-visible change, a
            // FALSE POSITIVE (the opposite failure mode from `PR-it580`/`PR-it643`,
            // which were false NEGATIVES) -- so each is sorted before joining, to
            // capture the SET (additions/removals still correctly flagged) without
            // being sensitive to source order. `props`, by contrast, genuinely IS
            // order-sensitive (`interp.rs`'s `instantiate`: "props: by name or
            // POSITION, else default" -- an unnamed positional call argument binds
            // by declaration index) and is deliberately left in declaration order
            // (production-hardening PR-it646).
            let mut fulfills = c.fulfills.clone();
            fulfills.sort();
            s.push_str(&format!("app={} fulfills[{}]", c.is_app, fulfills.join(",")));
            let mut ports: Vec<&crate::ast::Port> = c.ports.iter().collect();
            ports.sort_by_key(|p| &p.name);
            for p in ports {
                s.push_str(&format!(
                    " {}:{}:{}",
                    if p.dir == crate::ast::PortDir::In { "in" } else { "out" },
                    p.name,
                    ty_str(&p.ty)
                ));
            }
            for p in &c.props {
                // A prop's default VALUE (not just its presence) is caller-observable
                // too, the same reasoning as `param_fingerprint` below (PR-it679) --
                // this used to render only `req={bool}`, which caught a default being
                // added/removed but not an EXISTING default's value changing.
                let default_part =
                    p.default.as_ref().map(|d| format!("={}", expr_str(d, 0))).unwrap_or_default();
                s.push_str(&format!(" prop {}:{}{}", p.name, ty_str(&p.ty), default_part));
            }
            let mut exposes: Vec<&crate::ast::FunDecl> = c.exposes.iter().collect();
            exposes.sort_by_key(|f| &f.name);
            for f in exposes {
                s.push_str(&format!(" expose {}(", f.name));
                for p in &f.params {
                    s.push_str(&format!("{},", param_fingerprint(p)));
                }
                s.push_str(&format!(")->{}", f.ret.as_ref().map(ty_str).unwrap_or_else(|| "Unit".into())));
                // Same PR-it1043 fix as `Item::Fun`'s own effects above --
                // a component expose's `uses` budget is equally set-checked,
                // equally order-insensitive.
                let mut effects = f.effects.clone();
                effects.sort();
                s.push_str(&format!(" uses[{}]", effects.join(",")));
            }
        }
        Item::Contract(ct) => {
            // Same order-insensitivity as `Item::Component` above -- fulfilling
            // components satisfy contract `sigs`/`laws` by NAME (`check.rs` line
            // ~780: `for (fname, (params, ret, effects)) in &contract.sigs`, a
            // name-keyed lookup once checked), never by declaration position, so
            // reordering either list is not a real interface change (confirmed
            // live: reordering two `expose fun`s, or two `law`s, with no other
            // change, previously flagged `[INTERFACE — breaking]` incorrectly).
            let mut sigs: Vec<&crate::ast::FunSig> = ct.sigs.iter().collect();
            sigs.sort_by_key(|sig| &sig.name);
            for sig in sigs {
                s.push_str(&format!(" {}(", sig.name));
                for p in &sig.params {
                    s.push_str(&format!("{}:{},", p.name, ty_str(&p.ty)));
                }
                s.push_str(&format!(")->{}", sig.ret.as_ref().map(ty_str).unwrap_or_else(|| "Unit".into())));
                // A contract method's declared effect BUDGET is part of its public
                // interface, same as a top-level fun's or a component expose's `uses`
                // clause above -- any fulfilling component must satisfy it (K0264).
                // Widening it (e.g. adding `uses io`) is a genuine breaking change: an
                // EXISTING fulfilling component may no longer satisfy the contract.
                // This was the one sig-interface site missing `uses[...]`, so a
                // contract-only effect change was misclassified as "implementation
                // only" instead of "[INTERFACE — breaking]" (PR-it580).
                // Same PR-it1043 fix as the two `uses` sites above -- a
                // contract sig's own effect budget is equally order-
                // insensitive (checked via `.any()`, never by position).
                let mut effects = sig.effects.clone();
                effects.sort();
                s.push_str(&format!(" uses[{}]", effects.join(",")));
            }
            let mut laws: Vec<&crate::ast::Law> = ct.laws.iter().collect();
            laws.sort_by_key(|law| &law.name);
            for law in laws {
                // A REAL, LIVE-CONFIRMED misclassification bug found+fixed
                // (production-hardening PR-it1042, a close-read survey
                // finding, independently re-verified live before
                // implementing): only `law.name` was ever fingerprinted here
                // -- `law.body` (the actual `forall`/`expect` assertion
                // EVERY fulfilling component's `kupl test` run must satisfy)
                // was invisible to `interface_of`, so strengthening or
                // weakening a law's own assertion (same name, same contract
                // signature, different body) was classified `[implementation
                // only]` -- the SAME severity class as PR-it580 (a contract's
                // effect budget silently not flagged as breaking), just for
                // a law's own enforced guarantee instead. Live-confirmed: a
                // `contract Store` with `law "..." { forall k: Str { expect
                // get(k) >= 0 } }` strengthened to `expect get(k) > 100`,
                // alongside an UNCHANGED fulfilling `component ZeroStore`
                // (`expose fun get(k: Str) -> Int { 0 }`) -- `kupl test`
                // genuinely went from `1 passed, 0 failed` to `FAIL ... `get
                // (k) > 100` was not satisfied`` purely due to the contract
                // update, yet `kupl diff` reported `changed contract Store
                // [implementation only]` on both files, both of which
                // `kupl check` cleanly. A package author trusting that
                // verdict to bump `Store`'s version as a non-breaking patch
                // would silently break every downstream fulfiller's test
                // suite. Fixed by folding the law's own canonical body text
                // into its fingerprint, reusing `canonical()`'s own exact
                // "wrap in a synthetic single-item Program, run it through
                // the formatter" technique (`Law` is the SAME struct used by
                // both `Item::Law` and `ContractDecl.laws`, so this needs no
                // new machinery) -- `interface_of`'s result is ONLY ever
                // compared for string equality, never displayed, so
                // embedding the body's full (possibly multi-line) rendered
                // text here is safe.
                let body_text = crate::fmt::format_program(&crate::ast::Program {
                    items: vec![crate::ast::Item::Law(law.clone())],
                    uses: Vec::new(),
                });
                s.push_str(&format!(" law:{}[{body_text}]", law.name));
            }
        }
        // a top-level law has no public interface (it is a test, not surface)
        Item::Law(l) => s.push_str(&format!("law {}", l.name)),
    }
    s
}

#[cfg(test)]
mod tests {
    fn diff_lines(old: &str, new: &str) -> (Vec<String>, bool) {
        // in-memory variant of semantic_diff for tests -- shares the SAME
        // `(kind, name)`-keyed map-building as the real path (production-
        // hardening PR-it699: this used to independently reimplement the
        // bare-name-keyed map, so it wouldn't have caught the cross-kind
        // collision bug at all).
        let (pa, da) = crate::parser::parse(old);
        let (pb, db) = crate::parser::parse(new);
        assert!(da.is_empty() && db.is_empty());
        // Compared as a SET, matching the real path (PR-it776) -- `use`
        // declaration order is not semantically meaningful.
        let uses_a = super::use_paths(&pa);
        let uses_b = super::use_paths(&pb);
        let a = super::items_by_kind_and_name(pa.items).expect("old has no duplicate (kind, name)");
        let b = super::items_by_kind_and_name(pb.items).expect("new has no duplicate (kind, name)");
        let mut lines = Vec::new();
        let mut changed = false;
        for path in uses_a.difference(&uses_b) {
            changed = true;
            lines.push(format!("removed use {path}"));
        }
        for path in uses_b.difference(&uses_a) {
            changed = true;
            lines.push(format!("added use {path}"));
        }
        for (key, old) in &a {
            let name = &key.1;
            match b.get(key) {
                None => {
                    changed = true;
                    lines.push(format!("removed {name}"));
                }
                Some(new) => {
                    if super::canonical(old) == super::canonical(new) {
                        continue;
                    }
                    changed = true;
                    if super::interface_of(old) != super::interface_of(new) {
                        lines.push(format!("interface {name}"));
                    } else {
                        lines.push(format!("impl {name}"));
                    }
                }
            }
        }
        for key in b.keys() {
            if !a.contains_key(key) {
                changed = true;
                lines.push(format!("added {}", key.1));
            }
        }
        (lines, changed)
    }

    #[test]
    fn formatting_only_is_not_a_change() {
        let (lines, changed) = diff_lines(
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n",
            "// a comment\nfun add(a:Int,b:Int)->Int{ a + b }\n",
        );
        assert!(!changed, "{lines:?}");
    }

    /// A REAL bug found+fixed (production-hardening PR-it776, an Explore
    /// survey finding, agentId ad3c3f6ee2f0cd891, independently re-verified
    /// live before implementing): `use` declarations (`Program.uses`) were
    /// never compared at all -- `load_items` only ever extracted
    /// `program.items`. Two files with textually-identical item bodies but a
    /// REMOVED `use` line reported `semantically identical` (exit 0), even
    /// though the removal can make the new file fail to even compile.
    /// Confirmed live via a real two-package structure before this fix: an
    /// entry file calling `mathlib.value()` checks cleanly with `use
    /// mathlib` present, fails with K0240 (unknown name) with that ONE line
    /// deleted -- yet `kupl diff` reported no change at all either way.
    #[test]
    fn a_removed_or_added_use_declaration_is_a_reported_change() {
        let old = "use mathlib\n\nfun run() -> Int {\n    1\n}\n";
        let new = "fun run() -> Int {\n    1\n}\n";
        let (lines, changed) = diff_lines(old, new);
        assert!(changed, "removing a `use` must register as a change: {lines:?}");
        assert_eq!(lines, vec!["removed use mathlib"], "{lines:?}");

        // symmetric: the SAME pair, reversed, reports it as ADDED.
        let (lines2, changed2) = diff_lines(new, old);
        assert!(changed2, "{lines2:?}");
        assert_eq!(lines2, vec!["added use mathlib"], "{lines2:?}");
    }

    #[test]
    fn reordering_use_declarations_is_not_a_change() {
        // `use` declaration ORDER has no semantic meaning in KUPL -- a diff
        // tool must compare the SET of imports, not textual position,
        // mirroring how items are already compared by (kind, name) identity
        // rather than list position.
        let (lines, changed) = diff_lines(
            "use alpha\nuse beta\n\nfun run() -> Int {\n    1\n}\n",
            "use beta\nuse alpha\n\nfun run() -> Int {\n    1\n}\n",
        );
        assert!(!changed, "{lines:?}");
    }

    /// `diff_lines` (above) is an IN-MEMORY reimplementation of the
    /// comparison ALGORITHM (sharing `use_paths`/`items_by_kind_and_name`
    /// with the real path, but not `load_items`/`semantic_diff`
    /// themselves) -- it cannot catch a WIRING bug where `semantic_diff`
    /// simply never CALLS `use_paths` at all (the exact shape of the
    /// original PR-it776 bug). This test calls the REAL, public
    /// `semantic_diff` with REAL temp files end-to-end, closing that gap:
    /// confirms the actual exit code a `kupl diff` invocation would produce,
    /// not just the algorithm in isolation.
    #[test]
    fn semantic_diff_end_to_end_detects_a_removed_use_via_real_files() {
        let dir = std::env::temp_dir().join(format!("kupl-sdiff-uses-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_path = dir.join("old.kupl");
        let new_path = dir.join("new.kupl");
        std::fs::write(&old_path, "use mathlib\n\nfun run() -> Int {\n    1\n}\n").unwrap();
        std::fs::write(&new_path, "fun run() -> Int {\n    1\n}\n").unwrap();

        let code = super::semantic_diff(old_path.to_str().unwrap(), new_path.to_str().unwrap());
        assert_eq!(code, 1, "a removed `use` must be reported as a change, not exit 0 (semantically identical)");

        // the SAME file against itself must still report no change.
        let code_self = super::semantic_diff(old_path.to_str().unwrap(), old_path.to_str().unwrap());
        assert_eq!(code_self, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL, silent, CI-gate-defeating bug (production-hardening PR-it699): KUPL
    /// allows a `fun` and a `type` (or any two DIFFERENT item kinds) to share the
    /// same name with no diagnostic anywhere -- an ordinary pattern, e.g. a `Point`
    /// type alongside a `Point(...)` smart-constructor function. Keying the diff's
    /// item map by bare name alone let the later-loaded item silently CLOBBER the
    /// earlier one, so a real, breaking change to the type's shape was silently
    /// invisible to `kupl diff` whenever a same-named function also existed --
    /// confirmed live before this fix: `semantically identical` (exit 0) for a
    /// file pair where the colliding type's fields had genuinely changed.
    #[test]
    fn cross_kind_name_collision_does_not_hide_a_real_change() {
        let old = "type Point = Rec(x: Int, y: Int)\nfun Point(n: Int) -> Int {\n    n + 1\n}\n";
        // the TYPE's shape changed (a real, breaking interface change) while the
        // same-named FUNCTION is untouched.
        let new = "type Point = Rec(x: Int, y: Int, z: Int)\nfun Point(n: Int) -> Int {\n    n + 1\n}\n";
        let (lines, changed) = diff_lines(old, new);
        assert!(changed, "the type's shape genuinely changed: {lines:?}");
        assert_eq!(lines, vec!["interface Point"], "must report the TYPE's change, not silently drop it: {lines:?}");

        // conversely, only the FUNCTION's body changes -- the type is untouched.
        let new2 = "type Point = Rec(x: Int, y: Int)\nfun Point(n: Int) -> Int {\n    n + 2\n}\n";
        let (lines2, changed2) = diff_lines(old, new2);
        assert!(changed2, "{lines2:?}");
        assert_eq!(lines2, vec!["impl Point"], "must report the FUNCTION's change, not the type: {lines2:?}");

        // BOTH kinds present, BOTH unchanged -- no collision-induced false positive.
        let (lines3, changed3) = diff_lines(old, old);
        assert!(!changed3, "{lines3:?}");
    }

    /// A SECOND, sibling REAL bug (production-hardening PR-it757): PR-it699's
    /// own `(kind, name)`-keyed map (immediately above) closes the CROSS-kind
    /// collision but does nothing for the SAME-kind case -- two `fun add`s (or
    /// two `type Config`s) collide on the IDENTICAL key, so the plain
    /// `map.insert` still let the second silently clobber the first, with zero
    /// diagnostic (`load_items` only gates on PARSER errors, never running
    /// `check.rs`'s own K0203 "function defined more than once" check).
    /// Confirmed live before this fix: a file pair where the FIRST of two
    /// same-named `fun add`s had its body genuinely changed (the second
    /// left untouched) still reported `semantically identical` (exit 0),
    /// silently hiding the change purely because of iteration/insertion
    /// order. Rather than silently keeping whichever declaration happens to
    /// win (which would just swap WHICH real change gets hidden), a same-kind
    /// duplicate now makes the WHOLE file un-diffable.
    #[test]
    fn same_kind_duplicate_name_makes_the_file_undiffable_not_silently_wrong() {
        // items_by_kind_and_name directly: a same-kind duplicate returns an
        // Err identifying the exact (kind, name) collision, not a silently
        // clobbered map.
        let (p, d) = crate::parser::parse(
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n\
             fun add(a: Int, b: Int) -> Int {\n    a - b\n}\n",
        );
        assert!(d.is_empty(), "{d:?}");
        let err = super::items_by_kind_and_name(p.items).unwrap_err();
        assert_eq!(err, ("fun", "add".to_string()), "{err:?}");

        // the SAME collision for a TYPE (the more consequential case: an
        // interface-breaking field-shape change, not just an implementation
        // body).
        let (pt, dt) = crate::parser::parse(
            "type Config = Config(name: Str)\ntype Config = Config(name: Str, port: Int)\n",
        );
        assert!(dt.is_empty(), "{dt:?}");
        let errt = super::items_by_kind_and_name(pt.items).unwrap_err();
        assert_eq!(errt, ("type", "Config".to_string()), "{errt:?}");

        // end to end via `semantic_diff` (the REAL CLI entry point): a
        // same-kind duplicate must make the WHOLE FILE un-diffable (exit 1
        // -- a LOAD failure, the SAME code an unparseable/missing file
        // already returns since PR-it1147's own sibling-consistency fix,
        // not a usage error), not silently report "semantically identical"
        // while a real change to one of the colliding declarations goes
        // completely unreported.
        let dir = std::env::temp_dir().join(format!("kupl-sdiff-dup-it757-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_path = dir.join("old.kupl");
        let new_path = dir.join("new.kupl");
        std::fs::write(
            &old_path,
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\nfun add(a: Int, b: Int) -> Int {\n    a - b\n}\n",
        )
        .unwrap();
        std::fs::write(
            &new_path,
            // the FIRST add's body genuinely changed; the second is untouched.
            "fun add(a: Int, b: Int) -> Int {\n    a * 100\n}\nfun add(a: Int, b: Int) -> Int {\n    a - b\n}\n",
        )
        .unwrap();
        let code = super::semantic_diff(old_path.to_str().unwrap(), new_path.to_str().unwrap());
        assert_eq!(
            code, 1,
            "a same-kind duplicate must make the file un-diffable (exit 1, a load failure), not silently report success"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_change_is_implementation_only() {
        let (lines, _) = diff_lines(
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n",
            "fun add(a: Int, b: Int) -> Int {\n    b + a\n}\n",
        );
        assert_eq!(lines, vec!["impl add"]);
    }

    #[test]
    fn signature_change_is_interface() {
        let (lines, _) = diff_lines(
            "fun add(a: Int, b: Int) -> Int {\n    a + b\n}\n",
            "fun add(a: Float, b: Float) -> Float {\n    a + b\n}\n",
        );
        assert_eq!(lines, vec!["interface add"]);
    }

    #[test]
    fn effect_and_type_variant_changes_are_interface() {
        // Adding an effect changes the callable contract — a breaking interface
        // change, not merely an implementation detail.
        let (lines, _) = diff_lines(
            "pub fun f() -> Int {\n    1\n}\n",
            "pub fun f() uses io -> Int {\n    print(\"x\")\n    1\n}\n",
        );
        assert_eq!(lines, vec!["interface f"]);
        // Adding a variant to a sum type breaks exhaustive matches downstream.
        let (lines, _) = diff_lines("type C = Red | Green\n", "type C = Red | Green | Blue\n");
        assert_eq!(lines, vec!["interface C"]);
    }

    /// A REAL BUG found+fixed (PR-it679): a parameter's default value was
    /// completely invisible to `interface_of`'s fingerprint for `fun`s,
    /// component `expose`s, and a component `prop`'s VALUE specifically (its
    /// mere presence/absence was already caught via `req={bool}`, but not a
    /// change to an EXISTING default's value) -- so removing a default
    /// (turning an optional argument into a required one, breaking every
    /// existing call site that omits it) or changing its value (silently
    /// changing what an omitting call site receives) both misclassified as
    /// `[implementation only]`, confirmed live before this fix via `kupl
    /// diff` on both cases.
    #[test]
    fn parameter_default_value_change_is_interface_not_implementation() {
        // removing a default entirely (optional -> required) is breaking.
        let (lines, _) = diff_lines(
            "fun greet(name: Str = \"World\") -> Str {\n    name\n}\n",
            "fun greet(name: Str) -> Str {\n    name\n}\n",
        );
        assert_eq!(lines, vec!["interface greet"]);
        // changing an EXISTING default's value is also caller-observable.
        let (lines, _) = diff_lines(
            "fun greet(name: Str = \"World\") -> Str {\n    name\n}\n",
            "fun greet(name: Str = \"Kupl\") -> Str {\n    name\n}\n",
        );
        assert_eq!(lines, vec!["interface greet"]);
        // a component prop's default VALUE (not just presence) is the same story.
        let (lines, _) = diff_lines(
            "component W {\n    intent \"w\"\n    prop label: Str = \"a\"\n    expose fun get() -> Str {\n        label\n    }\n}\n",
            "component W {\n    intent \"w\"\n    prop label: Str = \"b\"\n    expose fun get() -> Str {\n        label\n    }\n}\n",
        );
        assert_eq!(lines, vec!["interface W"]);
        // an unchanged default, with only the body changing, is still implementation-only.
        let (lines, _) = diff_lines(
            "fun greet(name: Str = \"World\") -> Str {\n    name\n}\n",
            "fun greet(name: Str = \"World\") -> Str {\n    let n = name\n    n\n}\n",
        );
        assert_eq!(lines, vec!["impl greet"]);
    }

    /// A REAL BUG found+fixed (PR-it580): `interface_of`'s contract-sig branch was the
    /// ONE sig-interface site missing `uses[...]` -- a top-level `fun`'s effects (tested
    /// just above) and a component `expose`'s effects both count toward the interface,
    /// but a contract method's declared effect BUDGET (`FunSig.effects`) was silently
    /// dropped from its fingerprint entirely. Widening a contract's effect requirement
    /// (e.g. adding `uses io` to a method with none) is a genuine breaking change -- any
    /// EXISTING fulfilling component may no longer satisfy the contract (K0264) -- but
    /// `kupl diff` reported it as "[implementation only]" since the fingerprint before
    /// and after were byte-identical.
    #[test]
    fn contract_method_effect_change_is_interface() {
        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) uses io -> Int\n}\n",
        );
        assert_eq!(lines, vec!["interface Store"]);
        // a param-only rename with the SAME effects still correctly reports interface
        // (unrelated to this fix, but locks the sibling shape doesn't regress).
        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun get(key: Str) -> Int\n}\n",
        );
        assert_eq!(lines, vec!["interface Store"]);
    }

    /// A REAL, LIVE-CONFIRMED misclassification bug found+fixed (production-
    /// hardening PR-it1042, a close-read survey finding, independently
    /// re-verified live before implementing -- see `interface_of`'s own doc
    /// comment on the `Item::Contract` branch for the full writeup). Only
    /// `law.name` was ever fingerprinted -- `law.body` (the actual
    /// `forall`/`expect` assertion every fulfilling component's `kupl test`
    /// run must satisfy) was invisible, so strengthening/weakening a law's
    /// own assertion, same name, same contract signature, was classified
    /// "[implementation only]" -- the SAME severity class as PR-it580 (a
    /// contract's effect budget silently not flagged as breaking), just for
    /// a law's own enforced guarantee instead of its effect budget.
    #[test]
    fn contract_law_body_change_is_interface() {
        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    \
             law \"non-negative\" {\n        forall k: Str {\n            expect get(k) >= 0\n        }\n    }\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    \
             law \"non-negative\" {\n        forall k: Str {\n            expect get(k) > 100\n        }\n    }\n}\n",
        );
        assert_eq!(lines, vec!["interface Store"], "strengthening a law's own assertion is a breaking change to the contract");

        // an UNRELATED body-only change to the contract's own `intent` string
        // (display-only, not part of the enforced guarantee) must NOT be
        // conflated with a genuine law-body change -- sanity-checks the fix
        // is scoped to `law.body` specifically, not over-broadly triggered.
        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    \
             law \"non-negative\" {\n        forall k: Str {\n            expect get(k) >= 0\n        }\n    }\n}\n",
            "contract Store {\n    intent \"kv store\"\n    expose fun get(k: Str) -> Int\n    \
             law \"non-negative\" {\n        forall k: Str {\n            expect get(k) >= 0\n        }\n    }\n}\n",
        );
        assert_eq!(lines, vec!["impl Store"], "an intent-string-only change must remain implementation-only");
    }

    /// A REAL BUG found+fixed (production-hardening PR-it643): the SAME sig-
    /// interface gap `PR-it580` fixed for contract method effects, found in
    /// `Item::Type`'s branch this time -- `t.type_params` was entirely absent
    /// from the fingerprint, so a type parameter's arity/order was invisible
    /// to `kupl diff` whenever no FIELD's rendered type text happened to change.
    #[test]
    fn type_parameter_arity_change_is_interface_even_when_unused() {
        // an UNUSED (phantom) type parameter -- legal in KUPL, `T` never
        // appears in any field -- being dropped entirely changes the required
        // instantiation arity (`Tagged[Str]` becomes a type error) with ZERO
        // change to any field's `ty_str`.
        let (lines, _) = diff_lines(
            "type Tagged[T] = Tagged(value: Int)\n",
            "type Tagged = Tagged(value: Int)\n",
        );
        assert_eq!(lines, vec!["interface Tagged"]);
        // renaming a phantom type parameter is the same shape.
        let (lines, _) = diff_lines(
            "type Tagged[T] = Tagged(value: Int)\n",
            "type Tagged[U] = Tagged(value: Int)\n",
        );
        assert_eq!(lines, vec!["interface Tagged"]);
    }

    #[test]
    fn type_parameter_reorder_is_interface_even_with_identical_field_text() {
        // reordering a USED type parameter list swaps which concrete type
        // binds to which position (`Pair[Int, Str]` means A=Int,B=Str under
        // the old order but B=Int,A=Str under the new one) without changing
        // any field's rendered type text at all ("first:A,second:B," either way).
        let (lines, _) = diff_lines(
            "type Pair[A, B] = Pair(first: A, second: B)\n",
            "type Pair[B, A] = Pair(first: A, second: B)\n",
        );
        assert_eq!(lines, vec!["interface Pair"]);
    }

    /// A REAL bug found+fixed (production-hardening PR-it864, an Explore
    /// survey finding, independently re-verified live before implementing):
    /// the SAME sig-interface gap PR-it580/PR-it643 fixed for contract
    /// method effects and a type's own type parameters, found here for an
    /// `ai fun`'s `model` field -- `intent`/`tools` were both part of the
    /// fingerprint, but `model` was omitted, so a model-only change was
    /// misclassified as "[implementation only]" instead of "[INTERFACE —
    /// breaking]", even though it controls the exact same kind of
    /// caller-observable behavior change (which LLM the call routes to).
    #[test]
    fn ai_fun_model_change_is_interface_even_with_identical_intent_and_tools() {
        let (lines, _) = diff_lines(
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku about {topic}.\"\n    model \"claude-opus-4-8\"\n}\n",
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku about {topic}.\"\n    model \"a-totally-different-vendor-model\"\n}\n",
        );
        assert_eq!(lines, vec!["interface haiku"]);

        // sanity: an intent-only change (unrelated to this fix) still correctly
        // reports interface -- locks the sibling shape doesn't regress.
        let (lines, _) = diff_lines(
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku about {topic}.\"\n    model \"claude-opus-4-8\"\n}\n",
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Compose a haiku about {topic}.\"\n    model \"claude-opus-4-8\"\n}\n",
        );
        assert_eq!(lines, vec!["interface haiku"]);

        // sanity: no change at all (same model, same intent) is genuinely unchanged.
        let (lines, changed) = diff_lines(
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku about {topic}.\"\n    model \"claude-opus-4-8\"\n}\n",
            "ai fun haiku(topic: Str) -> Str {\n    intent \"Write a haiku about {topic}.\"\n    model \"claude-opus-4-8\"\n}\n",
        );
        assert!(lines.is_empty() && !changed, "identical ai fun declarations must report no change: {lines:?}");
    }

    /// A DELIBERATE, investigated-and-confirmed-correct DESIGN CHOICE
    /// (production-hardening PR-it1044, investigated as a queued follow-up
    /// from PR-it1043's own `uses`-effects reorder fix -- NOT a bug, this
    /// test exists to LOCK IN the current behavior and prevent a future
    /// well-meaning "fix" from silently introducing a real false-negative
    /// regression). Unlike a `fun`'s `uses` effect budget (fixed PR-it1043 --
    /// checked via SET membership in `check.rs`, provably order-insensitive)
    /// or `fulfills`/`ports`/`exposes` (fixed PR-it646 -- looked up BY NAME
    /// everywhere consumed), an `ai fun`'s `tools` list's declaration order
    /// is NOT provably safe to ignore: `check.rs::ai_tool_metas` builds
    /// `ToolMeta`s in `ai.tools`' own iteration order, and BOTH
    /// `ai.rs::anthropic_tools_json`/`openai_tools_json` render that SAME
    /// order, unchanged, directly into the `tools` array of the actual
    /// request sent to the model. Whether presentation order influences a
    /// specific LLM provider's own tool-selection behavior is NOT something
    /// this codebase controls or can statically prove either way (unlike the
    /// effects case, which had a clear, deterministic proof of order-
    /// insensitivity in `check.rs`'s own consumption code) -- so treating a
    /// tools-reorder as a potential interface change is the conservative,
    /// correct choice for a tool whose entire 17+-bug history is about
    /// eliminating FALSE NEGATIVES (a real behavioral change silently
    /// passing as non-breaking), not false positives. If a future iteration
    /// re-investigates this and finds concrete evidence tool order is
    /// genuinely irrelevant to every supported provider, revisit this test
    /// (and `ai.tools.join(",")`'s own lack of sorting) together.
    #[test]
    fn ai_fun_tools_reorder_is_interface_by_deliberate_design_not_a_bug() {
        let (lines, _) = diff_lines(
            "fun toolA(x: Int) -> Int {\n    x + 1\n}\nfun toolB(x: Int) -> Int {\n    x - 1\n}\n\
             ai fun classify(n: Int) -> Int tools [toolA, toolB] {\n    intent \"pick a tool for {n}\"\n}\n",
            "fun toolA(x: Int) -> Int {\n    x + 1\n}\nfun toolB(x: Int) -> Int {\n    x - 1\n}\n\
             ai fun classify(n: Int) -> Int tools [toolB, toolA] {\n    intent \"pick a tool for {n}\"\n}\n",
        );
        assert_eq!(
            lines,
            vec!["interface classify"],
            "reordering an ai fun's `tools` list is DELIBERATELY still interface-breaking -- see this test's own doc comment before changing this"
        );
    }

    #[test]
    fn reordering_items_is_not_a_semantic_change() {
        // The diff is keyed by item name, so swapping the order of two functions is
        // not a change (source order carries no semantic meaning).
        let (lines, changed) = diff_lines(
            "fun a() -> Int {\n    1\n}\nfun b() -> Int {\n    2\n}\n",
            "fun b() -> Int {\n    2\n}\nfun a() -> Int {\n    1\n}\n",
        );
        assert!(!changed, "reordering must not be a change: {lines:?}");
    }

    #[test]
    fn component_port_change_is_interface_state_change_is_impl() {
        let old = "component C {\n intent \"x\"\n in a: Int\n state n: Int = 0\n on a(v) { n += v }\n}\n";
        let impl_change = "component C {\n intent \"x\"\n in a: Int\n state n: Int = 100\n on a(v) { n += v }\n}\n";
        let iface_change = "component C {\n intent \"x\"\n in a: Str\n state n: Int = 0\n on a(v) { n += 1 }\n}\n";
        assert_eq!(diff_lines(old, impl_change).0, vec!["impl C"]);
        assert_eq!(diff_lines(old, iface_change).0, vec!["interface C"]);
    }

    /// A REAL BUG found+fixed (production-hardening PR-it646) — the OPPOSITE
    /// failure mode from `PR-it580`/`PR-it643` (which were false NEGATIVES,
    /// silently missing a genuine interface change): `fulfills`/`ports`/`exposes`
    /// were all captured in DECLARATION order, but none of the three is actually
    /// order-sensitive anywhere they're consumed (`fulfills` via set membership,
    /// `ports` via named wiring, `exposes` via a name-keyed lookup once checked)
    /// -- so a pure reorder with NO other change was a FALSE POSITIVE, incorrectly
    /// flagged `[INTERFACE — breaking]`.
    #[test]
    fn component_fulfills_ports_exposes_reorder_is_not_interface() {
        // NOTE: `canonical()` (the formatter) preserves DECLARATION order in its
        // printed text, so a pure reorder still registers as SOME change (the
        // formatted text literally differs) -- the fix under test is specifically
        // about the INTERFACE vs implementation CLASSIFICATION of that change,
        // not about whether it's flagged as a change at all. So the correct
        // expectation is `impl`, not "no change".
        let contracts = "contract A {\n    intent \"a\"\n    expose fun f() -> Int\n}\n\
                          contract B {\n    intent \"b\"\n    expose fun g() -> Int\n}\n";
        let (lines, _) = diff_lines(
            &format!("{contracts}component C fulfills A, B {{\n    expose fun f() -> Int {{ 1 }}\n    expose fun g() -> Int {{ 2 }}\n}}\n"),
            &format!("{contracts}component C fulfills B, A {{\n    expose fun f() -> Int {{ 1 }}\n    expose fun g() -> Int {{ 2 }}\n}}\n"),
        );
        assert_eq!(lines, vec!["impl C"], "reordering `fulfills` must be implementation-only");

        let (lines, _) = diff_lines(
            "component C {\n    in a: Int\n    in b: Str\n}\n",
            "component C {\n    in b: Str\n    in a: Int\n}\n",
        );
        assert_eq!(lines, vec!["impl C"], "reordering `ports` must be implementation-only");

        let (lines, _) = diff_lines(
            "component C {\n    expose fun a() -> Int { 1 }\n    expose fun b() -> Int { 2 }\n}\n",
            "component C {\n    expose fun b() -> Int { 2 }\n    expose fun a() -> Int { 1 }\n}\n",
        );
        assert_eq!(lines, vec!["impl C"], "reordering `exposes` must be implementation-only");

        // sanity: an ACTUAL fulfills change (not just reordered) still reports.
        let (lines, _) = diff_lines(
            &format!("{contracts}component C fulfills A {{\n    expose fun f() -> Int {{ 1 }}\n}}\n"),
            &format!("{contracts}component C fulfills A, B {{\n    expose fun f() -> Int {{ 1 }}\n    expose fun g() -> Int {{ 2 }}\n}}\n"),
        );
        assert_eq!(lines, vec!["interface C"]);
    }

    /// A REAL false-positive bug found+fixed (production-hardening PR-it1043,
    /// the SAME class as `PR-it646`'s `fulfills`/`ports`/`exposes` fix,
    /// independently re-verified live before implementing): a `uses` effect
    /// budget is checked via SET membership everywhere it's consumed
    /// (`check.rs`'s `effects.iter().any(|budget| covers_effect(budget, e))`),
    /// never by declaration order -- but all THREE `interface_of` sites that
    /// render `uses[...]` (a top-level `fun`, a component's own `expose fun`,
    /// and a contract's `sig`) used bare `.join(",")` with no sorting, so
    /// reordering a multi-effect `uses` clause with no other change was
    /// wrongly flagged `[INTERFACE — breaking]`. Covers all three sites, plus
    /// a sanity check that a GENUINE effect addition still correctly reports.
    #[test]
    fn uses_effects_reorder_is_not_interface() {
        let (lines, _) = diff_lines("fun f() uses io, net {\n    1\n}\n", "fun f() uses net, io {\n    1\n}\n");
        assert_eq!(lines, vec!["impl f"], "reordering a fun's `uses` effects must be implementation-only");

        let (lines, _) = diff_lines(
            "component C {\n    expose fun f() uses io, net -> Int {\n        1\n    }\n}\n",
            "component C {\n    expose fun f() uses net, io -> Int {\n        1\n    }\n}\n",
        );
        assert_eq!(lines, vec!["impl C"], "reordering a component expose's `uses` effects must be implementation-only");

        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get() uses io, net -> Int\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun get() uses net, io -> Int\n}\n",
        );
        assert_eq!(lines, vec!["impl Store"], "reordering a contract sig's `uses` effects must be implementation-only");

        // sanity: a GENUINE effect addition (not just reordered) still reports breaking.
        let (lines, _) = diff_lines("fun f() uses io, net {\n    1\n}\n", "fun f() uses io, net, fs {\n    1\n}\n");
        assert_eq!(lines, vec!["interface f"], "an actual new effect must still be flagged breaking");
    }

    /// Same false-positive shape as the component test above, for `Item::Contract`'s
    /// `sigs`/`laws` (production-hardening PR-it646) — a fulfilling component
    /// satisfies a contract's methods BY NAME (`check.rs`'s `for (fname, ...) in
    /// &contract.sigs`, name-keyed once checked), so declaration order is not
    /// interface-observable.
    #[test]
    fn contract_sigs_and_laws_reorder_is_not_interface() {
        // Same NOTE as the component test above: `canonical()` preserves source
        // order, so reordering still registers as SOME change -- the fix is about
        // the interface/impl CLASSIFICATION, so the expectation is `impl`.
        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    expose fun put(k: Str, v: Int) -> Int\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun put(k: Str, v: Int) -> Int\n    expose fun get(k: Str) -> Int\n}\n",
        );
        assert_eq!(lines, vec!["impl Store"], "reordering contract `sigs` must be implementation-only");

        let (lines, _) = diff_lines(
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    law \"one\" { assert(true) }\n    law \"two\" { assert(true) }\n}\n",
            "contract Store {\n    intent \"kv\"\n    expose fun get(k: Str) -> Int\n    law \"two\" { assert(true) }\n    law \"one\" { assert(true) }\n}\n",
        );
        assert_eq!(lines, vec!["impl Store"], "reordering `laws` must be implementation-only");
    }

    #[test]
    fn add_remove_detected() {
        let (lines, _) = diff_lines("fun a() {\n    1\n}\n", "fun b() {\n    1\n}\n");
        assert_eq!(lines, vec!["removed a", "added b"]);
    }
}
