//! The build-time compiler entry point
//!
//! [`compile`] runs the whole pipeline over a set of spokes: build each spoke's
//! normalized [`MappingIr`], derive the shared canonical hub from the union of
//! their canonical keys, then validate every spoke against the source metadata
//! and the hub. All diagnostics from every stage are aggregated into one
//! [`CompileOutput`] (R9: never first-error-only), in deterministic order.
//!
//! The IRs and hub it returns are the inputs to the static-analysis comparison
//! tool ([`crate::report`]) and to codegen.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Diagnostic, Severity};
use crate::hub::{CanonicalModel, derive_hub};
use crate::ir::{MappingIr, build_ir};
use crate::parse::ParsedMapping;
use crate::source_model::SourceModelMeta;
use crate::validate::{ValidationInput, validate};

/// The adapter names the compiler accepts in a node's `adapter` field.
///
/// Every name listed here must exist as a free function in
/// `einvoice_transformator::adapter` — generated code calls `adapter::<name>`
/// directly, so a missing implementation fails the consuming crate's build
/// loudly. Callers pass [`known_adapters`] to [`compile`].
pub const KNOWN_ADAPTERS: &[&str] = &["uppercase_currency"];

/// [`KNOWN_ADAPTERS`] as the set shape [`compile`] takes.
pub fn known_adapters() -> BTreeSet<String> {
    KNOWN_ADAPTERS.iter().map(|s| s.to_string()).collect()
}

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
pub fn compile(spokes: &[SpokeInput], adapters: &BTreeSet<String>) -> CompileOutput {
    let mut irs = BTreeMap::new();
    let mut sources: BTreeMap<String, SourceModelMeta> = BTreeMap::new();
    let mut diagnostics = Vec::new();

    // Stage 1–6 per spoke: build the normalized IR + synthesize its source model.
    for spoke in spokes {
        let (ir, source, ir_diags) = build_ir(spoke.chain);
        diagnostics.extend(prefix_spoke(&spoke.id, ir_diags));
        irs.insert(spoke.id.clone(), ir);
        sources.insert(spoke.id.clone(), source);
    }

    // Stage: derive the hub from every spoke's canonical keys. Borrow the IRs in
    // place; no need to clone them into a temporary Vec.
    let (hub, hub_diags) = derive_hub(irs.values());
    diagnostics.extend(hub_diags);

    // Stage 8–20 per spoke: validate against the synthesized source + adapters.
    for spoke in spokes {
        let ir = &irs[&spoke.id];
        let diags = validate(&ValidationInput {
            ir,
            source: &sources[&spoke.id],
            adapters,
        });
        diagnostics.extend(prefix_spoke(&spoke.id, diags));
    }

    CompileOutput {
        irs,
        sources,
        hub,
        diagnostics,
    }
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
        let out = compile(&spokes, &BTreeSet::new());
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        assert_eq!(out.irs.len(), 2);
        assert_eq!(out.hub.len(), 1, "shared canonical key merges");
    }

    #[test]
    fn test_compile_aggregates_errors_from_multiple_stages() {
        // A per-spoke validate error (E050 unknown adapter) AND a cross-spoke hub
        // conflict (E010) must both surface (R9 — never first-error-only).
        let a = mapping(
            "a:1",
            r#"[Doc.Total]
            type = "decimal"
            canonical_key = "Amount"
            adapter = "nope""#,
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
        let out = compile(&spokes, &BTreeSet::new());
        assert!(out.has_errors());
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&"E010"), "cross-spoke conflict: {codes:?}");
        assert!(codes.contains(&"E050"), "unknown adapter: {codes:?}");
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
        let out = compile(&spokes, &BTreeSet::new());
        let source = out.sources.get("a").expect("source for spoke `a`");
        assert_eq!(source.root, "Doc");
        assert!(source.structs.contains_key("Doc"));
    }

    #[test]
    fn test_compile_accepts_known_adapter() {
        // `uppercase_currency` is a compiler-known adapter; a mapping using it
        // must compile clean when the caller passes `known_adapters()`.
        let a = mapping(
            "a:1",
            r#"[Doc.Currency]
            type = "currency"
            canonical_key = "DocumentCurrency"
            adapter = "uppercase_currency""#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&a),
        }];
        let out = compile(&spokes, &known_adapters());
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
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
        let first = compile(&spokes, &BTreeSet::new());
        let second = compile(&spokes, &BTreeSet::new());
        assert_eq!(first.irs, second.irs);
        assert_eq!(first.hub, second.hub);
        assert_eq!(first.diagnostics, second.diagnostics);
    }
}
