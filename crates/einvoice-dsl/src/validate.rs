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
//! - `E014` canonical key (declared, or mirrored by `clone_of`) not `PascalCase`.
//! - `E020` source model id mismatch (`[meta].source_model` vs the metadata).
//! - `E021` unresolvable source path.
//! - `E022` collection node whose path is not a repeated (`Vec`) field.
//! - `E023` scalar node whose path resolves to a struct, not a leaf.
//! - `E030` fallback target does not exist (after resolution).
//! - `E031` fallback target type is incompatible.
//! - `E032` fallback target is not in the same scope as the referring node.
//! - `E033` fallback reference cycle.
//! - `E040` `multiple = "join"` without `join_with`, or `join_with` without join.
//! - `E043` `multiple` combined with `fallbacks`.
//! - `E060` `constant` on a collection node.
//! - `E061` `constant` literal does not parse under the node's `type`.
//! - `E062` `constant` combined with `fallbacks`, `multiple` or `codec` (the
//!   constant is emitted verbatim on write; none of these apply to it —
//!   `normalize` is read-side and may accompany it).
//! - `E070` `clone_of` on a collection node, or combined with `canonical_key`,
//!   `constant`, `fallbacks` or `multiple`.
//! - `E071` `clone_of` target key not declared by a primary node in the
//!   referenced scope (the node's own, `$parent`, or `$root`).
//! - `E072` `clone_of` node's `type` differs from its target's.
//! - `E093` `clone_of` derivation path is malformed (`$sibling.Key`,
//!   `$root.Lines.LineId`), or `$parent` is used at root scope.
//! - `E084` unknown codec id.
//! - `E085` codec on a collection, or codec `for_type` differs from the node's
//!   `type`.
//!
//! Unknown TOML fields (`E001`), missing `path`/`type` (`E002`), and cross-spoke
//! hub conflicts (`E010`/`E011`) are caught earlier (parse / resolve / hub), as
//! are the source-model shape checks — namespaces (`E080`–`E083`), codec wire
//! attributes (`E087`) and `match` selectors (`E090`–`E092`) — which synthesis
//! reports while it builds the struct tree.

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::CodecTable;
use crate::error::{Diagnostic, Severity};
use crate::ident::is_canonical_key;
use crate::ir::MappingIr;
use crate::node::{DerivationScope, NodeId, Scope, SourceNode};
use crate::source_model::{PathError, SourceModelMeta, resolve_path_from};
use crate::types::MappingType;

/// Inputs to the validation pipeline.
pub struct ValidationInput<'a> {
    /// The normalized mapping under validation.
    pub ir: &'a MappingIr,
    /// Typed source-model metadata to resolve `path`s against.
    pub source: &'a SourceModelMeta,
    /// The shared codec table a node's `codec` must name.
    pub codecs: &'a CodecTable,
}

