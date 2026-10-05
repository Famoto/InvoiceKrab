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
//!
//! A table that declares only `ns` and/or `required = true` (plus
//! `description`/`disabled`) and no `type` is a **structural node**: it names an
//! inferred interior element to give it a namespace prefix the
//! `[meta.ns_defaults]` would not, or to have the writer always materialize it
//! (`required = true`: the element is emitted even when empty, for schemas that
//! make it mandatory). Structural nodes never become [`SourceNode`]s; synthesis
//! reads them when it creates the interior struct field they describe.
//!
//! A collection node or a structural node may carry a `match` selector
//! (`match = { "TypeCode" = "916" }`): the node binds only the occurrences of
//! its element whose child values equal the selector's. Several such *logical*
//! nodes may share one *physical* element — a second node names the element
//! through `xml = "AdditionalReferencedDocument"` while its own id segment is a
//! free alias — so a format that tells its references apart by a type code
//! maps each kind to its own canonical key. The physical element is then
//! synthesized as one repeated field whose items the generated reader
//! partitions by selector and the generated writer re-merges with the
//! selector values written back as discriminators.
//!
//! `clone_of` may reach outside the node's scope: `$parent.Key` names a key of
//! the enclosing collection's scope and `$root.Key` a key of the invoice root
//! ([`parse_derivation`]) — how every line amount gets the document currency.
//!
//! Every raw node also carries its declaration [`RawNode::position`]: a
//! mapping's **declaration order is its schema order**. The parser records
//! where each table appears, synthesis orders the source-struct fields by it,
//! and the writer therefore emits sibling XML elements in the order the mapping
//! declares them — which the author keeps aligned with the XSD sequence.

