//! Source-record generation and XML I/O.
//!
//! A source document lives in memory as one packed record
//! ([`einvoice_transformator::record::Values`]): its values under the numeric
//! ids the mapping's source model lays out ([`super::access`]), no element
//! names, nesting or absent fields. The root struct is a newtype over that
//! buffer; every struct of the model gets a unit type whose associated
//! functions read, write and test it at a given `base` id.
//!
//! Per struct, in name order:
//!
//! - `read_xml`: the generated pull reader ([`einvoice_transformator::xml::Reader`]),
//!   reading the element into the record as quick-xml's serde deserializer
//!   read it into a typed struct (see the runtime module for the rules).
//! - `write_xml`: the element written straight into one output `String`, tag
//!   names as literals, in emission order (attributes, text, then child
//!   elements by declaration position: the schema's sequence order). An
//!   empty scalar is `<tag/>`, an element with no content is self-closed.
//! - `is_empty`: whether the element has no value and no item, which the
//!   writer's owner guards test.
//! - Aliased elements (`match` selectors): `class_<physical>` names the logical
//!   field an item of the physical element belongs to (the first selector it
//!   satisfies, else the default bucket); the reader picks logical items
//!   through it. `demux` reports, in the old order, the single-valued logical
//!   nodes that matched several items (`overflow(node, extra)`), and `mux`
//!   moves the items the writer put in logical fields into the physical
//!   element, writing the selector values as discriminators.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::source_model::{FieldMeta, FieldType, NamespaceMeta, SourceModelMeta, StructMeta};

use crate::cond::Condition;

use super::access::{access_expr, fields, presence_expr, size, slot};

const R: &str = "::einvoice_transformator::record";
const X: &str = "::einvoice_transformator::xml";

/// Emits the source record types and their functions for every struct in the
/// source model, in deterministic name order.
pub(super) fn generate_source_structs(out: &mut String, source: &SourceModelMeta) {
    out.push_str("// --- source records ---\n");
    let io = alias_io_structs(source);
    for (name, meta) in &source.structs {
        let root_ns = (name == &source.root).then_some(&source.namespaces);
        if root_ns.is_some() {
            out.push_str("/// The source document: its values, laid out by the mapping.\n");
            out.push_str("#[derive(Debug, Clone, Default, PartialEq, Eq)]\n");
            let _ = writeln!(out, "pub struct {name}(pub {R}::Values);");
        } else {
            let _ = writeln!(
                out,
                "/// The `{name}` element's functions over a record (`{}` ids).",
                size(source, name)
            );
            let _ = writeln!(out, "pub struct {name};");
        }
        let _ = writeln!(out, "impl {name} {{");
        let _ = writeln!(
            out,
            "    /// The number of ids the element's layout takes.\n    pub const SIZE: u32 = {};\n",
            size(source, name)
        );
        generate_read_xml(out, source, meta);
        generate_write_xml(out, source, meta, root_ns);
        generate_is_empty(out, source, name, meta);
        if io.get(name).copied().unwrap_or(false) {
            generate_alias_io(out, source, name, meta, &io);
        }
        out.push_str("}\n\n");
    }
}

/// Which structs have aliased elements: those with a logical field, or with
/// a struct-typed field whose struct has them.
fn alias_io_structs(source: &SourceModelMeta) -> BTreeMap<String, bool> {
    fn needs(source: &SourceModelMeta, name: &str, cache: &mut BTreeMap<String, bool>) -> bool {
        if let Some(known) = cache.get(name) {
            return *known;
        }
        // Guard against a (never expected) cycle.
        cache.insert(name.to_string(), false);
        let result = source.structs.get(name).is_some_and(|meta| {
            meta.fields.values().any(|f| {
                f.is_logical()
                    || matches!(&f.ty, FieldType::Struct(inner) if needs(source, inner, cache))
            })
        });
        cache.insert(name.to_string(), result);
        result
    }
    let mut cache = BTreeMap::new();
    for name in source.structs.keys() {
        needs(source, name, &mut cache);
    }
    cache
}

