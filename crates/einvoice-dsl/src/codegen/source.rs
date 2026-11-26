//! Typed source-struct generation and XML I/O.
//!
//! Emits one struct per struct in the source model (with generated `read_xml`
//! and `write_xml` methods, and a serde/XML binding derived in tests only) and
//! the `from_xml` / `to_xml` functions for the root.
//!
//! Struct fields are declared in the model's **emission order**
//! ([`StructMeta::ordered_fields`]: attributes, then element text, then child
//! elements by declaration position), and `write_xml` writes them in that
//! order — the order the mapping declares them, the schema's sequence order.
//!
//! Writing: `to_xml` calls the root's generated `write_xml`, which writes
//! straight into one output `String`, tag names as literals. It replaces
//! quick-xml's serde serializer, which buffered every element's children in a
//! string of their own and copied it into the parent: the whole document once
//! per nesting level, up to 5x the output in memory. The output is byte for
//! byte the serializer's, which stays derived in tests as the reference:
//! `to_xml_serde` (tests only) writes through it, and the interfaces crate
//! compares the two on every sample and target.
//!
//! Reading: `from_xml` calls the root's generated `read_xml`, a pull reader
//! over quick-xml's events (helpers in the interfaces crate's `xmlread`): it
//! matches attributes and child elements by local name, joins text, CDATA and
//! references untrimmed, skips unknown elements, comments and processing
//! instructions, reads `xsi:nil` as `None`, and refuses a single element read
//! twice, or a repeated one that comes back after another element, with
//! serde's ``duplicate field `X` ``. That is what quick-xml's serde
//! deserializer reads, about twice as fast and without the serde code. The
//! deserializer stays derived in tests as the reference: `from_xml_serde`
//! (tests only), compared on every sample, the corpus and edge cases.
//!
//! Namespaces: reading is namespace-agnostic (the generated reader, like
//! quick-xml's deserializer, matches local names), so every element field is renamed with a *split* rename —
//! `rename(serialize = "cbc:ID", deserialize = "ID")` — that writes the
//! prefixed name and still reads the bare one. The declarations themselves are
//! zero-size **marker types** (`XmlnsCbc` serializes as its URI) that the root
//! struct carries as `@xmlns[:prefix]` attribute fields, and `to_xml` writes the
//! XML declaration plus the prefixed root tag.
//!
//! Aliased elements: a *logical* field ([`FieldMeta::is_logical`]) is never
//! read or written (`serde(skip)` in tests); its items live in the struct's *physical* repeated field
//! on the wire. Every struct that holds such an element, directly or beneath
//! it, gets a `demux` method (physical → logical, by `match` selector; the
//! reader calls it on the root before mapping) and a `mux` method (logical →
//! physical, writing the selector values as discriminators; the writer calls it
//! on the root after mapping).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::source_model::{FieldMeta, FieldType, NamespaceMeta, SourceModelMeta, StructMeta};

use crate::cond::Condition;

use super::access::{access_expr, assign_target_expr, presence_expr};

/// Emits the namespace marker types and the typed source structs (with
/// serde/XML binding) for every struct in the source model, in deterministic
/// name order; each struct's fields follow the model's emission order.
pub(super) fn generate_source_structs(out: &mut String, source: &SourceModelMeta) {
    generate_namespace_markers(out, &source.namespaces);
    out.push_str("// --- typed source structs ---\n");
    let io = alias_io_structs(source);
    for (name, meta) in &source.structs {
        let root_ns = (name == &source.root).then_some(&source.namespaces);
        generate_one_struct(out, name, meta, root_ns);
        generate_write_xml_impl(out, name, meta, root_ns);
        generate_read_xml_impl(out, name, meta);
        if io.get(name).copied().unwrap_or(false) {
            generate_alias_io_impl(out, source, name, meta, &io);
        }
        out.push('\n');
    }
}

/// Which structs need `demux`/`mux`: those with a logical field, or with a
/// struct-typed field whose struct needs them.
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

