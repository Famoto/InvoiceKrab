//! The normalized mapping IR.
//!
//! [`MappingIr`] is the single artifact every downstream consumer reads — the
//! generated mapper, the reports, the comparison tools, and the diagnostics. Its
//! nodes are keyed by [`NodeId`] in a [`BTreeMap`] for deterministic ordering,
//! and defaults are already materialized, so consumers
//! never distinguish an omitted field from a defaulted one.
//!
//! [`build_ir`] runs the resolution chain (inheritance → disabled removal →
//! defaults) and returns the IR alongside aggregated diagnostics; it does *not*
//! run the compile-time validation pipeline (that is a later pass that consumes
//! the IR).

use std::collections::BTreeMap;

use crate::codec::CodecTable;
use crate::error::Diagnostic;
use crate::meta::MappingMeta;
use crate::node::{NodeId, SourceNode};
use crate::parse::ParsedMapping;
use crate::resolve::{apply_defaults, merge_inheritance, remove_disabled};
use crate::source_model::{NamespaceConfig, SourceModelMeta, synthesize_source_model_with};

/// The normalized, defaults-materialized mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingIr {
    /// The leaf mapping's `[meta]`, with the namespace entries (`root_ns`,
    /// `namespaces`, `ns_defaults`) and the `schema` table it omitted filled in
    /// from its ancestors.
    pub meta: MappingMeta,
    /// Effective active nodes, in deterministic id order.
    pub nodes: BTreeMap<NodeId, SourceNode>,
}

/// Builds the IR from an inheritance chain (ancestor-first, leaf-last) with no
/// codecs available; see [`build_ir_with`].
///
/// Also *synthesizes* the typed source model from the merged nodes
/// ([`synthesize_source_model`]) — the struct tree and each node's `source_path`
/// are compiler outputs, not authored input. Returns the IR (well-formed nodes
/// only), the synthesized [`SourceModelMeta`], and any diagnostics raised during
/// synthesis or default application (e.g. an active node missing `type`).
/// Validation of paths, canonical keys, fallbacks, etc. is a separate pass.
///
/// # Panics
///
/// Panics if `chain` is empty; the caller must supply at least the leaf mapping.
pub fn build_ir(chain: &[ParsedMapping]) -> (MappingIr, SourceModelMeta, Vec<Diagnostic>) {
    build_ir_with(chain, &CodecTable::new())
}

