//! Hub (`MainKey`) generation.
//!
//! Turns the derived [`CanonicalModel`] into the typed `MainKey` struct (one
//! field per Root-scope canonical key) plus one item struct per canonical
//! collection, at any nesting depth. Every struct also gets a value walker,
//! `MainKey::values`, that lists the populated canonical values by their
//! scope-qualified label — the generic view schema-conformance round trips
//! compare two hubs by.
//!
//! The engine transforms through `CompactHub`, `MainKey`'s fields with each
//! root collection packed (`record::Packed`): an item struct is decoded from
//! its record when it is read and encoded when it is pushed, each field under
//! its position in key order (`record::Pack`). The mappers and the
//! derivation rules run on `CompactHub`; `MainKey` converts to and from it.

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
    out.push_str("use einvoice_transformator::record::{Pack, Packed, Rec, Slot as _, Values};\n");
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
    derivations_const(&mut out, derivations);
    out.push_str(
        "    /// Fills every key a derivation rule computes and the hub lacks, rule by\n\
         \x20\x20\x20\x20/// rule; a value the hub carries is never replaced. Returns the\n\
         \x20\x20\x20\x20/// `(label, rule)` of each value derived.\n\
         \x20\x20\x20\x20pub fn derive_missing(&mut self) -> Vec<(&'static str, &'static str)> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let mut hub = CompactHub::from(std::mem::take(self));\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let derived = hub.derive_missing();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20*self = hub.into();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20derived\n\
         \x20\x20\x20\x20}\n\n\
         \x20\x20\x20\x20/// Recomputes every checked rule whose target the hub carries and\n\
         \x20\x20\x20\x20/// returns each mismatch as `(label, rule, is_error, carried, computed)`;\n\
         \x20\x20\x20\x20/// `computed` is `None` when the computation overflows the decimal range.\n\
         \x20\x20\x20\x20/// Run after [`Self::derive_missing`], so derived operands take part.\n\
         \x20\x20\x20\x20pub fn check_derived(\n\
         \x20\x20\x20\x20\x20\x20\x20\x20&self,\n\
         \x20\x20\x20\x20) -> Vec<(&'static str, &'static str, bool, Decimal, Option<Decimal>)> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20CompactHub::from(self.clone()).check_derived()\n\
         \x20\x20\x20\x20}\n",
    );
    out.push_str("}\n\n");

    // CompactHub: MainKey's fields, root collections packed.
    out.push_str(
        "/// The hub as the engine transforms through it: [`MainKey`]'s fields, each\n\
         /// root collection packed, so an item takes about its values' size.\n",
    );
    out.push_str("#[derive(Debug, Default)]\n");
    out.push_str("pub struct CompactHub {\n");
    let root: Vec<&CanonicalField> = hub
        .fields
        .values()
        .filter(|f| f.scope == CanonicalScope::Root)
        .collect();
    for f in &root {
        if f.is_collection {
            let _ = writeln!(
                out,
                "    pub {}: Packed<{}>,",
                snake_case(&f.key),
                item_struct_name(&f.key)
            );
        } else {
            hub_field_decl(&mut out, f);
        }
    }
    out.push_str("}\n\n");
    for (from, to) in [("MainKey", "CompactHub"), ("CompactHub", "MainKey")] {
        let _ = writeln!(
            out,
            "impl From<{from}> for {to} {{\n    fn from(hub: {from}) -> Self {{\n        {to} {{"
        );
        for f in &root {
            let field = snake_case(&f.key);
            if f.is_collection {
                let _ = writeln!(
                    out,
                    "            {field}: hub.{field}.into_iter().collect(),"
                );
            } else {
                let _ = writeln!(out, "            {field}: hub.{field},");
            }
        }
        out.push_str("        }\n    }\n}\n\n");
    }
    out.push_str("impl CompactHub {\n");
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
        out.push_str("}\n\n");
        pack_impl(&mut out, hub, &inner, &item_struct_name(&coll.key));
    }
    out
}

