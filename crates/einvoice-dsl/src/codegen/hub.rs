//! Hub (`MainKey`) generation.
//!
//! Turns the derived [`CanonicalModel`] into the typed `MainKey` struct (one
//! field per Root-scope canonical key) plus one item struct per canonical
//! collection, at any nesting depth.

use std::fmt::Write as _;

use crate::hub::{CanonicalField, CanonicalModel, CanonicalScope};

use super::naming::{canonical_rust_type, field_name, item_struct_name};

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
        out.push_str("}\n");
    }
    out
}

/// Emits one `MainKey`/item-struct field declaration into `out`: `Vec<…Item>` for
/// a (possibly nested) canonical collection, `Option<…>` for a scalar.
fn hub_field_decl(out: &mut String, f: &CanonicalField) {
    if f.is_collection {
        let _ = writeln!(
            out,
            "    pub {}: Vec<{}>,",
            field_name(&f.key),
            item_struct_name(&f.key)
        );
    } else {
        let _ = writeln!(
            out,
            "    pub {}: Option<{}>,",
            field_name(&f.key),
            canonical_rust_type(f.ty)
        );
    }
}
