//! Typed source-struct generation and XML I/O.
//!
//! Emits one `#[derive(...)]` struct per struct in the source model (with
//! serde/XML binding) and the `from_xml` / `to_xml` functions for the root.
//!
//! Struct fields are declared in the model's **emission order**
//! ([`StructMeta::ordered_fields`]: attributes, then element text, then child
//! elements by declaration position). serde serializes struct fields in
//! declaration order, so this is what makes the writer emit sibling elements in
//! the order the mapping declares them — the schema's sequence order.
//!
//! Namespaces: reading is namespace-agnostic (quick-xml's deserializer matches
//! local names), so every element field is renamed with a *split* rename —
//! `rename(serialize = "cbc:ID", deserialize = "ID")` — that writes the
//! prefixed name and still reads the bare one. The declarations themselves are
//! zero-size **marker types** (`XmlnsCbc` serializes as its URI) that the root
//! struct carries as `@xmlns[:prefix]` attribute fields, and `to_xml` writes the
//! XML declaration plus the prefixed root tag.
//!
//! Aliased elements: a *logical* field ([`FieldMeta::is_logical`]) is
//! `#[serde(skip)]`; its items live in the struct's *physical* repeated field
//! on the wire. Every struct that holds such an element, directly or beneath
//! it, gets a `demux` method (physical → logical, by `match` selector; the
//! reader calls it on the root before mapping) and a `mux` method (logical →
//! physical, writing the selector values as discriminators; the writer calls it
//! on the root after mapping).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::source_model::{FieldMeta, FieldType, NamespaceMeta, SourceModelMeta, StructMeta};

use super::access::{access_expr, assign_target_expr};

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

    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    /// Partitions each aliased element's items into its logical fields by\n");
    out.push_str(
        "    /// `match` selector, recursing into children. `overflow(node, extra)` reports\n",
    );
    out.push_str(
        "    /// a single-valued logical node that matched `extra` items beyond the one kept.\n",
    );
    out.push_str(
        "    pub fn demux(&mut self, overflow: &mut dyn FnMut(&'static str, usize)) {
",
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
                .map(|(path, value)| {
                    format!(
                        "{}.is_some_and(|v| v.trim() == {value:?})",
                        access_expr(source, &item_struct, path, "item")
                    )
                })
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
            child.demux(overflow);
        }}"
            );
        } else {
            let _ = writeln!(
                out,
                "        if let Some(child) = self.{fname}.as_mut() {{
            child.demux(overflow);
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
    out.push_str(
        "    pub fn mux(&mut self) {
",
    );
    for (fname, f) in &children {
        if f.repeated {
            let _ = writeln!(
                out,
                "        for child in &mut self.{fname} {{
            child.mux();
        }}"
            );
        } else {
            let _ = writeln!(
                out,
                "        if let Some(child) = self.{fname}.as_mut() {{
            child.mux();
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
                .map(|(path, value)| {
                    format!(
                        "{} = Some(CompactString::from({value:?}));",
                        assign_target_expr(source, &item_struct, path, "item")
                    )
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

/// Emits one `#[derive(...)]` source struct, fields in emission order. The root
/// struct (`root_ns` given) first carries one `@xmlns[:prefix]` marker field per
/// declared namespace, so every emitted document declares them.
fn generate_one_struct(
    out: &mut String,
    name: &str,
    meta: &StructMeta,
    root_ns: Option<&NamespaceMeta>,
) {
    out.push_str("#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]\n");
    let _ = writeln!(out, "pub struct {name} {{");
    if let Some(ns) = root_ns {
        for prefix in ns.declared.keys() {
            let _ = writeln!(
                out,
                "    #[serde(rename = {:?}, default)]",
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
        let _ = writeln!(out, "impl Serialize for {ty} {{");
        out.push_str(
            "    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {\n",
        );
        let _ = writeln!(out, "        s.serialize_str({uri:?})");
        out.push_str("    }\n}\n");
        let _ = writeln!(out, "impl<'de> Deserialize<'de> for {ty} {{");
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
        return Some("#[serde(skip)]".to_string());
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
        Some(format!("#[serde({})]", parts.join(", ")))
    }
}

/// Emits `from_xml` / `to_xml` for the root struct. `to_xml` writes the XML
/// declaration and the root tag qualified with the mapping's `root_ns`.
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
    out.push_str("    quick_xml::de::from_str(s)\n");
    out.push_str("}\n\n");

    let _ = writeln!(
        out,
        "/// Serializes a typed `{root}` back into XML, with the XML declaration and\n\
         /// the namespace-qualified root tag."
    );
    let _ = writeln!(
        out,
        "pub fn to_xml(source: &{root}) -> Result<String, quick_xml::SeError> {{"
    );
    out.push_str(
        "    let mut out = String::from(\"<?xml version=\\\"1.0\\\" encoding=\\\"UTF-8\\\"?>\\n\");\n",
    );
    let _ = writeln!(
        out,
        "    quick_xml::se::to_writer_with_root(&mut out, {:?}, source)?;",
        ns.qualified_root(root)
    );
    out.push_str("    Ok(out)\n}\n");
}