/// Whether the root struct has aliased elements (so the mappers call
/// `demux`/`mux`).
pub(super) fn root_has_alias_io(source: &SourceModelMeta) -> bool {
    alias_io_structs(source)
        .get(&source.root)
        .copied()
        .unwrap_or(false)
}

/// The statements binding `refs`, every `$root.Key` value the selectors
/// compare against read from `base_var` (trimmed, in
/// [`SourceModelMeta::match_refs`] order), and the argument passing them on.
/// Both empty when no selector references a value.
pub(super) fn match_refs_binding(
    source: &SourceModelMeta,
    base_var: &str,
) -> (String, &'static str) {
    if source.match_refs.is_empty() {
        return (String::new(), "");
    }
    let reads: Vec<String> = source
        .match_refs
        .values()
        .map(|path| {
            format!(
                "{}.map(str::trim)",
                access_expr(source, &source.root, path, base_var)
            )
        })
        .collect();
    (format!("    let refs = [{}];\n", reads.join(", ")), "&refs")
}

/// The index of `reference` in the `refs` slice the selectors read.
fn ref_index(source: &SourceModelMeta, reference: &str) -> usize {
    source
        .match_refs
        .keys()
        .position(|k| k == reference)
        .expect("synthesis resolves every reference")
}

/// The name a field is written under: its XML name (prefixed), or the Rust
/// field name when the field has no XML binding.
fn written_name(fname: &str, field: &FieldMeta) -> String {
    field.serialized_name().unwrap_or_else(|| fname.to_string())
}

/// The name a field is read under: its XML local name (`@name` for an
/// attribute, `$text`), the name serde's errors give.
fn read_name(fname: &str, field: &FieldMeta) -> String {
    field.xml.clone().unwrap_or_else(|| fname.to_string())
}