/// The `let match_refs = [..];` statement reading every `$root.Key` value the
/// selectors compare against from `base_var` (trimmed, in
/// [`SourceModelMeta::match_refs`] order), and the leading argument passing
/// them to `demux`/`mux`. Both empty when no selector references a value.
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
                "{}.map(|v| v.trim().to_string())",
                access_expr(source, &source.root, path, base_var)
            )
        })
        .collect();
    (
        format!("    let match_refs = [{}];\n", reads.join(", ")),
        "&match_refs.each_ref().map(|r| r.as_deref())",
    )
}

/// The index of `reference` in the `refs` slice `demux`/`mux` receive.
fn ref_index(source: &SourceModelMeta, reference: &str) -> usize {
    source
        .match_refs
        .keys()
        .position(|k| k == reference)
        .expect("synthesis resolves every reference")
}

/// Whether the root struct of `source` has a `demux`/`mux` pair (so the
/// mappers must call them).
pub(super) fn root_has_alias_io(source: &SourceModelMeta) -> bool {
    alias_io_structs(source)
        .get(&source.root)
        .copied()
        .unwrap_or(false)
}

/// Emits `impl Name { pub fn demux(..); pub fn mux(..) }`.
///
/// `demux` drains each physical field, handing every item to the first logical
/// field whose selector it satisfies (a non-repeated logical field keeps the
/// first and reports the surplus through `overflow(node, extra)`; the
/// selector-less logical field, if any, takes what nothing matched; the rest is
/// dropped), then recurses into every child struct that has aliases. `mux`
/// recurses first, then moves each logical field's items back into the
/// physical field in logical declaration order, assigning the selector values.
fn generate_alias_io_impl(
    out: &mut String,
    source: &SourceModelMeta,
    name: &str,
    meta: &StructMeta,
    io: &BTreeMap<String, bool>,
) {
    let groups = meta.aliased_fields();
    let physical_names: Vec<&String> = groups.iter().map(|(p, _)| *p).collect();
    // Child struct fields to recurse into: everything struct-typed with alias
    // I/O beneath it, except the physical fields (empty after demux, filled
    // after the logical ones are muxed).
    let children: Vec<(&String, &FieldMeta)> = meta
        .ordered_fields()
        .into_iter()
        .filter(|(fname, f)| {
            !physical_names.contains(fname)
                && matches!(&f.ty, FieldType::Struct(inner) if io.get(inner).copied().unwrap_or(false))
        })
        .collect();

    // `refs`: the referenced document values, threaded through every level.
    let (refs_param, refs_arg) = if source.match_refs.is_empty() {
        ("", "")
    } else {
        ("refs: &[Option<&str>], ", "refs, ")
    };
    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    /// Partitions each aliased element's items into its logical fields by\n");
    out.push_str(
        "    /// `match` selector, recursing into children. `overflow(node, extra)` reports\n",
    );
    out.push_str(
        "    /// a single-valued logical node that matched `extra` items beyond the one kept.\n",
    );
    let _ = writeln!(
        out,
        "    pub fn demux(&mut self, {refs_param}overflow: &mut dyn FnMut(&'static str, usize)) {{"
    );
    for (physical, logical) in &groups {
        let item_struct = match &meta.fields[*physical].ty {
            FieldType::Struct(inner) => inner.clone(),
            FieldType::Scalar => continue,
        };
        // Selectors first, in declaration order; the default bucket (no
        // selector) last so it only sees what nothing matched.
        let (selected, default): (Vec<_>, Vec<_>) = logical
            .iter()
            .partition(|(_, f)| f.alias.as_ref().is_some_and(|a| !a.selector.is_empty()));
        // Every single-valued logical field (selected or the default bucket)
        // counts the surplus it drops, so no item vanishes without a warning.
        let single: Vec<&String> = selected
            .iter()
            .chain(&default)
            .filter(|(_, f)| !f.repeated)
            .map(|(lname, _)| *lname)
            .collect();
        for lname in &single {
            let _ = writeln!(out, "        let mut extra_{lname} = 0usize;");
        }
        let _ = writeln!(
            out,
            "        for item in std::mem::take(&mut self.{physical}) {{"
        );
        for (lname, f) in &selected {
            let alias = f.alias.as_ref().expect("selected fields carry a binding");
            let cond: Vec<String> = alias
                .selector
                .iter()
                .map(|(path, cond)| condition_expr(source, &item_struct, path, cond))
                .collect();
            let _ = writeln!(out, "            if {} {{", cond.join(" && "));
            if f.repeated {
                let _ = writeln!(out, "                self.{lname}.push(item);");
            } else {
                let _ = writeln!(out, "                if self.{lname}.is_none() {{");
                let _ = writeln!(
                    out,
                    "                    self.{lname} = Some(Box::new(item));"
                );
                let _ = writeln!(out, "                }} else {{");
                let _ = writeln!(out, "                    extra_{lname} += 1;");
                let _ = writeln!(out, "                }}");
            }
            out.push_str(
                "                continue;
",
            );
            out.push_str(
                "            }
",
            );
        }
        match default.first() {
            Some((lname, f)) if f.repeated => {
                let _ = writeln!(out, "            self.{lname}.push(item);");
            }
            Some((lname, _)) => {
                let _ = writeln!(out, "            if self.{lname}.is_none() {{");
                let _ = writeln!(out, "                self.{lname} = Some(Box::new(item));");
                out.push_str("            } else {\n");
                let _ = writeln!(out, "                extra_{lname} += 1;");
                out.push_str("            }\n");
            }
            // No default bucket: an item no selector claims is not mapped.
            None => out.push_str(
                "            drop(item);
",
            ),
        }
        out.push_str(
            "        }
",
        );
        for lname in &single {
            let node = &logical
                .iter()
                .find(|(n, _)| *n == *lname)
                .and_then(|(_, f)| f.alias.as_ref())
                .expect("binding")
                .node;
            let _ = writeln!(out, "        if extra_{lname} > 0 {{");
            let _ = writeln!(out, "            overflow({node:?}, extra_{lname});");
            out.push_str("        }\n");
        }
    }
    for (fname, f) in &children {
        if f.repeated {
            let _ = writeln!(
                out,
                "        for child in &mut self.{fname} {{
            child.demux({refs_arg}overflow);
        }}"
            );
        } else {
            let _ = writeln!(
                out,
                "        if let Some(child) = self.{fname}.as_mut() {{
            child.demux({refs_arg}overflow);
        }}"
            );
        }
    }
    out.push_str(
        "    }

",
    );

    out.push_str(
        "    /// Moves every logical field's items back into its physical element, writing\n",
    );
    out.push_str("    /// the `match` selector values as discriminators; children first.\n");
    let _ = writeln!(
        out,
        "    pub fn mux(&mut self{}) {{",
        if refs_param.is_empty() {
            ""
        } else {
            ", refs: &[Option<&str>]"
        }
    );
    let refs_arg_mux = refs_arg.trim_end_matches(", ");
    for (fname, f) in &children {
        if f.repeated {
            let _ = writeln!(
                out,
                "        for child in &mut self.{fname} {{
            child.mux({refs_arg_mux});
        }}"
            );
        } else {
            let _ = writeln!(
                out,
                "        if let Some(child) = self.{fname}.as_mut() {{
            child.mux({refs_arg_mux});
        }}"
            );
        }
    }
    for (physical, logical) in &groups {
        let item_struct = match &meta.fields[*physical].ty {
            FieldType::Struct(inner) => inner.clone(),
            FieldType::Scalar => continue,
        };
        for (lname, f) in logical {
            let alias = f.alias.as_ref().expect("logical fields carry a binding");
            let discriminators: Vec<String> = alias
                .selector
                .iter()
                .filter_map(|(path, cond)| {
                    let target = assign_target_expr(source, &item_struct, path, "item");
                    match cond {
                        Condition::Is(reference) if cond.reference().is_some() => Some(format!(
                            "if let Some(v) = refs[{}] {{ {target} = Some(CompactString::from(v)); }}",
                            ref_index(source, reference)
                        )),
                        Condition::Is(value) => {
                            Some(format!("{target} = Some(CompactString::from({value:?}));"))
                        }
                        // A list writes its first value unless the item already
                        // has one of the listed ones; presence writes nothing.
                        Condition::OneOf(values) => values.first().map(|value| {
                            let listed: Vec<String> =
                                values.iter().map(|v| format!("{v:?}")).collect();
                            format!(
                                "{{ let slot = &mut {target}; if !slot.as_deref().is_some_and(|v| matches!(v.trim(), {})) {{ *slot = Some(CompactString::from({value:?})); }} }}",
                                listed.join(" | ")
                            )
                        }),
                        Condition::Present(_) => None,
                    }
                })
                .collect();
            let binding = if discriminators.is_empty() {
                "item"
            } else {
                "mut item"
            };
            if f.repeated {
                let _ = writeln!(
                    out,
                    "        for {binding} in std::mem::take(&mut self.{lname}) {{"
                );
                for d in &discriminators {
                    let _ = writeln!(out, "            {d}");
                }
                let _ = writeln!(out, "            self.{physical}.push(item);");
                out.push_str(
                    "        }
",
                );
            } else {
                let _ = writeln!(out, "        if let Some(boxed) = self.{lname}.take() {{");
                let _ = writeln!(out, "            let {binding} = *boxed;");
                for d in &discriminators {
                    let _ = writeln!(out, "            {d}");
                }
                let _ = writeln!(out, "            self.{physical}.push(item);");
                out.push_str(
                    "        }
",
                );
            }
        }
    }
    out.push_str(
        "    }
",
    );
    out.push_str(
        "}
",
    );
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

