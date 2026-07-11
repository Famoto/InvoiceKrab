//! Build-time codegen of native Rust mappers.
//!
//! This module is the bridge from the compiler's static artifacts to runtime
//! code. Everything is generated from TOML — there are no hand-written model
//! structs. Two entry points emit **Rust source text**:
//!
//! - [`generate_hub`] turns the derived [`CanonicalModel`](crate::hub::CanonicalModel)
//!   into the typed `MainKey` hub struct (one field per canonical key) plus an
//!   item struct per canonical collection.
//! - [`generate_spoke`] turns a spoke's [`MappingIr`](crate::ir::MappingIr) and
//!   its [`SourceModelMeta`](crate::source_model::SourceModelMeta) into: the
//!   typed source structs (with serde/XML binding), `from_xml`/`to_xml`, and the
//!   `read` (source → `MainKey`) / `write` (`MainKey` → source) mappers.
//! - [`generate_source_module`] / [`generate_mapper_module`] split the same
//!   output in two, so a build script can emit one structs module shared by
//!   every spoke whose synthesized source model generates identical text.
//!
//! The runtime never interprets the TOML; it links against the generated Rust,
//! which targets the small `einvoice-transformator` helper API (`normalize`,
//! `validate`, `adapter`, `MappingResult`) and uses native Rust types
//! (`compact_str::CompactString`, `rust_decimal::Decimal`, `bool`, `Vec<…>`)
//! directly.
//!
//! # Structure
//!
//! The generators are split into focused submodules (see `README.md`):
//! [`naming`] (identifier/type helpers), [`hub`], [`source`] (structs + XML I/O),
//! [`read`], [`write`], [`access`] (source-path expressions), [`plan`] (IR
//! classification), and [`diag`] (diagnostic emission).
//!
//! # Behavior
//!
//! The generators are **pure and deterministic**: text in, text out, with all
//! `BTreeMap`s iterated in sorted order so identical inputs yield byte-identical
//! output. The emitted reader, per node: reads the source field, applies
//! `normalize` ops, falls back through `fallbacks`, decodes/validates by `type`,
//! applies an optional `adapter`, enforces `required`/`min_items`, and assigns
//! into the typed `MainKey`. Helper nodes (no `canonical_key`) are read only as
//! fallback sources. A node with a `constant` is written from that literal
//! instead of the hub (spec-pinned values like CIUS `CustomizationID` URNs);
//! its read side is unchanged.
//!
//! # Testing
//!
//! Unit tests cover the pure sub-generators (struct/field rendering, the
//! normalize chain, the decode snippet) and assert on the generated hub + spoke
//! for the reference UBL mapping.

mod access;
mod diag;
mod hub;
pub(crate) mod naming;
mod plan;
mod read;
mod source;
mod write;

pub use hub::generate_hub;

use crate::ir::MappingIr;
use crate::source_model::SourceModelMeta;

use plan::{GenCtx, MappingPlan};

use std::fmt::Write as _;

/// Generates a self-contained Rust module (as text) for one spoke: the typed
/// source structs, `from_xml`/`to_xml`, and the `read`/`write` mappers.
///
/// `hub_module` is the Rust path to the generated hub module (e.g. `super::hub`);
/// the emitted module glob-imports `MainKey` and the item structs from it. The
/// output is deterministic for identical inputs.
pub fn generate_spoke(ir: &MappingIr, source: &SourceModelMeta, hub_module: &str) -> String {
    let mut out = String::new();

    // Plain `//` comments (not `//!`): the output is `include!`d into a module,
    // where an inner doc comment after the brace is rejected (E0753).
    out.push_str("// Generated spoke mapper. Do not edit by hand.\n");
    let _ = writeln!(out, "// Source model: {}", source.model_id);
    let _ = writeln!(
        out,
        "// Mapping: {} v{}",
        ir.meta.doc_format, ir.meta.mapping_version
    );
    out.push('\n');
    out.push_str("use serde::{Deserialize, Serialize};\n");
    mapper_imports(&mut out, hub_module);
    out.push('\n');
    source_section(&mut out, source);
    out.push('\n');
    mapper_section(&mut out, ir, source);

    out
}

/// Generates a self-contained module (as text) holding only a source model's
/// typed structs and `from_xml`/`to_xml` — no mappers. Spokes whose mappings
/// synthesize identical source models can share one such module (the output is
/// deterministic, so equal models yield byte-identical text for deduplication).
pub fn generate_source_module(source: &SourceModelMeta) -> String {
    let mut out = String::new();
    out.push_str("// Generated shared source model. Do not edit by hand.\n");
    let _ = writeln!(out, "// Source model: {}", source.model_id);
    out.push('\n');
    out.push_str("use compact_str::CompactString;\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");
    source_section(&mut out, source);
    out
}