/// Emits `read_xml`: the element `elem` read into `values` at `base`.
///
/// Attributes and child elements match by local name; a single-valued one
/// seen twice is serde's ``duplicate field `X` ``, as is a repeated element
/// whose run was interrupted (by another element or by text) or changed
/// prefix. An attribute's value is decoded only when mapped. An interior
/// element is read inline (an empty one marks its own id); a repeated
/// element's item is nested in place. `xsi:nil` makes an optional value or
/// an interior element absent. A required value never seen is ``missing
/// field `X` ``.
fn generate_read_xml(out: &mut String, source: &SourceModelMeta, meta: &StructMeta) {
    let laid: Vec<_> = fields(source, meta)
        .into_iter()
        .filter(|(_, f, _)| !f.is_logical())
        .collect();
    out.push_str("    /// Reads the element `elem` into `values`, laid out at `base`.\n");
    let _ = writeln!(
        out,
        "    pub fn read_xml<'a>(r: &mut {X}::Reader<'a>, elem: {X}::Elem<'a>, values: &mut {R}::Values, base: u32) -> Result<(), ::quick_xml::DeError> {{"
    );
    let _ = writeln!(
        out,
        "        let mut seen = [false; {}];",
        laid.len().max(1)
    );
    let attrs: Vec<_> = laid
        .iter()
        .enumerate()
        .filter(|(_, (_, f, _))| f.is_attribute())
        .collect();
    if !attrs.is_empty() {
        out.push_str("        for attr in elem.attributes() {\n");
        out.push_str("            let (name, attr) = attr?;\n");
        out.push_str("            match name {\n");
        for (k, (fname, field, id)) in &attrs {
            let key = read_name(fname, field);
            let _ = writeln!(
                out,
                "                {:?} => {{ if seen[{k}] {{ return Err({X}::duplicate({key:?})); }} seen[{k}] = true; values.push(base + {id}, &attr.value()?); }}",
                key.trim_start_matches('@')
            );
        }
        out.push_str("                _ => {}\n            }\n        }\n");
    }
    out.push_str("        let mut run = usize::MAX;\n");
    // A run continues only under the prefix it started with: the
    // deserializer matched a field by local name but a sequence's items by
    // qualified name. A prefix is short, so it stays inline.
    let runs = laid
        .iter()
        .any(|(_, f, _)| f.repeated && !f.is_attribute() && !f.is_text());
    if runs {
        out.push_str(
            "        let mut run_prefix = ::compact_str::CompactString::const_new(\"\");\n",
        );
    }
    out.push_str("        loop {\n");
    let _ = writeln!(out, "            match r.next(&elem)? {{");
    let _ = writeln!(
        out,
        "                {X}::Node::Child(child) => match child.local_name() {{"
    );
    for (k, (fname, field, id)) in laid.iter().enumerate() {
        if field.is_attribute() || field.is_text() {
            continue;
        }
        let key = read_name(fname, field);
        let check = if field.repeated {
            format!(
                "if seen[{k}] && (run != {k} || run_prefix != child.prefix()) {{ return Err({X}::duplicate({key:?})); }} if run != {k} {{ run_prefix = child.prefix().into(); }} seen[{k}] = true; run = {k};"
            )
        } else {
            format!(
                "if seen[{k}] {{ return Err({X}::duplicate({key:?})); }} seen[{k}] = true; run = usize::MAX;"
            )
        };
        let read = match (&field.ty, field.repeated) {
            (FieldType::Scalar, _) => {
                format!("let text = r.text(child)?; values.push(base + {id}, &text);")
            }
            (FieldType::Struct(inner), true) => format!(
                "let at = values.begin(base + {id}); {inner}::read_xml(r, child, values, 0)?; values.end(at);"
            ),
            (FieldType::Struct(inner), false) => format!(
                "let before = values.len(); {inner}::read_xml(r, child, values, base + {id})?; if values.len() == before {{ values.push(base + {id}, \"\"); }}"
            ),
        };
        // `xsi:nil` reads as absent where the value is an `Option`: an
        // optional scalar, an interior element.
        let nil = !field.repeated && (field.optional || matches!(field.ty, FieldType::Struct(_)));
        let body = if nil {
            format!("if child.is_nil() {{ r.skip(child)?; }} else {{ {read} }}")
        } else {
            read
        };
        let _ = writeln!(out, "                    {key:?} => {{ {check} {body} }}");
    }
    out.push_str("                    _ => {\n                        run = usize::MAX;\n                        r.skip(child)?;\n                    }\n                },\n");
    match laid.iter().position(|(_, f, _)| f.is_text()) {
        Some(k) => {
            let id = laid[k].2;
            let _ = writeln!(
                out,
                "                {X}::Node::Text(text) => {{ if seen[{k}] {{ return Err({X}::duplicate(\"$text\")); }} seen[{k}] = true; run = usize::MAX; values.push(base + {id}, &text); }}"
            );
        }
        None => {
            let _ = writeln!(
                out,
                "                {X}::Node::Text(_) => run = usize::MAX,"
            );
        }
    }
    let _ = writeln!(out, "                {X}::Node::End => break,");
    out.push_str("            }\n        }\n");
    for (k, (fname, field, _)) in laid.iter().enumerate() {
        if !field.repeated && !field.optional && matches!(field.ty, FieldType::Scalar) {
            let _ = writeln!(
                out,
                "        if !seen[{k}] {{ return Err({X}::missing({:?})); }}",
                read_name(fname, field)
            );
        }
    }
    out.push_str("        let _ = run;\n        Ok(())\n    }\n\n");
}

