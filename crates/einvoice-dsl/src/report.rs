//! Static mapping comparison reports.
//!
//! Pure, deterministic views over a [`CompileOutput`] for review without XLSX as
//! the source of truth. This module implements the core views:
//!
//! - [`coverage_matrix`] — which spokes cover each canonical hub field.
//! - [`gap_report`] — canonical fields a spoke does *not* cover.
//! - [`fallback_graph`] — the fallback edges of one spoke.
//! - [`render_coverage_markdown`] — a Markdown rendering of the matrix.
//!
//! Everything is sorted, so identical inputs render identically.

use std::collections::{BTreeMap, BTreeSet};

use crate::compile::CompileOutput;
use crate::hub::{CanonicalScope, canonical_scope_of};
use crate::ir::MappingIr;
use crate::node::{NodeId, SourceNode};

/// A canonical field identity in a report (scope + key).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldKey {
    /// The field's canonical scope.
    pub scope: CanonicalScope,
    /// The canonical key.
    pub key: String,
}

impl FieldKey {
    /// A flat label like `InvoiceNumber`, `InvoiceLines/LineId`, or
    /// `InvoiceLines/LineAllowances/Amount` for a nested collection field.
    pub fn label(&self) -> String {
        match &self.scope {
            CanonicalScope::Root => self.key.clone(),
            CanonicalScope::Collection(chain) => format!("{}/{}", chain.join("/"), self.key),
        }
    }
}

/// Which spokes cover each canonical field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageMatrix {
    /// Spoke ids, sorted.
    pub spokes: Vec<String>,
    /// Per canonical field, the set of spoke ids that map it.
    pub rows: BTreeMap<FieldKey, BTreeSet<String>>,
}

/// Builds the coverage matrix from a compile output.
pub fn coverage_matrix(out: &CompileOutput) -> CoverageMatrix {
    let spokes: Vec<String> = out.irs.keys().cloned().collect();
    let mut rows: BTreeMap<FieldKey, BTreeSet<String>> = BTreeMap::new();

    // Seed every hub field (so uncovered-by-some rows still appear).
    for (scope, key) in out.hub.fields.keys() {
        rows.entry(FieldKey {
            scope: scope.clone(),
            key: key.clone(),
        })
        .or_default();
    }

    for (spoke_id, ir) in &out.irs {
        for node in ir.nodes.values() {
            let Some(key) = &node.canonical_key else {
                continue;
            };
            let Some(scope) = canonical_scope_of(node, ir) else {
                continue;
            };
            rows.entry(FieldKey {
                scope,
                key: key.clone(),
            })
            .or_default()
            .insert(spoke_id.clone());
        }
    }

    CoverageMatrix { spokes, rows }
}

/// A canonical field a given spoke does not cover.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gap {
    /// The spoke missing the field.
    pub spoke: String,
    /// The uncovered field.
    pub field: FieldKey,
}

/// Reports, per spoke, the hub fields it does not map. Deterministically sorted.
pub fn gap_report(out: &CompileOutput) -> Vec<Gap> {
    let matrix = coverage_matrix(out);
    let mut gaps = Vec::new();
    for (field, covered_by) in &matrix.rows {
        for spoke in &matrix.spokes {
            if !covered_by.contains(spoke) {
                gaps.push(Gap {
                    spoke: spoke.clone(),
                    field: field.clone(),
                });
            }
        }
    }
    gaps.sort();
    gaps
}

/// The fallback edges of one spoke: each node id to its declared fallbacks, in
/// declared order. Nodes without fallbacks are omitted.
pub fn fallback_graph(ir: &MappingIr) -> BTreeMap<NodeId, Vec<NodeId>> {
    ir.nodes
        .values()
        .filter(|n| !n.fallbacks.is_empty())
        .map(|n| (n.id.clone(), n.fallbacks.clone()))
        .collect()
}

/// The set of canonical fields one spoke maps, as scope-qualified labels
/// (e.g. `InvoiceNumber`, `InvoiceLines/LineId`).
///
/// This is the spoke's footprint in the shared hub vocabulary — the basis the
/// Mapping Comparison Tool uses to reason about cross-format transforms: a value
/// can only survive a transform if both the source and the target cover its
/// canonical field.
///
/// # Examples
///
/// ```
/// use einvoice_dsl::{build_ir, covered_canonical_fields, parse_mapping};
/// let toml = r#"
///     [meta]
///     doc_format = "f"
///     format_version = "1"
///     mapping_version = "1"
///     canonical_model = "c:1"
///     root = "Invoice"
///     [Invoice.ID]
///     type = "identifier"
///     canonical_key = "InvoiceNumber"
/// "#;
/// let (ir, _src, _diags) = build_ir(&[parse_mapping(toml).unwrap()]);
/// assert!(covered_canonical_fields(&ir).contains("InvoiceNumber"));
/// ```
pub fn covered_canonical_fields(ir: &MappingIr) -> BTreeSet<String> {
    canonical_field_labels(ir, |_| true)
}

/// The canonical fields one spoke marks `required = true`, as scope-qualified
/// labels.
///
/// These are the labels a transform's *target* must have filled from the hub: if
/// the source does not cover a target-required field, the engine emits a
/// `REQUIRED_MISSING` diagnostic and the transform cannot produce valid output.
pub fn required_canonical_fields(ir: &MappingIr) -> BTreeSet<String> {
    canonical_field_labels(ir, |node| node.required)
}

