//! The source-model metadata types.
//!
//! Rust has no general runtime reflection, so the compiler validates source
//! paths against *metadata* describing the typed source struct tree. This module
//! defines that vocabulary — the model, its structs and fields, and the
//! resolution outcome / error types — shared by the resolver
//! ([`super::resolve`]) and the synthesizer ([`super::synth`]).

use std::collections::BTreeMap;

/// Metadata for one typed source model.
///
/// Synthesized from the mapping nodes via
/// [`synthesize_source_model`](super::synthesize_source_model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceModelMeta {
    /// Model id (e.g. `ubl-invoice:2.1`), from `[meta].source_model` or derived
    /// from `doc_format`/`format_version`.
    pub model_id: String,
    /// Name of the root struct in [`Self::structs`].
    pub root: String,
    /// All named structs reachable from the root.
    pub structs: BTreeMap<String, StructMeta>,
}

/// A named struct's fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructMeta {
    /// Fields by Rust identifier (lookup order). Emission order is
    /// [`StructMeta::ordered_fields`].
    pub fields: BTreeMap<String, FieldMeta>,
}

impl StructMeta {
    /// The fields in **emission order**: attributes first, then the element's
    /// own text (`$text`), then child elements by declaration [`FieldMeta::order`]
    /// (field name breaks ties).
    ///
    /// Codegen declares the generated struct's fields in this order, and serde
    /// serializes struct fields in declaration order, so this is the order of
    /// sibling elements in emitted XML. Attributes and text have no schema
    /// order of their own; child elements follow the mapping's declaration
    /// order, which the author keeps aligned with the XSD sequence.
    pub fn ordered_fields(&self) -> Vec<(&String, &FieldMeta)> {
        fn rank(field: &FieldMeta) -> u8 {
            if field.is_attribute() {
                0
            } else if field.is_text() {
                1
            } else {
                2
            }
        }
        let mut fields: Vec<(&String, &FieldMeta)> = self.fields.iter().collect();
        fields.sort_by(|(name_a, a), (name_b, b)| {
            (rank(a), a.order, *name_a).cmp(&(rank(b), b.order, *name_b))
        });
        fields
    }
}

/// One field of a source struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMeta {
    /// Wrapped in `Option<...>` (a `None` resolves to missing).
    pub optional: bool,
    /// Wrapped in `Vec<...>` (a collection / multiple values).
    pub repeated: bool,
    /// What the field holds.
    pub ty: FieldType,
    /// The XML element/attribute name this field binds to (e.g. `ID`,
    /// `@currencyID`, `$text`). Drives the serde rename on the generated source
    /// struct; `None` means use the field name verbatim.
    pub xml: Option<String>,
    /// Emission order among the struct's fields: the declaration position of
    /// the first mapping node that contributes to this field. An inferred
    /// interior element inherits the position of its first declared
    /// descendant. See [`StructMeta::ordered_fields`].
    pub order: usize,
}

impl FieldMeta {
    /// Whether two definitions bind the same shape and XML name. Emission
    /// `order` is deliberately excluded: two nodes contributing to one field
    /// (a valued element and its attribute, two leaves under one interior)
    /// legitimately carry different positions, and the field takes the
    /// earliest.
    pub fn same_binding(&self, other: &FieldMeta) -> bool {
        self.optional == other.optional
            && self.repeated == other.repeated
            && self.ty == other.ty
            && self.xml == other.xml
    }

    /// Whether this field binds an XML attribute (`@name`).
    pub fn is_attribute(&self) -> bool {
        self.xml.as_deref().is_some_and(|xml| xml.starts_with('@'))
    }

    /// Whether this field binds the element's own text content (`$text`).
    pub fn is_text(&self) -> bool {
        self.xml.as_deref() == Some("$text")
    }
}

/// The element type of a field, after stripping `Option`/`Vec` wrappers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// A scalar leaf (its decode type is decided by the mapping node's `type`).
    Scalar,
    /// A nested struct, named in [`SourceModelMeta::structs`].
    Struct(String),
}

/// The outcome of resolving a path against the source model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedField {
    /// Any segment along the path was `Vec<...>` (the value can repeat).
    pub repeated: bool,
    /// Any segment along the path was `Option<...>` (the value can be missing).
    pub optional: bool,
    /// The final field is a nested struct rather than a scalar leaf.
    pub is_struct: bool,
    /// The element struct name when the final field is a struct (used to resolve
    /// collection-child paths against the collection item).
    pub struct_name: Option<String>,
}

/// Why a source path failed to resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path is empty.
    #[error("empty source path")]
    Empty,
    /// The root struct named by the model is missing from the struct table.
    #[error("root struct `{0}` is not defined in the source model")]
    UnknownRoot(String),
    /// A segment names a field that does not exist on the current struct.
    #[error("field `{field}` does not exist on struct `{struct_name}`")]
    UnknownField {
        /// The struct being indexed.
        struct_name: String,
        /// The missing field name.
        field: String,
    },
    /// A non-final segment is a scalar leaf, so it cannot be descended into.
    #[error("cannot descend into scalar field `{field}` of struct `{struct_name}`")]
    NotAStruct {
        /// The struct being indexed.
        struct_name: String,
        /// The scalar field that was treated as a struct.
        field: String,
    },
}