/// Emits `write_xml`: the element laid out at `base` in `rec` as the element
/// `tag`, as quick-xml's serializer wrote the typed struct. Attributes (the
/// root's namespace declarations first), then text and child elements in
/// emission order; absent values and logical fields are skipped, a required
/// value is always written.
fn generate_write_xml(
    out: &mut String,
    source: &SourceModelMeta,
    meta: &StructMeta,
    root_ns: Option<&NamespaceMeta>,
) {
    out.push_str("    /// Writes the element laid out at `base` in `rec` as `tag` onto `out`.\n");
    let _ = writeln!(
        out,
        "    pub fn write_xml(rec: &{R}::Index<'_>, base: u32, tag: &str, out: &mut String) {{"
    );
    out.push_str("        out.push('<');\n        out.push_str(tag);\n");
    for (prefix, uri) in root_ns.map(|ns| &ns.declared).into_iter().flatten() {
        let open = format!(" {}=\"", xmlns_attribute(prefix));
        let _ = writeln!(
            out,
            "        out.push_str({open:?});\n        {X}::attr(out, {uri:?});\n        out.push('\"');"
        );
    }
    let laid: Vec<_> = fields(source, meta)
        .into_iter()
        .filter(|(_, f, _)| !f.is_logical())
        .collect();
    // A required value is written even when the record has none, as the
    // serializer wrote the struct's `Default` empty string.
    let value = |field: &FieldMeta, id: u32, write: &str| -> String {
        if field.optional {
            format!("if let Some(value) = rec.get(base + {id}) {{ {write} }}")
        } else {
            format!("{{ let value = rec.get(base + {id}).unwrap_or(\"\"); {write} }}")
        }
    };
    for (fname, field, id) in laid.iter().filter(|(_, f, _)| f.is_attribute()) {
        let open = format!(" {}=\"", written_name(fname, field).trim_start_matches('@'));
        let write = format!("out.push_str({open:?}); {X}::attr(out, value); out.push('\"');");
        let _ = writeln!(out, "        {}", value(field, *id, &write));
    }
    // `>` first; taken back for `/>` when nothing follows it.
    out.push_str("        out.push('>');\n        let content = out.len();\n");
    for (fname, field, id) in laid.iter().filter(|(_, f, _)| !f.is_attribute()) {
        let tag = written_name(fname, field);
        let line = match (&field.ty, field.repeated, field.is_text()) {
            (_, _, true) => value(field, *id, &format!("{X}::text(out, value);")),
            (FieldType::Scalar, true, _) => format!(
                "for value in rec.all(base + {id}) {{ {X}::element(out, {tag:?}, value); }}"
            ),
            (FieldType::Scalar, false, _) => {
                value(field, *id, &format!("{X}::element(out, {tag:?}, value);"))
            }
            (FieldType::Struct(inner), true, _) => format!(
                "for item in rec.items(base + {id}) {{ {inner}::write_xml(&{R}::Index::new(item, {inner}::SIZE), 0, {tag:?}, out); }}"
            ),
            (FieldType::Struct(inner), false, _) => format!(
                "if rec.any(base + {id}, base + {}) {{ {inner}::write_xml(rec, base + {id}, {tag:?}, out); }}",
                id + size(source, inner)
            ),
        };
        let _ = writeln!(out, "        {line}");
    }
    out.push_str(
        "        if out.len() == content {\n            out.pop();\n            out.push_str(\"/>\");\n        } else {\n            out.push_str(\"</\");\n            out.push_str(tag);\n            out.push('>');\n        }\n",
    );
    out.push_str("    }\n\n");
}

/// The local ids (relative to the struct's own) of every repeated field in
/// the struct's inline subtree: an entry under one is an item, which makes
/// the element non-empty even when it holds nothing itself.
fn repeated_ids(source: &SourceModelMeta, meta: &StructMeta, base: u32, out: &mut Vec<u32>) {
    for (_, field, id) in fields(source, meta) {
        match &field.ty {
            _ if field.repeated => out.push(base + id),
            FieldType::Struct(inner) => {
                if let Some(inner_meta) = source.structs.get(inner) {
                    repeated_ids(source, inner_meta, base + id, out);
                }
            }
            FieldType::Scalar => {}
        }
    }
}

