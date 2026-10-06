//! Hub (`MainKey`) generation.
//!
//! Turns the derived [`CanonicalModel`] into the typed `MainKey` struct (one
//! field per Root-scope canonical key) plus one item struct per canonical
//! collection, at any nesting depth. Every struct also gets a value walker,
//! `MainKey::values`, that lists the populated canonical values by their
//! scope-qualified label — the generic view schema-conformance round trips
//! compare two hubs by.

use std::fmt::Write as _;

use crate::derive::{CheckLevel, Derivation, DerivationKind};
use crate::hub::{CanonicalField, CanonicalModel, CanonicalScope};
use crate::report::FieldKey;
use crate::types::MappingType;

use super::naming::{canonical_rust_type, item_struct_name, snake_case};

/// Generates the typed canonical hub module: the `MainKey` struct plus an item
/// struct per canonical collection. Returns a self-contained module string.
/// The hub derives nothing; see [`generate_hub_with`].
pub fn generate_hub(hub: &CanonicalModel) -> String {
    generate_hub_with(hub, &[])
}

/// [`generate_hub`] with the derivation rules `MainKey::derive_missing`
/// applies (checked against `hub` by `check_derivations` beforehand).
pub fn generate_hub_with(hub: &CanonicalModel, derivations: &[Derivation]) -> String {
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
    out.push('\n');
    derive_fns(&mut out, hub, derivations);
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

/// Emits `MainKey::DERIVATIONS` (each rule's target label, reference and the
/// labels it needs), `MainKey::derive_missing`, which applies the rules in
/// order to the keys the hub lacks and returns `(label, rule)` per value it
/// derived, and `MainKey::check_derived` (see [`check_fn`]).
fn derive_fns(out: &mut String, hub: &CanonicalModel, derivations: &[Derivation]) {
    out.push_str(
        "    /// The derivation rules (`config/derivations.toml`), in order: target\n\
         \x20\x20\x20\x20/// label, rule reference, and the labels the rule cannot derive without.\n",
    );
    out.push_str(
        "    pub const DERIVATIONS: &'static [(&'static str, &'static str, &'static [&'static str])] = &[\n",
    );
    for d in derivations {
        let needs: Vec<String> = d.needs().iter().map(|n| format!("{n:?}")).collect();
        let _ = writeln!(
            out,
            "        ({:?}, {:?}, &[{}]),",
            d.key,
            d.rule,
            needs.join(", ")
        );
    }
    out.push_str("    ];\n\n");
    out.push_str(
        "    /// Fills every key a derivation rule computes and the hub lacks, rule by\n\
         \x20\x20\x20\x20/// rule; a value the hub carries is never replaced. Returns the\n\
         \x20\x20\x20\x20/// `(label, rule)` of each value derived.\n",
    );
    out.push_str("    pub fn derive_missing(&mut self) -> Vec<(&'static str, &'static str)> {\n");
    out.push_str("        let mut derived = Vec::new();\n");
    for d in derivations {
        let _ = writeln!(out, "        // {} ({})", d.key, d.rule);
        let push = format!("derived.push(({:?}, {:?}));", d.key, d.rule);
        match &d.kind {
            DerivationKind::Sum {
                collection,
                item,
                filter,
            } => {
                let target = snake_case(&d.key);
                let scope = CanonicalScope::Root.child(collection);
                let _ = writeln!(out, "        if self.{target}.is_none() {{");
                out.push_str("            let mut sum: Option<Decimal> = None;\n");
                let _ = writeln!(
                    out,
                    "            for item in &self.{} {{",
                    snake_case(collection)
                );
                emit_filter(out, hub, &scope, filter, "item", "                ");
                let _ = writeln!(
                    out,
                    "                if let Some(value) = item.{} {{\n                    sum = Some(sum.unwrap_or_default() + value);\n                }}",
                    snake_case(item)
                );
                out.push_str("            }\n");
                let _ = writeln!(
                    out,
                    "            if let Some(value) = sum {{\n                self.{target} = Some(value);\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Arithmetic {
                add,
                subtract,
                requires,
                skip_zero,
            } => {
                let target = snake_case(&d.key);
                let mut present = vec![format!("self.{target}.is_none()")];
                for r in add.iter().take(1).chain(requires) {
                    present.push(format!("self.{}.is_some()", snake_case(r)));
                }
                let _ = writeln!(out, "        if {} {{", present.join(" && "));
                let mut expr = format!("self.{}.unwrap_or_default()", snake_case(&add[0]));
                for a in &add[1..] {
                    expr.push_str(&format!(" + self.{}.unwrap_or_default()", snake_case(a)));
                }
                for s in subtract {
                    expr.push_str(&format!(" - self.{}.unwrap_or_default()", snake_case(s)));
                }
                let _ = writeln!(out, "            let value: Decimal = {expr};");
                let guard = if *skip_zero {
                    "!value.is_zero()"
                } else {
                    "true"
                };
                let _ = writeln!(
                    out,
                    "            if {guard} {{\n                self.{target} = Some(value);\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Value {
                value,
                filter,
                unless,
            } => {
                let (collections, key) = d.target_path();
                let mut scope = CanonicalScope::Root;
                let mut var = "self".to_string();
                let mut pad = "        ".to_string();
                out.push_str("        let mut filled = false;\n");
                for (depth, coll) in collections.iter().enumerate() {
                    let item = format!("i{depth}");
                    let _ = writeln!(out, "{pad}for {item} in &mut {var}.{} {{", snake_case(coll));
                    scope = scope.child(coll);
                    var = item;
                    pad.push_str("    ");
                }
                emit_filter(out, hub, &scope, filter, &var, &pad);
                let ty = hub.get(&scope, key).map_or(MappingType::String, |f| f.ty);
                let mut absent = vec![format!("{var}.{}.is_none()", snake_case(key))];
                absent.extend(
                    unless
                        .iter()
                        .map(|u| format!("{var}.{}.is_none()", snake_case(u))),
                );
                let _ = writeln!(
                    out,
                    "{pad}if {} {{\n{pad}    {var}.{} = Some({});\n{pad}    filled = true;\n{pad}}}",
                    absent.join(" && "),
                    snake_case(key),
                    hub_literal(ty, value)
                );
                for _ in &collections {
                    pad.truncate(pad.len() - 4);
                    let _ = writeln!(out, "{pad}}}");
                }
                let _ = writeln!(out, "        if filled {{\n            {push}\n        }}");
            }
        }
    }
    out.push_str("        derived\n    }\n");
    check_fn(out, hub, derivations);
}

/// Emits `MainKey::check_derived`: every checked `sum` / `add` rule whose
/// target the hub carries is recomputed from its operands (under the same
/// presence conditions as deriving it), and each mismatch is returned as
/// `(label, rule, is_error, carried, computed)`.
fn check_fn(out: &mut String, hub: &CanonicalModel, derivations: &[Derivation]) {
    out.push_str(
        "\n    /// Recomputes every checked rule whose target the hub carries and\n\
         \x20\x20\x20\x20/// returns each mismatch as `(label, rule, is_error, carried, computed)`.\n\
         \x20\x20\x20\x20/// Run after [`Self::derive_missing`], so derived operands take part.\n",
    );
    out.push_str(
        "    pub fn check_derived(&self) -> Vec<(&'static str, &'static str, bool, Decimal, Decimal)> {\n",
    );
    out.push_str("        let mut mismatches = Vec::new();\n");
    for d in derivations {
        let is_error = match d.check {
            CheckLevel::Off => continue,
            CheckLevel::Warning => false,
            CheckLevel::Error => true,
        };
        let target = snake_case(&d.key);
        let push = format!(
            "mismatches.push(({:?}, {:?}, {is_error}, carried, computed));",
            d.key, d.rule
        );
        match &d.kind {
            DerivationKind::Sum {
                collection,
                item,
                filter,
            } => {
                let scope = CanonicalScope::Root.child(collection);
                let _ = writeln!(out, "        // {} ({})", d.key, d.rule);
                let _ = writeln!(out, "        if let Some(carried) = self.{target} {{");
                out.push_str("            let mut sum: Option<Decimal> = None;\n");
                let _ = writeln!(
                    out,
                    "            for item in &self.{} {{",
                    snake_case(collection)
                );
                emit_filter(out, hub, &scope, filter, "item", "                ");
                let _ = writeln!(
                    out,
                    "                if let Some(value) = item.{} {{\n                    sum = Some(sum.unwrap_or_default() + value);\n                }}",
                    snake_case(item)
                );
                out.push_str("            }\n");
                let _ = writeln!(
                    out,
                    "            if let Some(computed) = sum && computed != carried {{\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Arithmetic {
                add,
                subtract,
                requires,
                ..
            } => {
                let _ = writeln!(out, "        // {} ({})", d.key, d.rule);
                let mut present = Vec::new();
                for r in add.iter().take(1).chain(requires) {
                    present.push(format!("self.{}.is_some()", snake_case(r)));
                }
                let _ = writeln!(
                    out,
                    "        if let Some(carried) = self.{target} && {} {{",
                    present.join(" && ")
                );
                let mut expr = format!("self.{}.unwrap_or_default()", snake_case(&add[0]));
                for a in &add[1..] {
                    expr.push_str(&format!(" + self.{}.unwrap_or_default()", snake_case(a)));
                }
                for s in subtract {
                    expr.push_str(&format!(" - self.{}.unwrap_or_default()", snake_case(s)));
                }
                let _ = writeln!(out, "            let computed: Decimal = {expr};");
                let _ = writeln!(
                    out,
                    "            if computed != carried {{\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Value { .. } => {}
        }
    }
    out.push_str("        mismatches\n    }\n");
}

/// Emits the `continue` guards of a rule's `where` filters on the item `var`
/// of `scope`.
fn emit_filter(
    out: &mut String,
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    filter: &[(String, String)],
    var: &str,
    pad: &str,
) {
    for (key, literal) in filter {
        let field = snake_case(key);
        let test = match hub.get(scope, key).map(|f| f.ty) {
            Some(MappingType::Boolean) => format!("{var}.{field} != Some({literal})"),
            _ => format!("{var}.{field}.as_deref() != Some({literal:?})"),
        };
        let _ = writeln!(out, "{pad}if {test} {{\n{pad}    continue;\n{pad}}}");
    }
}

/// The Rust expression of a canonical literal of type `ty` in the hub.
fn hub_literal(ty: MappingType, value: &str) -> String {
    match ty {
        MappingType::Decimal => format!(
            "<Decimal as std::str::FromStr>::from_str({value:?}).expect(\"checked at build time (E114)\")"
        ),
        MappingType::Boolean => (value == "true").to_string(),
        _ => format!("CompactString::from({value:?})"),
    }
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