/// Emits one `#[derive(...)]` source struct, fields in emission order. The root
/// struct (`root_ns` given) first carries one `@xmlns[:prefix]` marker field per
/// declared namespace, so every emitted document declares them.
fn generate_one_struct(
    out: &mut String,
    name: &str,
    meta: &StructMeta,
    root_ns: Option<&NamespaceMeta>,
) {
    out.push_str("#[derive(Default)]\n#[cfg_attr(test, derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize))]\n");
    let _ = writeln!(out, "pub struct {name} {{");
    if let Some(ns) = root_ns {
        for prefix in ns.declared.keys() {
            let _ = writeln!(
                out,
                "    #[cfg_attr(test, serde(rename = {:?}, default))]",
                xmlns_attribute(prefix)
            );
            let _ = writeln!(
                out,
                "    pub {}: {},",
                xmlns_field(prefix),
                xmlns_marker_type(prefix)
            );
        }
    }
    for (fname, field) in meta.ordered_fields() {
        if let Some(attr) = serde_attr(field) {
            let _ = writeln!(out, "    {attr}");
        }
        let _ = writeln!(out, "    pub {fname}: {},", source_field_type(field));
    }
    out.push_str("}\n");
    generate_is_empty_impl(out, name, meta);
}

/// Emits `impl Name { pub fn write_xml(&self, tag, out) }`: the struct as the
/// element `tag`, as quick-xml's serializer wrote it. Attributes (the root's
/// namespace declarations first), then text and child elements in emission
/// order; `None` fields and logical fields are skipped, an empty scalar is
/// `<tag/>`, and an element with no content is self-closed.
fn generate_write_xml_impl(
    out: &mut String,
    name: &str,
    meta: &StructMeta,
    root_ns: Option<&NamespaceMeta>,
) {
    const XML: &str = "::einvoice_transformator::xml";
    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    /// Writes `self` as the element `tag` onto `out`.\n");
    out.push_str("    pub fn write_xml(&self, tag: &str, out: &mut String) {\n");
    out.push_str("        out.push('<');\n        out.push_str(tag);\n");
    for (prefix, uri) in root_ns.map(|ns| &ns.declared).into_iter().flatten() {
        let open = format!(" {}=\"", xmlns_attribute(prefix).trim_start_matches('@'));
        let _ = writeln!(
            out,
            "        out.push_str({open:?});\n        {XML}::attr(out, {uri:?});\n        out.push('\"');"
        );
    }
    let fields: Vec<_> = meta
        .ordered_fields()
        .into_iter()
        .filter(|(_, field)| !field.is_logical())
        .collect();
    for (fname, field) in fields.iter().filter(|(_, field)| field.is_attribute()) {
        let open = format!(" {}=\"", written_name(fname, field).trim_start_matches('@'));
        let write = format!("out.push_str({open:?}); {XML}::attr(out, value); out.push('\"');");
        if field.optional {
            let _ = writeln!(
                out,
                "        if let Some(value) = &self.{fname} {{ {write} }}"
            );
        } else {
            let _ = writeln!(out, "        {{ let value = &self.{fname}; {write} }}");
        }
    }
    // `>` first; taken back for `/>` when nothing follows it.
    out.push_str("        out.push('>');\n        let content = out.len();\n");
    for (fname, field) in fields.iter().filter(|(_, field)| !field.is_attribute()) {
        let tag = written_name(fname, field);
        let write = match (&field.ty, field.is_text()) {
            (_, true) => format!("{XML}::text(out, value);"),
            (FieldType::Scalar, false) => format!("{XML}::element(out, {tag:?}, value);"),
            (FieldType::Struct(_), false) => format!("value.write_xml({tag:?}, out);"),
        };
        if field.repeated {
            let _ = writeln!(out, "        for value in &self.{fname} {{ {write} }}");
        } else if field.optional || matches!(field.ty, FieldType::Struct(_)) {
            let _ = writeln!(
                out,
                "        if let Some(value) = &self.{fname} {{ {write} }}"
            );
        } else {
            let _ = writeln!(out, "        {{ let value = &self.{fname}; {write} }}");
        }
    }
    out.push_str(
        "        if out.len() == content {\n            out.pop();\n            out.push_str(\"/>\");\n        } else {\n            out.push_str(\"</\");\n            out.push_str(tag);\n            out.push('>');\n        }\n",
    );
    out.push_str("    }\n}\n");
}

