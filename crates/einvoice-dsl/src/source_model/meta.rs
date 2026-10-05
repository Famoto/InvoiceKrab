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
    /// Name of the root struct in [`Self::structs`] — the root element's
    /// *local* name (the prefix lives in [`Self::namespaces`]).
    pub root: String,
    /// All named structs reachable from the root.
    pub structs: BTreeMap<String, StructMeta>,
    /// The namespaces the written document declares and the root's prefix.
    pub namespaces: NamespaceMeta,
}

/// The namespace declarations of a source model: what the root element emits
/// as `xmlns` attributes and which prefix the root itself carries. Element
/// prefixes live on each [`FieldMeta::prefix`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamespaceMeta {
    /// Prefix of the root element (`""` = unprefixed).
    pub root_prefix: String,
    /// Declared namespaces, prefix → URI; `""` is the default namespace.
    pub declared: BTreeMap<String, String>,
}

impl NamespaceMeta {
    /// The root element's qualified name as written (`rsm:CrossIndustryInvoice`,
    /// or the bare local name when the root is unprefixed).
    pub fn qualified_root(&self, root: &str) -> String {
        qualify(&self.root_prefix, root)
    }
}

/// `prefix:local`, or `local` when `prefix` is empty.
pub fn qualify(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_string()
    } else {
        format!("{prefix}:{local}")
    }
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
        /// Assigns sort priority: attributes (0), element text (1), children (2).
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

    /// The physical repeated fields that logical fields partition, each with
    /// its logical fields in emission order: `(physical, [(logical, meta)])`.
    /// The physical fields come in emission order too.
    pub fn aliased_fields(&self) -> Vec<(&String, Vec<(&String, &FieldMeta)>)> {
        let ordered = self.ordered_fields();
        let mut out: Vec<(&String, Vec<(&String, &FieldMeta)>)> = Vec::new();
        for (physical, _) in &ordered {
            let logical: Vec<(&String, &FieldMeta)> = ordered
                .iter()
                .filter(|(_, f)| f.alias.as_ref().is_some_and(|a| &a.physical == *physical))
                .copied()
                .collect();
            if !logical.is_empty() {
                out.push((physical, logical));
            }
        }
        out
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
    /// The XML element/attribute *local* name this field binds to (e.g. `ID`,
    /// `@currencyID`, `$text`). Drives the serde rename on the generated source
    /// struct; `None` means use the field name verbatim.
    pub xml: Option<String>,
    /// Namespace prefix the element is *written* with (`""` = unprefixed).
    /// Reading ignores prefixes, so this only shapes the serialize-side rename.
    /// Always empty for attributes and `$text`.
    pub prefix: String,
    /// Interior struct fields only: the writer always materializes this element,
    /// even empty (a structural node with `required = true`, for schemas that
    /// make the element mandatory). Merged by *or* when several nodes
    /// contribute to the field.
    pub always_present: bool,
    /// Emission order among the struct's fields: the declaration position of
    /// the first mapping node that contributes to this field. An inferred
    /// interior element inherits the position of its first declared
    /// descendant. See [`StructMeta::ordered_fields`].
    pub order: usize,
    /// Set on a *logical* field: one that is not serialized itself but holds
    /// the items of a sibling *physical* repeated field selected by a `match`
    /// selector (or left over by every selector). The generated reader fills it
    /// from the physical field before mapping and the writer merges it back
    /// afterwards. `None` for every ordinary field.
    pub alias: Option<AliasBinding>,
}

/// How a logical field partitions its physical element's items.
///
/// Several logical nodes of a mapping may bind one physical XML element
/// (`AdditionalReferencedDocument` with `TypeCode` 50 / 130 / 916). The
/// physical element is synthesized once, as a repeated field holding the union
/// item struct; each logical node gets a field of its own, carrying this
/// binding, that the reader fills with the items its selector picks (the first
/// such item for a non-repeated logical field) and the writer drains back into
/// the physical field, setting the selector values on every item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasBinding {
    /// The physical field, in the same struct, whose items this field partitions.
    pub physical: String,
    /// The id of the logical mapping node (named in runtime diagnostics).
    pub node: String,
    /// The selector as `(field path inside the item struct, expected text)`
    /// pairs, in key order. Empty for the default bucket: the logical field
    /// that takes every item no selector matched.
    pub selector: Vec<(String, String)>,
}

