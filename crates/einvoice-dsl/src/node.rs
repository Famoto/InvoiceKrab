//! Source-node model.
//!
//! Two representations:
//!
//! - [`RawNode`] — the as-declared node, every field optional, so an inheritance
//!   override can replace a whole node while omitting fields (which then take
//!   defaults, *not* the parent's value).
//!   Unknown fields are rejected (E001).
//! - [`SourceNode`] — the *effective* node after inheritance, disabled removal,
//!   and default materialization. Every active node has a `source_path` and
//!   `source_type`; this is what the IR, validators, reports, and codegen consume.
//!
//! A node's [`NodeId`] is its full dotted TOML table name (e.g. `Invoice.ID`).
//! The id *mirrors the XML element tree*: its segments are the XML element local
//! names (`Invoice` is the root element/struct, `ID` is a child element). The
//! optional `xml` field marks a leaf as an attribute (`@currencyID`) or element
//! text (`$text`), or renames the element. The compiler *synthesizes* the typed
//! source struct tree and each node's `source_path` from these ids; the author
//! never writes a struct tree.
//!
//! Bidirectionality (N–1–N): the synthesized `source_path` is symmetric — the
//! generated reader reads `source.<path>` into the canonical key, and the writer
//! writes the canonical key back to `source.<path>`. No separate read/write spec.
//!
//! The one asymmetry is `constant`: a node with a `constant` writes that fixed
//! literal on the write side (the hub value, if any, is ignored), while the read
//! side is untouched — with a `canonical_key` the source value still fills the
//! hub, without one the node is write-only. This is how a spoke pins
//! spec-mandated values (CIUS `CustomizationID` URNs, `UBLVersionID`, …) without
//! leaking another format's value into its output.
//!
//! `clone_of` is the second asymmetry: the node mirrors an existing canonical
//! key declared in its scope. The writer fans the key's hub value out to this
//! path too (a format storing one value in several places); the reader never
//! fills the hub from it, only checks the copy against the canonical value and
//! warns (`CLONE_MISMATCH`) when a document's copies disagree.

use serde::Deserialize;

use crate::multiple::MultiplePolicy;
use crate::normalize::NormalizeOp;
use crate::types::MappingType;

/// A source node's stable identifier: its full dotted TOML table name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Wraps a dotted name as a node id.
    pub fn new(id: impl Into<String>) -> Self {
        NodeId(id.into())
    }

    /// The dotted name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The dot-separated segments (e.g. `Invoice.ID` → `["Invoice", "ID"]`).
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    /// The id of the immediate parent table, or `None` at the top level.
    pub fn parent(&self) -> Option<NodeId> {
        self.0.rsplit_once('.').map(|(head, _)| NodeId::new(head))
    }

    /// Whether `self` is a descendant of `ancestor` (strict prefix on segments).
    pub fn is_descendant_of(&self, ancestor: &NodeId) -> bool {
        self.0
            .strip_prefix(&ancestor.0)
            .is_some_and(|rest| rest.starts_with('.'))
    }

    /// The nearest ancestor for which `is_collection` returns true, as a
    /// [`Scope::Collection`]; or [`Scope::Root`] when no ancestor qualifies.
    ///
    /// Shared by source-model synthesis and default application so both compute
    /// scopes identically; they differ only in how they recognize a collection
    /// node.
    pub fn nearest_collection_scope(&self, is_collection: impl Fn(&NodeId) -> bool) -> Scope {
        let mut ancestor = self.parent();
        while let Some(a) = ancestor {
            if is_collection(&a) {
                return Scope::Collection(a);
            }
            ancestor = a.parent();
        }
        Scope::Root
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        NodeId::new(s)
    }
}

/// The evaluation scope of a node.
///
/// The root mapping scope is the invoice root; a collection node creates a child
/// scope for its descendants.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The invoice root.
    Root,
    /// Inside the collection identified by this node id.
    Collection(NodeId),
}

/// A node exactly as declared, before inheritance and defaults.
///
/// Every field is optional so an override can replace a node yet omit fields.
/// Unknown fields are rejected (E001). The set of "source-node fields" here is
/// also what marks a TOML table as a node rather than a container.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNode {
    /// Leaf XML binding override. Marks the node's
    /// final id segment as an attribute (`@currencyID`) or element text (`$text`),
    /// or renames the element. Absent means the element local name equals the id
    /// segment. Interior segments are always taken verbatim from the id.
    pub xml: Option<String>,
    /// Value type.
    #[serde(rename = "type")]
    pub ty: Option<MappingType>,
    /// Target field in the canonical model.
    pub canonical_key: Option<String>,
    /// Whether the value is required.
    pub required: Option<bool>,
    /// Fallback node ids, in declared order.
    pub fallbacks: Option<Vec<String>>,
    /// Human description (reports only).
    pub description: Option<String>,
    /// Minimum item count for a collection node.
    pub min_items: Option<usize>,
    /// Repeated-scalar policy.
    pub multiple: Option<MultiplePolicy>,
    /// Separator, required iff `multiple = "join"`.
    pub join_with: Option<String>,
    /// Normalization operations, in declared order.
    pub normalize: Option<Vec<NormalizeOp>>,
    /// Compiler-known adapter name.
    pub adapter: Option<String>,
    /// Fixed write-side value: the writer always emits this literal at the
    /// node's source path, ignoring the hub. Read side is unaffected.
    pub constant: Option<String>,
    /// Canonical key this node mirrors: the writer fans the key's hub value out
    /// to this path too; the reader checks the copy against the canonical value
    /// (`CLONE_MISMATCH`). Mutually exclusive with `canonical_key`.
    pub clone_of: Option<String>,
    /// Whether the node is removed from the effective mapping.
    pub disabled: Option<bool>,
}