/// Emits `impl Pack` for the item struct `name` holding `scope`'s fields: each
/// field under its position among them.
fn pack_impl(out: &mut String, hub: &CanonicalModel, scope: &CanonicalScope, name: &str) {
    let fields: Vec<String> = hub
        .fields
        .values()
        .filter(|f| &f.scope == scope)
        .map(|f| snake_case(&f.key))
        .collect();
    let _ = writeln!(out, "impl Pack for {name} {{");
    out.push_str("    fn pack(&self, out: &mut Values) {\n");
    for (id, field) in fields.iter().enumerate() {
        let _ = writeln!(out, "        self.{field}.put({id}, out);");
    }
    out.push_str("    }\n\n");
    out.push_str("    fn unpack(rec: Rec<'_>) -> Self {\n");
    out.push_str("        let mut item = Self::default();\n");
    out.push_str("        for (id, payload) in rec.entries() {\n            match id {\n");
    for (id, field) in fields.iter().enumerate() {
        let _ = writeln!(out, "                {id} => item.{field}.read(payload),");
    }
    out.push_str("                _ => {}\n            }\n        }\n        item\n    }\n}\n");
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

/// Emits `MainKey::DERIVATIONS`: each rule's target label, reference and the
/// labels it needs.
fn derivations_const(out: &mut String, derivations: &[Derivation]) {
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
}

/// Emits `CompactHub::derive_missing`, which applies the rules in order to
/// the keys the hub lacks and returns `(label, rule)` per value it derived,
/// and `CompactHub::check_derived` (see [`check_fn`]). A rule that fills a
/// root collection's items runs as `Packed::update`'s closure, so a filter
/// skips an item with `return`.
fn derive_fns(out: &mut String, hub: &CanonicalModel, derivations: &[Derivation]) {
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
                emit_checked_sum(out, hub, &scope, collection, item, filter);
                // An overflowing sum derives nothing: the target stays
                // absent, as for a source that lacks it.
                let _ = writeln!(
                    out,
                    "            if !overflow && let Some(value) = sum {{\n                self.{target} = Some(value);\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Arithmetic {
                add,
                subtract,
                requires,
                skip_zero,
                skip_negative,
            } => {
                let target = snake_case(&d.key);
                let mut present = vec![format!("self.{target}.is_none()")];
                for r in add.iter().take(1).chain(requires) {
                    present.push(format!("self.{}.is_some()", snake_case(r)));
                }
                let _ = writeln!(out, "        if {} {{", present.join(" && "));
                // An overflowing result (`None`) derives nothing.
                let _ = writeln!(
                    out,
                    "            if let Some(value) = {} {{",
                    checked_expr(add, subtract)
                );
                let mut guards = Vec::new();
                if *skip_zero {
                    guards.push("!value.is_zero()");
                }
                if *skip_negative {
                    guards.push("!value.is_sign_negative()");
                }
                let guard = if guards.is_empty() {
                    "true".to_string()
                } else {
                    guards.join(" && ")
                };
                let _ = writeln!(
                    out,
                    "                if {guard} {{\n                    self.{target} = Some(value);\n                    {push}\n                }}"
                );
                out.push_str("            }\n");
                out.push_str("        }\n");
            }
            DerivationKind::Value {
                value,
                filter,
                unless,
            } => {
                let (collections, key) = d.target_path();
                let wanted = wanted_items(hub, &collections, |scope, r| {
                    let mut tests: Vec<String> = filter
                        .iter()
                        .map(|(k, literal)| rec_test(hub, scope, k, literal, r, "=="))
                        .collect();
                    for k in std::iter::once(key).chain(unless.iter().map(String::as_str)) {
                        tests.push(format!("{r}.get({}).is_none()", field_id(hub, scope, k)));
                    }
                    tests.join(" && ")
                });
                let mut scope = CanonicalScope::Root;
                let mut var = "self".to_string();
                let mut pad = "        ".to_string();
                out.push_str("        let mut filled = false;\n");
                for (depth, coll) in collections.iter().enumerate() {
                    open_item_loop(out, depth, coll, &wanted, &mut var, &mut pad);
                    scope = scope.child(coll);
                }
                let skip = if collections.len() == 1 {
                    "return"
                } else {
                    "continue"
                };
                emit_filter(out, hub, &scope, filter, &var, &pad, skip);
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
                close_item_loops(out, collections.len(), &mut pad);
                let _ = writeln!(out, "        if filled {{\n            {push}\n        }}");
            }
            DerivationKind::Copy { from, negate_where } => {
                let (collections, key) = d.target_path();
                let wanted = wanted_items(hub, &collections, |scope, r| {
                    format!(
                        "{r}.get({}).is_none() && {r}.get({}).is_some()",
                        field_id(hub, scope, key),
                        field_id(hub, scope, from)
                    )
                });
                let mut scope = CanonicalScope::Root;
                let mut var = "self".to_string();
                let mut pad = "        ".to_string();
                out.push_str("        let mut filled = false;\n");
                for (depth, coll) in collections.iter().enumerate() {
                    open_item_loop(out, depth, coll, &wanted, &mut var, &mut pad);
                    scope = scope.child(coll);
                }
                // `-0` would be written `-0.00`: a zero is never negated.
                let target = snake_case(key);
                let _ = writeln!(
                    out,
                    "{pad}if {var}.{target}.is_none() && let Some(value) = {var}.{} {{",
                    snake_case(from)
                );
                let _ = writeln!(
                    out,
                    "{pad}    let negate = {};",
                    filter_expr(hub, &scope, negate_where, &var)
                );
                let _ = writeln!(
                    out,
                    "{pad}    {var}.{target} = Some(if negate && !value.is_zero() {{ -value }} else {{ value }});\n{pad}    filled = true;\n{pad}}}"
                );
                close_item_loops(out, collections.len(), &mut pad);
                let _ = writeln!(out, "        if filled {{\n            {push}\n        }}");
            }
            DerivationKind::Number { collection, item } => {
                // Whole numbers only: a value that is not one (a UBL line id
                // `A-1`) neither takes part nor stops the numbering.
                let (collections, key) = d.target_path();
                let target_coll = snake_case(collections[0]);
                let target = snake_case(key);
                let target_scope = CanonicalScope::Root.child(collections[0]);
                let target_id = field_id(hub, &target_scope, key);
                out.push_str("        {\n            let mut next: u64 = 1;\n");
                for (coll, id) in [
                    (
                        snake_case(collection),
                        field_id(hub, &CanonicalScope::Root.child(collection), item),
                    ),
                    (target_coll.clone(), target_id),
                ] {
                    let _ = writeln!(
                        out,
                        "            for r0 in self.{coll}.records() {{\n                if let Some(n) = r0.get({id}).and_then(|v| v.trim().parse::<u64>().ok()) {{\n                    next = next.max(n.saturating_add(1));\n                }}\n            }}"
                    );
                }
                let _ = writeln!(
                    out,
                    "            let mut filled = false;\n            self.{target_coll}.update(|r0| r0.get({target_id}).is_none(), |i0| {{\n                if i0.{target}.is_none() {{\n                    i0.{target} = Some(CompactString::from(next.to_string()));\n                    next = next.saturating_add(1);\n                    filled = true;\n                }}\n            }});\n            if filled {{\n                {push}\n            }}\n        }}"
                );
            }
        }
    }
    out.push_str("        derived\n    }\n");
    check_fn(out, hub, derivations);
}