/// Emits `is_empty`: whether the element laid out at `base` has no
/// non-empty value and no item of a repeated element (an empty value and a
/// presence mark do not count, as an empty string and an empty element did
/// not count in the typed struct's emptiness).
fn generate_is_empty(out: &mut String, source: &SourceModelMeta, name: &str, meta: &StructMeta) {
    let mut repeated = Vec::new();
    repeated_ids(source, meta, 0, &mut repeated);
    let items = if repeated.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = repeated.iter().map(u32::to_string).collect();
        format!(" && !matches!(local, {})", ids.join(" | "))
    };
    out.push_str("    /// Whether the element laid out at `base` holds no value and no item.\n");
    let _ = writeln!(
        out,
        "    pub fn is_empty(rec: {R}::Rec<'_>, base: u32) -> bool {{"
    );
    let _ = writeln!(
        out,
        "        rec.entries().all(|(id, payload)| {{ let local = id.wrapping_sub(base); local >= {} || (payload.is_empty(){items}) }})",
        size(source, name)
    );
    out.push_str("    }\n");
}

/// The `refs` parameter of the alias functions, and the argument passing it
/// on, when the selectors reference document values.
fn refs_param(source: &SourceModelMeta) -> (&'static str, &'static str) {
    if source.match_refs.is_empty() {
        ("", "")
    } else {
        (", refs: &[Option<&str>]", ", refs")
    }
}

/// Emits `class_<physical>` per aliased element, `demux` and `mux`.
fn generate_alias_io(
    out: &mut String,
    source: &SourceModelMeta,
    name: &str,
    meta: &StructMeta,
    io: &BTreeMap<String, bool>,
) {
    let (param, arg) = refs_param(source);
    let laid = fields(source, meta);
    let id_of = |fname: &str| {
        laid.iter()
            .find(|(n, _, _)| n.as_str() == fname)
            .map(|(_, _, id)| *id)
            .expect("a field of the struct")
    };
    let groups = meta.aliased_fields();

    // class_<physical>: the selectors in declaration order, the default
    // bucket for what none matched (`usize::MAX`: no logical field).
    for (physical, logical) in &groups {
        let FieldType::Struct(item_struct) = &meta.fields[*physical].ty else {
            continue;
        };
        let _ = writeln!(
            out,
            "\n    /// The logical field (by position) an item of `{physical}` belongs to."
        );
        let _ = writeln!(
            out,
            "    pub fn class_{physical}(item: {R}::Rec<'_>{param}) -> usize {{"
        );
        let mut default = "usize::MAX".to_string();
        for (i, (_, f)) in logical.iter().enumerate() {
            let alias = f.alias.as_ref().expect("logical fields carry a binding");
            if alias.selector.is_empty() {
                if default == "usize::MAX" {
                    default = i.to_string();
                }
                continue;
            }
            let cond: Vec<String> = alias
                .selector
                .iter()
                .map(|(path, cond)| condition_expr(source, item_struct, path, cond))
                .collect();
            let _ = writeln!(out, "        if {} {{ return {i}; }}", cond.join(" && "));
        }
        let _ = writeln!(out, "        {default}\n    }}");
    }

    // demux: the MATCH_MULTIPLE reports, per aliased element then per child
    // with aliases beneath it, as the typed demux emitted them.
    let physical_names: Vec<&String> = groups.iter().map(|(p, _)| *p).collect();
    out.push_str("\n    /// Reports each single-valued logical node that matched `extra` items beyond the one it reads.\n");
    let _ = writeln!(
        out,
        "    pub fn demux(rec: {R}::Rec<'_>, base: u32{param}, overflow: &mut dyn FnMut(&'static str, usize)) {{"
    );
    for (physical, logical) in &groups {
        if !matches!(meta.fields[*physical].ty, FieldType::Struct(_)) {
            continue;
        }
        // Selected singles first, then the default bucket's.
        let (selected, default): (Vec<_>, Vec<_>) = logical
            .iter()
            .enumerate()
            .partition(|(_, (_, f))| f.alias.as_ref().is_some_and(|a| !a.selector.is_empty()));
        let single: Vec<_> = selected
            .iter()
            .chain(default.first())
            .filter(|(_, (_, f))| !f.repeated)
            .collect();
        if single.is_empty() {
            continue;
        }
        let _ = writeln!(out, "        let mut counts = [0usize; {}];", logical.len());
        let _ = writeln!(
            out,
            "        for item in rec.items(base + {}) {{ let class = Self::class_{physical}(item{arg}); if class < {} {{ counts[class] += 1; }} }}",
            id_of(physical),
            logical.len()
        );
        for (i, (_, f)) in single {
            let node = &f.alias.as_ref().expect("binding").node;
            let _ = writeln!(
                out,
                "        if counts[{i}] > 1 {{ overflow({node:?}, counts[{i}] - 1); }}"
            );
        }
    }
    for (fname, field, id) in &laid {
        let FieldType::Struct(inner) = &field.ty else {
            continue;
        };
        if physical_names.contains(fname) || !io.get(inner).copied().unwrap_or(false) {
            continue;
        }
        let call = format!("{inner}::demux(item, 0{arg}, overflow);");
        match &field.alias {
            Some(alias) => {
                let (_, logical) = groups
                    .iter()
                    .find(|(p, _)| **p == alias.physical)
                    .expect("its group");
                let index = logical
                    .iter()
                    .position(|(n, _)| n == fname)
                    .expect("in group");
                let picked = format!(
                    "rec.items(base + {}).filter(|i| Self::class_{}(*i{arg}) == {index})",
                    id_of(&alias.physical),
                    alias.physical
                );
                if field.repeated {
                    let _ = writeln!(out, "        for item in {picked} {{ {call} }}");
                } else {
                    let _ = writeln!(
                        out,
                        "        if let Some(item) = {picked}.next() {{ {call} }}"
                    );
                }
            }
            None if field.repeated => {
                let _ = writeln!(
                    out,
                    "        for item in rec.items(base + {id}) {{ {call} }}"
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "        {inner}::demux(rec, base + {id}{arg}, overflow);"
                );
            }
        }
    }
    out.push_str("    }\n");

    generate_mux(out, source, name, io);
}