/// Validates one mapping, returning every diagnostic in deterministic order.
pub fn validate(input: &ValidationInput) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    check_source_model_id(input, &mut diags);
    for node in input.ir.nodes.values() {
        check_path(node, input, &mut diags);
        check_canonical_key(node, &mut diags);
        check_structural(node, &mut diags);
        check_fallbacks(node, input.ir, &mut diags);
        check_codec(node, input.codecs, &mut diags);
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

/// `E014` when a node's canonical key — the one it declares, or the one its
/// `clone_of` mirrors — is not a `PascalCase` identifier. The key names a hub
/// field and, for a collection, the `<Key>Item` struct, so anything else would
/// surface as a rustc error in the generated hub rather than here.
fn check_canonical_key(node: &SourceNode, diags: &mut Vec<Diagnostic>) {
    let mirrored = match node.derivation() {
        Some(Ok(derivation)) => Some(derivation.key),
        _ => None, // absent, or malformed (E093)
    };
    for (field, key) in [
        ("canonical_key", node.canonical_key.as_deref()),
        ("clone_of", mirrored),
    ] {
        if let Some(key) = key
            && !is_canonical_key(key)
        {
            diags.push(err(
                "E014",
                &node.id,
                format!(
                    "`{field}` names `{key}`, which is not a canonical key: use PascalCase — an \
                     upper-case ASCII letter, then ASCII letters and digits (and not `Self`)"
                ),
            ));
        }
    }
}

fn check_path(node: &SourceNode, input: &ValidationInput, diags: &mut Vec<Diagnostic>) {
    // Skip path resolution when the model id is wrong (already reported); the
    // struct table would not be the right one to resolve against.
    if source_model_mismatch(input) {
        return;
    }
    // A node synthesis could not place has no source path, and synthesis has
    // already reported why (E024, E025, E026, E081, E087, …).
    if node.source_path.is_empty() {
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

    // `normalize` is read-side and may accompany a constant; the read collapse
    // and transform features below have no meaning for a pinned write value.
    for (set, field) in [
        (!node.fallbacks.is_empty(), "fallbacks"),
        (node.multiple.is_some(), "multiple"),
        (node.codec.is_some(), "codec"),
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
    match ty {
        MappingType::String | MappingType::Identifier => None,
        MappingType::UnitCode => (!(1..=3).contains(&value.len())
            || !value.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| "expected 1–3 ASCII letters or digits".to_string()),
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
        MappingType::Date => (!is_iso_date(value))
            .then(|| "expected `YYYY-MM-DD` with month 01–12 and day 01–31".to_string()),
        MappingType::Datetime => (!is_iso_datetime(value)).then(|| {
            "expected `YYYY-MM-DDThh:mm:ss` (in range, optional fraction/zone)".to_string()
        }),
        MappingType::Collection => unreachable!("E060 rejects collections before this check"),
    }
}

/// Two ASCII digits at `b[i..i + 2]` as a number, if both are digits.
fn two_digits(b: &[u8], i: usize) -> Option<u8> {
    match b.get(i..i + 2)? {
        [hi, lo] if hi.is_ascii_digit() && lo.is_ascii_digit() => {
            Some((hi - b'0') * 10 + (lo - b'0'))
        }
        _ => None,
    }
}

/// A `YYYY-MM-DD` date with month 01–12 and day 01–31: the same shape check the
/// runtime's `validate::is_date` applies to read values, so a constant the
/// build accepts is one the runtime would accept too.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && two_digits(b, 5).is_some_and(|m| (1..=12).contains(&m))
        && two_digits(b, 8).is_some_and(|d| (1..=31).contains(&d))
}

/// An [`is_iso_date`] date, `T`, an in-range `hh:mm:ss`, then nothing, `Z`, a
/// fraction or a zone offset — mirroring the runtime's `validate::is_datetime`.
fn is_iso_datetime(s: &str) -> bool {
    let Some((date, time)) = s.split_once('T') else {
        return false;
    };
    let b = time.as_bytes();
    let in_range = |i: usize, max: u8| two_digits(b, i).is_some_and(|v| v <= max);
    let hms = b.len() >= 8
        && b[2] == b':'
        && b[5] == b':'
        && in_range(0, 23)
        && in_range(3, 59)
        && in_range(6, 59);
    // `hms` guarantees bytes 0..8 are ASCII, so slicing at 8 is a char boundary.
    is_iso_date(date)
        && hms
        && matches!(&time[8..], tail if tail.is_empty() || tail == "Z" || tail.starts_with(['.', '+', '-']))
}

/// Validates a node's `clone_of`: role exclusions (E070), a well-formed
/// derivation path that resolves to a scope (E093), target key existence in
/// that scope (E071), and type agreement with the target node (E072).
///
/// A clone is a write-only mirror plus a read-side consistency check, so it
/// cannot also be a primary (`canonical_key`), a `constant`, or carry read
/// collapse features (`fallbacks`, `multiple`) — and a
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

    // Resolve the derivation to the scope the key must be declared in.
    let derivation = match crate::node::parse_derivation(target_key) {
        Ok(d) => d,
        Err(reason) => {
            diags.push(err("E093", &node.id, format!("invalid clone_of: {reason}")));
            return;
        }
    };
    let target_scope = match derivation.scope {
        DerivationScope::Own => node.scope.clone(),
        DerivationScope::Root => Scope::Root,
        DerivationScope::Parent => match &node.scope {
            Scope::Collection(coll) => match ir.nodes.get(coll) {
                Some(coll_node) => coll_node.scope.clone(),
                None => return,
            },
            Scope::Root => {
                diags.push(err(
                    "E093",
                    &node.id,
                    format!("invalid clone_of `{target_key}`: a root-scope node has no `$parent`"),
                ));
                return;
            }
        },
    };
    let key = derivation.key;
    let Some(target) = ir
        .nodes
        .values()
        .find(|n| n.canonical_key.as_deref() == Some(key) && n.scope == target_scope)
    else {
        let where_ = match derivation.scope {
            DerivationScope::Own => "in this scope",
            DerivationScope::Parent => "in the parent scope",
            DerivationScope::Root => "at the root",
        };
        diags.push(err(
            "E071",
            &node.id,
            format!("clone_of target `{key}` is not a canonical key declared {where_}"),
        ));
        return;
    };
    if target.source_type != node.source_type {
        diags.push(err(
            "E072",
            &node.id,
            format!(
                "clone of `{key}` is declared `{}` but the target is `{}`; \
                 the types must match",
                node.source_type, target.source_type
            ),
        ));
    }
}

/// `E084`/`E085`: a node's `codec` must name a known codec whose `for_type` is
/// the node's own scalar type.
fn check_codec(node: &SourceNode, codecs: &CodecTable, diags: &mut Vec<Diagnostic>) {
    let Some(id) = &node.codec else {
        return;
    };
    if node.is_collection() {
        diags.push(err(
            "E085",
            &node.id,
            "`codec` is only valid on a scalar node".to_string(),
        ));
        return;
    }
    match codecs.get(id) {
        None => diags.push(err("E084", &node.id, format!("unknown codec `{id}`"))),
        Some(codec) if codec.for_type != node.source_type => diags.push(err(
            "E085",
            &node.id,
            format!(
                "codec `{id}` is for `{}` values but the node's type is `{}`",
                codec.for_type, node.source_type
            ),
        )),
        Some(_) => {}
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
        run_with(body, &CodecTable::new())
    }

    fn run_with(body: &str, codecs: &CodecTable) -> Vec<Diagnostic> {
        let (ir, source) = compiled(body);
        validate(&ValidationInput {
            ir: &ir,
            source: &source,
            codecs,
        })
    }

    fn date_codecs() -> CodecTable {
        crate::codec::parse_codecs(
            "[codec.cii-date-102]\nfor_type = \"date\"\nlexical = \"YYYYMMDD\"\nwire = { \"@format\" = \"102\" }",
        )
        .expect("codecs parse")
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect()
    }

    const ROOT_CURRENCY_LINES: &str = r#"[Invoice.DocumentCurrencyCode]
            type = "currency"
            canonical_key = "DocumentCurrency"

            [InvoiceLine]
            type = "collection"
            canonical_key = "Lines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId"

            [InvoiceLine.AllowanceCharge]
            type = "collection"
            canonical_key = "LineCharges"

            [InvoiceLine.AllowanceCharge.Amount]
            type = "decimal"
            canonical_key = "ChargeAmount""#;

    #[test]
    fn test_root_and_parent_derivations_resolve() {
        let diags = run(&format!(
            "{ROOT_CURRENCY_LINES}\n\n[InvoiceLine.AllowanceCharge.Amount.currencyID]\nxml = \"@currencyID\"\ntype = \"currency\"\nclone_of = \"$root.DocumentCurrency\"\n\n[InvoiceLine.AllowanceCharge.Ref]\ntype = \"identifier\"\nclone_of = \"$parent.LineId\""
        ));
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_parent_derivation_at_root_is_e093() {
        let diags = run(&format!(
            "{ROOT_CURRENCY_LINES}\n\n[Invoice.Ref]\ntype = \"currency\"\nclone_of = \"$parent.DocumentCurrency\""
        ));
        assert_eq!(codes(&diags), ["E093"], "{diags:?}");
    }

    #[test]
    fn test_malformed_derivation_is_e093() {
        for bad in ["$sibling.DocumentCurrency", "$root.Lines.LineId", "$root"] {
            let diags = run(&format!(
                "{ROOT_CURRENCY_LINES}\n\n[InvoiceLine.Ref]\ntype = \"identifier\"\nclone_of = \"{bad}\""
            ));
            assert_eq!(codes(&diags), ["E093"], "{bad}: {diags:?}");
        }
    }

    #[test]
    fn test_root_derivation_of_a_line_key_is_e071() {
        // `LineId` lives in the line scope, not at the root.
        let diags = run(&format!(
            "{ROOT_CURRENCY_LINES}\n\n[InvoiceLine.AllowanceCharge.Ref]\ntype = \"identifier\"\nclone_of = \"$root.LineId\""
        ));
        assert_eq!(codes(&diags), ["E071"], "{diags:?}");
        assert!(diags[0].message.contains("at the root"));
    }

    #[test]
    fn test_root_derivation_type_mismatch_is_e072() {
        let diags = run(&format!(
            "{ROOT_CURRENCY_LINES}\n\n[InvoiceLine.Ref]\ntype = \"string\"\nclone_of = \"$root.DocumentCurrency\""
        ));
        assert_eq!(codes(&diags), ["E072"], "{diags:?}");
    }

    #[test]
    fn test_known_codec_of_matching_type_is_clean() {
        let diags = run_with(
            r#"[Invoice.IssueDate]
            type = "date"
            canonical_key = "IssueDate"
            codec = "cii-date-102""#,
            &date_codecs(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_unknown_codec_is_e084() {
        let diags = run_with(
            r#"[Invoice.IssueDate]
            type = "date"
            codec = "nope""#,
            &date_codecs(),
        );
        assert_eq!(codes(&diags), ["E084"]);
    }

    #[test]
    fn test_codec_type_mismatch_is_e085() {
        let diags = run_with(
            r#"[Invoice.Note]
            type = "string"
            codec = "cii-date-102""#,
            &date_codecs(),
        );
        assert_eq!(codes(&diags), ["E085"]);
        assert!(diags[0].message.contains("`date`") && diags[0].message.contains("`string`"));
    }

    #[test]
    fn test_codec_on_collection_is_e085() {
        let diags = run_with(
            r#"[Lines]
            type = "collection"
            codec = "cii-date-102""#,
            &date_codecs(),
        );
        assert!(codes(&diags).contains(&"E085"), "{diags:?}");
    }

    #[test]
    fn test_constant_with_codec_is_e062() {
        let diags = run_with(
            r#"[Invoice.IssueDate]
            type = "date"
            constant = "2026-01-01"
            codec = "cii-date-102""#,
            &date_codecs(),
        );
        assert!(codes(&diags).contains(&"E062"), "{diags:?}");
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
    #[case::date_month_out_of_range("date", "2026-13-01")]
    #[case::date_day_out_of_range("date", "2026-01-32")]
    #[case::date_day_zero("date", "2026-01-00")]
    #[case::datetime_bad_month("datetime", "2026-00-01T10:00:00")]
    #[case::datetime_no_time("datetime", "2026-01-01T")]
    #[case::datetime_garbage_time("datetime", "2026-01-01Tgarbage")]
    #[case::datetime_hour_out_of_range("datetime", "2026-01-01T24:00:00")]
    #[case::datetime_bad_tail("datetime", "2026-01-01T10:00:00X")]
    #[case::unit_code_too_long("unit_code", "ABCD")]
    #[case::unit_code_symbol("unit_code", "m²")]

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
    #[case::datetime_zulu("datetime", "2024-01-01T23:59:59Z")]
    #[case::datetime_fraction_offset("datetime", "2024-01-01T10:00:00.5+01:00")]
    #[case::date_day_31("date", "2024-12-31")]
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
    fn test_constant_combined_with_read_collapse_is_e062(#[case] extra: &str) {
        let diags = run(&format!(
            "[Invoice.X]\ntype = \"identifier\"\nconstant = \"v\"\n{extra}"
        ));
        assert!(codes(&diags).contains(&"E062"), "{extra}: {diags:?}");
    }

    #[test]
    fn test_constant_with_normalize_is_clean() {
        // `normalize` shapes what is read; the constant is what is written.
        let diags = run(
            "[Invoice.X]\ntype = \"identifier\"\ncanonical_key = \"X\"\nconstant = \"v\"\nnormalize = [\"trim\"]",
        );
        assert!(diags.is_empty(), "{diags:?}");
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
    fn test_clone_of_combined_with_other_roles_is_e070(#[case] extra: &str) {
        let diags = run(&format!(
            "[Invoice.ID]\ntype = \"identifier\"\ncanonical_key = \"InvoiceNumber\"\n\n\
             [Invoice.Copy]\ntype = \"identifier\"\nclone_of = \"InvoiceNumber\"\n{extra}"
        ));
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
            fallbacks = ["Invoice.Alt"]

            [Invoice.Alt]
            type = "string""#);
        let c = codes(&diags);
        assert!(c.contains(&"E040"), "{c:?}");
        assert!(c.contains(&"E043"), "{c:?}");
    }
}
