//! Typed source-struct generation and XML I/O.
//!
//! Emits one `#[derive(...)]` struct per struct in the source model (with
//! serde/XML binding) and the `from_xml` / `to_xml` functions for the root.

use std::fmt::Write as _;

use crate::source_model::{FieldMeta, FieldType, SourceModelMeta, StructMeta};

/// Emits the typed source structs (with serde/XML binding) for every struct in
/// the source model, in deterministic name order.
pub(super) fn generate_source_structs(out: &mut String, source: &SourceModelMeta) {
    out.push_str("// --- typed source structs ---\n");
    for (name, meta) in &source.structs {
        generate_one_struct(out, name, meta);
        out.push('\n');
    }
}

/// Emits one `#[derive(...)]` source struct.
fn generate_one_struct(out: &mut String, name: &str, meta: &StructMeta) {
    out.push_str("#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]\n");
    let _ = writeln!(out, "pub struct {name} {{");
    for (fname, field) in &meta.fields {
        if let Some(attr) = serde_attr(field) {
            let _ = writeln!(out, "    {attr}");
        }
        let _ = writeln!(out, "    pub {fname}: {},", source_field_type(field));
    }
    out.push_str("}\n");
    generate_is_empty_impl(out, name, meta);
}

/// Emits a small structural emptiness predicate used by writer pruning and
/// serde `skip_serializing_if` hooks on generated container fields.
fn generate_is_empty_impl(out: &mut String, name: &str, meta: &StructMeta) {
    let _ = writeln!(out, "impl {name} {{");
    out.push_str("    pub fn is_empty(&self) -> bool {\n");

    let mut exprs = meta.fields.iter().map(|(fname, field)| {
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
/// rename/default is needed.
pub(super) fn serde_attr(field: &FieldMeta) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(xml) = &field.xml {
        parts.push(format!("rename = {xml:?}"));
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

/// Emits `from_xml` / `to_xml` for the root struct.
pub(super) fn generate_xml_io(out: &mut String, root: &str) {
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

    let _ = writeln!(out, "/// Serializes a typed `{root}` back into XML.");
    let _ = writeln!(
        out,
        "pub fn to_xml(source: &{root}) -> Result<String, quick_xml::SeError> {{"
    );
    let _ = writeln!(
        out,
        "    quick_xml::se::to_string_with_root({root:?}, source)"
    );
    out.push_str("}\n");
}