/// One logical field `mux` moves: its ids in the record being muxed, the
/// physical element's id, and the selector's discriminator statements.
struct Move {
    node: String,
    /// `lo..hi` for a single logical field laid out inline, `id..id + 1` for
    /// a repeated one.
    ids: (u32, u32),
    repeated: bool,
    phys: u32,
    item: String,
    discriminators: Vec<String>,
}

/// Collects, for the struct `name` laid out at `base` and the interiors
/// inside it, the logical fields to move and the repeated non-physical
/// elements whose items have aliases of their own.
fn mux_plan(
    source: &SourceModelMeta,
    name: &str,
    base: u32,
    io: &BTreeMap<String, bool>,
    moves: &mut Vec<Move>,
    nested: &mut Vec<(u32, String)>,
) {
    let Some(meta) = source.structs.get(name) else {
        return;
    };
    let laid = fields(source, meta);
    let id_of = |fname: &str| {
        laid.iter()
            .find(|(n, _, _)| n.as_str() == fname)
            .map(|(_, _, id)| base + id)
            .expect("a field of the struct")
    };
    let groups = meta.aliased_fields();
    for (physical, logical) in &groups {
        let FieldType::Struct(item_struct) = &meta.fields[*physical].ty else {
            continue;
        };
        for (lname, f) in logical {
            let alias = f.alias.as_ref().expect("logical fields carry a binding");
            let lo = id_of(lname);
            let hi = if f.repeated {
                lo + 1
            } else {
                lo + size(source, item_struct)
            };
            moves.push(Move {
                node: alias.node.clone(),
                ids: (lo, hi),
                repeated: f.repeated,
                phys: id_of(physical),
                item: item_struct.clone(),
                discriminators: discriminators(source, item_struct, &alias.selector),
            });
        }
    }
    for (fname, field, id) in &laid {
        let FieldType::Struct(inner) = &field.ty else {
            continue;
        };
        if field.is_logical() {
            continue;
        }
        if !field.repeated {
            mux_plan(source, inner, base + id, io, moves, nested);
        } else if io.get(inner).copied().unwrap_or(false)
            && !groups.iter().any(|(p, _)| *p == *fname)
        {
            nested.push((base + id, inner.clone()));
        }
    }
}