/// Emits `impl Name { pub fn read_xml(r, start, empty) }`: the struct read
/// from the element whose start tag is `start` with a pull parser, matching
/// what quick-xml's serde deserializer reads into it.
fn generate_read_xml_impl(out: &mut String, name: &str, meta: &StructMeta) {
    const X: &str = "crate::xmlread";
    let fields: Vec<_> = meta
        .ordered_fields()
        .into_iter()
        .filter(|(_, field)| !field.is_logical())
        .collect();
    let read_name = |fname: &str, field: &FieldMeta| -> String {
        field.xml.clone().unwrap_or_else(|| fname.to_string())
    };
    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    /// Reads the element whose start tag is `start` (`empty`: `<tag/>`).\n");
    let _ = writeln!(
        out,
        "    pub fn read_xml(r: &mut {X}::XmlIn<'_>, start: &quick_xml::events::BytesStart<'_>, empty: bool) -> Result<Self, quick_xml::DeError> {{"
    );
    out.push_str("        let mut this = Self::default();\n");
    let attrs: Vec<_> = fields.iter().filter(|(_, f)| f.is_attribute()).collect();
    if attrs.is_empty() {
        out.push_str("        let _ = start;\n");
    } else {
        let _ = writeln!(out, "        let mut seen_attr = [false; {}];", attrs.len());
        out.push_str("        for attr in start.attributes() {\n");
        out.push_str("            let attr = attr.map_err(|e| quick_xml::DeError::Custom(e.to_string()))?;\n");
        let _ = writeln!(out, "            match {X}::attr_key(&attr) {{");
        for (j, (fname, field)) in attrs.iter().enumerate() {
            let key = read_name(fname, field);
            let dup = format!(
                "if seen_attr[{j}] {{ return Err({X}::duplicate({key:?})); }} seen_attr[{j}] = true; "
            );
            let key = key.trim_start_matches('@');
            let set = if field.optional {
                format!("this.{fname} = Some({X}::attr_value(&attr)?);")
            } else {
                format!("this.{fname} = {X}::attr_value(&attr)?;")
            };
            let _ = writeln!(out, "                b{key:?} => {{ {dup}{set} }}");
        }
        out.push_str("                _ => {}\n            }\n        }\n");
    }
    out.push_str("        if empty {\n            return Ok(this);\n        }\n");
    let elements: Vec<_> = fields.iter().filter(|(_, f)| !f.is_attribute()).collect();
    let _ = writeln!(
        out,
        "        let mut seen = [false; {}];",
        elements.len().max(1)
    );
    out.push_str("        let mut last = usize::MAX;\n");
    out.push_str("        loop {\n            let (e, empty) = match r.next()? {\n");
    let _ = writeln!(out, "                {X}::Ev::Start(e) => (e, false),");
    let _ = writeln!(out, "                {X}::Ev::Empty(e) => (e, true),");
    let _ = writeln!(out, "                {X}::Ev::End => break,");
    match elements.iter().position(|(_, f)| f.is_text()) {
        Some(i) => {
            let (fname, field) = elements[i];
            let set = if field.optional { "Some(text)" } else { "text" };
            let _ = writeln!(
                out,
                "                {X}::Ev::Text(text) => {{ if seen[{i}] {{ return Err({X}::duplicate(\"$text\")); }} seen[{i}] = true; last = {i}; this.{fname} = {set}; continue; }}"
            );
        }
        None => {
            let _ = writeln!(out, "                {X}::Ev::Text(_) => continue,");
        }
    }
    out.push_str("            };\n");
    let _ = writeln!(out, "            match {X}::local(&e) {{");
    for (i, (fname, field)) in elements.iter().enumerate() {
        if field.is_text() {
            continue;
        }
        let key = read_name(fname, field);
        let check = if field.repeated {
            format!("if seen[{i}] && last != {i} {{ return Err({X}::duplicate({key:?})); }}")
        } else {
            format!("if seen[{i}] {{ return Err({X}::duplicate({key:?})); }}")
        };
        let value = match &field.ty {
            FieldType::Scalar => "r.text(empty)?".to_string(),
            FieldType::Struct(inner) => format!("{inner}::read_xml(r, &e, empty)?"),
        };
        let nil =
            format!("if {X}::is_nil(&e) {{ r.skip(&e, empty)?; this.{fname} = None; }} else ");
        let set = match (&field.ty, field.repeated, field.optional) {
            (_, true, _) => format!("this.{fname}.push({value});"),
            (FieldType::Struct(_), false, _) => {
                format!("{nil}{{ this.{fname} = Some(Box::new({value})); }}")
            }
            (FieldType::Scalar, false, true) => format!("{nil}{{ this.{fname} = Some({value}); }}"),
            (FieldType::Scalar, false, false) => format!("this.{fname} = {value};"),
        };
        let _ = writeln!(
            out,
            "                b{key:?} => {{ {check} seen[{i}] = true; last = {i}; {set} }}"
        );
    }
    out.push_str("                _ => {\n                    last = usize::MAX - 1;\n                    r.skip(&e, empty)?;\n                }\n");
    out.push_str(
        "            }\n        }\n        let _ = (seen, last);\n        Ok(this)\n    }\n}\n",
    );
}