use std::collections::BTreeMap;

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
    /// XML binding override. On a leaf, marks the node's final id segment as an
    /// attribute (`@currencyID`) or element text (`$text`), or renames the
    /// element. On a collection or structural node, names the physical element
    /// the node binds — so two logical nodes with different ids can share one
    /// element (see `match`). Absent means the element local name equals the
    /// id segment. Interior segments are always taken verbatim from the id.
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
    /// Repeated-scalar policy.
    pub multiple: Option<MultiplePolicy>,
    /// Separator, required iff `multiple = "join"`.
    pub join_with: Option<String>,
    /// Normalization operations, in declared order.
    pub normalize: Option<Vec<NormalizeOp>>,
    /// Fixed write-side value: the writer always emits this literal at the
    /// node's source path, ignoring the hub. Read side is unaffected.
    pub constant: Option<String>,
    /// Canonical key this node mirrors: the writer fans the key's hub value out
    /// to this path too; the reader checks the copy against the canonical value
    /// (`CLONE_MISMATCH`). Mutually exclusive with `canonical_key`.
    pub clone_of: Option<String>,
    /// Whether the node is removed from the effective mapping.
    pub disabled: Option<bool>,
    /// Inheritance: when `true`, this declaration *replaces* the inherited node
    /// whole instead of merging its fields over it. Never carried into the
    /// effective node.
    pub replace: Option<bool>,
    /// Lexical codec id (from `config/codecs/`): decodes the source text into
    /// the canonical form on read and encodes it back — emitting the codec's
    /// wire attributes — on write. Only on `date`, `datetime` and `boolean`
    /// nodes (E084 unknown id, E085 type mismatch, E087 wire collision).
    pub codec: Option<String>,
    /// Namespace prefix of this node's own element (write side), overriding
    /// `[meta.ns_defaults]`. `""` means unprefixed. Must be declared in
    /// `[meta.namespaces]` (E080); not valid on an attribute leaf (E081). On a
    /// `type`-less table this alone makes the table a structural node.
    pub ns: Option<String>,
    /// Structural selector on a collection or structural node: the node binds
    /// only the occurrences of its element whose child values equal these.
    /// Keys are child element paths inside the element (`TypeCode`,
    /// `TaxScheme.ID`, `ID.@schemeID`, `@schemeID`), values are the literal
    /// text to match (trimmed, exact). Several logical nodes may bind one
    /// physical element with disjoint selectors; at most one may carry none
    /// and takes what no selector matched. Not valid on a scalar node (E091);
    /// every key must name a scalar declared beneath one of the element's
    /// logical nodes (E092); selectors on one element must be disjoint (E090).
    #[serde(rename = "match")]
    pub match_: Option<BTreeMap<String, String>>,
    /// Declaration position within the mapping document (0-based, document
    /// order). Assigned by the parser, never authored — a `position` key in the
    /// TOML is rejected like any unknown field. It drives the order of sibling
    /// fields in the synthesized source structs and therefore the order of
    /// sibling elements in emitted XML. Inheritance keeps an overridden node
    /// at its base position and appends a child's new nodes after the base's.
    #[serde(skip)]
    pub position: usize,
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
        self.ty.is_some() || self.has_structural_field() || self.has_mapping_field()
    }

    /// Whether this node carries a field a structural node may declare besides
    /// `required`: `ns`, `xml` (the physical element it binds) or `match`.
    fn has_structural_field(&self) -> bool {
        self.ns.is_some() || self.xml.is_some() || self.match_.is_some()
    }

    /// Whether this node carries any field that only a *mapped* (typed) node
    /// may have — everything beyond `type`, `ns`, `xml`, `match`, `required`,
    /// `description`, `disabled`, `replace`.
    pub fn has_mapping_field(&self) -> bool {
        self.canonical_key.is_some()
            || self.fallbacks.is_some()
            || self.multiple.is_some()
            || self.join_with.is_some()
            || self.normalize.is_some()
            || self.constant.is_some()
            || self.clone_of.is_some()
            || self.codec.is_some()
    }

    /// Whether this is a structural node: no `type`, at least one of `ns`,
    /// `xml`, `match` or `required = true`, and nothing a mapped node would
    /// declare. It names an inferred interior element to set its namespace
    /// prefix, force its emission, or bind it to a physical element by
    /// selector, and is consumed by synthesis, never by the IR.
    pub fn is_structural(&self) -> bool {
        self.ty.is_none()
            && (self.has_structural_field() || self.required == Some(true))
            && !self.has_mapping_field()
    }

    /// Whether this is a structural node the writer must always materialize.
    pub fn is_always_present(&self) -> bool {
        self.is_structural() && self.required == Some(true)
    }

    /// Folds `child` (a later declaration of the same id) over `self`: every
    /// field the child sets wins, every field it omits keeps the base value —
    /// unless the child says `replace = true`, which discards the base. The
    /// declaration position is the base's either way.
    pub fn merged_with(&self, child: &RawNode) -> RawNode {
        let mut out = if child.replace == Some(true) {
            child.clone()
        } else {
            RawNode {
                xml: child.xml.clone().or_else(|| self.xml.clone()),
                ty: child.ty.or(self.ty),
                canonical_key: child
                    .canonical_key
                    .clone()
                    .or_else(|| self.canonical_key.clone()),
                required: child.required.or(self.required),
                fallbacks: child.fallbacks.clone().or_else(|| self.fallbacks.clone()),
                description: child
                    .description
                    .clone()
                    .or_else(|| self.description.clone()),
                multiple: child.multiple.or(self.multiple),
                join_with: child.join_with.clone().or_else(|| self.join_with.clone()),
                normalize: child.normalize.clone().or_else(|| self.normalize.clone()),
                constant: child.constant.clone().or_else(|| self.constant.clone()),
                clone_of: child.clone_of.clone().or_else(|| self.clone_of.clone()),
                disabled: child.disabled.or(self.disabled),
                replace: None,
                codec: child.codec.clone().or_else(|| self.codec.clone()),
                ns: child.ns.clone().or_else(|| self.ns.clone()),
                match_: child.match_.clone().or_else(|| self.match_.clone()),
                position: self.position,
            }
        };
        out.replace = None;
        out.position = self.position;
        out
    }
}

/// Which scope a `clone_of` derivation reads its key from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivationScope {
    /// The node's own scope (`Key`).
    Own,
    /// The scope enclosing the node's collection (`$parent.Key`).
    Parent,
    /// The invoice root (`$root.Key`).
    Root,
}

/// A parsed `clone_of` value: the scope to look in and the canonical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivation<'a> {
    /// Where the key lives.
    pub scope: DerivationScope,
    /// The canonical key.
    pub key: &'a str,
}