/// The statements writing a selector's values into `item` (the first of a
/// list, unless the item has one of the listed values already; presence
/// writes nothing).
fn discriminators(
    source: &SourceModelMeta,
    item_struct: &str,
    selector: &[(String, Condition)],
) -> Vec<String> {
    selector
        .iter()
        .filter_map(|(path, cond)| {
            let id = match slot(source, item_struct, path) {
                Ok(slot) => slot.id,
                Err(e) => return Some(format!("{e};")),
            };
            match cond {
                Condition::Is(reference) if cond.reference().is_some() => Some(format!(
                    "if let Some(v) = refs[{}] {{ item.set({id}, v); }}",
                    ref_index(source, reference)
                )),
                Condition::Is(value) => Some(format!("item.set({id}, {value:?});")),
                Condition::OneOf(values) => values.first().map(|value| {
                    let listed: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
                    format!(
                        "if !item.get({id}).is_some_and(|v| matches!(v.trim(), {})) {{ item.set({id}, {value:?}); }}",
                        listed.join(" | ")
                    )
                }),
                Condition::Present(_) => None,
            }
        })
        .collect()
}

/// Emits `mux`: the record copied with every logical field's items moved
/// into its physical element, after the physical items and in logical
/// declaration order, each with its selector's discriminators; items of a
/// repeated element with aliases of their own are muxed in turn.
fn generate_mux(
    out: &mut String,
    source: &SourceModelMeta,
    name: &str,
    io: &BTreeMap<String, bool>,
) {
    let (param, arg) = refs_param(source);
    let mut moves = Vec::new();
    let mut nested = Vec::new();
    mux_plan(source, name, 0, io, &mut moves, &mut nested);
    out.push_str(
        "\n    /// The record with every logical field's items moved into its physical element.\n",
    );
    let _ = writeln!(
        out,
        "    pub fn mux(rec: {R}::Rec<'_>{param}) -> {R}::Values {{"
    );
    let _ = writeln!(out, "        let mut out = {R}::Values::default();");
    out.push_str("        out.reserve(rec.len());\n");
    out.push_str("        for (id, payload) in rec.entries() {\n            match id {\n");
    if !moves.is_empty() {
        let ranges: Vec<String> = moves
            .iter()
            .map(|m| format!("{}..={}", m.ids.0, m.ids.1 - 1))
            .collect();
        let _ = writeln!(out, "                {} => {{}}", ranges.join(" | "));
    }
    for (id, inner) in &nested {
        let _ = writeln!(
            out,
            "                {id} => out.push_record({id}, {inner}::mux(payload.into(){arg}).rec()),"
        );
    }
    out.push_str("                _ => out.push_raw(id, payload),\n            }\n        }\n");
    for m in &moves {
        let has_io = io.get(&m.item).copied().unwrap_or(false);
        let binding = if m.discriminators.is_empty() {
            "item"
        } else {
            "mut item"
        };
        let _ = writeln!(out, "        // {}", m.node);
        if m.repeated && !has_io && m.discriminators.is_empty() {
            let _ = writeln!(
                out,
                "        for item in rec.items({}) {{ out.push_record({}, item); }}",
                m.ids.0, m.phys
            );
            continue;
        }
        let (head, take) = if m.repeated {
            (
                format!("for item in rec.items({}) {{", m.ids.0),
                if has_io {
                    format!("{}::mux(item{arg})", m.item)
                } else {
                    "item.to_values()".to_string()
                },
            )
        } else {
            let extract = format!("rec.extract({}, {})", m.ids.0, m.ids.1);
            (
                format!("if rec.any({}, {}) {{", m.ids.0, m.ids.1),
                if has_io {
                    format!("{}::mux({extract}.rec(){arg})", m.item)
                } else {
                    extract
                },
            )
        };
        let _ = writeln!(out, "        {head}");
        let _ = writeln!(out, "            let {binding} = {take};");
        for d in &m.discriminators {
            let _ = writeln!(out, "            {d}");
        }
        let _ = writeln!(out, "            out.push_record({}, item.rec());", m.phys);
        out.push_str("        }\n");
    }
    out.push_str("        out\n    }\n");
}