/// Emits `MainKey::check_derived`: every checked `sum` / `add` rule whose
/// target the hub carries is recomputed from its operands (under the same
/// presence conditions as deriving it), and each mismatch is returned as
/// `(label, rule, is_error, carried, computed)`, `computed` being `None` when
/// the computation overflows the decimal range.
fn check_fn(out: &mut String, hub: &CanonicalModel, derivations: &[Derivation]) {
    out.push_str(
        "\n    /// Recomputes every checked rule whose target the hub carries and\n\
         \x20\x20\x20\x20/// returns each mismatch as `(label, rule, is_error, carried, computed)`;\n\
         \x20\x20\x20\x20/// `computed` is `None` when the computation overflows the decimal range.\n\
         \x20\x20\x20\x20/// Run after [`Self::derive_missing`], so derived operands take part.\n",
    );
    out.push_str(
        "    pub fn check_derived(\n        &self,\n    ) -> Vec<(&'static str, &'static str, bool, Decimal, Option<Decimal>)> {\n",
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
                emit_checked_sum(out, hub, &scope, collection, item, filter);
                let _ = writeln!(
                    out,
                    "            if overflow {{\n                let computed = None;\n                {push}\n            }} else if let Some(computed) = sum && computed != carried {{\n                let computed = Some(computed);\n                {push}\n            }}"
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
                let _ = writeln!(
                    out,
                    "            let computed: Option<Decimal> = {};",
                    checked_expr(add, subtract)
                );
                let _ = writeln!(
                    out,
                    "            if computed != Some(carried) {{\n                {push}\n            }}"
                );
                out.push_str("        }\n");
            }
            DerivationKind::Value { .. }
            | DerivationKind::Copy { .. }
            | DerivationKind::Number { .. } => {}
        }
    }
    out.push_str("        mismatches\n    }\n");
}

