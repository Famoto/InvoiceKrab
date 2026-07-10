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
//! - `E060` `constant` on a collection node.
//! - `E061` `constant` literal does not parse under the node's `type`.
//! - `E062` `constant` combined with `fallbacks`, `multiple`, `adapter`, or
//!   `normalize` (the constant is emitted verbatim on write; none of these
//!   apply to it).
//! - `E070` `clone_of` on a collection node, or combined with `canonical_key`,
//!   `constant`, `fallbacks`, `multiple`, or `adapter`.
//! - `E071` `clone_of` target key not declared by a primary node in the same
//!   scope.
//! - `E072` `clone_of` node's `type` differs from its target's.
//!
//! Unknown TOML fields (`E001`), missing `path`/`type` (`E002`), and cross-spoke
//! hub conflicts (`E010`/`E011`) are caught earlier (parse / resolve / hub).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Not;

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
        check_constant(node, &mut diags);
        check_clone_of(node, input.ir, &mut diags);
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

/// Validates a node's `constant`: structural exclusions (E060/E062) and the
/// literal parsing under the node's declared `type` (E061), so a typo'd URN or
/// malformed code fails the build instead of surfacing in emitted documents.
fn check_constant(node: &SourceNode, diags: &mut Vec<Diagnostic>) {
    let Some(value) = &node.constant else {
        return;
    };

    if node.is_collection() {
        diags.push(err(
            "E060",
            &node.id,
            "`constant` is not valid on a collection node".to_string(),
        ));
        return;
    }

    if let Some(reason) = constant_literal_error(node.source_type, value) {
        diags.push(err(
            "E061",
            &node.id,
            format!(
                "constant `{value}` is not a valid `{}` literal: {reason}",
                node.source_type
            ),
        ));
    }

    for (set, field) in [
        (!node.fallbacks.is_empty(), "fallbacks"),
        (node.multiple.is_some(), "multiple"),
        (node.adapter.is_some(), "adapter"),
        (!node.normalize.is_empty(), "normalize"),
    ] {
        if set {
            diags.push(err(
                "E062",
                &node.id,
                format!(
                    "`constant` cannot be combined with `{field}`; the literal is \
                     emitted verbatim on write"
                ),
            ));
        }
    }
}

/// Shape checks only — no ISO-4217 table, no calendar arithmetic; the goal is
/// catching typos at compile time, not re-implementing the runtime validators.
//  currency = 3 uppercase letters, date = digit/dash shape; wire the
// runtime `validate` helpers in if a real code table is ever needed.
fn constant_literal_error(ty: MappingType, value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return Some("it is empty".to_string());
    }
    let date_shaped = |s: &str| {
        s.len() == 10
            && s.bytes().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    };
    match ty {
        MappingType::String | MappingType::Identifier | MappingType::UnitCode => None,
        MappingType::Boolean => {
            (value != "true" && value != "false").then(|| "expected `true` or `false`".to_string())
        }
        MappingType::Currency => (value.len() != 3
            || !value.bytes().all(|b| b.is_ascii_uppercase()))
        .then(|| "expected three uppercase ASCII letters".to_string()),
        MappingType::Decimal => {
            let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
            let (int, frac) = digits.split_once('.').unwrap_or((digits, "0"));
            let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            (!all_digits(int) || !all_digits(frac))
                .then(|| "expected a plain decimal number".to_string())
        }
        MappingType::Date => (!date_shaped(value)).then(|| "expected `YYYY-MM-DD`".to_string()),
        MappingType::Datetime => (match (value.get(..10), value.as_bytes().get(10)) {
            (Some(date), Some(b'T')) => date_shaped(date),
            _ => false,
        })
        .not()
        .then(|| "expected `YYYY-MM-DDThh:mm:ss…`".to_string()),
        MappingType::Collection => unreachable!("E060 rejects collections before this check"),
    }
}

