//! Compile-time validation pipeline.
//!
//! Runs over the normalized [`MappingIr`] (inheritance, disabled removal, and
//! defaults already applied) together with the typed source-model metadata and
//! the derived canonical hub. Every check appends to a diagnostic list rather
//! than stopping at the first error (R9: never first-error-only); the list is
//! returned in deterministic order.
//!
//! # Checks
//!
//! - `E020` source model id mismatch (`[meta].source_model` vs the metadata).
//! - `E021` unresolvable source path.
//! - `E022` collection node whose path is not a repeated (`Vec`) field.
//! - `E023` scalar node whose path resolves to a struct, not a leaf.
//! - `E030` fallback target does not exist (after resolution).
//! - `E031` fallback target type is incompatible.
//! - `E032` fallback target is not in the same scope as the referring node.
//! - `E033` fallback reference cycle.
//! - `E040` `multiple = "join"` without `join_with`, or `join_with` without join.
//! - `E041` `min_items` on a non-collection node.
//! - `E043` `multiple` combined with `fallbacks`.
//! - `E050` unknown adapter name (against the known-adapter set).
//!
//! Unknown TOML fields (`E001`), missing `path`/`type` (`E002`), and cross-spoke
//! hub conflicts (`E010`/`E011`) are caught earlier (parse / resolve / hub).

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Diagnostic, Severity};
use crate::ir::MappingIr;
use crate::node::{NodeId, Scope, SourceNode};
use crate::source_model::{PathError, SourceModelMeta, resolve_path_from};
use crate::types::MappingType;

/// Inputs to the validation pipeline.
pub struct ValidationInput<'a> {
    /// The normalized mapping under validation.
    pub ir: &'a MappingIr,
    /// Typed source-model metadata to resolve `path`s against.
    pub source: &'a SourceModelMeta,
    /// Known adapter names. Empty means none are known.
    pub adapters: &'a BTreeSet<String>,
}

/// Validates one mapping, returning every diagnostic in deterministic order.
pub fn validate(input: &ValidationInput) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    check_source_model_id(input, &mut diags);
    for node in input.ir.nodes.values() {
        check_path(node, input, &mut diags);
        check_structural(node, &mut diags);
        check_fallbacks(node, input.ir, &mut diags);
        check_adapter(node, input.adapters, &mut diags);
    }
    check_fallback_cycles(input.ir, &mut diags);

    diags
}

fn err(code: &str, node: &NodeId, message: String) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity: Severity::Error,
        source_node: Some(node.to_string()),
        message,
        span: None,
    }
}

/// Whether the mapping declares a `source_model` id that disagrees with the
/// supplied metadata. When `[meta].source_model` is omitted (the source tree is
/// defined inline), there is nothing to disagree with.
fn source_model_mismatch(input: &ValidationInput) -> bool {
    input
        .ir
        .meta
        .source_model
        .as_deref()
        .is_some_and(|declared| declared != input.source.model_id)
}

fn check_source_model_id(input: &ValidationInput, diags: &mut Vec<Diagnostic>) {
    if source_model_mismatch(input) {
        diags.push(Diagnostic {
            code: "E020".to_string(),
            severity: Severity::Error,
            source_node: None,
            message: format!(
                "mapping targets source model `{}` but the supplied metadata is for `{}`",
                input.ir.meta.source_model.as_deref().unwrap_or(""),
                input.source.model_id
            ),
            span: None,
        });
    }
}

fn check_path(node: &SourceNode, input: &ValidationInput, diags: &mut Vec<Diagnostic>) {
    // Skip path resolution when the model id is wrong (already reported); the
    // struct table would not be the right one to resolve against.
    if source_model_mismatch(input) {
        return;
    }
    // Collection-child paths resolve against the collection's item struct, not
    // the model root. If the enclosing collection's own
    // path is broken, that node already carries the diagnostic — skip the child.
    let base = match base_struct(node, input.ir, input.source) {
        Ok(b) => b,
        Err(_) => return,
    };
    match resolve_path_from(input.source, &base, &node.source_path) {
        Err(e) => diags.push(err(
            "E021",
            &node.id,
            format!("source path `{}` is invalid: {e}", node.source_path),
        )),
        Ok(resolved) => {
            if node.is_collection() && !resolved.repeated {
                diags.push(err(
                    "E022",
                    &node.id,
                    format!(
                        "collection node path `{}` does not resolve to a repeated (Vec) field",
                        node.source_path
                    ),
                ));
            }
            if !node.is_collection() && resolved.is_struct {
                diags.push(err(
                    "E023",
                    &node.id,
                    format!(
                        "scalar node path `{}` resolves to a struct, not a leaf value",
                        node.source_path
                    ),
                ));
            }
        }
    }
}

