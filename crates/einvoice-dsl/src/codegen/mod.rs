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
    out.push_str("use compact_str::{CompactString, ToCompactString as _};\n");
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