/// Generates a spoke module (as text) that re-exports its typed source structs
/// and XML I/O from `structs_module` (a generated [`generate_source_module`]
/// sibling, e.g. `super::shared_0`) and defines only the `read`/`write`
/// mappers. Pairs with [`generate_source_module`] to deduplicate spokes that
/// share a source model; together they cover exactly what [`generate_spoke`]
/// emits as one module.
pub fn generate_mapper_module(
    ir: &MappingIr,
    source: &SourceModelMeta,
    hub_module: &str,
    structs_module: &str,
) -> String {
    let mut out = String::new();
    out.push_str("// Generated spoke mapper. Do not edit by hand.\n");
    let _ = writeln!(out, "// Source model: {} (structs shared)", source.model_id);
    let _ = writeln!(
        out,
        "// Mapping: {} v{}",
        ir.meta.doc_format, ir.meta.mapping_version
    );
    out.push('\n');
    let _ = writeln!(out, "pub use {structs_module}::*;");
    out.push('\n');
    mapper_imports(&mut out, hub_module);
    out.push('\n');
    mapper_section(&mut out, ir, source);
    out
}

/// The deduplicated codegen plan for a set of spokes: which shared structs
/// modules to emit, and per spoke either its module text or the earlier spoke
/// it aliases. Pure — callers (a build script) own the file writes.
pub struct SpokeDedupPlan {
    /// Shared structs modules to emit, as `(module_name, module_text)`.
    pub shared_modules: Vec<(String, String)>,
    /// Per spoke, in input order: its module text or the spoke it aliases.
    pub modules: Vec<SpokeModule>,
}

/// One spoke's planned output.
#[derive(Debug)]
pub enum SpokeModule {
    /// The spoke's module text — write it to `<slug>.rs`.
    Emit(String),
    /// Byte-identical to this earlier spoke's module: no file, alias it.
    Alias(String),
}

/// Plans deduplicated codegen for `spokes` (`(slug, ir, source)` triples, in
/// emission order):
///
/// 1. Spokes whose synthesized source models generate identical struct text
///    share one `shared_<n>` structs module (collapsing the serde-derive cost
///    of `inherits` families to one expansion) and get mappers-only modules.
/// 2. A spoke whose module body is byte-identical to an earlier spoke's (e.g.
///    an `inherits` child overriding nothing) gets no text of its own — just
///    an alias to the earlier slug.
///
/// Codegen is byte-deterministic, so equality of text is equality of behavior.
/// Generated text starts with an identity comment block (spoke/model names)
/// that legitimately differs between behaviorally identical spokes; all
/// comparisons use the body after the first blank line.
pub fn plan_spoke_dedup(
    spokes: &[(&str, &MappingIr, &SourceModelMeta)],
    hub_module: &str,
) -> SpokeDedupPlan {
    fn body(text: &str) -> &str {
        text.split_once("\n\n").map_or(text, |(_, b)| b)
    }

    let source_texts: Vec<String> = spokes
        .iter()
        .map(|(_, _, source)| generate_source_module(source))
        .collect();
    // Group spokes by identical struct text, in emission order (deterministic).
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, text) in source_texts.iter().enumerate() {
        match groups.iter_mut().find(|(t, _)| body(t) == body(text)) {
            Some((_, members)) => members.push(i),
            None => groups.push((text, vec![i])),
        }
    }

    let mut shared_modules: Vec<(String, String)> = Vec::new();
    let mut structs_module_of: Vec<Option<String>> = vec![None; spokes.len()];
    for (text, members) in &groups {
        if members.len() < 2 {
            continue;
        }
        let name = format!("shared_{}", shared_modules.len());
        // The shared module's header names every sharer instead of carrying
        // the first member's identity.
        let sharers: Vec<&str> = members.iter().map(|&i| spokes[i].0).collect();
        let text = format!(
            "// Generated shared source model. Do not edit by hand.\n// Shared by: {}\n\n{}",
            sharers.join(", "),
            body(text)
        );
        for &i in members {
            structs_module_of[i] = Some(name.clone());
        }
        shared_modules.push((name, text));
    }

    let mut seen: Vec<(String, &str)> = Vec::new(); // (module text, canonical slug)
    let mut modules: Vec<SpokeModule> = Vec::with_capacity(spokes.len());
    for (i, &(slug, ir, source)) in spokes.iter().enumerate() {
        let code = match &structs_module_of[i] {
            Some(shared) => {
                generate_mapper_module(ir, source, hub_module, &format!("super::{shared}"))
            }
            None => generate_spoke(ir, source, hub_module),
        };
        match seen.iter().find(|(text, _)| body(text) == body(&code)) {
            Some((_, canonical)) => modules.push(SpokeModule::Alias(canonical.to_string())),
            None => {
                seen.push((code.clone(), slug));
                modules.push(SpokeModule::Emit(code));
            }
        }
    }

    SpokeDedupPlan {
        shared_modules,
        modules,
    }
}