/// The struct a node's path resolves against: the model root for root-scoped
/// nodes, or the element struct of the enclosing collection for collection
/// children (resolved recursively to support nesting).
fn base_struct(
    node: &SourceNode,
    ir: &MappingIr,
    source: &SourceModelMeta,
) -> Result<String, PathError> {
    match &node.scope {
        Scope::Root => Ok(source.root.clone()),
        Scope::Collection(coll_id) => {
            let coll = ir
                .nodes
                .get(coll_id)
                .ok_or_else(|| PathError::UnknownRoot(coll_id.to_string()))?;
            let coll_base = base_struct(coll, ir, source)?;
            let resolved = resolve_path_from(source, &coll_base, &coll.source_path)?;
            resolved.struct_name.ok_or_else(|| PathError::NotAStruct {
                struct_name: coll_base,
                field: coll.source_path.clone(),
            })
        }
    }
}

fn check_structural(node: &SourceNode, diags: &mut Vec<Diagnostic>) {
    // join_with is required iff the policy is join.
    let policy_is_join = node.multiple == Some(crate::multiple::MultiplePolicy::Join);
    match (policy_is_join, node.join_with.is_some()) {
        (true, false) => diags.push(err(
            "E040",
            &node.id,
            "multiple = \"join\" requires `join_with`".to_string(),
        )),
        (false, true) => diags.push(err(
            "E040",
            &node.id,
            "`join_with` is only valid with multiple = \"join\"".to_string(),
        )),
        _ => {}
    }

    // min_items only applies to collections.
    if node.min_items.is_some() && !node.is_collection() {
        diags.push(err(
            "E041",
            &node.id,
            "`min_items` is only valid on a collection node".to_string(),
        ));
    }

    // A multi-valued node collapses its own values; a fallback chain on top of
    // that has no defined order of application, so the combination is rejected.
    if node.multiple.is_some() && !node.fallbacks.is_empty() {
        diags.push(err(
            "E043",
            &node.id,
            "`multiple` cannot be combined with `fallbacks`".to_string(),
        ));
    }
}

fn check_fallbacks(node: &SourceNode, ir: &MappingIr, diags: &mut Vec<Diagnostic>) {
    for target_id in &node.fallbacks {
        let Some(target) = ir.nodes.get(target_id) else {
            diags.push(err(
                "E030",
                &node.id,
                format!("fallback target `{target_id}` does not exist or is disabled"),
            ));
            continue;
        };
        if !fallback_type_compatible(node.source_type, target.source_type) {
            diags.push(err(
                "E031",
                &node.id,
                format!(
                    "fallback `{target_id}` has incompatible type `{}` for primary type `{}`",
                    target.source_type, node.source_type
                ),
            ));
        }
        if target.scope != node.scope {
            diags.push(err(
                "E032",
                &node.id,
                format!(
                    "fallback `{target_id}` is in a different scope; a fallback must \
                     share the referring node's scope (codegen reads it against that scope)"
                ),
            ));
        }
    }
}

/// Fallback type compatibility table.
fn fallback_type_compatible(primary: MappingType, fallback: MappingType) -> bool {
    use MappingType::*;
    match primary {
        String | Identifier => matches!(fallback, String | Identifier),
        Date => fallback == Date,
        Datetime => fallback == Datetime,
        Decimal => fallback == Decimal,
        Currency => fallback == Currency,
        UnitCode => fallback == UnitCode,
        Boolean => fallback == Boolean,
        Collection => fallback == Collection,
    }
}

fn check_adapter(node: &SourceNode, adapters: &BTreeSet<String>, diags: &mut Vec<Diagnostic>) {
    if let Some(name) = &node.adapter
        && !adapters.contains(name)
    {
        diags.push(err("E050", &node.id, format!("unknown adapter `{name}`")));
    }
}