/// The `bool` expression testing one selector entry against `item`.
fn condition_expr(
    source: &SourceModelMeta,
    item_struct: &str,
    path: &str,
    cond: &Condition,
) -> String {
    match cond {
        Condition::Is(reference) if cond.reference().is_some() => format!(
            "{}.is_some_and(|v| refs[{}] == Some(v.trim()))",
            access_expr(source, item_struct, path, "item"),
            ref_index(source, reference)
        ),
        Condition::Is(value) => format!(
            "{}.is_some_and(|v| v.trim() == {value:?})",
            access_expr(source, item_struct, path, "item")
        ),
        Condition::OneOf(values) => format!(
            "{}.is_some_and(|v| matches!(v.trim(), {}))",
            access_expr(source, item_struct, path, "item"),
            values
                .iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        Condition::Present(p) => {
            let present = presence_expr(source, item_struct, path, "item");
            if p.present {
                present
            } else {
                format!("!({present})")
            }
        }
    }
}

/// The attribute name of a namespace declaration: `xmlns` for the default
/// namespace, `xmlns:prefix` otherwise.
fn xmlns_attribute(prefix: &str) -> String {
    if prefix.is_empty() {
        "xmlns".to_string()
    } else {
        format!("xmlns:{prefix}")
    }
}

/// Emits `from_xml` / `to_xml` for the root struct. `from_xml` reads the
/// document element into one record; `to_xml` writes the XML declaration and
/// the root tag qualified with the mapping's `root_ns`.
pub(super) fn generate_xml_io(out: &mut String, root: &str, ns: &NamespaceMeta) {
    let _ = writeln!(out, "/// Reads source XML bytes into the `{root}` record.");
    let _ = writeln!(
        out,
        "pub fn from_xml(bytes: &[u8]) -> Result<{root}, quick_xml::DeError> {{"
    );
    out.push_str("    let s = std::str::from_utf8(bytes)\n");
    out.push_str(
        "        .map_err(|e| quick_xml::DeError::Custom(format!(\"invalid utf-8: {e}\")))?;\n",
    );
    let _ = writeln!(out, "    let mut reader = {X}::Reader::new(s);");
    let _ = writeln!(out, "    let elem = reader.root({root:?})?;");
    let _ = writeln!(out, "    let mut values = {R}::Values::default();");
    let _ = writeln!(
        out,
        "    {root}::read_xml(&mut reader, elem, &mut values, 0)?;"
    );
    out.push_str("    values.shrink();\n");
    let _ = writeln!(out, "    Ok({root}(values))\n}}\n");

    let _ = writeln!(
        out,
        "/// Writes a `{root}` record as XML, with the XML declaration and the\n\
         /// namespace-qualified root tag, into a buffer reserved for `capacity`\n\
         /// bytes (a size hint: the buffer still grows past it)."
    );
    let qualified = ns.qualified_root(root);
    // ponytail: the `Result` never fails since the writer is generated; it
    // keeps the engine's serialize error path.
    let _ = writeln!(
        out,
        "pub fn to_xml(source: &{root}, capacity: usize) -> Result<String, quick_xml::SeError> {{"
    );
    out.push_str("    let mut out = String::with_capacity(capacity);\n");
    out.push_str("    out.push_str(\"<?xml version=\\\"1.0\\\" encoding=\\\"UTF-8\\\"?>\\n\");\n");
    let _ = writeln!(
        out,
        "    {root}::write_xml(&{R}::Index::new(source.0.rec(), {root}::SIZE), 0, {qualified:?}, &mut out);"
    );
    out.push_str("    Ok(out)\n}\n");
}