/// Emits the summation of `collection`'s `item` values (filtered by `where`)
/// into `sum: Option<Decimal>` (`None` with no contributing item), with
/// checked addition: `overflow` is set, and the loop left, when the sum
/// leaves the decimal range. Decimal `+` would panic there instead, on
/// values an untrusted document controls.
fn emit_checked_sum(
    out: &mut String,
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    collection: &str,
    item: &str,
    filter: &[(String, String)],
) {
    out.push_str("            let mut sum: Option<Decimal> = None;\n");
    out.push_str("            let mut overflow = false;\n");
    let _ = writeln!(
        out,
        "            for item in self.{}.records() {{",
        snake_case(collection)
    );
    for (key, literal) in filter {
        let _ = writeln!(
            out,
            "                if {} {{\n                    continue;\n                }}",
            rec_test(hub, scope, key, literal, "item", "!=")
        );
    }
    let _ = writeln!(
        out,
        "                if let Some(value) = item.field::<Decimal>({}) {{\n                    match sum.unwrap_or_default().checked_add(value) {{\n                        Some(next) => sum = Some(next),\n                        None => {{\n                            overflow = true;\n                            break;\n                        }}\n                    }}\n                }}",
        field_id(hub, scope, item)
    );
    out.push_str("            }\n");
}

/// The id a packed item of `scope` keeps `key` under: its position among
/// the scope's fields, as `pack_impl` numbers them. A key the scope lacks
/// (a rule `check_derivations` refuses, E110) gets an id no field has.
fn field_id(hub: &CanonicalModel, scope: &CanonicalScope, key: &str) -> u32 {
    hub.fields
        .values()
        .filter(|f| &f.scope == scope)
        .position(|f| f.key == key)
        .map_or(u32::MAX, |id| id as u32)
}

/// The test that the packed item `r` of `scope` holds `literal` under `key`
/// (`op` `==`), or does not (`!=`): a filter of a rule's `where`.
fn rec_test(
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    key: &str,
    literal: &str,
    r: &str,
    op: &str,
) -> String {
    let id = field_id(hub, scope, key);
    match hub.get(scope, key).map(|f| f.ty) {
        Some(MappingType::Boolean) => format!("{r}.field::<bool>({id}) {op} Some({literal})"),
        _ => format!("{r}.get({id}) {op} Some({literal:?})"),
    }
}