/// Parses a `clone_of` value: `Key`, `$parent.Key` or `$root.Key`.
///
/// # Errors
///
/// A human-readable reason for anything else: an unknown `$scope`, a missing
/// key, or a key that itself contains a path (`$root.Lines.LineId` — a
/// derivation never traverses a collection).
pub fn parse_derivation(value: &str) -> Result<Derivation<'_>, String> {
    let (scope, key) = if let Some(rest) = value.strip_prefix('$') {
        let Some((scope, key)) = rest.split_once('.') else {
            return Err(format!(
                "`{value}` names a scope but no key (`$root.Key`, `$parent.Key`)"
            ));
        };
        let scope = match scope {
            "root" => DerivationScope::Root,
            "parent" => DerivationScope::Parent,
            other => {
                return Err(format!(
                    "unknown derivation scope `${other}`; use `$root` or `$parent`"
                ));
            }
        };
        (scope, key)
    } else {
        (DerivationScope::Own, value)
    };
    if key.is_empty() {
        return Err(format!("`{value}` has an empty key"));
    }
    if key.contains('.') || key.contains('$') {
        return Err(format!(
            "`{value}`: a derivation names one canonical key; it cannot traverse into a collection"
        ));
    }
    Ok(Derivation { scope, key })
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
    /// Whether the value is required: the node must have a deterministic write
    /// route (its hub key filled by the source, a `constant`, a resolved
    /// `clone_of`, or a `match` discriminator). A required scalar missing at
    /// runtime, or a required collection without items, is a `REQUIRED_MISSING`
    /// error.
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
    /// Join separator (present iff `multiple = Join`).
    pub join_with: Option<String>,
    /// Normalization operations in declared order.
    pub normalize: Vec<NormalizeOp>,
    /// Fixed write-side value (writer emits this literal, hub ignored on write).
    pub constant: Option<String>,
    /// Canonical key this node mirrors (write fan-out + read consistency check).
    pub clone_of: Option<String>,
    /// Human description.
    pub description: Option<String>,
    /// Declared namespace prefix of the node's own element, if any (validated
    /// against `[meta.namespaces]`, E080).
    pub ns: Option<String>,
    /// Lexical codec id, if any (validated against the codec table, E084/E085).
    pub codec: Option<String>,
}

