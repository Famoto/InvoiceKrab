//! Resolution passes for parsed mapping nodes.
//!
//! Three ordered transforms turn parsed raw nodes into effective ones:
//!
//! 1. [`merge_inheritance`] — fold an inheritance chain (ancestor → … → leaf).
//!    Overrides are *full-node replacements*: a later node wholly replaces an
//!    earlier node of the same id.
//! 2. [`remove_disabled`] — drop disabled nodes and the descendants of any
//!    disabled collection, whose scope no longer exists. Runs *before* defaults.
//! 3. [`apply_defaults`] — materialize defaults onto each surviving active node
//!    default values. An omitted field takes its default, never the parent's
//!    value, because inheritance already replaced whole nodes.
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
/// mapping fully replaces the earlier one; new ids are added.
pub fn merge_inheritance(chain: &[ParsedMapping]) -> BTreeMap<NodeId, RawNode> {
    let mut merged = BTreeMap::new();
    for mapping in chain {
        for (id, node) in &mapping.nodes {
            merged.insert(id.clone(), node.clone());
        }
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
/// synthesized path. An active node missing the no-default `type` is reported as
/// an `E002` diagnostic and excluded; the rest still resolve so diagnostics
/// aggregate (R9 — never first-error-only).
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
                min_items: raw.min_items,
                join_with: raw.join_with.clone(),
                normalize: raw.normalize.clone().unwrap_or_default(),
                adapter: raw.adapter.clone(),
                description: raw.description.clone(),
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
        assert_eq!(n.min_items, None);
        assert_eq!(n.adapter, None);
        assert_eq!(n.scope, Scope::Root);
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
            r#"[Bad]
            xml = "x""#,
        );
        assert!(nodes.is_empty());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E002");
        assert!(diags[0].message.contains("type"));
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
    fn test_inheritance_override_replaces_whole_node() {
        // Parent node has required=true + a fallback; child re-declares the node
        // with neither. Full-node replacement: the omitted `required`/`fallbacks`
        // revert to defaults, they are NOT inherited from the parent.
        let parent = parsed(
            r#"[Invoice.ID]
            type = "identifier"
            required = true
            fallbacks = ["Invoice.UUID"]"#,
        );
        let child = parsed(
            r#"[Invoice.ID]
            type = "identifier""#,
        );
        let merged = merge_inheritance(&[parent, child]);
        let active = remove_disabled(merged);
        let (_m, paths, _sd) = synthesize_source_model(&active, "Invoice", "s:1");
        let (nodes, _) = apply_defaults(&active, &paths);
        let n = &nodes[&NodeId::new("Invoice.ID")];
        assert_eq!(n.source_path, "id");
        assert!(!n.required, "omitted field reverts to default, not parent");
        assert!(n.fallbacks.is_empty());
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