/// Emits the typed source structs and the root's `from_xml`/`to_xml`.
fn source_section(out: &mut String, source: &SourceModelMeta) {
    source::generate_source_structs(out, source);
    out.push('\n');
    source::generate_xml_io(out, &source.root);
}

/// Emits the imports the `read`/`write` mappers need (the source structs and
/// their serde imports are emitted separately).
fn mapper_imports(out: &mut String, hub_module: &str) {
    out.push_str("use std::str::FromStr as _;\n");
    out.push_str("use compact_str::{CompactString, ToCompactString as _};\n");
    out.push_str("use rust_decimal::Decimal;\n");
    out.push_str(
        "use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};\n",
    );
    out.push_str("use einvoice_transformator::{adapter, normalize, validate};\n");
    let _ = writeln!(out, "use {hub_module}::*;");
}

/// Emits the `read` and `write` mapper functions.
fn mapper_section(out: &mut String, ir: &MappingIr, source: &SourceModelMeta) {
    // The IR classification is the same for both mappers, so build it once and
    // share it across the reader and writer generators.
    let plan = MappingPlan::build(ir);
    let ctx = GenCtx {
        ir,
        source,
        plan: &plan,
    };
    read::generate_read(out, &ctx, &source.root);
    out.push('\n');
    write::generate_write(out, &ctx, &source.root);
}

#[cfg(test)]
mod tests {
    use super::naming::snake_case;
    use super::source::serde_attr;
    use super::{generate_hub, generate_spoke};
    use crate::hub::{CanonicalModel, derive_hub};
    use crate::ir::{MappingIr, build_ir};
    use crate::parse::parse_mapping;
    use crate::source_model::{FieldMeta, FieldType, SourceModelMeta};

    const UBL: &str = r#"
        [meta]
        doc_format = "ubl-invoice"
        format_version = "2.1"
        mapping_version = "1.0"
        canonical_model = "canonical-invoice:1.0"
        root = "Invoice"

        [Invoice.ID]
        type = "identifier"
        canonical_key = "InvoiceNumber"
        required = true
        normalize = ["trim", "empty_as_missing"]

        [Invoice.DocumentCurrencyCode]
        type = "currency"
        canonical_key = "DocumentCurrency"
        normalize = ["trim", "uppercase"]

        [Invoice.LegalMonetaryTotal.PayableAmount]
        type = "decimal"
        canonical_key = "PayableAmount"
        required = true

        [Invoice.LegalMonetaryTotal.PayableAmount.currencyID]
        xml = "@currencyID"
        type = "currency"
        canonical_key = "PayableAmountCurrency"

        [InvoiceLine]
        type = "collection"
        canonical_key = "InvoiceLines"
        required = true
        min_items = 1

        [InvoiceLine.InvoicedQuantity]
        type = "decimal"
        canonical_key = "Quantity"
    "#;

    fn compiled() -> (MappingIr, CanonicalModel, SourceModelMeta) {
        let (ir, source, diags) = build_ir(&[parse_mapping(UBL).expect("parses")]);
        assert!(diags.is_empty(), "{diags:?}");
        let (hub, hub_diags) = derive_hub(std::slice::from_ref(&ir));
        assert!(hub_diags.is_empty(), "{hub_diags:?}");
        (ir, hub, source)
    }

    #[test]
    fn test_snake_case() {
        assert_eq!(snake_case("InvoiceNumber"), "invoice_number");
        assert_eq!(snake_case("LineId"), "line_id");
        assert_eq!(snake_case("Quantity"), "quantity");
    }

    #[test]
    fn test_generate_hub_has_typed_fields_and_item_struct() {
        let (_, hub, _) = compiled();
        let out = generate_hub(&hub);
        assert!(out.contains("pub struct MainKey {"), "{out}");
        assert!(
            out.contains("pub invoice_number: Option<CompactString>,"),
            "{out}"
        );
        assert!(
            out.contains("pub payable_amount: Option<Decimal>,"),
            "{out}"
        );
        assert!(
            out.contains("pub invoice_lines: Vec<InvoiceLinesItem>,"),
            "{out}"
        );
        assert!(out.contains("pub struct InvoiceLinesItem {"), "{out}");
        assert!(out.contains("pub quantity: Option<Decimal>,"), "{out}");
    }