/// [`build_ir`] with the shared codec table: a node's `codec` may add the
/// codec's wire attributes to the synthesized source model.
pub fn build_ir_with(
    chain: &[ParsedMapping],
    codecs: &CodecTable,
) -> (MappingIr, SourceModelMeta, Vec<Diagnostic>) {
    let (leaf, ancestors) = chain
        .split_last()
        .expect("inheritance chain must contain at least the leaf mapping");
    // Namespace and schema meta are inherited: the nearest ancestor that
    // declares an entry supplies it when the leaf (and every closer ancestor)
    // omits it.
    let mut meta = leaf.meta.clone();
    for ancestor in ancestors.iter().rev() {
        meta.inherit_namespaces(&ancestor.meta);
        meta.inherit_schema(&ancestor.meta);
    }
    let ns = NamespaceConfig {
        root_prefix: meta.root_prefix().to_string(),
        leaf_prefix: meta.leaf_prefix().to_string(),
        aggregate_prefix: meta.aggregate_prefix().to_string(),
        declared: meta.declared_namespaces(),
    };

    let root = meta.root.clone().unwrap_or_else(|| "Root".to_string());
    let model_id = meta
        .source_model
        .clone()
        .unwrap_or_else(|| format!("{}:{}", meta.doc_format, meta.format_version));

    let merged = merge_inheritance(chain);
    let active = remove_disabled(merged);
    let (source, paths, mut diags) =
        synthesize_source_model_with(&active, &root, &model_id, &ns, codecs);
    let (nodes, default_diags) = apply_defaults(&active, &paths);
    diags.extend(default_diags);

    (MappingIr { meta, nodes }, source, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_mapping;
    use crate::types::MappingType;

    const META: &str = r#"
        [meta]
        doc_format = "ubl-invoice"
        format_version = "2.1"
        mapping_version = "1.0"
        source_model = "ubl-invoice:2.1"
        canonical_model = "canonical-invoice:1.0"
        root = "Invoice"
    "#;

    fn parsed(extra: &str) -> ParsedMapping {
        parse_mapping(&format!("{META}\n{extra}")).expect("parses")
    }

    #[test]
    fn test_build_ir_minimal_example() {
        // The mapping DSL's minimal example.
        let (ir, source, diags) = build_ir(&[parsed(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            required = true
            fallbacks = ["Invoice.UUID"]
            normalize = ["trim", "empty_as_missing"]

            [Invoice.UUID]
            type = "identifier"
            description = "Fallback invoice identifier."
            normalize = ["trim", "empty_as_missing"]
        "#,
        )]);
        assert!(diags.is_empty());
        assert_eq!(ir.meta.doc_format, "ubl-invoice");
        assert_eq!(ir.nodes.len(), 2);

        let id = &ir.nodes[&NodeId::new("Invoice.ID")];
        assert_eq!(id.source_type, MappingType::Identifier);
        assert_eq!(id.source_path, "id", "source_path is synthesized");
        assert!(id.required);
        assert_eq!(id.fallbacks, [NodeId::new("Invoice.UUID")]);

        let uuid = &ir.nodes[&NodeId::new("Invoice.UUID")];
        assert!(uuid.is_helper(), "UUID has no canonical_key");
        assert_eq!(
            uuid.description.as_deref(),
            Some("Fallback invoice identifier.")
        );

        // The source model is synthesized alongside the IR.
        assert_eq!(source.root, "Invoice");
        assert!(source.structs["Invoice"].fields.contains_key("id"));
        assert!(source.structs["Invoice"].fields.contains_key("uuid"));
    }

    #[test]
    fn test_build_ir_uses_leaf_meta() {
        let mut parent = parsed(
            r#"[Invoice.A]
            type = "string""#,
        );
        parent.meta.mapping_version = "0.9".to_string();
        let child = parsed(
            r#"[Invoice.B]
            type = "string""#,
        );
        let (ir, _source, _) = build_ir(&[parent, child]);
        assert_eq!(ir.meta.mapping_version, "1.0", "leaf meta wins");
        assert_eq!(ir.nodes.len(), 2, "inherited + own nodes");
    }

    #[test]
    fn test_build_ir_inherits_namespace_meta_and_prefixes_fields() {
        let parent = parse_mapping(&format!(
            "{META}\nroot_ns = \"\"\n[meta.namespaces]\n\"\" = \"urn:inv\"\ncbc = \"urn:cbc\"\ncac = \"urn:cac\"\n[meta.ns_defaults]\nleaf = \"cbc\"\naggregate = \"cac\"\n\n[Invoice.ID]\ntype = \"identifier\"\n\n[Invoice.Totals.Amount]\ntype = \"decimal\""
        ))
        .expect("parses");
        let child = parsed(
            r#"[Invoice.ID]
            type = "identifier"
            required = true"#,
        );
        let (ir, source, diags) = build_ir(&[parent, child]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(ir.meta.leaf_prefix(), "cbc", "inherited into the leaf meta");
        assert_eq!(source.structs["Invoice"].fields["id"].prefix, "cbc");
        assert_eq!(source.structs["Invoice"].fields["totals"].prefix, "cac");
        assert_eq!(source.structs["Totals"].fields["amount"].prefix, "cbc");
        assert_eq!(source.namespaces.declared["cac"], "urn:cac");
    }

    #[test]
    fn test_build_ir_inherits_schema_but_not_samples() {
        let parent = parse_mapping(&format!(
            "{META}\n[meta.schema]\nxsd = \"base.xsd\"\n[[meta.samples]]\nfile = \"base.xml\"\n\n[Invoice.ID]\ntype = \"identifier\""
        ))
        .expect("parses");
        let child = parsed(
            r#"[Invoice.ID]
            type = "identifier"
            required = true"#,
        );
        let (ir, _, diags) = build_ir(&[parent, child]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            ir.meta.schema.as_ref().map(|s| s.xsd.as_str()),
            Some("base.xsd"),
            "the CIUS validates against its base syntax's schema"
        );
        assert!(ir.meta.samples.is_empty(), "samples stay with their spoke");
    }

    #[test]
    fn test_build_ir_undeclared_default_prefix_is_e080() {
        let p = parse_mapping(&format!(
            "{META}\n[meta.ns_defaults]\nleaf = \"cbc\"\n\n[Invoice.ID]\ntype = \"identifier\""
        ))
        .expect("parses");
        let (_, _, diags) = build_ir(&[p]);
        assert!(diags.iter().any(|d| d.code == "E080"), "{diags:?}");
    }

    #[test]
    fn test_build_ir_is_deterministic() {
        let p = parsed(
            r#"[Invoice.Zeta]
            type = "string"
            [Invoice.Alpha]
            type = "string""#,
        );
        let (a, sa, _) = build_ir(std::slice::from_ref(&p));
        let (b, sb, _) = build_ir(&[p]);
        assert_eq!(a, b);
        assert_eq!(sa, sb);
        let ids: Vec<&str> = a.nodes.keys().map(NodeId::as_str).collect();
        assert_eq!(ids, ["Invoice.Alpha", "Invoice.Zeta"]);
    }
}
