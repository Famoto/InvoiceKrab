//! Hub (`MainKey`) generation.
//!
//! Turns the derived [`CanonicalModel`] into the typed `MainKey` struct (one
//! field per Root-scope canonical key) plus one item struct per canonical
//! collection, at any nesting depth. Every struct also gets a value walker,
//! `MainKey::values`, that lists the populated canonical values by their
//! scope-qualified label — the generic view schema-conformance round trips
//! compare two hubs by.

use std::fmt::Write as _;

use crate::hub::{CanonicalField, CanonicalModel, CanonicalScope};
use crate::report::FieldKey;

use super::naming::{canonical_rust_type, item_struct_name, snake_case};

/// Generates the typed canonical hub module: the `MainKey` struct plus an item
/// struct per canonical collection. Returns a self-contained module string.
pub fn generate_hub(hub: &CanonicalModel) -> String {
    let mut out = String::new();
    out.push_str("// Generated canonical hub. Do not edit by hand.\n\n");
    out.push_str("use compact_str::CompactString;\n");
    out.push_str("use rust_decimal::Decimal;\n\n");

    // MainKey: every Root-scope canonical field.
    out.push_str("/// The canonical invoice hub (union of every spoke's canonical keys).\n");
    out.push_str("#[derive(Debug, Clone, Default, PartialEq)]\n");
    out.push_str("pub struct MainKey {\n");
    for f in hub.fields.values() {
        if f.scope == CanonicalScope::Root {
            hub_field_decl(&mut out, f);
        }
    }
    out.push_str("}\n\n");

    out.push_str("impl MainKey {\n");
    out.push_str(
        "    /// Every populated canonical value as `(label, value)`, labels\n\
         \x20\x20\x20\x20/// scope-qualified (`InvoiceNumber`, `InvoiceLines/LineId`). Root\n\
         \x20\x20\x20\x20/// fields come in key order; a collection contributes its items' values,\n\
         \x20\x20\x20\x20/// item by item in order, nested collections depth-first. Absent values\n\
         \x20\x20\x20\x20/// and empty collections contribute nothing.\n",
    );
    out.push_str("    pub fn values(&self) -> Vec<(&'static str, String)> {\n");
    out.push_str("        let mut out = Vec::new();\n");
    out.push_str("        self.push_values(&mut out);\n");
    out.push_str("        out\n");
    out.push_str("    }\n\n");
    push_values_fn(&mut out, hub, &CanonicalScope::Root);
    out.push_str("}\n");

    // One item struct per canonical collection, at any nesting depth, holding the
    // fields (scalars and further-nested collections) of its item scope.
    for coll in hub.fields.values() {
        if !coll.is_collection {
            continue;
        }
        let inner = coll.scope.child(&coll.key);
        out.push('\n');
        let _ = writeln!(
            out,
            "/// One item of the `{}` canonical collection.",
            coll.key
        );
        out.push_str("#[derive(Debug, Clone, Default, PartialEq)]\n");
        let _ = writeln!(out, "pub struct {} {{", item_struct_name(&coll.key));
        for f in hub.fields.values() {
            if f.scope == inner {
                hub_field_decl(&mut out, f);
            }
        }
        out.push_str("}\n\n");
        let _ = writeln!(out, "impl {} {{", item_struct_name(&coll.key));
        push_values_fn(&mut out, hub, &inner);
        out.push_str("}\n");
    }
    out
}

/// Emits the `push_values` walker of the struct holding `scope`'s fields: each
/// populated scalar as `(label, value)`, each collection's items in turn.
fn push_values_fn(out: &mut String, hub: &CanonicalModel, scope: &CanonicalScope) {
    out.push_str("    fn push_values(&self, out: &mut Vec<(&'static str, String)>) {\n");
    for f in hub.fields.values().filter(|f| &f.scope == scope) {
        let field = snake_case(&f.key);
        if f.is_collection {
            let _ = writeln!(
                out,
                "        for item in &self.{field} {{\n            item.push_values(out);\n        }}"
            );
        } else {
            let label = FieldKey {
                scope: f.scope.clone(),
                key: f.key.clone(),
            }
            .label();
            let _ = writeln!(
                out,
                "        if let Some(value) = &self.{field} {{\n            out.push(({label:?}, value.to_string()));\n        }}"
            );
        }
    }
    out.push_str("    }\n");
}

/// Emits one `MainKey`/item-struct field declaration into `out`: `Vec<…Item>` for
/// a (possibly nested) canonical collection, `Option<…>` for a scalar.
fn hub_field_decl(out: &mut String, f: &CanonicalField) {
    if f.is_collection {
        let _ = writeln!(
            out,
            "    pub {}: Vec<{}>,",
            snake_case(&f.key),
            item_struct_name(&f.key)
        );
    } else {
        let _ = writeln!(
            out,
            "    pub {}: Option<{}>,",
            snake_case(&f.key),
            canonical_rust_type(f.ty)
        );
    }
}