    #[test]
    fn test_generate_spoke_emits_source_structs_and_mappers() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        // typed source struct with XML rename; every leaf is Option + default
        // so absent elements parse (required is a reader diagnostic).
        assert!(out.contains("pub struct Invoice {"), "{out}");
        assert!(
            out.contains(
                "#[serde(rename = \"ID\", default, skip_serializing_if = \"Option::is_none\")]"
            ),
            "{out}"
        );
        // The optional currencyID attribute also carries `default`/skip attrs.
        assert!(out.contains("rename = \"@currencyID\""), "{out}");
        // xml io + mappers
        assert!(out.contains("pub fn from_xml"), "{out}");
        assert!(out.contains("pub fn to_xml"), "{out}");
        assert!(
            out.contains("pub fn read(mut source: Invoice) -> MappingResult<MainKey>"),
            "{out}"
        );
        assert!(
            out.contains("pub fn write(mut main: MainKey) -> MappingResult<Invoice>"),
            "{out}"
        );
    }

    #[test]
    fn test_reader_assigns_typed_fields_and_validates() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        // currency is validated; identifier carried verbatim; decimal parsed.
        assert!(out.contains("validate::is_currency(raw.trim())"), "{out}");
        assert!(out.contains("main.document_currency = Some(raw);"), "{out}");
        assert!(out.contains("Decimal::from_str(raw.trim())"), "{out}");
        // The normalize chain threads the one moved-out inline string by value
        // (no per-op re-borrow / reallocation, no clone).
        assert!(
            out.contains(".take().map(normalize::trim).map(normalize::uppercase)"),
            "{out}"
        );
        assert!(!out.contains("normalize::trim(&s)"), "{out}");
        // Collection loops use per-depth variable names (`item0`, `element0`) and
        // reserve the hub collection up front from the known source length.
        assert!(out.contains("let count0 = elements0.len();"), "{out}");
        assert!(out.contains("main.invoice_lines.reserve(count0);"), "{out}");
        assert!(out.contains("main.invoice_lines.push(item0);"), "{out}");
        assert!(out.contains("item0.quantity = Some(d)"), "{out}");
    }

    #[test]
    fn test_reader_omits_required_missing_branch_for_optional_fields() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        // Optional fields must not carry a dead `else if false { … }`
        // diagnostic block; required fields keep a plain `else` branch.
        assert!(!out.contains("else if false"), "{out}");
        assert!(!out.contains("else if true"), "{out}");
        assert!(out.contains("REQUIRED_MISSING"), "{out}");
    }

    #[test]
    fn test_generate_source_module_is_self_contained_structs_and_xml_io() {
        let (_, _, source) = compiled();
        let out = super::generate_source_module(&source);
        // Self-contained: carries its own imports, structs, and XML I/O …
        assert!(out.contains("use compact_str::CompactString;"), "{out}");
        assert!(
            out.contains("use serde::{Deserialize, Serialize};"),
            "{out}"
        );
        assert!(out.contains("pub struct Invoice {"), "{out}");
        assert!(out.contains("pub fn from_xml"), "{out}");
        assert!(out.contains("pub fn to_xml"), "{out}");
        // … and no mappers.
        assert!(!out.contains("pub fn read"), "{out}");
        assert!(!out.contains("pub fn write"), "{out}");
    }

    #[test]
    fn test_generate_mapper_module_reexports_structs_and_omits_them() {
        let (ir, _, source) = compiled();
        let out = super::generate_mapper_module(&ir, &source, "super::hub", "super::shared_0");
        // Structs come from the shared module, re-exported for callers.
        assert!(out.contains("pub use super::shared_0::*;"), "{out}");
        assert!(!out.contains("pub struct Invoice {"), "{out}");
        assert!(!out.contains("pub fn from_xml"), "{out}");
        // Mappers are present.
        assert!(
            out.contains("pub fn read(mut source: Invoice) -> MappingResult<MainKey>"),
            "{out}"
        );
        assert!(
            out.contains("pub fn write(mut main: MainKey) -> MappingResult<Invoice>"),
            "{out}"
        );
    }

    #[test]
    fn test_split_modules_cover_the_monolithic_spoke() {
        // The split pair must carry the same structs and mappers the monolith
        // does, so build-time dedup can swap representations freely.
        let (ir, _, source) = compiled();
        let monolith = generate_spoke(&ir, &source, "super::hub");
        let src = super::generate_source_module(&source);
        let map = super::generate_mapper_module(&ir, &source, "super::hub", "super::shared_0");
        for needle in ["pub struct Invoice {", "pub fn from_xml", "pub fn to_xml"] {
            assert!(
                monolith.contains(needle) && src.contains(needle),
                "{needle}"
            );
        }
        for needle in [
            "pub fn read(mut source: Invoice)",
            "pub fn write(mut main: MainKey)",
        ] {
            assert!(
                monolith.contains(needle) && map.contains(needle),
                "{needle}"
            );
        }
    }

    #[test]
    fn test_writer_renders_typed_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        assert!(
            out.contains("if let Some(value) = main.invoice_number.take() {"),
            "{out}"
        );
        // Typed values render straight to the inline string type (no
        // intermediate heap `String` for short decimals).
        assert!(
            out.contains("let rendered = value.to_compact_string();"),
            "{out}"
        );
        // Values are rendered once, skipped if empty, then assigned. Interior
        // container structs are `Option<Box<…>>` and materialize lazily on
        // write; every leaf is Option-typed, so assignment wraps in `Some`.
        assert!(
            out.contains(
                "source.legal_monetary_total.get_or_insert_default().payable_amount.get_or_insert_default().value = Some(rendered);"
            ),
            "{out}"
        );
        assert!(out.contains("source.id = Some(rendered);"), "{out}");
        // The source collection is reserved once from the hub item count.
        assert!(
            out.contains("source.invoice_line.reserve(hub_items0.len());"),
            "{out}"
        );
        assert!(out.contains("REQUIRED_MISSING"), "{out}");
    }

    #[test]
    fn test_interior_struct_field_is_boxed_optional() {
        // An interior container is `Option<Box<…>>`: a document that omits the
        // whole element costs one `None` (8 bytes, no allocation) instead of a
        // full inline `Default` struct. `default` keeps absent elements
        // parseable; `Option::is_none` skips never-materialized subtrees on
        // write.
        let field = FieldMeta {
            optional: false,
            repeated: false,
            ty: FieldType::Struct("Party".into()),
            xml: Some("Party".into()),
        };
        let attr = serde_attr(&field).expect("interior struct needs a serde attr");
        assert!(attr.contains("default"), "{attr}");
        assert!(
            attr.contains("skip_serializing_if = \"Option::is_none\""),
            "{attr}"
        );
    }

    /// Compiles an arbitrary mapping body into (ir, hub, source).
    fn compile(body: &str) -> (MappingIr, CanonicalModel, SourceModelMeta) {
        let src = format!(
            "[meta]\ndoc_format = \"f\"\nformat_version = \"1\"\nmapping_version = \"1\"\ncanonical_model = \"c:1\"\nroot = \"Invoice\"\n{body}"
        );
        let (ir, source, diags) = build_ir(&[parse_mapping(&src).expect("parses")]);
        assert!(diags.is_empty(), "{diags:?}");
        let (hub, hd) = derive_hub(std::slice::from_ref(&ir));
        assert!(hd.is_empty(), "{hd:?}");
        (ir, hub, source)
    }

    /// Compiles a mapping body under a distinct doc_format into (ir, source).
    fn compile_named(doc_format: &str, body: &str) -> (MappingIr, SourceModelMeta) {
        let src = format!(
            "[meta]\ndoc_format = \"{doc_format}\"\nformat_version = \"1\"\nmapping_version = \"1\"\ncanonical_model = \"c:1\"\nroot = \"Invoice\"\n{body}"
        );
        let (ir, source, diags) = build_ir(&[parse_mapping(&src).expect("parses")]);
        assert!(diags.is_empty(), "{diags:?}");
        (ir, source)
    }

    const PLAIN_ID: &str = r#"
        [Invoice.ID]
        type = "identifier"
        canonical_key = "InvoiceNumber"
    "#;

    #[test]
    fn test_plan_dedup_shares_structs_and_aliases_identical_spokes() {
        let (ir_a, src_a) = compile_named("alpha", PLAIN_ID);
        let (ir_b, src_b) = compile_named("beta", PLAIN_ID);
        let spokes = [("alpha", &ir_a, &src_a), ("beta", &ir_b, &src_b)];
        let plan = super::plan_spoke_dedup(&spokes, "super::hub");

        // One shared structs module, its header naming every sharer.
        assert_eq!(plan.shared_modules.len(), 1);
        let (name, text) = &plan.shared_modules[0];
        assert_eq!(name, "shared_0");
        assert!(text.contains("Shared by: alpha, beta"), "{text}");
        assert!(text.contains("pub struct Invoice {"), "{text}");
        // The first spoke imports the shared structs; the second spoke's
        // module body is byte-identical: aliased, no file.
        let first = emitted(&plan.modules[0]);
        assert!(first.contains("pub use super::shared_0::*;"), "{first}");
        assert!(!first.contains("pub struct Invoice {"), "{first}");
        assert!(
            matches!(&plan.modules[1], super::SpokeModule::Alias(a) if a == "alpha"),
            "{:?}",
            plan.modules[1]
        );
    }

    #[test]
    fn test_plan_dedup_shares_structs_but_not_mappers_on_mapping_delta() {
        // Same element tree, but one spoke marks the field required: identical
        // structs (shared) yet different mappers (no alias) — the
        // XRechnung-over-UBL case.
        let strict = r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            required = true
        "#;
        let (ir_a, src_a) = compile_named("alpha", PLAIN_ID);
        let (ir_b, src_b) = compile_named("beta", strict);
        let spokes = [("alpha", &ir_a, &src_a), ("beta", &ir_b, &src_b)];
        let plan = super::plan_spoke_dedup(&spokes, "super::hub");

        assert_eq!(plan.shared_modules.len(), 1);
        let a = emitted(&plan.modules[0]);
        let b = emitted(&plan.modules[1]);
        assert_ne!(a, b);
        assert!(b.contains("REQUIRED_MISSING"), "{b}");
    }

    #[test]
    fn test_plan_dedup_keeps_distinct_source_models_standalone() {
        let other = r#"
            [Invoice.UUID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
        "#;
        let (ir_a, src_a) = compile_named("alpha", PLAIN_ID);
        let (ir_b, src_b) = compile_named("beta", other);
        let spokes = [("alpha", &ir_a, &src_a), ("beta", &ir_b, &src_b)];
        let plan = super::plan_spoke_dedup(&spokes, "super::hub");

        // Nothing shared, nothing aliased: each spoke keeps a self-contained
        // module with its structs inline.
        assert!(plan.shared_modules.is_empty());
        for module in &plan.modules {
            let code = emitted(module);
            assert!(code.contains("pub struct Invoice {"), "{code}");
        }
    }

    /// Unwraps an [`Emit`](super::SpokeModule::Emit) plan entry.
    fn emitted(module: &super::SpokeModule) -> &str {
        match module {
            super::SpokeModule::Emit(code) => code,
            super::SpokeModule::Alias(a) => panic!("expected emitted code, got alias of {a}"),
        }
    }

    #[test]
    fn test_generated_source_structs_have_empty_pruning_hooks() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");

        assert!(out.contains("impl InvoiceLine {"), "{out}");
        assert!(out.contains("pub fn is_empty(&self) -> bool {"), "{out}");
        assert!(
            out.contains("skip_serializing_if = \"Vec::is_empty\""),
            "{out}"
        );
        // Interior containers are boxed-optional: absent subtree = `None`.
        assert!(
            out.contains("pub legal_monetary_total: Option<Box<LegalMonetaryTotal>>,"),
            "{out}"
        );
        assert!(
            out.contains(
                "self.legal_monetary_total.as_ref().map_or(true, |value| value.is_empty())"
            ),
            "{out}"
        );
    }

    #[test]
    fn test_nested_collection_emits_nested_struct_and_loops() {
        let (ir, hub, source) = compile(
            r#"
            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.AllowanceCharge]
            type = "collection"
            canonical_key = "LineAllowances"

            [InvoiceLine.AllowanceCharge.Amount]
            type = "decimal"
            canonical_key = "LineAllowanceAmount"
            "#,
        );

        // Hub: the line item struct carries a nested collection field, and the
        // nested item struct exists with its scalar field.
        let hub_src = generate_hub(&hub);
        assert!(
            hub_src.contains("pub line_allowances: Vec<LineAllowancesItem>,"),
            "{hub_src}"
        );
        assert!(
            hub_src.contains("pub struct LineAllowancesItem {"),
            "{hub_src}"
        );
        assert!(
            hub_src.contains("pub line_allowance_amount: Option<Decimal>,"),
            "{hub_src}"
        );

        // Spoke: nested read/write loops consume the inner Vec, keyed by depth.
        let spoke = generate_spoke(&ir, &source, "super::hub");
        assert!(
            spoke.contains("let elements1 = std::mem::take(&mut element0.allowance_charge);"),
            "{spoke}"
        );
        assert!(
            spoke.contains("for (idx1, mut element1) in elements1.into_iter().enumerate()"),
            "{spoke}"
        );
        assert!(
            spoke.contains("item0.line_allowances.push(item1);"),
            "{spoke}"
        );
        assert!(
            spoke.contains("let hub_items1 = std::mem::take(&mut hub_item0.line_allowances);"),
            "{spoke}"
        );
        assert!(
            spoke.contains("for (idx1, mut hub_item1) in hub_items1.into_iter().enumerate()"),
            "{spoke}"
        );
        assert!(
            spoke.contains("element0.allowance_charge.push(element1);"),
            "{spoke}"
        );
        assert!(spoke.contains("if !element1.is_empty()"), "{spoke}");
    }

    #[test]
    fn test_multiple_join_reads_vec_and_writes_single_element() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.Note]
            type = "string"
            canonical_key = "Notes"
            multiple = "join"
            join_with = "\n"
            normalize = ["trim", "empty_as_missing"]
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Source struct: repeated scalar leaf.
        assert!(out.contains("pub note: Vec<CompactString>,"), "{out}");
        // Reader: consume the repeated leaf, collect normalized values, join
        // with the separator.
        assert!(
            out.contains("std::mem::take(&mut source.note).into_iter().filter_map(|s| Some(s)"),
            "{out}"
        );
        // Slice join yields a `String`; `.into()` moves it into the inline
        // string type (O(1) for heap-sized joins).
        assert!(out.contains("Some(values.join(\"\\n\").into())"), "{out}");
        // Writer: the joined canonical value is pushed as one element.
        assert!(out.contains("source.note.push(rendered);"), "{out}");
    }

    #[test]
    fn test_multiple_error_and_first_emit_multiple_values_diag() {
        for (policy, severity) in [("error", "Severity::Error"), ("first", "Severity::Warning")] {
            let (ir, _, source) = compile(&format!(
                r#"
                [Invoice.Note]
                type = "string"
                canonical_key = "Notes"
                multiple = "{policy}"
                "#
            ));
            let out = generate_spoke(&ir, &source, "super::hub");
            assert!(out.contains("if values.len() > 1 {"), "{out}");
            assert!(out.contains("MULTIPLE_VALUES"), "{out}");
            assert!(out.contains(severity), "{policy}: {out}");
        }
    }

    #[test]
    fn test_reader_moves_unique_source_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        // The reader consumes the source struct so uniquely-read values move
        // into the hub instead of being cloned.
        assert!(
            out.contains("pub fn read(mut source: Invoice) -> MappingResult<MainKey>"),
            "{out}"
        );
        assert!(out.contains("source.id.take()"), "{out}");
        // A take through a boxed interior chains `as_mut` and moves the leaf.
        assert!(
            out.contains("source.legal_monetary_total.as_mut()"),
            "{out}"
        );
        // Collections are consumed by value, element by element.
        assert!(
            out.contains("let elements0 = std::mem::take(&mut source.invoice_line);"),
            "{out}"
        );
        assert!(
            out.contains("for (idx0, mut element0) in elements0.into_iter().enumerate()"),
            "{out}"
        );
        // No path in this mapping is read twice, so nothing is cloned.
        assert!(!out.contains(".map(|s| s.to_string())"), "{out}");
    }

    #[test]
    fn test_reader_clones_shared_fallback_path() {
        // Two primaries share `Invoice.UUID` as a fallback: that path is read
        // twice, so it must stay a borrow + clone (a move would leave the
        // second read empty). Unique paths still move.
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            fallbacks = ["Invoice.UUID"]

            [Invoice.Alt]
            type = "identifier"
            canonical_key = "AltNumber"
            fallbacks = ["Invoice.UUID"]

            [Invoice.UUID]
            type = "identifier"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        assert!(!out.contains("source.uuid.take()"), "{out}");
        assert!(out.contains("source.uuid.as_ref()"), "{out}");
        assert!(out.contains("source.id.take()"), "{out}");
        assert!(out.contains("source.alt.take()"), "{out}");
    }

    #[test]
    fn test_writer_moves_hub_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        // The writer consumes the hub so values move into the target struct.
        assert!(
            out.contains("pub fn write(mut main: MainKey) -> MappingResult<Invoice>"),
            "{out}"
        );
        assert!(out.contains("main.invoice_number.take()"), "{out}");
        assert!(
            out.contains("let hub_items0 = std::mem::take(&mut main.invoice_lines);"),
            "{out}"
        );
        assert!(!out.contains("value.clone()"), "{out}");
    }

    #[test]
    fn test_wrapped_collection_crosses_boxed_interior() {
        // A collection under an interior container (the Factur-X shape) must
        // read through the boxed wrapper without materializing it, and write
        // through `get_or_insert_default` only when there are items to push.
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.Wrap.Line]
            type = "collection"
            canonical_key = "InvoiceLines"

            [Invoice.Wrap.Line.ID]
            type = "identifier"
            canonical_key = "LineId"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Reader: absent wrapper yields an empty Vec, no insertion.
        assert!(
            out.contains(
                "let elements0 = source.wrap.as_mut().and_then(|v0| Some(std::mem::take(&mut v0.line))).unwrap_or_default();"
            ),
            "{out}"
        );
        // Writer: the wrapper materializes only when items exist.
        assert!(out.contains("if !hub_items0.is_empty() {"), "{out}");
        assert!(
            out.contains("source.wrap.get_or_insert_default().line.reserve(hub_items0.len());"),
            "{out}"
        );
        assert!(
            out.contains("source.wrap.get_or_insert_default().line.push(element0);"),
            "{out}"
        );
    }

    #[test]
    fn test_constant_only_node_is_written_not_read() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.UBLVersionID]
            type = "identifier"
            constant = "2.1"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Writer pins the literal at the source path.
        assert!(
            out.contains("source.ubl_version_id = Some(CompactString::from(\"2.1\"));"),
            "{out}"
        );
        // Reader never touches the field: write-only node.
        assert!(!out.contains("source.ubl_version_id.take()"), "{out}");
        assert!(!out.contains("source.ubl_version_id.as_ref()"), "{out}");
    }

    #[test]
    fn test_keyed_constant_reads_transparently_and_writes_fixed() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.CustomizationID]
            type = "identifier"
            canonical_key = "SpecificationId"
            constant = "urn:cen.eu:en16931:2017"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Reader fills the hub from the document as usual.
        assert!(out.contains("source.customization_id.take()"), "{out}");
        assert!(out.contains("main.specification_id = Some(raw);"), "{out}");
        // Writer emits the constant and never consults the hub value.
        assert!(
            out.contains(
                "source.customization_id = Some(CompactString::from(\"urn:cen.eu:en16931:2017\"));"
            ),
            "{out}"
        );
        assert!(!out.contains("main.specification_id.take()"), "{out}");
    }

    #[test]
    fn test_collection_scoped_constant_written_per_nonempty_item() {
        let (ir, _, source) = compile(
            r#"
            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId"

            [InvoiceLine.TypeCode]
            type = "identifier"
            constant = "380"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // The constant assignment sits inside the non-empty guard, before the
        // push: it never resurrects an otherwise-empty item.
        let guard = out
            .find("if !element0.is_empty() {")
            .expect("non-empty guard exists");
        let assign = out
            .find("element0.type_code = Some(CompactString::from(\"380\"));")
            .expect("constant assigned on the element");
        let push = out
            .find("source.invoice_line.push(element0);")
            .expect("element pushed");
        assert!(guard < assign && assign < push, "{out}");
    }

    #[test]
    fn test_clone_of_writes_hub_value_to_both_paths() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.BuyerReference]
            type = "identifier"
            clone_of = "InvoiceNumber"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // The hub key is written twice (primary + clone), so both writes borrow
        // instead of moving.
        assert_eq!(
            out.matches("if let Some(value) = &main.invoice_number {")
                .count(),
            2,
            "{out}"
        );
        assert!(!out.contains("main.invoice_number.take()"), "{out}");
        assert!(out.contains("source.id = Some(rendered);"), "{out}");
        assert!(
            out.contains("source.buyer_reference = Some(rendered);"),
            "{out}"
        );
    }

    #[test]
    fn test_clone_of_reader_checks_copy_and_warns_on_mismatch() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.BuyerReference]
            type = "identifier"
            clone_of = "InvoiceNumber"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Only the primary fills the hub.
        assert_eq!(
            out.matches("main.invoice_number = Some(raw);").count(),
            1,
            "{out}"
        );
        // The copy is read and compared against the canonical value; a
        // disagreeing copy is a warning, not a silent pick.
        assert!(out.contains("CLONE_MISMATCH"), "{out}");
        assert!(out.contains("Severity::Warning"), "{out}");
    }

    #[test]
    fn test_collection_scoped_clone_written_per_item() {
        let (ir, _, source) = compile(
            r#"
            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId"

            [InvoiceLine.DocumentReference]
            type = "identifier"
            clone_of = "LineId"
            "#,
        );
        let out = generate_spoke(&ir, &source, "super::hub");
        // Writer: both element fields written from the same hub item key.
        assert_eq!(
            out.matches("if let Some(value) = &hub_item0.line_id {")
                .count(),
            2,
            "{out}"
        );
        assert!(
            out.contains("element0.document_reference = Some(rendered);"),
            "{out}"
        );
        // Reader: per-item mismatch check.
        assert!(out.contains("CLONE_MISMATCH"), "{out}");
    }

    #[test]
    fn test_generation_is_deterministic() {
        let (ir, hub, source) = compiled();
        assert_eq!(generate_hub(&hub), generate_hub(&hub));
        assert_eq!(
            generate_spoke(&ir, &source, "super::hub"),
            generate_spoke(&ir, &source, "super::hub")
        );
    }
}