/// Programmatic builder for [`SourceModelMeta`], used by tests (production
/// synthesizes it from the mapping nodes via
/// [`synthesize_source_model`](super::synthesize_source_model)).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct SourceModelBuilder {
    model_id: String,
    root: String,
    structs: BTreeMap<String, StructMeta>,
}

#[cfg(test)]
impl SourceModelBuilder {
    /// Starts a builder for `model_id` whose root struct is `root`.
    pub fn new(model_id: &str, root: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            root: root.to_string(),
            structs: BTreeMap::new(),
        }
    }

    /// Adds (or replaces) a struct and its fields.
    ///
    /// Each field is `(name, optional, repeated, FieldType)`; emission order is
    /// the slice order.
    pub fn struct_def(mut self, name: &str, fields: &[(&str, bool, bool, FieldType)]) -> Self {
        let mut meta = StructMeta::default();
        for (order, (fname, optional, repeated, ty)) in fields.iter().enumerate() {
            meta.fields.insert(
                (*fname).to_string(),
                FieldMeta {
                    optional: *optional,
                    repeated: *repeated,
                    ty: ty.clone(),
                    xml: None,
                    order,
                },
            );
        }
        self.structs.insert(name.to_string(), meta);
        self
    }

    /// Finalizes the metadata.
    pub fn build(self) -> SourceModelMeta {
        SourceModelMeta {
            model_id: self.model_id,
            root: self.root,
            structs: self.structs,
        }
    }
}

/// The shared `ubl-invoice:2.1` fixture model used across the resolver tests.
///
/// ```text
/// root Invoice { id: String, uuid: Option<String>,
///   monetary_total: LegalMonetaryTotal,
///   invoice_lines: Vec<InvoiceLine> }
/// LegalMonetaryTotal { payable_amount: Amount }
/// Amount { value: String, currency_id: String }
/// InvoiceLine { id: String }
/// ```
#[cfg(test)]
pub(crate) fn ubl() -> SourceModelMeta {
    use FieldType::{Scalar, Struct};
    SourceModelBuilder::new("ubl-invoice:2.1", "Invoice")
        .struct_def(
            "Invoice",
            &[
                ("id", false, false, Scalar),
                ("uuid", true, false, Scalar),
                (
                    "monetary_total",
                    false,
                    false,
                    Struct("LegalMonetaryTotal".into()),
                ),
                ("invoice_lines", false, true, Struct("InvoiceLine".into())),
            ],
        )
        .struct_def(
            "LegalMonetaryTotal",
            &[("payable_amount", false, false, Struct("Amount".into()))],
        )
        .struct_def(
            "Amount",
            &[
                ("value", false, false, Scalar),
                ("currency_id", false, false, Scalar),
            ],
        )
        .struct_def("InvoiceLine", &[("id", false, false, Scalar)])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(xml: &str, order: usize) -> FieldMeta {
        FieldMeta {
            optional: true,
            repeated: false,
            ty: FieldType::Scalar,
            xml: Some(xml.to_string()),
            order,
        }
    }

    #[test]
    fn test_ordered_fields_attributes_then_text_then_elements_by_order() {
        let mut meta = StructMeta::default();
        // Inserted in an order that disagrees with both name and position.
        meta.fields.insert("zeta".into(), field("Zeta", 5));
        meta.fields.insert("value".into(), field("$text", 9));
        meta.fields.insert("alpha".into(), field("Alpha", 7));
        meta.fields
            .insert("currency_id".into(), field("@currencyID", 8));
        let names: Vec<&str> = meta
            .ordered_fields()
            .into_iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["currency_id", "value", "zeta", "alpha"]);
    }

    #[test]
    fn test_ordered_fields_breaks_equal_order_by_name() {
        let mut meta = StructMeta::default();
        meta.fields.insert("b".into(), field("B", 1));
        meta.fields.insert("a".into(), field("A", 1));
        let names: Vec<&str> = meta
            .ordered_fields()
            .into_iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn test_same_binding_ignores_order_only() {
        let a = field("ID", 1);
        let b = field("ID", 42);
        assert!(a.same_binding(&b));
        assert_ne!(a, b, "full equality still sees the order");
        let mut c = field("ID", 1);
        c.optional = false;
        assert!(!a.same_binding(&c));
        let d = field("Id", 1);
        assert!(!a.same_binding(&d));
    }

    #[test]
    fn test_builder_assigns_slice_order() {
        let model = ubl();
        let names: Vec<&str> = model.structs["Invoice"]
            .ordered_fields()
            .into_iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["id", "uuid", "monetary_total", "invoice_lines"]);
    }
}
