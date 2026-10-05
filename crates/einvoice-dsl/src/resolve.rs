//! Resolution passes for parsed mapping nodes.
//!
//! Three ordered transforms turn parsed raw nodes into effective ones:
//!
//! 1. [`merge_inheritance`] — fold an inheritance chain (ancestor → … → leaf).
//!    A later declaration of an id *merges* over the earlier one: fields it
//!    sets win, fields it omits keep the base value, so a CIUS restates only
//!    its delta (`required = true`). `replace = true` opts back into whole-node
//!    replacement. Declaration positions are merged too: an override keeps the
//!    base node's position (it stays where the base put the element), and nodes
//!    new to a later mapping are appended after everything already merged, in
//!    their own declaration order.
//! 2. [`remove_disabled`] — drop disabled nodes and the descendants of any
//!    disabled collection, whose scope no longer exists. Runs *before* defaults.
//! 3. [`apply_defaults`] — materialize defaults onto each surviving active node.
//!    An omitted field takes its default — after the merge, so a field a child
//!    omits still carries the base's value.
//!
//! Ordering matters: inheritance, then disabled removal, then defaults. The
//! [`crate::ir::build_ir`] entry point chains them.

use std::collections::BTreeMap;

use crate::error::{Diagnostic, Severity};
use crate::node::{NodeId, RawNode, SourceNode};
use crate::parse::ParsedMapping;
use crate::types::MappingType;

/// Folds an inheritance chain into one raw node set.
///
/// `chain` is ordered ancestor-first, leaf-last. A node id present in a later
/// mapping merges field-wise over the earlier one ([`RawNode::merged_with`]);
/// with `replace = true` it replaces it whole. New ids are added.
///
/// Positions: an override takes over the overridden node's position, so a CIUS
/// that restates a node does not move the element. Ids new to a later mapping
/// are offset past every position merged so far, so they follow the base's
/// nodes in the later mapping's own declaration order.
pub fn merge_inheritance(chain: &[ParsedMapping]) -> BTreeMap<NodeId, RawNode> {
    let mut merged: BTreeMap<NodeId, RawNode> = BTreeMap::new();
    let mut offset = 0usize;
    for mapping in chain {
        let span = mapping
            .nodes
            .values()
            .map(|n| n.position + 1)
            .max()
            .unwrap_or(0);
        for (id, node) in &mapping.nodes {
            let node = match merged.get(id) {
                Some(existing) => existing.merged_with(node),
                None => {
                    let mut node = node.clone();
                    node.position += offset;
                    node.replace = None;
                    node
                }
            };
            merged.insert(id.clone(), node);
        }
        offset += span;
    }
    merged
}

/// Removes disabled nodes and the descendants of disabled collections.
///
/// A disabled node's scope is gone, so any node nested beneath a disabled node
/// is removed too.
pub fn remove_disabled(nodes: BTreeMap<NodeId, RawNode>) -> BTreeMap<NodeId, RawNode> {
    let disabled: Vec<NodeId> = nodes
        .iter()
        .filter(|(_, node)| node.is_disabled())
        .map(|(id, _)| id.clone())
        .collect();

    nodes
        .into_iter()
        .filter(|(id, node)| {
            !node.is_disabled() && !disabled.iter().any(|d| id.is_descendant_of(d))
        })
        .collect()
}