/// Validates a node's `clone_of`: role exclusions (E070), target key existence
/// in the node's scope (E071), and type agreement with the target node (E072).
///
/// A clone is a write-only mirror plus a read-side consistency check, so it
/// cannot also be a primary (`canonical_key`), a `constant`, or carry read
/// collapse/transform features (`fallbacks`, `multiple`, `adapter`) — and a
/// collection has no single value to mirror. Clone chains are impossible by
/// construction: the target is a canonical *key*, and clones declare none.
fn check_clone_of(node: &SourceNode, ir: &MappingIr, diags: &mut Vec<Diagnostic>) {
    let Some(target_key) = &node.clone_of else {
        return;
    };

    if node.is_collection() {
        diags.push(err(
            "E070",
            &node.id,
            "`clone_of` is not valid on a collection node".to_string(),
        ));
        return;
    }
    for (set, field) in [
        (node.canonical_key.is_some(), "canonical_key"),
        (node.constant.is_some(), "constant"),
        (!node.fallbacks.is_empty(), "fallbacks"),
        (node.multiple.is_some(), "multiple"),
        (node.adapter.is_some(), "adapter"),
    ] {
        if set {
            diags.push(err(
                "E070",
                &node.id,
                format!(
                    "`clone_of` cannot be combined with `{field}`; a clone only \
                     mirrors its target key"
                ),
            ));
        }
    }

    // The target key must be declared by a primary node in the same scope.
    let Some(target) = ir
        .nodes
        .values()
        .find(|n| n.canonical_key.as_deref() == Some(target_key) && n.scope == node.scope)
    else {
        diags.push(err(
            "E071",
            &node.id,
            format!(
                "clone_of target `{target_key}` is not a canonical key declared \
                 in this scope"
            ),
        ));
        return;
    };
    if target.source_type != node.source_type {
        diags.push(err(
            "E072",
            &node.id,
            format!(
                "clone of `{target_key}` is declared `{}` but the target is `{}`; \
                 the types must match",
                node.source_type, target.source_type
            ),
        ));
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
    fn test_constant_only_node_is_clean() {
        let diags = run(r#"[Invoice.UBLVersionID]
            type = "identifier"
            constant = "2.1""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_constant_with_canonical_key_is_clean() {
        // Transparent read, fixed write: the flagship CustomizationID shape.
        let diags = run(r#"[Invoice.CustomizationID]
            type = "identifier"
            canonical_key = "SpecificationId"
            constant = "urn:cen.eu:en16931:2017""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_constant_on_collection_is_e060() {
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"
            constant = "x"

            [Line.ID]
            type = "identifier"
            canonical_key = "LineId""#);
        assert_eq!(codes(&diags), ["E060"]);
    }

    use rstest::rstest;

    #[rstest]
    #[case::empty("identifier", "  ")]
    #[case::bad_boolean("boolean", "yes")]
    #[case::bad_currency("currency", "eur")]
    #[case::bad_currency_len("currency", "EURO")]
    #[case::bad_decimal("decimal", "1,5")]
    #[case::bad_date("date", "2024-1-1")]
    #[case::bad_datetime("datetime", "2024-01-01 10:00:00")]
    #[case::bad_datetime_literal("datetime", "é234-56-78T12:34:56")]

    fn test_invalid_constant_literal_is_e061(#[case] ty: &str, #[case] value: &str) {
        let diags = run(&format!(
            "[Invoice.X]\ntype = \"{ty}\"\nconstant = \"{value}\""
        ));
        assert_eq!(codes(&diags), ["E061"], "{ty} / {value:?}: {diags:?}");
    }

    #[rstest]
    #[case::boolean("boolean", "false")]
    #[case::currency("currency", "EUR")]
    #[case::decimal_plain("decimal", "19")]
    #[case::decimal_signed("decimal", "-19.00")]
    #[case::date("date", "2024-01-01")]
    #[case::datetime("datetime", "2024-01-01T10:00:00")]
    #[case::unit_code("unit_code", "C62")]
    fn test_valid_constant_literal_is_clean(#[case] ty: &str, #[case] value: &str) {
        let diags = run(&format!(
            "[Invoice.X]\ntype = \"{ty}\"\nconstant = \"{value}\""
        ));
        assert!(diags.is_empty(), "{ty} / {value:?}: {diags:?}");
    }

    #[rstest]
    #[case::fallbacks("fallbacks = [\"Invoice.Alt\"]\n\n[Invoice.Alt]\ntype = \"identifier\"")]
    #[case::multiple("multiple = \"first\"")]
    #[case::adapter("adapter = \"known\"")]
    #[case::normalize("normalize = [\"trim\"]")]
    fn test_constant_combined_with_read_features_is_e062(#[case] extra: &str) {
        let adapters: BTreeSet<String> = ["known".to_string()].into_iter().collect();
        let diags = run_with_adapters(
            &format!("[Invoice.X]\ntype = \"identifier\"\nconstant = \"v\"\n{extra}"),
            &adapters,
        );
        assert!(codes(&diags).contains(&"E062"), "{extra}: {diags:?}");
    }

    #[test]
    fn test_clone_of_valid_is_clean() {
        let diags = run(r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.BuyerReference]
            type = "identifier"
            clone_of = "InvoiceNumber""#);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[rstest]
    #[case::canonical_key("canonical_key = \"Other\"")]
    #[case::constant("constant = \"v\"")]
    #[case::fallbacks("fallbacks = [\"Invoice.Alt\"]\n\n[Invoice.Alt]\ntype = \"identifier\"")]
    #[case::multiple("multiple = \"first\"")]
    #[case::adapter("adapter = \"known\"")]
    fn test_clone_of_combined_with_other_roles_is_e070(#[case] extra: &str) {
        let adapters: BTreeSet<String> = ["known".to_string()].into_iter().collect();
        let diags = run_with_adapters(
            &format!(
                "[Invoice.ID]\ntype = \"identifier\"\ncanonical_key = \"InvoiceNumber\"\n\n\
                 [Invoice.Copy]\ntype = \"identifier\"\nclone_of = \"InvoiceNumber\"\n{extra}"
            ),
            &adapters,
        );
        assert!(codes(&diags).contains(&"E070"), "{extra}: {diags:?}");
    }

    #[test]
    fn test_clone_of_on_collection_is_e070() {
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"

            [Copies]
            type = "collection"
            clone_of = "Lines""#);
        assert!(codes(&diags).contains(&"E070"), "{diags:?}");
    }

    #[test]
    fn test_clone_of_unknown_key_is_e071() {
        let diags = run(r#"[Invoice.Copy]
            type = "identifier"
            clone_of = "Ghost""#);
        assert_eq!(codes(&diags), ["E071"]);
    }

    #[test]
    fn test_clone_of_key_in_other_scope_is_e071() {
        // Target key exists, but only inside a collection scope — a root clone
        // cannot mirror it.
        let diags = run(r#"[Line]
            type = "collection"
            canonical_key = "Lines"

            [Line.ID]
            type = "identifier"
            canonical_key = "LineId"

            [Invoice.Copy]
            type = "identifier"
            clone_of = "LineId""#);
        assert!(codes(&diags).contains(&"E071"), "{diags:?}");
    }

    #[test]
    fn test_clone_of_type_mismatch_is_e072() {
        let diags = run(r#"[Invoice.Total]
            type = "decimal"
            canonical_key = "PayableAmount"

            [Invoice.Copy]
            type = "string"
            clone_of = "PayableAmount""#);
        assert!(codes(&diags).contains(&"E072"), "{diags:?}");
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
