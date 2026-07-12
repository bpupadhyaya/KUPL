//! `kupl diff` — semantic comparison of two KUPL files.
//!
//! Items are compared by canonical form (the formatter), so whitespace,
//! comments, and layout never register as changes. Changes are classified:
//!   - interface: signature/ports/props/exposes/fulfills changed (breaking)
//!   - implementation: only bodies/state/wiring changed (non-breaking)
//! Exit code 0 = semantically identical, 1 = changes found.

use std::collections::BTreeMap;

use crate::ast::{Item, Program};
use crate::fmt::{format_program, ty_str};
use crate::parser;

pub fn semantic_diff(old_path: &str, new_path: &str) -> i32 {
    let (old_items, ok_a) = load_items(old_path);
    let (new_items, ok_b) = load_items(new_path);
    if !ok_a || !ok_b {
        return 2;
    }

    let mut changes = 0usize;
    let report = |line: String| {
        println!("{line}");
    };

    for (name, old) in &old_items {
        match new_items.get(name) {
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
    for (name, new) in &new_items {
        if !old_items.contains_key(name) {
            changes += 1;
            report(format!("added      {} {name}", kind(new)));
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

fn load_items(path: &str) -> (BTreeMap<String, Item>, bool) {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return (BTreeMap::new(), false);
        }
    };
    let (program, diags) = parser::parse(&src);
    if diags
        .iter()
        .any(|d| d.severity == crate::diag::Severity::Error)
    {
        eprintln!("error: {path} does not parse; fix it before diffing");
        return (BTreeMap::new(), false);
    }
    let mut map = BTreeMap::new();
    for item in program.items {
        map.insert(item_name(&item).to_string(), item);
    }
    (map, true)
}

fn item_name(item: &Item) -> &str {
    match item {
        Item::Fun(f) => &f.name,
        Item::Type(t) => &t.name,
        Item::Component(c) => &c.name,
        Item::Contract(ct) => &ct.name,
        Item::Law(l) => &l.name,
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
                s.push_str(&format!(" {}:{}", p.name, ty_str(&p.ty)));
            }
            if let Some(r) = &f.ret {
                s.push_str(&format!(" -> {}", ty_str(r)));
            }
            s.push_str(&format!(" uses[{}]", f.effects.join(",")));
            if let Some(ai) = &f.ai {
                s.push_str(&format!(" ai[intent={} tools={}]", ai.intent, ai.tools.join(",")));
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
                s.push_str(&format!(" prop {}:{} req={}", p.name, ty_str(&p.ty), p.default.is_none()));
            }
            let mut exposes: Vec<&crate::ast::FunDecl> = c.exposes.iter().collect();
            exposes.sort_by_key(|f| &f.name);
            for f in exposes {
                s.push_str(&format!(" expose {}(", f.name));
                for p in &f.params {
                    s.push_str(&format!("{}:{},", p.name, ty_str(&p.ty)));
                }
                s.push_str(&format!(")->{}", f.ret.as_ref().map(ty_str).unwrap_or_else(|| "Unit".into())));
                s.push_str(&format!(" uses[{}]", f.effects.join(",")));
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
                s.push_str(&format!(" uses[{}]", sig.effects.join(",")));
            }
            let mut laws: Vec<&crate::ast::Law> = ct.laws.iter().collect();
            laws.sort_by_key(|law| &law.name);
            for law in laws {
                s.push_str(&format!(" law:{}", law.name));
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
        // in-memory variant of semantic_diff for tests
        let (pa, da) = crate::parser::parse(old);
        let (pb, db) = crate::parser::parse(new);
        assert!(da.is_empty() && db.is_empty());
        let items = |p: crate::ast::Program| {
            let mut m = std::collections::BTreeMap::new();
            for item in p.items {
                m.insert(super::item_name(&item).to_string(), item);
            }
            m
        };
        let (a, b) = (items(pa), items(pb));
        let mut lines = Vec::new();
        let mut changed = false;
        for (name, old) in &a {
            match b.get(name) {
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
        for name in b.keys() {
            if !a.contains_key(name) {
                changed = true;
                lines.push(format!("added {name}"));
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