/// Materializes defaults onto each active node, producing effective
/// [`SourceNode`]s.
///
/// `source_paths` is the `NodeId → source_path` map produced by
/// [`crate::source_model::synthesize_source_model`]; each active node takes its
/// synthesized path. A structural node (`ns` only, no `type`) is not a mapping
/// node and is skipped silently — synthesis has already consumed it. Any other
/// active node missing the no-default `type` is reported as an `E002`
/// diagnostic and excluded; the rest still resolve so diagnostics aggregate
/// (R9 — never first-error-only).
pub fn apply_defaults(
    nodes: &BTreeMap<NodeId, RawNode>,
    source_paths: &BTreeMap<NodeId, String>,
) -> (BTreeMap<NodeId, SourceNode>, Vec<Diagnostic>) {
    // Active node types drive scope computation (a node's scope is its nearest
    // enclosing *collection*).
    let types: BTreeMap<NodeId, MappingType> = nodes
        .iter()
        .filter_map(|(id, node)| node.ty.map(|ty| (id.clone(), ty)))
        .collect();

    let mut out = BTreeMap::new();
    let mut diags = Vec::new();

    for (id, raw) in nodes {
        if raw.is_structural() {
            continue;
        }
        let Some(ty) = raw.ty else {
            diags.push(Diagnostic {
                code: "E002".to_string(),
                severity: Severity::Error,
                source_node: Some(id.to_string()),
                message: format!("active node `{id}` is missing required `type`"),
                span: None,
            });
            continue;
        };
        // Synthesis runs over the same active nodes, so a typed node always has a
        // path; fall back to the empty string only to stay total.
        let path = source_paths.get(id).cloned().unwrap_or_default();

        out.insert(
            id.clone(),
            SourceNode {
                id: id.clone(),
                scope: id
                    .nearest_collection_scope(|a| types.get(a) == Some(&MappingType::Collection)),
                source_path: path,
                source_type: ty,
                canonical_key: raw.canonical_key.clone(),
                required: raw.required.unwrap_or(false),
                fallbacks: raw
                    .fallbacks
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(NodeId::new)
                    .collect(),
                multiple: raw.multiple,
                join_with: raw.join_with.clone(),
                normalize: raw.normalize.clone().unwrap_or_default(),
                constant: raw.constant.clone(),
                clone_of: raw.clone_of.clone(),
                description: raw.description.clone(),
                ns: raw.ns.clone(),
                codec: raw.codec.clone(),
            },
        );
    }

    (out, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Scope;
    use crate::parse::parse_mapping;
    use crate::source_model::synthesize_source_model;

    const META: &str = r#"
        [meta]
        doc_format = "f"
        format_version = "1"
        mapping_version = "1"
        source_model = "s:1"
        canonical_model = "c:1"
    "#;

    fn parsed(extra: &str) -> ParsedMapping {
        parse_mapping(&format!("{META}\n{extra}")).expect("parses")
    }

    /// Runs the resolution chain over one mapping, synthesizing source paths just
    /// as [`crate::ir::build_ir`] does.
    fn resolved(extra: &str) -> (BTreeMap<NodeId, SourceNode>, Vec<Diagnostic>) {
        let merged = merge_inheritance(&[parsed(extra)]);
        let active = remove_disabled(merged);
        let (_model, paths, sdiags) = synthesize_source_model(&active, "Invoice", "s:1");
        assert!(
            sdiags.is_empty(),
            "unexpected synth diagnostics: {sdiags:?}"
        );
        apply_defaults(&active, &paths)
    }

    /// Like [`resolved`], with the `rsm`/`ram` prefixes declared, for `ns` tests.
    fn resolved_with_ns(extra: &str) -> (BTreeMap<NodeId, SourceNode>, Vec<Diagnostic>) {
        use crate::source_model::{NamespaceConfig, synthesize_source_model_with};
        let merged = merge_inheritance(&[parsed(extra)]);
        let active = remove_disabled(merged);
        let ns = NamespaceConfig {
            declared: [
                ("rsm".to_string(), "urn:rsm".to_string()),
                ("ram".to_string(), "urn:ram".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let (_model, paths, sdiags) = synthesize_source_model_with(
            &active,
            "Invoice",
            "s:1",
            &ns,
            &crate::codec::CodecTable::new(),
        );
        assert!(
            sdiags.is_empty(),
            "unexpected synth diagnostics: {sdiags:?}"
        );
        apply_defaults(&active, &paths)
    }

    fn effective(extra: &str) -> BTreeMap<NodeId, SourceNode> {
        let (nodes, diags) = resolved(extra);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        nodes
    }

    #[test]
    fn test_defaults_materialized_on_omitted_fields() {
        let nodes = effective(
            r#"[Invoice.ID]
            type = "identifier""#,
        );
        let n = &nodes[&NodeId::new("Invoice.ID")];
        assert_eq!(n.source_path, "id");
        assert!(!n.required);
        assert_eq!(n.multiple, None, "undeclared multiple stays None");
        assert!(n.fallbacks.is_empty());
        assert!(n.normalize.is_empty());
        assert_eq!(n.constant, None, "undeclared constant stays None");
        assert_eq!(n.codec, None);
        assert_eq!(n.scope, Scope::Root);
    }

    #[test]
    fn test_constant_carried_through_resolution() {
        let nodes = effective(
            r#"[Invoice.UBLVersionID]
            type = "identifier"
            constant = "2.1""#,
        );
        let n = &nodes[&NodeId::new("Invoice.UBLVersionID")];
        assert_eq!(n.constant.as_deref(), Some("2.1"));
        assert!(n.is_helper(), "constant-only node has no canonical key");
    }

    #[test]
    fn test_clone_of_carried_through_resolution() {
        let nodes = effective(
            r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.BuyerReference]
            type = "identifier"
            clone_of = "InvoiceNumber""#,
        );
        let n = &nodes[&NodeId::new("Invoice.BuyerReference")];
        assert_eq!(n.clone_of.as_deref(), Some("InvoiceNumber"));
        assert!(n.is_helper(), "clone node has no canonical key of its own");
    }

    #[test]
    fn test_explicit_false_wins_over_default() {
        let nodes = effective(
            r#"[Invoice.ID]
            type = "identifier"
            required = false"#,
        );
        assert!(!nodes[&NodeId::new("Invoice.ID")].required);
    }

    #[test]
    fn test_missing_type_is_e002_and_excluded() {
        let (nodes, diags) = resolved(
            r#"[Invoice.Bad]
            canonical_key = "X""#,
        );
        assert!(nodes.is_empty());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E002");
        assert!(diags[0].message.contains("type"));
    }

    #[test]
    fn test_structural_node_is_skipped_without_e002() {
        // `ns` on a type-less table names an interior element's prefix; it is
        // consumed by synthesis and never becomes a mapping node.
        let (nodes, diags) = resolved_with_ns(
            r#"[Invoice.Wrapper]
            ns = "rsm"

            [Invoice.Wrapper.ID]
            type = "identifier"
            ns = "ram""#,
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert!(!nodes.contains_key(&NodeId::new("Invoice.Wrapper")));
        let id = &nodes[&NodeId::new("Invoice.Wrapper.ID")];
        assert_eq!(id.ns.as_deref(), Some("ram"), "ns carried onto the node");
    }

    #[test]
    fn test_ns_with_mapping_field_but_no_type_is_still_e002() {
        let (nodes, diags) = resolved_with_ns(
            r#"[Invoice.Bad]
            ns = "ram"
            canonical_key = "X""#,
        );
        assert!(nodes.is_empty());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E002");
    }

    #[test]
    fn test_disabled_node_removed() {
        let merged = merge_inheritance(&[parsed(
            r#"[Invoice.Legacy]
            disabled = true"#,
        )]);
        let active = remove_disabled(merged);
        assert!(!active.contains_key(&NodeId::new("Invoice.Legacy")));
    }

    #[test]
    fn test_disabled_collection_removes_descendants() {
        let merged = merge_inheritance(&[parsed(
            r#"[Lines]
            disabled = true

            [Lines.ID]
            type = "identifier""#,
        )]);
        let active = remove_disabled(merged);
        assert!(!active.contains_key(&NodeId::new("Lines")));
        assert!(
            !active.contains_key(&NodeId::new("Lines.ID")),
            "descendant of a disabled collection must be removed"
        );
    }

    #[test]
    fn test_collection_child_scope_is_the_collection() {
        let nodes = effective(
            r#"[InvoiceLine]
            type = "collection"

            [InvoiceLine.ID]
            type = "identifier""#,
        );
        assert_eq!(nodes[&NodeId::new("InvoiceLine")].scope, Scope::Root);
        assert_eq!(
            nodes[&NodeId::new("InvoiceLine.ID")].scope,
            Scope::Collection(NodeId::new("InvoiceLine"))
        );
    }

    #[test]
    fn test_inheritance_override_merges_over_the_base_node() {
        // The base has type + fallback; the child restates only `required`. The
        // merge keeps the base's type and fallback and takes the child's
        // `required` — the CIUS delta is all the child has to write.
        let parent = parsed(
            r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Invoice.UUID"]

            [Invoice.UUID]
            type = "identifier""#,
        );
        let child = parsed(
            r#"[Invoice.ID]
            required = true"#,
        );
        let merged = merge_inheritance(&[parent, child]);
        let active = remove_disabled(merged);
        let (_m, paths, _sd) = synthesize_source_model(&active, "Invoice", "s:1");
        let (nodes, diags) = apply_defaults(&active, &paths);
        assert!(diags.is_empty(), "{diags:?}");
        let n = &nodes[&NodeId::new("Invoice.ID")];
        assert_eq!(
            n.source_type,
            MappingType::Identifier,
            "type kept from base"
        );
        assert!(n.required, "child's delta applied");
        assert_eq!(n.fallbacks, [NodeId::new("Invoice.UUID")], "fallback kept");
    }

    #[test]
    fn test_inheritance_replace_true_discards_the_base_node() {
        let parent = parsed(
            r#"[Invoice.ID]
            type = "identifier"
            required = true
            fallbacks = ["Invoice.UUID"]

            [Invoice.UUID]
            type = "identifier""#,
        );
        let child = parsed(
            r#"[Invoice.ID]
            replace = true
            type = "identifier""#,
        );
        let merged = merge_inheritance(&[parent, child]);
        let active = remove_disabled(merged);
        let (_m, paths, _sd) = synthesize_source_model(&active, "Invoice", "s:1");
        let (nodes, _) = apply_defaults(&active, &paths);
        let n = &nodes[&NodeId::new("Invoice.ID")];
        assert!(!n.required, "replaced whole: omitted fields take defaults");
        assert!(n.fallbacks.is_empty());
    }

    #[test]
    fn test_inheritance_override_keeps_base_position_and_appends_new_nodes() {
        // Base declares B then A. The child restates A (an override) and adds
        // C. A keeps its base position — the element does not move because a
        // CIUS re-declared it — and C lands after every base node.
        let parent = parsed(
            r#"[Invoice.B]
            type = "string"

            [Invoice.A]
            type = "string""#,
        );
        let child = parsed(
            r#"[Invoice.C]
            type = "string"

            [Invoice.A]
            type = "string"
            required = true"#,
        );
        let merged = merge_inheritance(&[parent, child]);
        assert_eq!(merged[&NodeId::new("Invoice.B")].position, 0);
        assert_eq!(merged[&NodeId::new("Invoice.A")].position, 1);
        assert_eq!(merged[&NodeId::new("Invoice.A")].required, Some(true));
        assert_eq!(
            merged[&NodeId::new("Invoice.C")].position,
            2,
            "new child nodes are appended after the base's"
        );
    }

    #[test]
    fn test_inheritance_appends_new_nodes_in_child_order_across_three_levels() {
        let base = parsed(
            r#"[Invoice.A]
            type = "string""#,
        );
        let mid = parsed(
            r#"[Invoice.Z]
            type = "string"

            [Invoice.Y]
            type = "string""#,
        );
        let leaf = parsed(
            r#"[Invoice.M]
            type = "string""#,
        );
        let merged = merge_inheritance(&[base, mid, leaf]);
        let mut by_position: Vec<(usize, &str)> = merged
            .iter()
            .map(|(id, n)| (n.position, id.as_str()))
            .collect();
        by_position.sort_unstable();
        let order: Vec<&str> = by_position.into_iter().map(|(_, id)| id).collect();
        assert_eq!(order, ["Invoice.A", "Invoice.Z", "Invoice.Y", "Invoice.M"]);
    }

    #[test]
    fn test_inheritance_disable_removes_inherited_node() {
        let parent = parsed(
            r#"[Invoice.Legacy]
            type = "string""#,
        );
        let child = parsed(
            r#"[Invoice.Legacy]
            disabled = true"#,
        );
        let active = remove_disabled(merge_inheritance(&[parent, child]));
        assert!(!active.contains_key(&NodeId::new("Invoice.Legacy")));
    }
}