impl FieldMeta {
    /// Whether two definitions bind the same shape and XML name. Emission
    /// `order` and `always_present` are deliberately excluded: two nodes
    /// contributing to one field (a valued element and its attribute, two leaves
    /// under one interior, a structural node and a leaf beneath it)
    /// legitimately differ there, and the field takes the earliest order and
    /// the *or* of the presence flags.
    pub fn same_binding(&self, other: &FieldMeta) -> bool {
        self.optional == other.optional
            && self.repeated == other.repeated
            && self.ty == other.ty
            && self.xml == other.xml
            && self.prefix == other.prefix
            && self.alias == other.alias
    }

    /// Whether this is a logical (alias) field: never serialized, filled from
    /// and drained into its physical sibling by the generated mappers.
    pub fn is_logical(&self) -> bool {
        self.alias.is_some()
    }

    /// The name serde *writes* for this field: the prefixed element name, or
    /// the attribute / `$text` binding unchanged.
    pub fn serialized_name(&self) -> Option<String> {
        let xml = self.xml.as_deref()?;
        if self.is_attribute() || self.is_text() {
            Some(xml.to_string())
        } else {
            Some(qualify(&self.prefix, xml))
        }
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
                    prefix: String::new(),
                    always_present: false,
                    order,
                    alias: None,
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
            namespaces: NamespaceMeta::default(),
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
            prefix: String::new(),
            always_present: false,
            order,
            alias: None,
        }
    }

    #[test]
    fn test_serialized_name_qualifies_elements_only() {
        let mut element = field("ID", 0);
        element.prefix = "cbc".into();
        assert_eq!(element.serialized_name().as_deref(), Some("cbc:ID"));
        let plain = field("ID", 0);
        assert_eq!(plain.serialized_name().as_deref(), Some("ID"));
        let mut attr = field("@currencyID", 0);
        attr.prefix = "cbc".into();
        assert_eq!(attr.serialized_name().as_deref(), Some("@currencyID"));
        let mut text = field("$text", 0);
        text.prefix = "cbc".into();
        assert_eq!(text.serialized_name().as_deref(), Some("$text"));
    }

    #[test]
    fn test_same_binding_distinguishes_prefix() {
        let a = field("ID", 0);
        let mut b = field("ID", 0);
        b.prefix = "cbc".into();
        assert!(!a.same_binding(&b));
    }

    #[test]
    fn test_qualified_root() {
        let ns = NamespaceMeta {
            root_prefix: "rsm".into(),
            declared: BTreeMap::new(),
        };
        assert_eq!(
            ns.qualified_root("CrossIndustryInvoice"),
            "rsm:CrossIndustryInvoice"
        );
        assert_eq!(
            NamespaceMeta::default().qualified_root("Invoice"),
            "Invoice"
        );
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
    fn test_aliased_fields_groups_logical_by_physical_in_order() {
        let mut meta = StructMeta::default();
        meta.fields.insert("all_ref".into(), field("Ref", 3));
        let logical = |physical: &str, node: &str, selector: &[(&str, &str)], order| FieldMeta {
            alias: Some(AliasBinding {
                physical: physical.into(),
                node: node.into(),
                selector: selector
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            }),
            xml: None,
            ..field("x", order)
        };
        meta.fields.insert(
            "tender".into(),
            logical("all_ref", "A.Tender", &[("type_code", "50")], 5),
        );
        meta.fields.insert(
            "object".into(),
            logical("all_ref", "A.Object", &[("type_code", "130")], 4),
        );
        meta.fields.insert("plain".into(), field("Plain", 0));
        let groups = meta.aliased_fields();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "all_ref");
        let names: Vec<&str> = groups[0].1.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["object", "tender"],
            "logical fields in emission order"
        );
        assert!(meta.fields["object"].is_logical());
        assert!(!meta.fields["plain"].is_logical());
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