/// The `Packed::update` filter of a rule filling the items at the end of
/// `collections` (from a root collection down): the root item `r0` holds,
/// at that depth, an item `guard` holds for (`guard(scope, var)` tests the
/// packed item `var` of `scope`). Exactly the items the rule changes pass.
fn wanted_items(
    hub: &CanonicalModel,
    collections: &[&str],
    guard: impl Fn(&CanonicalScope, &str) -> String,
) -> String {
    let mut scopes = Vec::new();
    let mut scope = CanonicalScope::Root;
    for coll in collections {
        scope = scope.child(coll);
        scopes.push(scope.clone());
    }
    let Some(last) = collections.len().checked_sub(1) else {
        // A root target (refused, E110) has no items to filter.
        return "|_| true".to_string();
    };
    let mut test = guard(&scopes[last], &format!("r{last}"));
    for depth in (1..=last).rev() {
        let id = field_id(hub, &scopes[depth - 1], collections[depth]);
        test = format!("r{}.items({id}).any(|r{depth}| {test})", depth - 1);
    }
    format!("|r0| {test}")
}

/// Opens the loop over the items of `var`'s collection `coll` at `depth`,
/// making `var` the item and indenting `pad`: a root collection's items are
/// packed and change through `Packed::update`, which decodes only the items
/// `wanted` ([`wanted_items`]) accepts; a nested one's are a `Vec`.
fn open_item_loop(
    out: &mut String,
    depth: usize,
    coll: &str,
    wanted: &str,
    var: &mut String,
    pad: &mut String,
) {
    let item = format!("i{depth}");
    let field = snake_case(coll);
    if depth == 0 {
        let _ = writeln!(out, "{pad}{var}.{field}.update({wanted}, |{item}| {{");
    } else {
        let _ = writeln!(out, "{pad}for {item} in &mut {var}.{field} {{");
    }
    *var = item;
    pad.push_str("    ");
}

/// Closes `depth` loops [`open_item_loop`] opened.
fn close_item_loops(out: &mut String, depth: usize, pad: &mut String) {
    for level in (0..depth).rev() {
        pad.truncate(pad.len() - 4);
        let close = if level == 0 { "});" } else { "}" };
        let _ = writeln!(out, "{pad}{close}");
    }
}

/// An `Option<Decimal>` expression for `add[0] + add[1..] - subtract[..]`
/// over root keys (absent ones count as zero), with checked arithmetic:
/// `None` when an intermediate result leaves the decimal range.
fn checked_expr(add: &[String], subtract: &[String]) -> String {
    let mut expr = format!("Some(self.{}.unwrap_or_default())", snake_case(&add[0]));
    for a in &add[1..] {
        expr.push_str(&format!(
            ".and_then(|v: Decimal| v.checked_add(self.{}.unwrap_or_default()))",
            snake_case(a)
        ));
    }
    for s in subtract {
        expr.push_str(&format!(
            ".and_then(|v: Decimal| v.checked_sub(self.{}.unwrap_or_default()))",
            snake_case(s)
        ));
    }
    expr
}

/// Emits the guards of a rule's `where` filters on the item `var` of
/// `scope`, leaving the item by `skip` (`continue`, or `return` from an
/// update closure).
fn emit_filter(
    out: &mut String,
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    filter: &[(String, String)],
    var: &str,
    pad: &str,
    skip: &str,
) {
    for (key, literal) in filter {
        let field = snake_case(key);
        let test = match hub.get(scope, key).map(|f| f.ty) {
            Some(MappingType::Boolean) => format!("{var}.{field} != Some({literal})"),
            _ => format!("{var}.{field}.as_deref() != Some({literal:?})"),
        };
        let _ = writeln!(out, "{pad}if {test} {{\n{pad}    {skip};\n{pad}}}");
    }
}

/// The condition that every one of `filter`'s equalities holds on the item
/// `var` of `scope` (`false` for no equalities: nothing is negated).
fn filter_expr(
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    filter: &[(String, String)],
    var: &str,
) -> String {
    if filter.is_empty() {
        return "false".to_string();
    }
    filter
        .iter()
        .map(|(key, literal)| {
            let field = snake_case(key);
            match hub.get(scope, key).map(|f| f.ty) {
                Some(MappingType::Boolean) => format!("{var}.{field} == Some({literal})"),
                _ => format!("{var}.{field}.as_deref() == Some({literal:?})"),
            }
        })
        .collect::<Vec<_>>()
        .join(" && ")
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
