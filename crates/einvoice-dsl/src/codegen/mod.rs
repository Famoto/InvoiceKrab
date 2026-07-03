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
//!
//! The runtime never interprets the TOML; it links against the generated Rust,
//! which targets the small `einvoice-transformator` helper API (`normalize`,
//! `validate`, `adapter`, `MappingResult`) and uses native Rust types
//! (`String`, `rust_decimal::Decimal`, `bool`, `Vec<…>`) directly.
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
//! fallback sources.
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
    let root = &source.root;
    // The IR classification is the same for both mappers, so build it once and
    // share it across the reader and writer generators.
    let plan = MappingPlan::build(ir);
    let ctx = GenCtx {
        ir,
        source,
        plan: &plan,
    };
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
    out.push_str("use std::str::FromStr as _;\n");
    out.push_str("use rust_decimal::Decimal;\n");
    out.push_str("use serde::{Deserialize, Serialize};\n");
    out.push_str(
        "use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};\n",
    );
    out.push_str("use einvoice_transformator::{adapter, normalize, validate};\n");
    let _ = writeln!(out, "use {hub_module}::*;");
    out.push('\n');

    source::generate_source_structs(&mut out, source);
    out.push('\n');
    source::generate_xml_io(&mut out, root);
    out.push('\n');
    read::generate_read(&mut out, &ctx, root);
    out.push('\n');
    write::generate_write(&mut out, &ctx, root);

    out
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
        assert!(out.contains("pub invoice_number: Option<String>,"), "{out}");
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
            out.contains("pub fn read(source: &Invoice) -> MappingResult<MainKey>"),
            "{out}"
        );
        assert!(
            out.contains("pub fn write(main: &MainKey) -> MappingResult<Invoice>"),
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
        // The normalize chain threads one owned `String` by value (no per-op
        // re-borrow / reallocation).
        assert!(
            out.contains(".map(|s| s.to_string()).map(normalize::trim).map(normalize::uppercase)"),
            "{out}"
        );
        assert!(!out.contains("normalize::trim(&s)"), "{out}");
        // Collection loops use per-depth variable names (`item0`, `element0`) and
        // reserve the hub collection up front from the known source length.
        assert!(
            out.contains("let count0 = source.invoice_line.len();"),
            "{out}"
        );
        assert!(out.contains("main.invoice_lines.reserve(count0);"), "{out}");
        assert!(out.contains("main.invoice_lines.push(item0);"), "{out}");
        assert!(out.contains("item0.quantity = Some(d)"), "{out}");
    }

    #[test]
    fn test_writer_renders_typed_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, "super::hub");
        assert!(
            out.contains("if let Some(value) = &main.invoice_number {"),
            "{out}"
        );
        // Values are rendered once, skipped if empty, then assigned. Every leaf
        // is Option-typed, so assignment wraps in `Some`.
        assert!(
            out.contains("source.legal_monetary_total.payable_amount.value = Some(rendered);"),
            "{out}"
        );
        assert!(out.contains("source.id = Some(rendered);"), "{out}");
        // The source collection is reserved once from the hub item count.
        assert!(
            out.contains("source.invoice_line.reserve(main.invoice_lines.len());"),
            "{out}"
        );
        assert!(out.contains("REQUIRED_MISSING"), "{out}");
    }

    #[test]
    fn test_interior_struct_field_is_defaultable() {
        // A non-optional interior container must carry `default` so a document
        // that omits the whole element deserializes to an empty struct instead
        // of failing. On write, empty containers are skipped.
        let field = FieldMeta {
            optional: false,
            repeated: false,
            ty: FieldType::Struct("Party".into()),
            xml: Some("Party".into()),
        };
        let attr = serde_attr(&field).expect("interior struct needs a serde attr");
        assert!(attr.contains("default"), "{attr}");
        assert!(
            attr.contains("skip_serializing_if = \"Party::is_empty\""),
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
        assert!(
            out.contains("skip_serializing_if = \"LegalMonetaryTotal::is_empty\""),
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

        // Spoke: nested read/write loops over the inner source Vec, keyed by depth.
        let spoke = generate_spoke(&ir, &source, "super::hub");
        assert!(
            spoke.contains("for (idx1, element1) in element0.allowance_charge.iter().enumerate()"),
            "{spoke}"
        );
        assert!(
            spoke.contains("item0.line_allowances.push(item1);"),
            "{spoke}"
        );
        assert!(
            spoke.contains("for (idx1, hub_item1) in hub_item0.line_allowances.iter().enumerate()"),
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
        assert!(out.contains("pub note: Vec<String>,"), "{out}");
        // Reader: collect normalized values, join with the separator.
        assert!(
            out.contains("source.note.iter().filter_map(|s| Some(s.as_str())"),
            "{out}"
        );
        assert!(out.contains("Some(values.join(\"\\n\"))"), "{out}");
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
    fn test_generation_is_deterministic() {
        let (ir, hub, source) = compiled();
        assert_eq!(generate_hub(&hub), generate_hub(&hub));
        assert_eq!(
            generate_spoke(&ir, &source, "super::hub"),
            generate_spoke(&ir, &source, "super::hub")
        );
    }
}