impl RawNode {
    /// Whether this node is disabled (default `false`).
    pub fn is_disabled(&self) -> bool {
        self.disabled.unwrap_or(false)
    }

    /// Whether this raw node carries any *active* source-node field — i.e. any
    /// field beyond a bare `disabled` / `description`. A disabled-only override
    /// (`disabled = true` plus optional `description`) is still a node, so the
    /// caller distinguishes that case via [`RawNode::is_disabled`].
    pub fn has_active_field(&self) -> bool {
        self.xml.is_some()
            || self.ty.is_some()
            || self.canonical_key.is_some()
            || self.required.is_some()
            || self.fallbacks.is_some()
            || self.min_items.is_some()
            || self.multiple.is_some()
            || self.join_with.is_some()
            || self.normalize.is_some()
            || self.adapter.is_some()
            || self.constant.is_some()
            || self.clone_of.is_some()
    }
}

/// An effective node after inheritance, disabled removal, and defaults.
///
/// Active nodes always have a `source_path` and `source_type`. Defaults are
/// materialized here so consumers never distinguish omitted from defaulted
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceNode {
    /// Stable node id.
    pub id: NodeId,
    /// Evaluation scope (root or an enclosing collection).
    pub scope: Scope,
    /// Field path into the *synthesized* typed source model, relative to the
    /// node's scope struct.
    pub source_path: String,
    /// Value type.
    pub source_type: MappingType,
    /// Target canonical field, or `None` for a fallback-only helper node.
    pub canonical_key: Option<String>,
    /// Whether the value is required.
    pub required: bool,
    /// Fallback node ids in declared order.
    pub fallbacks: Vec<NodeId>,
    /// Repeated-scalar policy. `None` means the node is strictly single-valued
    /// (its source field is not a `Vec`; a repeated element fails
    /// deserialization). `Some(policy)` makes the source field `Vec<String>`
    /// and collapses the values per the policy. Unlike the other fields this is
    /// not defaulted away: whether `multiple` was declared changes the
    /// synthesized source shape.
    pub multiple: Option<MultiplePolicy>,
    /// Minimum item count for a collection node.
    pub min_items: Option<usize>,
    /// Join separator (present iff `multiple = Join`).
    pub join_with: Option<String>,
    /// Normalization operations in declared order.
    pub normalize: Vec<NormalizeOp>,
    /// Compiler-known adapter name.
    pub adapter: Option<String>,
    /// Fixed write-side value (writer emits this literal, hub ignored on write).
    pub constant: Option<String>,
    /// Canonical key this node mirrors (write fan-out + read consistency check).
    pub clone_of: Option<String>,
    /// Human description.
    pub description: Option<String>,
}

impl SourceNode {
    /// Whether this is a collection node (opens a child scope).
    pub fn is_collection(&self) -> bool {
        self.source_type.is_collection()
    }

    /// Whether this node is a fallback-only helper (no canonical target).
    pub fn is_helper(&self) -> bool {
        self.canonical_key.is_none()
    }

    /// The effective minimum item count for a collection node: an explicit
    /// `min_items`, else 1 when `required`, else 0.
    pub fn effective_min_items(&self) -> usize {
        self.min_items.unwrap_or(if self.required { 1 } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_id_segments_and_parent() {
        let id = NodeId::new("Invoice.Line.ID");
        assert_eq!(id.segments().collect::<Vec<_>>(), ["Invoice", "Line", "ID"]);
        assert_eq!(id.parent(), Some(NodeId::new("Invoice.Line")));
        assert_eq!(NodeId::new("Invoice").parent(), None);
    }

    #[test]
    fn test_node_id_descendant() {
        let line = NodeId::new("InvoiceLine");
        assert!(NodeId::new("InvoiceLine.ID").is_descendant_of(&line));
        // Prefix that is not a segment boundary is not a descendant.
        assert!(!NodeId::new("InvoiceLineExtra").is_descendant_of(&line));
        assert!(!line.is_descendant_of(&line));
    }

    #[test]
    fn test_raw_node_unknown_field_rejected() {
        assert!(toml::from_str::<RawNode>(r#"bogus = 1"#).is_err());
    }

    #[test]
    fn test_raw_node_all_fields_optional() {
        let n: RawNode = toml::from_str("").unwrap();
        assert_eq!(n, RawNode::default());
        assert!(!n.has_active_field());
        assert!(!n.is_disabled());
    }

    #[test]
    fn test_raw_node_disabled_only_has_no_active_field() {
        let n: RawNode = toml::from_str("disabled = true\ndescription = \"gone\"").unwrap();
        assert!(n.is_disabled());
        assert!(!n.has_active_field());
    }

    #[test]
    fn test_raw_node_type_marks_active() {
        let n: RawNode = toml::from_str(r#"type = "identifier""#).unwrap();
        assert!(n.has_active_field());
        assert_eq!(n.ty, Some(MappingType::Identifier));
    }

    #[test]
    fn test_raw_node_xml_marks_active() {
        let n: RawNode = toml::from_str(r#"xml = "@currencyID""#).unwrap();
        assert!(n.has_active_field());
        assert_eq!(n.xml.as_deref(), Some("@currencyID"));
    }

    #[test]
    fn test_raw_node_constant_marks_active() {
        let n: RawNode = toml::from_str(r#"constant = "2.1""#).unwrap();
        assert!(n.has_active_field());
        assert_eq!(n.constant.as_deref(), Some("2.1"));
    }
}