/// Detects fallback reference cycles.
fn check_fallback_cycles(ir: &MappingIr, diags: &mut Vec<Diagnostic>) {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let mut state: BTreeMap<&NodeId, Mark> = BTreeMap::new();
    let mut reported: BTreeSet<&NodeId> = BTreeSet::new();

    // Iterative DFS over each node, following only existing fallback edges.
    fn visit<'a>(
        id: &'a NodeId,
        ir: &'a MappingIr,
        state: &mut BTreeMap<&'a NodeId, Mark>,
        reported: &mut BTreeSet<&'a NodeId>,
        diags: &mut Vec<Diagnostic>,
    ) {
        match state.get(id) {
            Some(Mark::Done) => return,
            Some(Mark::Visiting) => {
                if reported.insert(id) {
                    diags.push(err(
                        "E033",
                        id,
                        format!("fallback cycle detected through `{id}`"),
                    ));
                }
                return;
            }
            None => {}
        }
        state.insert(id, Mark::Visiting);
        if let Some(node) = ir.nodes.get(id) {
            for next in &node.fallbacks {
                if ir.nodes.contains_key(next) {
                    visit(next, ir, state, reported, diags);
                }
            }
        }
        state.insert(id, Mark::Done);
    }

    for id in ir.nodes.keys() {
        visit(id, ir, &mut state, &mut reported, diags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::build_ir;
    use crate::parse::parse_mapping;

    const META: &str = r#"
        [meta]
        doc_format = "f"
        format_version = "1"
        mapping_version = "1"
        source_model = "s:1"
        canonical_model = "c:1"
        root = "Invoice"
    "#;

    /// Compiles `body` into its IR + synthesized source model (clean of IR diags).
    fn compiled(body: &str) -> (MappingIr, SourceModelMeta) {
        let src = format!("{META}\n{body}");
        let (ir, source, diags) = build_ir(&[parse_mapping(&src).expect("parses")]);
        assert!(diags.is_empty(), "ir diags: {diags:?}");
        (ir, source)
    }

    fn run(body: &str) -> Vec<Diagnostic> {
        run_with_adapters(body, &BTreeSet::new())
    }

    fn run_with_adapters(body: &str, adapters: &BTreeSet<String>) -> Vec<Diagnostic> {
        let (ir, source) = compiled(body);
        validate(&ValidationInput {
            ir: &ir,
            source: &source,
            adapters,
        })
    }

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_str()).collect()
    }

    #[test]
    fn test_clean_mapping_has_no_diagnostics() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_collection_node_resolves_clean() {
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"

            [Line.ID]
            type = "identifier"
            canonical_key = "LineId""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_collection_child_resolves_against_item_struct() {
        // A collection child's synthesized path resolves against the item struct.
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"

            [Line.Qty]
            type = "decimal"
            canonical_key = "Quantity""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_missing_fallback_target_is_e030() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Invoice.Ghost"]"#);
        assert_eq!(codes(&diags), ["E030"]);
    }

    #[test]
    fn test_incompatible_fallback_type_is_e031() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Invoice.Flag"]

            [Invoice.Flag]
            type = "boolean""#);
        assert!(codes(&diags).contains(&"E031"));
    }

    #[test]
    fn test_compatible_identifier_string_fallback_ok() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Invoice.Alt"]

            [Invoice.Alt]
            type = "string""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_fallback_into_descendant_scope_is_e032() {
        // Root node falling back into a collection-scoped node.
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Line.ID"]

            [Line]
            type = "collection"
            canonical_key = "Lines"

            [Line.ID]
            type = "identifier""#);
        assert!(codes(&diags).contains(&"E032"));
    }

    #[test]
    fn test_fallback_into_ancestor_scope_is_e032() {
        // A collection child falling back to a root-scope node: codegen would read
        // the root path against the item struct, so validation must reject it.
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"

            [Line.ID]
            type = "identifier"
            canonical_key = "LineId"
            fallbacks = ["Invoice.ID"]

            [Invoice.ID]
            type = "identifier""#);
        assert!(codes(&diags).contains(&"E032"), "{diags:?}");
    }

    #[test]
    fn test_fallback_cycle_is_e033() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            fallbacks = ["Invoice.UUID"]

            [Invoice.UUID]
            type = "identifier"
            fallbacks = ["Invoice.ID"]"#);
        assert!(codes(&diags).contains(&"E033"));
    }

    #[test]
    fn test_join_without_join_with_is_e040() {
        let diags = run(r#"[Invoice.Note]
            type = "string"
            multiple = "join""#);
        assert!(codes(&diags).contains(&"E040"));
    }

    #[test]
    fn test_join_with_on_non_join_is_e040() {
        let diags = run(r#"[Invoice.Note]
            type = "string"
            multiple = "first"
            join_with = ", ""#);
        assert!(codes(&diags).contains(&"E040"));
    }

    #[test]
    fn test_multiple_with_fallbacks_is_e043() {
        let diags = run(r#"[Invoice.Note]
            type = "string"
            multiple = "first"
            fallbacks = ["Invoice.Alt"]

            [Invoice.Alt]
            type = "string""#);
        assert!(codes(&diags).contains(&"E043"), "{diags:?}");
    }

    #[test]
    fn test_multiple_join_with_pair_is_clean() {
        let diags = run(r#"[Invoice.Note]
            type = "string"
            canonical_key = "Notes"
            multiple = "join"
            join_with = "\n""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_min_items_on_scalar_is_e041() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            min_items = 1"#);
        assert!(codes(&diags).contains(&"E041"));
    }

    #[test]
    fn test_unknown_adapter_is_e050() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            adapter = "nope""#);
        assert_eq!(codes(&diags), ["E050"]);
    }

    #[test]
    fn test_known_adapter_ok() {
        let adapters: BTreeSet<String> = ["known".to_string()].into_iter().collect();
        let diags = run_with_adapters(
            r#"[Invoice.ID]
            type = "identifier"
            adapter = "known""#,
            &adapters,
        );
        assert!(diags.is_empty());
    }

    #[test]
    fn test_diagnostics_aggregate_not_first_error_only() {
        // Two independent problems on one node must both surface (R9).
        let diags = run(r#"[Invoice.Note]
            type = "string"
            multiple = "join"
            min_items = 1"#);
        let c = codes(&diags);
        assert!(c.contains(&"E040"), "{c:?}");
        assert!(c.contains(&"E041"), "{c:?}");
    }
}
