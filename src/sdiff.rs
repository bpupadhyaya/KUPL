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
    }
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::Fun(_) => "fun      ",
        Item::Type(_) => "type     ",
        Item::Component(c) if c.is_app => "app      ",
        Item::Component(_) => "component",
        Item::Contract(_) => "contract ",
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
            s.push_str(&format!("fun {}[{}] pub={}", f.name, f.type_params.join(","), f.is_pub));
            for p in &f.params {
                s.push_str(&format!(" {}:{}", p.name, ty_str(&p.ty)));
            }
            if let Some(r) = &f.ret {
                s.push_str(&format!(" -> {}", ty_str(r)));
            }
            s.push_str(&format!(" uses[{}]", f.effects.join(",")));
        }
        Item::Type(t) => {
            for v in &t.variants {
                s.push_str(&format!("{}(", v.name));
                for fld in &v.fields {
                    s.push_str(&format!("{}:{},", fld.name, ty_str(&fld.ty)));
                }
                s.push(')');
            }
        }
        Item::Component(c) => {
            s.push_str(&format!("app={} fulfills[{}]", c.is_app, c.fulfills.join(",")));
            for p in &c.ports {
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
            for f in &c.exposes {
                s.push_str(&format!(" expose {}(", f.name));
                for p in &f.params {
                    s.push_str(&format!("{}:{},", p.name, ty_str(&p.ty)));
                }
                s.push_str(&format!(")->{}", f.ret.as_ref().map(ty_str).unwrap_or_else(|| "Unit".into())));
                s.push_str(&format!(" uses[{}]", f.effects.join(",")));
            }
        }
        Item::Contract(ct) => {
            for sig in &ct.sigs {
                s.push_str(&format!(" {}(", sig.name));
                for p in &sig.params {
                    s.push_str(&format!("{}:{},", p.name, ty_str(&p.ty)));
                }
                s.push_str(&format!(")->{}", sig.ret.as_ref().map(ty_str).unwrap_or_else(|| "Unit".into())));
            }
            for law in &ct.laws {
                s.push_str(&format!(" law:{}", law.name));
            }
        }
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
    fn component_port_change_is_interface_state_change_is_impl() {
        let old = "component C {\n intent \"x\"\n in a: Int\n state n: Int = 0\n on a(v) { n += v }\n}\n";
        let impl_change = "component C {\n intent \"x\"\n in a: Int\n state n: Int = 100\n on a(v) { n += v }\n}\n";
        let iface_change = "component C {\n intent \"x\"\n in a: Str\n state n: Int = 0\n on a(v) { n += 1 }\n}\n";
        assert_eq!(diff_lines(old, impl_change).0, vec!["impl C"]);
        assert_eq!(diff_lines(old, iface_change).0, vec!["interface C"]);
    }

    #[test]
    fn add_remove_detected() {
        let (lines, _) = diff_lines("fun a() {\n    1\n}\n", "fun b() {\n    1\n}\n");
        assert_eq!(lines, vec!["removed a", "added b"]);
    }
}