/// The name a field is written under: its serde name, which is the Rust field
/// name when the field has no XML binding.
fn written_name(fname: &str, field: &FieldMeta) -> String {
    field.serialized_name().unwrap_or_else(|| fname.to_string())
}

/// Emits one zero-size marker type per declared namespace. Each serializes as
/// its URI (so the root's `@xmlns[:prefix]` field writes the declaration) and
/// deserializes by ignoring whatever the document carries, so a struct that
/// holds it still round-trips and compares equal.
fn generate_namespace_markers(out: &mut String, ns: &NamespaceMeta) {
    if ns.declared.is_empty() {
        return;
    }
    out.push_str("// --- namespace declarations (written on the root element) ---\n");
    for (prefix, uri) in &ns.declared {
        let ty = xmlns_marker_type(prefix);
        let _ = writeln!(
            out,
            "/// `{}=\"{uri}\"`",
            xmlns_attribute(prefix).trim_start_matches('@')
        );
        out.push_str("#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]\n");
        let _ = writeln!(out, "pub struct {ty};");
        let _ = writeln!(out, "#[cfg(test)]\nimpl Serialize for {ty} {{");
        out.push_str(
            "    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {\n",
        );
        let _ = writeln!(out, "        s.serialize_str({uri:?})");
        out.push_str("    }\n}\n");
        let _ = writeln!(out, "#[cfg(test)]\nimpl<'de> Deserialize<'de> for {ty} {{");
        out.push_str(
            "    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {\n",
        );
        let _ = writeln!(
            out,
            "        serde::de::IgnoredAny::deserialize(d).map(|_| {ty})"
        );
        out.push_str("    }\n}\n\n");
    }
}