/// Collects the scope-qualified labels of every mapped node satisfying `keep`.
fn canonical_field_labels(ir: &MappingIr, keep: impl Fn(&SourceNode) -> bool) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for node in ir.nodes.values() {
        let Some(key) = &node.canonical_key else {
            continue;
        };
        if !keep(node) {
            continue;
        }
        let Some(scope) = canonical_scope_of(node, ir) else {
            continue;
        };
        out.insert(
            FieldKey {
                scope,
                key: key.clone(),
            }
            .label(),
        );
    }
    out
}

/// Renders the coverage matrix as a Markdown table.
pub fn render_coverage_markdown(matrix: &CoverageMatrix) -> String {
    let mut out = String::from("| Canonical field |");
    for spoke in &matrix.spokes {
        out.push(' ');
        out.push_str(spoke);
        out.push_str(" |");
    }
    out.push_str("\n|---|");
    for _ in &matrix.spokes {
        out.push_str("---|");
    }
    out.push('\n');
    for (field, covered) in &matrix.rows {
        out.push_str("| ");
        out.push_str(&field.label());
        out.push_str(" |");
        for spoke in &matrix.spokes {
            out.push(' ');
            out.push_str(if covered.contains(spoke) { "✓" } else { "·" });
            out.push_str(" |");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{SpokeInput, compile};
    use crate::parse::parse_mapping;
    use std::collections::BTreeSet;

    fn mapping(model_id: &str, body: &str) -> crate::parse::ParsedMapping {
        let s = format!(
            r#"
            [meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            source_model = "{model_id}"
            canonical_model = "c:1"
            root = "Doc"
            {body}
        "#
        );
        parse_mapping(&s).expect("parses")
    }

    fn two_spoke_output() -> CompileOutput {
        // Spoke A covers InvoiceNumber + Amount; spoke B covers only InvoiceNumber.
        let a = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            [Doc.Total]
            type = "decimal"
            canonical_key = "Amount""#,
        );
        let b = mapping(
            "b:1",
            r#"[Doc.Number]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let spokes = [
            SpokeInput {
                id: "a".into(),
                chain: std::slice::from_ref(&a),
            },
            SpokeInput {
                id: "b".into(),
                chain: std::slice::from_ref(&b),
            },
        ];
        compile(&spokes, &BTreeSet::new())
    }

    #[test]
    fn test_coverage_matrix_tracks_who_covers_each_field() {
        let out = two_spoke_output();
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let m = coverage_matrix(&out);
        assert_eq!(m.spokes, ["a", "b"]);

        let invoice_number = FieldKey {
            scope: CanonicalScope::Root,
            key: "InvoiceNumber".into(),
        };
        let amount = FieldKey {
            scope: CanonicalScope::Root,
            key: "Amount".into(),
        };
        assert_eq!(
            m.rows[&invoice_number],
            ["a".to_string(), "b".to_string()].into_iter().collect()
        );
        assert_eq!(
            m.rows[&amount],
            ["a".to_string()].into_iter().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn test_gap_report_lists_uncovered_fields() {
        let out = two_spoke_output();
        let gaps = gap_report(&out);
        // Only spoke b is missing Amount.
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].spoke, "b");
        assert_eq!(gaps[0].field.key, "Amount");
    }

    #[test]
    fn test_fallback_graph_lists_edges() {
        let a = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            fallbacks = ["Doc.Alt"]
            [Doc.Alt]
            type = "identifier""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&a),
        }];
        let out = compile(&spokes, &BTreeSet::new());
        let graph = fallback_graph(&out.irs["a"]);
        assert_eq!(graph[&NodeId::new("Doc.ID")], [NodeId::new("Doc.Alt")]);
        assert!(!graph.contains_key(&NodeId::new("Doc.Alt")));
    }

    #[test]
    fn test_covered_canonical_fields_lists_mapped_keys() {
        let out = two_spoke_output();
        let covered = covered_canonical_fields(&out.irs["a"]);
        assert_eq!(
            covered,
            ["Amount".to_string(), "InvoiceNumber".to_string()]
                .into_iter()
                .collect()
        );
        // Spoke b maps only InvoiceNumber.
        assert_eq!(
            covered_canonical_fields(&out.irs["b"]),
            ["InvoiceNumber".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn test_required_canonical_fields_only_required_nodes() {
        let m = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            required = true
            canonical_key = "InvoiceNumber"
            [Doc.Total]
            type = "decimal"
            canonical_key = "Amount""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&m),
        }];
        let out = compile(&spokes, &BTreeSet::new());
        let ir = &out.irs["a"];
        assert_eq!(
            required_canonical_fields(ir),
            ["InvoiceNumber".to_string()].into_iter().collect()
        );
        // The optional Amount is covered but not required.
        assert!(covered_canonical_fields(ir).contains("Amount"));
    }

    #[test]
    fn test_collection_child_fields_are_scope_qualified() {
        let m = mapping(
            "a:1",
            r#"[Line]
            type = "collection"
            canonical_key = "Lines"
            [Line.ID]
            type = "identifier"
            required = true
            canonical_key = "LineId""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&m),
        }];
        let out = compile(&spokes, &BTreeSet::new());
        let ir = &out.irs["a"];
        assert!(covered_canonical_fields(ir).contains("Lines/LineId"));
        assert!(required_canonical_fields(ir).contains("Lines/LineId"));
    }

    #[test]
    fn test_render_coverage_markdown_is_deterministic() {
        let out = two_spoke_output();
        let m = coverage_matrix(&out);
        let a = render_coverage_markdown(&m);
        let b = render_coverage_markdown(&m);
        assert_eq!(a, b);
        assert!(a.contains("Canonical field"));
        assert!(a.contains("Amount"));
    }
}
