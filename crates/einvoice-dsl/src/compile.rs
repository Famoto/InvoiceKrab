//! The build-time compiler entry point
//!
//! [`compile`] runs the whole pipeline over a set of spokes: build each spoke's
//! normalized [`MappingIr`], derive the shared canonical hub from the union of
//! their canonical keys, validate every spoke against the source metadata and
//! the hub, then check the spokes' `required` write routes against each other
//! (`W095`: a required key no other spoke supplies). All diagnostics from every stage are aggregated into one
//! [`CompileOutput`] (R9: never first-error-only), in deterministic order.
//!
//! The IRs and hub it returns are the inputs to the static-analysis comparison
//! tool ([`crate::report`]) and to codegen.

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::CodecTable;
use crate::codegen::naming::item_struct_name;
use crate::contract::{check_required_routes, spoke_contract};
use crate::error::{Diagnostic, Severity};
use crate::hub::{CanonicalModel, derive_hub};
use crate::ir::{MappingIr, build_ir_with};
use crate::parse::ParsedMapping;
use crate::source_model::SourceModelMeta;
use crate::validate::{ValidationInput, validate};

/// One spoke to compile: its id and its inheritance chain (ancestor-first). The
/// typed source model is synthesized from the chain's nodes by [`build_ir`].
pub struct SpokeInput<'a> {
    /// Stable spoke id (used to key the output IRs and in reports).
    pub id: String,
    /// Inheritance chain, ancestor-first and leaf-last (usually one element).
    pub chain: &'a [ParsedMapping],
}

/// The aggregated result of compiling a set of spokes.
#[derive(Debug, Clone)]
pub struct CompileOutput {
    /// Normalized IR per spoke id (deterministic order).
    pub irs: BTreeMap<String, MappingIr>,
    /// The synthesized typed source model per spoke id, keyed identically to
    /// [`Self::irs`]. Codegen consumes these alongside the IRs so it compiles
    /// through this one validated pipeline instead of re-running [`build_ir`].
    pub sources: BTreeMap<String, SourceModelMeta>,
    /// The canonical hub derived from all spokes.
    pub hub: CanonicalModel,
    /// Every diagnostic, in deterministic order.
    pub diagnostics: Vec<Diagnostic>,
}

impl CompileOutput {
    /// Whether any diagnostic is an error (compilation fails).
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Compiles a set of spokes into IRs + the derived hub + aggregated diagnostics.
/// `codecs` is the shared codec table (loaded from `config/codecs/`) that nodes
/// may name with `codec`.
pub fn compile(spokes: &[SpokeInput], codecs: &CodecTable) -> CompileOutput {
    let mut irs = BTreeMap::new();
    let mut sources: BTreeMap<String, SourceModelMeta> = BTreeMap::new();
    let mut diagnostics = Vec::new();

    // Stage 1–6 per spoke: build the normalized IR + synthesize its source model.
    for spoke in spokes {
        let (ir, source, ir_diags) = build_ir_with(spoke.chain, codecs);
        diagnostics.extend(prefix_spoke(&spoke.id, ir_diags));
        irs.insert(spoke.id.clone(), ir);
        sources.insert(spoke.id.clone(), source);
    }

    // Stage: derive the hub from every spoke's canonical keys. Borrow the IRs in
    // place; no need to clone them into a temporary Vec.
    let (hub, hub_diags) = derive_hub(irs.values());
    diagnostics.extend(hub_diags);
    diagnostics.extend(check_hub_type_shadowing(&hub, &sources));

    // Stage 8–20 per spoke: validate against the synthesized source + codecs.
    for spoke in spokes {
        let ir = &irs[&spoke.id];
        let diags = validate(&ValidationInput {
            ir,
            source: &sources[&spoke.id],
            codecs,
        });
        diagnostics.extend(prefix_spoke(&spoke.id, diags));
    }

    // Stage: the `required` contract across spokes — a key a spoke requires
    // from the hub must be mapped by some other spoke, else no transform can
    // supply it (W095).
    let contracts = spokes
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                spoke_contract(&s.id, &irs[&s.id], &sources[&s.id]),
            )
        })
        .collect();
    diagnostics.extend(check_required_routes(&contracts));

    CompileOutput {
        irs,
        sources,
        hub,
        diagnostics,
    }
}