/// The serde attribute name of a namespace declaration: `@xmlns` for the
/// default namespace, `@xmlns:prefix` otherwise.
fn xmlns_attribute(prefix: &str) -> String {
    if prefix.is_empty() {
        "@xmlns".to_string()
    } else {
        format!("@xmlns:{prefix}")
    }
}

/// The root-struct field holding a namespace marker (`xmlns`, `xmlns_cbc`).
fn xmlns_field(prefix: &str) -> String {
    if prefix.is_empty() {
        "xmlns".to_string()
    } else {
        format!("xmlns_{}", sanitize_ident(prefix))
    }
}

/// The marker type of a namespace (`XmlnsDefault`, `XmlnsCbc`).
fn xmlns_marker_type(prefix: &str) -> String {
    if prefix.is_empty() {
        return "XmlnsDefault".to_string();
    }
    let mut out = String::from("Xmlns");
    let mut upper = true;
    for c in prefix.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(if upper { c.to_ascii_uppercase() } else { c });
            upper = false;
        } else {
            upper = true;
        }
    }
    out
}

/// A prefix as a Rust identifier fragment (non-alphanumerics become `_`).
fn sanitize_ident(prefix: &str) -> String {
    prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Appends a structural emptiness predicate used by writer pruning to `out`.
///
/// The generated predicate returns true when every field is absent or empty,
/// including for a struct with no fields. Present containers are checked
/// recursively; repeated fields must have no items, even if their items are
/// themselves empty.
fn generate_is_empty_impl(out: &mut String, name: &str, meta: &StructMeta) {
    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    pub fn is_empty(&self) -> bool {\n");

    let mut exprs = meta.ordered_fields().into_iter().map(|(fname, field)| {
        if field.repeated {
            format!("self.{fname}.is_empty()")
        } else if field.optional || matches!(field.ty, FieldType::Struct(_)) {
            // `Option`-typed: optional scalar or boxed interior container
            // (`Box` derefs transparently to the struct's own `is_empty`).
            format!("self.{fname}.as_ref().map_or(true, |value| value.is_empty())")
        } else {
            format!("self.{fname}.is_empty()")
        }
    });

    if let Some(first) = exprs.next() {
        let _ = writeln!(out, "        {first}");
        for expr in exprs {
            let _ = writeln!(out, "            && {expr}");
        }
    } else {
        out.push_str("        true\n");
    }

    out.push_str("    }\n");
    out.push_str("}\n");
}

/// The Rust type of a source field, per its `Option`/`Vec`/struct markers.
///
/// Scalars are inline strings (`CompactString`, values ≤ 24 bytes stay off the
/// heap). Non-repeated interior structs are `Option<Box<…>>`: an absent
/// subtree costs one `None` instead of a full inline `Default` struct — the
/// per-item struct width, not string data, dominates peak memory on
/// line-dense documents.
fn source_field_type(field: &FieldMeta) -> String {
    match &field.ty {
        FieldType::Scalar => {
            if field.repeated {
                "Vec<CompactString>".to_string()
            } else if field.optional {
                "Option<CompactString>".to_string()
            } else {
                "CompactString".to_string()
            }
        }
        FieldType::Struct(name) => {
            if field.repeated {
                format!("Vec<{name}>")
            } else {
                format!("Option<Box<{name}>>")
            }
        }
    }
}

/// The `#[serde(...)]` attribute line for a source field, or `None` when no
/// rename/default is needed. A prefixed element gets a split rename: the
/// prefixed name is written, the bare local name is what reading matches.
pub(super) fn serde_attr(field: &FieldMeta) -> Option<String> {
    // A logical field is never on the wire: its items travel in the physical
    // field, moved across by `demux`/`mux`.
    if field.is_logical() {
        return Some("#[cfg_attr(test, serde(skip))]".to_string());
    }
    let mut parts: Vec<String> = Vec::new();
    if let (Some(xml), Some(written)) = (&field.xml, field.serialized_name()) {
        if written == *xml {
            parts.push(format!("rename = {xml:?}"));
        } else {
            parts.push(format!(
                "rename(serialize = {written:?}, deserialize = {xml:?})"
            ));
        }
    }
    if field.repeated {
        parts.push("default".to_string());
        parts.push("skip_serializing_if = \"Vec::is_empty\"".to_string());
    } else if field.optional || matches!(field.ty, FieldType::Struct(_)) {
        // Optional scalars and interior containers are both `Option`-typed
        // (containers as `Option<Box<…>>`): `default` keeps a document that
        // omits the element parseable, and the writer only materializes a
        // container when it assigns a value into it, so `Option::is_none`
        // suppresses empty subtrees on write.
        parts.push("default".to_string());
        parts.push("skip_serializing_if = \"Option::is_none\"".to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("#[cfg_attr(test, serde({}))]", parts.join(", ")))
    }
}

/// Emits `from_xml` / `to_xml` for the root struct. `to_xml` writes the XML
/// declaration and the root tag qualified with the mapping's `root_ns`, with
/// the generated `write_xml`; `to_xml_serde` (tests only) writes the same
/// through quick-xml's serializer, the reference `to_xml` must equal.
pub(super) fn generate_xml_io(out: &mut String, root: &str, ns: &NamespaceMeta) {
    let _ = writeln!(
        out,
        "/// Deserializes source XML bytes into the typed `{root}`."
    );
    let _ = writeln!(
        out,
        "pub fn from_xml(bytes: &[u8]) -> Result<{root}, quick_xml::DeError> {{"
    );
    out.push_str("    let s = std::str::from_utf8(bytes)\n");
    out.push_str(
        "        .map_err(|e| quick_xml::DeError::Custom(format!(\"invalid utf-8: {e}\")))?;\n",
    );
    out.push_str("    let mut r = crate::xmlread::XmlIn::new(s);\n");
    out.push_str("    let (start, empty) = r.root()?;\n");
    let _ = writeln!(out, "    {root}::read_xml(&mut r, &start, empty)");
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "/// `from_xml` through quick-xml's serde deserializer: the reference the\n\
         /// generated reader is compared against.\n\
         #[cfg(test)]\n\
         pub fn from_xml_serde(bytes: &[u8]) -> Result<{root}, quick_xml::DeError> {{"
    );
    out.push_str("    let s = std::str::from_utf8(bytes)\n");
    out.push_str(
        "        .map_err(|e| quick_xml::DeError::Custom(format!(\"invalid utf-8: {e}\")))?;\n",
    );
    out.push_str("    quick_xml::de::from_str(s)\n");
    out.push_str("}\n\n");

    let _ = writeln!(
        out,
        "/// Serializes a typed `{root}` back into XML, with the XML declaration and\n\
         /// the namespace-qualified root tag, into a buffer reserved for `capacity`\n\
         /// bytes (a size hint: the buffer still grows past it)."
    );
    let declaration =
        "    out.push_str(\"<?xml version=\\\"1.0\\\" encoding=\\\"UTF-8\\\"?>\\n\");\n";
    let qualified = ns.qualified_root(root);
    // ponytail: the `Result` never fails since the writer is generated; it
    // keeps the engine's serialize error path for the serde reference.
    let _ = writeln!(
        out,
        "pub fn to_xml(source: &{root}, capacity: usize) -> Result<String, quick_xml::SeError> {{"
    );
    out.push_str("    let mut out = String::with_capacity(capacity);\n");
    out.push_str(declaration);
    let _ = writeln!(out, "    source.write_xml({qualified:?}, &mut out);");
    out.push_str("    Ok(out)\n}\n\n");

    let _ = writeln!(
        out,
        "/// `to_xml` through quick-xml's serde serializer: the reference the\n\
         /// generated writer is tested against.\n\
         #[cfg(test)]\n\
         pub fn to_xml_serde(source: &{root}) -> Result<String, quick_xml::SeError> {{"
    );
    out.push_str("    let mut out = String::new();\n");
    out.push_str(declaration);
    let _ = writeln!(
        out,
        "    quick_xml::se::to_writer_with_root(&mut out, {qualified:?}, source)?;"
    );
    out.push_str("    Ok(out)\n}\n");
}
