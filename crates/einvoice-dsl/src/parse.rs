//! TOML front end for spoke mapping documents.
//!
//! Parses one spoke mapping document and flattens its dotted tables into source
//! nodes keyed by [`NodeId`]. The reserved `[meta]` table becomes [`MappingMeta`].
//!
//! # What is a source node?
//!
//! A TOML table is a *source node* iff it carries at least one source-node field
//! (`type`, `xml`, `canonical_key`, `disabled`, …). A table that holds only
//! sub-tables is a *container* — it organizes nodes but is not itself one. Node
//! uniqueness is guaranteed by TOML itself (duplicate tables are a parse error).
//!
//! # Behavior
//!
//! - The full dotted table name is the node id (`[Invoice.ID]` → `Invoice.ID`).
//! - A table may be *both* a node and a container (a collection node with child
//!   nodes). Own scalar/array fields define the node; sub-tables recurse.
//! - Unknown fields inside a node are rejected (E001).
//! - Stray top-level scalar keys (outside `[meta]` and any table) are rejected.

use std::collections::BTreeMap;

use serde::Deserialize;
use toml::Value;

use crate::error::ConfigError;
use crate::meta::MappingMeta;
use crate::node::{NodeId, RawNode};

/// A parsed mapping document: its `[meta]` and its raw nodes (pre-resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMapping {
    /// The `[meta]` table.
    pub meta: MappingMeta,
    /// Raw nodes keyed by dotted id, in deterministic id order.
    pub nodes: BTreeMap<NodeId, RawNode>,
}

/// Parses a spoke mapping document.
pub fn parse_mapping(src: &str) -> Result<ParsedMapping, ConfigError> {
    // Step 1: parse TOML. A whole-document parse surfaces syntax errors and
    // duplicate-table errors (node uniqueness) with spans.
    let root: toml::Table = toml::from_str(src)?;

    // The reserved [meta] table.
    let meta_value = root
        .get("meta")
        .ok_or_else(|| ConfigError::msg("missing required [meta] table"))?;
    let meta = MappingMeta::deserialize(meta_value.clone())?;

    // Step 3: flatten dotted tables into node ids.
    let mut nodes = BTreeMap::new();
    for (key, value) in &root {
        if key == "meta" {
            continue;
        }
        match value {
            Value::Table(table) => {
                flatten(NodeId::new(key.as_str()), table, &mut nodes)?;
            }
            _ => {
                return Err(ConfigError::msg(format!(
                    "top-level key `{key}` must be a source-node table, not a bare value"
                )));
            }
        }
    }

    Ok(ParsedMapping { meta, nodes })
}

/// Recursively flattens `table` (named `id`) into source nodes. Own fields make
/// `id` a node; sub-tables recurse with extended ids.
fn flatten(
    id: NodeId,
    table: &toml::Table,
    nodes: &mut BTreeMap<NodeId, RawNode>,
) -> Result<(), ConfigError> {
    let mut own = toml::Table::new();
    let mut children: Vec<(NodeId, &toml::Table)> = Vec::new();

    for (key, value) in table {
        match value {
            Value::Table(child) => {
                children.push((NodeId::new(format!("{id}.{key}")), child));
            }
            other => {
                own.insert(key.clone(), other.clone());
            }
        }
    }

    if !own.is_empty() {
        let node = RawNode::deserialize(Value::Table(own))
            .map_err(|e| ConfigError::msg(format!("in node `{id}`: {e}")))?;
        nodes.insert(id, node);
    }

    for (child_id, child_table) in children {
        flatten(child_id, child_table, nodes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MappingType;

    const META: &str = r#"
        [meta]
        doc_format = "ubl-invoice"
        format_version = "2.1"
        mapping_version = "1.0"
        source_model = "ubl-invoice:2.1"
        canonical_model = "canonical-invoice:1.0"
    "#;

    fn parse(extra: &str) -> ParsedMapping {
        parse_mapping(&format!("{META}\n{extra}")).expect("parses")
    }

    #[test]
    fn test_meta_only_yields_no_nodes() {
        let m = parse("");
        assert_eq!(m.meta.doc_format, "ubl-invoice");
        assert!(m.nodes.is_empty());
    }

    #[test]
    fn test_missing_meta_is_error() {
        let err = parse_mapping(
            r#"[Invoice.ID]
            type = "identifier""#,
        )
        .unwrap_err();
        assert!(err.message.contains("[meta]"));
    }

    #[test]
    fn test_single_node_flattens_to_dotted_id() {
        let m = parse(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            required = true
        "#,
        );
        assert_eq!(m.nodes.len(), 1);
        let node = &m.nodes[&NodeId::new("Invoice.ID")];
        assert_eq!(node.ty, Some(MappingType::Identifier));
        assert_eq!(node.canonical_key.as_deref(), Some("InvoiceNumber"));
        assert_eq!(node.required, Some(true));
    }

    #[test]
    fn test_leaf_xml_binding_is_captured() {
        let m = parse(
            r#"
            [Invoice.PayableAmount.currencyID]
            xml = "@currencyID"
            type = "currency"
        "#,
        );
        let node = &m.nodes[&NodeId::new("Invoice.PayableAmount.currencyID")];
        assert_eq!(node.xml.as_deref(), Some("@currencyID"));
        assert_eq!(node.ty, Some(MappingType::Currency));
    }

    #[test]
    fn test_collection_node_and_children_both_captured() {
        let m = parse(
            r#"
            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId"
        "#,
        );
        assert_eq!(m.nodes.len(), 2);
        assert_eq!(
            m.nodes[&NodeId::new("InvoiceLine")].ty,
            Some(MappingType::Collection)
        );
        assert_eq!(
            m.nodes[&NodeId::new("InvoiceLine.ID")]
                .canonical_key
                .as_deref(),
            Some("LineId")
        );
    }

    #[test]
    fn test_container_only_table_is_not_a_node() {
        // `[Supplier]` is never declared with own fields; only its children are.
        let m = parse(
            r#"
            [Supplier.Name]
            type = "string"

            [Supplier.RegistrationName]
            type = "string"
        "#,
        );
        assert!(!m.nodes.contains_key(&NodeId::new("Supplier")));
        assert!(m.nodes.contains_key(&NodeId::new("Supplier.Name")));
        assert!(
            m.nodes
                .contains_key(&NodeId::new("Supplier.RegistrationName"))
        );
    }

    #[test]
    fn test_unknown_node_field_is_rejected() {
        let err = parse_mapping(&format!(
            "{META}\n[Invoice.ID]\ntype = \"identifier\"\nbogus = 1"
        ))
        .unwrap_err();
        assert!(err.message.contains("Invoice.ID"));
    }

    #[test]
    fn test_duplicate_table_is_rejected_by_toml() {
        let err = parse_mapping(&format!(
            "{META}\n[Invoice.ID]\ntype=\"identifier\"\n[Invoice.ID]\ntype=\"string\""
        ))
        .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[test]
    fn test_stray_top_level_scalar_is_rejected() {
        let err = parse_mapping(&format!("{META}\nstray = 1")).unwrap_err();
        assert!(err.message.contains("stray"));
    }

    #[test]
    fn test_nodes_ordered_by_id() {
        let m = parse(
            r#"
            [Zeta]
            type = "string"
            [Alpha]
            type = "string"
        "#,
        );
        let ids: Vec<&str> = m.nodes.keys().map(NodeId::as_str).collect();
        assert_eq!(ids, ["Alpha", "Zeta"]);
    }
}