/// `E012` for a synthesized source struct named like a type of the generated
/// hub (`MainKey`, or a collection's `<Key>Item`). Spoke modules glob-import the
/// hub, so such a struct would shadow the hub type its mappers name. Synthesis
/// cannot rename it away — the hub is derived from every spoke afterwards — so
/// it is reported, naming the element to rename.
fn check_hub_type_shadowing(
    hub: &CanonicalModel,
    sources: &BTreeMap<String, SourceModelMeta>,
) -> Vec<Diagnostic> {
    let hub_types: BTreeSet<String> = std::iter::once("MainKey".to_string())
        .chain(
            hub.fields
                .values()
                .filter(|f| f.is_collection)
                .map(|f| item_struct_name(&f.key)),
        )
        .collect();
    let mut diags = Vec::new();
    for (spoke, source) in sources {
        for name in source.structs.keys().filter(|n| hub_types.contains(*n)) {
            diags.push(Diagnostic {
                code: "E012".to_string(),
                severity: Severity::Error,
                source_node: Some(spoke.clone()),
                message: format!(
                    "the source struct `{name}` synthesized for an element of this spoke has the \
                     name of a generated hub type; rename the element's node id and bind the \
                     element with `xml`"
                ),
                span: None,
            });
        }
    }
    diags
}

/// Tags each diagnostic's node id with its spoke so identical node ids across
/// spokes stay distinguishable in the aggregated report.
fn prefix_spoke(spoke: &str, diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diags
        .into_iter()
        .map(|mut d| {
            d.source_node = Some(match d.source_node {
                Some(node) => format!("{spoke}::{node}"),
                None => spoke.to_string(),
            });
            d
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_mapping;

    fn mapping(model_id: &str, body: &str) -> ParsedMapping {
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

    #[test]
    fn test_compile_clean_two_spokes() {
        let a = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
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
        let out = compile(&spokes, &CodecTable::new());
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        assert_eq!(out.irs.len(), 2);
        assert_eq!(out.hub.len(), 1, "shared canonical key merges");
    }

    #[test]
    fn test_compile_aggregates_errors_from_multiple_stages() {
        // A per-spoke validate error (E084 unknown codec) AND a cross-spoke hub
        // conflict (E010) must both surface (R9 — never first-error-only).
        let a = mapping(
            "a:1",
            r#"[Doc.Total]
            type = "decimal"
            canonical_key = "Amount"
            codec = "nope""#,
        );
        let b = mapping(
            "b:1",
            r#"[Doc.Total]
            type = "string"
            canonical_key = "Amount""#,
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
        let out = compile(&spokes, &CodecTable::new());
        assert!(out.has_errors());
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&"E010"), "cross-spoke conflict: {codes:?}");
        assert!(codes.contains(&"E084"), "unknown codec: {codes:?}");
    }

    #[test]
    fn test_compile_exposes_synthesized_source_per_spoke() {
        // Codegen consumers (the build script) need each spoke's synthesized
        // source model, keyed by the same id as `irs`, so they can compile
        // through the one validated pipeline rather than re-running `build_ir`.
        let a = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&a),
        }];
        let out = compile(&spokes, &CodecTable::new());
        let source = out.sources.get("a").expect("source for spoke `a`");
        assert_eq!(source.root, "Doc");
        assert!(source.structs.contains_key("Doc"));
    }

    #[test]
    fn test_compile_is_deterministic() {
        let a = mapping(
            "a:1",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&a),
        }];
        let first = compile(&spokes, &CodecTable::new());
        let second = compile(&spokes, &CodecTable::new());
        assert_eq!(first.irs, second.irs);
        assert_eq!(first.hub, second.hub);
        assert_eq!(first.diagnostics, second.diagnostics);
    }
}
