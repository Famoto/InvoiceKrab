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
    /// Fields by Rust identifier.
    pub fields: BTreeMap<String, FieldMeta>,
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
    /// Each field is `(name, optional, repeated, FieldType)`.
    pub fn struct_def(mut self, name: &str, fields: &[(&str, bool, bool, FieldType)]) -> Self {
        let mut meta = StructMeta::default();
        for (fname, optional, repeated, ty) in fields {
            meta.fields.insert(
                (*fname).to_string(),
                FieldMeta {
                    optional: *optional,
                    repeated: *repeated,
                    ty: ty.clone(),
                    xml: None,
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