impl SourceNode {
    /// The parsed `clone_of` derivation, if the node is a clone.
    pub fn derivation(&self) -> Option<Result<Derivation<'_>, String>> {
        self.clone_of.as_deref().map(parse_derivation)
    }

    /// Whether this is a collection node (opens a child scope).
    pub fn is_collection(&self) -> bool {
        self.source_type.is_collection()
    }

    /// Whether this node is a fallback-only helper (no canonical target).
    pub fn is_helper(&self) -> bool {
        self.canonical_key.is_none()
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
        assert!(
            !n.has_mapping_field(),
            "xml alone does not demand a type: a structural node may alias"
        );
    }

    #[test]
    fn test_raw_node_match_and_xml_make_a_structural_node() {
        let n: RawNode =
            toml::from_str(r#"match = { "TypeCode" = "130", "ID.@schemeID" = "X" }"#).unwrap();
        assert!(n.is_structural());
        assert!(n.has_active_field());
        assert!(!n.is_always_present());
        let sel = n.match_.as_ref().unwrap();
        assert_eq!(sel["TypeCode"], "130");
        assert_eq!(sel["ID.@schemeID"], "X");
        let aliased: RawNode = toml::from_str(r#"xml = "AdditionalReferencedDocument""#).unwrap();
        assert!(aliased.is_structural(), "xml alone aliases an element");
        let coll: RawNode =
            toml::from_str("type = \"collection\"\nmatch = { \"TypeCode\" = \"916\" }").unwrap();
        assert!(!coll.is_structural(), "a typed node is a mapped node");
        assert!(coll.match_.is_some());
    }

    #[test]
    fn test_merged_with_carries_match() {
        let base: RawNode = toml::from_str(r#"match = { "TypeCode" = "50" }"#).unwrap();
        let child: RawNode = toml::from_str("required = true").unwrap();
        let merged = base.merged_with(&child);
        assert_eq!(merged.match_, base.match_, "kept from base");
        assert_eq!(merged.required, Some(true));
        let override_: RawNode = toml::from_str(r#"match = { "TypeCode" = "51" }"#).unwrap();
        assert_eq!(
            base.merged_with(&override_).match_.unwrap()["TypeCode"],
            "51",
            "child wins"
        );
    }

    #[test]
    fn test_raw_node_structural_is_ns_only_without_type() {
        let n: RawNode = toml::from_str(r#"ns = "rsm""#).unwrap();
        assert!(n.is_structural());
        assert!(n.has_active_field());
        let described: RawNode = toml::from_str("ns = \"rsm\"\ndescription = \"wrapper\"").unwrap();
        assert!(described.is_structural(), "description is allowed");
        let typed: RawNode = toml::from_str("ns = \"ram\"\ntype = \"string\"").unwrap();
        assert!(!typed.is_structural(), "a typed node is a mapped node");
        let keyed: RawNode = toml::from_str("ns = \"ram\"\ncanonical_key = \"X\"").unwrap();
        assert!(!keyed.is_structural(), "mapping fields need a type (E002)");
        assert!(!RawNode::default().is_structural(), "no ns: not structural");
    }

    #[test]
    fn test_raw_node_required_only_is_structural_and_always_present() {
        let n: RawNode = toml::from_str("required = true").unwrap();
        assert!(n.is_structural());
        assert!(n.is_always_present());
        let ns_only: RawNode = toml::from_str(r#"ns = "ram""#).unwrap();
        assert!(ns_only.is_structural() && !ns_only.is_always_present());
        let not_required: RawNode = toml::from_str("required = false").unwrap();
        assert!(
            !not_required.is_structural(),
            "required = false names nothing"
        );
        let typed: RawNode = toml::from_str("required = true\ntype = \"string\"").unwrap();
        assert!(!typed.is_structural());
    }

    #[test]
    fn test_merged_with_merges_fields_and_keeps_base_position() {
        let mut base: RawNode =
            toml::from_str("type = \"identifier\"\nrequired = true\nnormalize = [\"trim\"]")
                .unwrap();
        base.position = 7;
        let mut child: RawNode = toml::from_str("canonical_key = \"X\"\nrequired = false").unwrap();
        child.position = 99;
        let merged = base.merged_with(&child);
        assert_eq!(merged.ty, Some(MappingType::Identifier), "kept from base");
        assert_eq!(merged.required, Some(false), "child wins");
        assert_eq!(merged.canonical_key.as_deref(), Some("X"), "added by child");
        assert!(merged.normalize.is_some(), "kept from base");
        assert_eq!(merged.position, 7);
        assert_eq!(merged.replace, None);
    }

    #[test]
    fn test_merged_with_replace_discards_base() {
        let mut base: RawNode =
            toml::from_str("type = \"identifier\"\nrequired = true\nnormalize = [\"trim\"]")
                .unwrap();
        base.position = 7;
        let child: RawNode = toml::from_str("replace = true\ntype = \"string\"").unwrap();
        let merged = base.merged_with(&child);
        assert_eq!(merged.ty, Some(MappingType::String));
        assert_eq!(merged.required, None, "base fields are gone");
        assert_eq!(merged.normalize, None);
        assert_eq!(merged.replace, None, "never carried forward");
        assert_eq!(merged.position, 7, "but the position stays the base's");
    }

    #[test]
    fn test_parse_derivation_forms() {
        assert_eq!(
            parse_derivation("DocumentCurrency").unwrap(),
            Derivation {
                scope: DerivationScope::Own,
                key: "DocumentCurrency"
            }
        );
        assert_eq!(
            parse_derivation("$root.DocumentCurrency").unwrap(),
            Derivation {
                scope: DerivationScope::Root,
                key: "DocumentCurrency"
            }
        );
        assert_eq!(
            parse_derivation("$parent.LineId").unwrap(),
            Derivation {
                scope: DerivationScope::Parent,
                key: "LineId"
            }
        );
        for bad in [
            "$sibling.X",
            "$root",
            "$root.",
            "$root.Lines.LineId",
            "",
            "A.B",
        ] {
            assert!(parse_derivation(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_raw_node_codec_is_a_mapping_field() {
        let n: RawNode = toml::from_str(r#"codec = "cii-date-102""#).unwrap();
        assert!(n.has_active_field());
        assert!(n.has_mapping_field(), "a codec needs a typed node");
        assert!(!n.is_structural());
        assert_eq!(n.codec.as_deref(), Some("cii-date-102"));
    }

    #[test]
    fn test_raw_node_position_is_not_an_authoring_field() {
        // `position` is parser-assigned: authoring it is an unknown field.
        assert!(toml::from_str::<RawNode>("position = 3").is_err());
        let n: RawNode = toml::from_str(r#"type = "string""#).unwrap();
        assert_eq!(n.position, 0, "parser-assigned, defaults to 0");
    }

    #[test]
    fn test_raw_node_constant_marks_active() {
        let n: RawNode = toml::from_str(r#"constant = "2.1""#).unwrap();
        assert!(n.has_active_field());
        assert_eq!(n.constant.as_deref(), Some("2.1"));
    }
}
