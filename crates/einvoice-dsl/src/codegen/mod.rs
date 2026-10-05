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
//! `validate`, `codec`, `MappingResult`) and uses native Rust types
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
//! output. The one deliberate exception is the field order *inside* a generated
//! source struct: fields follow the mapping's declaration order (attributes
//! first), because serde serializes in declaration order and the emitted XML
//! must follow the schema's sequence. The emitted reader, per node: reads the source field, applies
//! `normalize` ops, falls back through `fallbacks`, decodes/validates by `type`,
//! enforces `required`, and assigns into the typed `MainKey`. Helper nodes (no `canonical_key`) are read only as
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

use crate::codec::CodecTable;
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
pub fn generate_spoke(
    ir: &MappingIr,
    source: &SourceModelMeta,
    codecs: &CodecTable,
    hub_module: &str,
) -> String {
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
    mapper_section(&mut out, ir, source, codecs);

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
    codecs: &CodecTable,
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
    mapper_section(&mut out, ir, source, codecs);
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
    codecs: &CodecTable,
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
                generate_mapper_module(ir, source, codecs, hub_module, &format!("super::{shared}"))
            }
            None => generate_spoke(ir, source, codecs, hub_module),
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
    source::generate_xml_io(out, &source.root, &source.namespaces);
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
    out.push_str("use einvoice_transformator::{codec, normalize, validate};\n");
    let _ = writeln!(out, "use {hub_module}::*;");
}

/// Emits the `read` and `write` mapper functions.
fn mapper_section(out: &mut String, ir: &MappingIr, source: &SourceModelMeta, codecs: &CodecTable) {
    // The IR classification is the same for both mappers, so build it once and
    // share it across the reader and writer generators.
    let plan = MappingPlan::build(ir);
    let ctx = GenCtx {
        ir,
        source,
        plan: &plan,
        codecs,
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
    use crate::codec::CodecTable;
    use crate::hub::{CanonicalModel, derive_hub};
    use crate::ir::{MappingIr, build_ir, build_ir_with};
    use crate::parse::parse_mapping;
    use crate::source_model::{FieldMeta, FieldType, SourceModelMeta};

    fn no_codecs() -> CodecTable {
        CodecTable::new()
    }

    /// The CII `102` date codec (wire `@format = 102`) plus a boolean codec.
    fn cii_codecs() -> CodecTable {
        crate::codec::parse_codecs(
            r#"
            [codec.cii-date-102]
            for_type = "date"
            lexical = "YYYYMMDD"
            wire = { "@format" = "102" }

            [codec.boolean-1-0]
            for_type = "boolean"
            lexical = "1|0"
            "#,
        )
        .expect("codecs parse")
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect()
    }

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
    fn test_generate_hub_walks_values_by_scope_qualified_label() {
        let (_, hub, _) = compiled();
        let out = generate_hub(&hub);
        assert!(
            out.contains("pub fn values(&self) -> Vec<(&'static str, String)> {"),
            "{out}"
        );
        // A root scalar under its key, a collection item's scalar under its
        // scope-qualified label, and the collection by walking its items.
        assert!(
            out.contains("if let Some(value) = &self.invoice_number {\n            out.push((\"InvoiceNumber\", value.to_string()));"),
            "{out}"
        );
        assert!(
            out.contains("for item in &self.invoice_lines {\n            item.push_values(out);"),
            "{out}"
        );
        let item = &out[out.find("impl InvoiceLinesItem {").expect("item walker")..];
        assert!(
            item.contains("out.push((\"InvoiceLines/Quantity\", value.to_string()));"),
            "{item}"
        );
    }

    #[test]
    fn test_generate_spoke_emits_source_structs_and_mappers() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = super::generate_mapper_module(
            &ir,
            &source,
            &no_codecs(),
            "super::hub",
            "super::shared_0",
        );
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
        let monolith = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let src = super::generate_source_module(&source);
        let map = super::generate_mapper_module(
            &ir,
            &source,
            &no_codecs(),
            "super::hub",
            "super::shared_0",
        );
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
    fn test_aliased_element_generates_skip_fields_and_demux_mux() {
        let (ir, hub, source) = compile(
            r#"
            [Invoice.AdditionalDocumentReference]
            type = "collection"
            canonical_key = "SupportingDocuments"

            [Invoice.AdditionalDocumentReference.ID]
            type = "identifier"
            canonical_key = "SupportingDocumentReference"

            [Invoice.AdditionalDocumentReference.DocumentTypeCode]
            type = "string"

            [Invoice.InvoicedObjectReference]
            xml = "AdditionalDocumentReference"
            match = { "DocumentTypeCode" = "130" }

            [Invoice.InvoicedObjectReference.ID]
            type = "identifier"
            canonical_key = "InvoicedObjectIdentifier"
            "#,
        );
        let _ = hub;
        let out = generate_spoke(&ir, &source, &CodecTable::new(), "super::hub");
        // One physical field on the wire, two logical fields off it.
        assert!(
            out.contains(
                "#[serde(rename = \"AdditionalDocumentReference\", default, skip_serializing_if = \"Vec::is_empty\")]\n    pub all_additional_document_reference: Vec<AdditionalDocumentReference>,"
            ),
            "{out}"
        );
        assert!(
            out.contains("#[serde(skip)]\n    pub additional_document_reference: Vec<AdditionalDocumentReference>,"),
            "{out}"
        );
        assert!(
            out.contains("#[serde(skip)]\n    pub invoiced_object_reference: Option<Box<AdditionalDocumentReference>>,"),
            "{out}"
        );
        // demux: selector first, default bucket last, overflow reported.
        assert!(
            out.contains(
                "for item in std::mem::take(&mut self.all_additional_document_reference) {"
            ),
            "{out}"
        );
        assert!(
            out.contains("if item.document_type_code.as_ref().and_then(|v0| Some(v0.as_str())).is_some_and(|v| v.trim() == \"130\") {"),
            "{out}"
        );
        assert!(
            out.contains("self.additional_document_reference.push(item);"),
            "{out}"
        );
        assert!(
            out.contains(
                "overflow(\"Invoice.InvoicedObjectReference\", extra_invoiced_object_reference);"
            ),
            "{out}"
        );
        // mux: the discriminator is written from the selector.
        assert!(
            out.contains("item.document_type_code = Some(CompactString::from(\"130\"));"),
            "{out}"
        );
        // The mappers call them on the root.
        assert!(out.contains("source.demux(&mut |node, extra| {"), "{out}");
        assert!(out.contains("\"MATCH_MULTIPLE\""), "{out}");
        assert!(out.contains("    source.mux();\n"), "{out}");
        // The nodes map through the logical fields: read by moving out of the
        // logical struct, written by materializing it.
        assert!(
            out.contains("source.invoiced_object_reference.as_mut().and_then(|v0| v0.id.take())"),
            "{out}"
        );
        assert!(
            out.contains(
                "source.invoiced_object_reference.get_or_insert_default().id = Some(rendered);"
            ),
            "{out}"
        );
    }

    #[test]
    fn test_valued_element_attribute_is_written_after_the_clone_that_fills_its_text() {
        // `Amount`'s text is a clone, its `@currencyID` a primary: the attribute
        // must be written after the clone so its non-empty-owner guard holds.
        let (ir, _hub, source) = compile(
            r#"
            [Invoice.Total]
            type = "decimal"
            canonical_key = "Total"

            [Invoice.Amount]
            type = "decimal"
            clone_of = "Total"

            [Invoice.Amount.currencyID]
            xml = "@currencyID"
            type = "currency"
            canonical_key = "Currency"
            "#,
        );
        let out = generate_spoke(&ir, &source, &CodecTable::new(), "super::hub");
        let clone_at = out
            .find("// Total -> amount.value")
            .expect("clone block present");
        let attr_at = out
            .find("// Currency -> amount.currency_id")
            .expect("attribute block present");
        assert!(
            clone_at < attr_at,
            "clone must precede the attribute:\n{out}"
        );
        assert!(
            out.contains("Some(&source).and_then(|v0| v0.amount.as_ref()).is_some_and(|owner| !owner.is_empty())"),
            "the attribute keeps its owner guard:\n{out}"
        );
    }

    #[test]
    fn test_single_valued_default_bucket_reports_surplus_items() {
        // A structural node without a selector shares the element with a
        // selected node: it keeps the first unclaimed item and reports the rest.
        let (ir, _hub, source) = compile(
            r#"
            [Invoice.Ref]
            match = { "TypeCode" = "130" }

            [Invoice.Ref.ID]
            type = "identifier"
            canonical_key = "ObjectId"

            [Invoice.Ref.TypeCode]
            type = "string"

            [Invoice.OtherRef]
            xml = "Ref"

            [Invoice.OtherRef.ID]
            type = "identifier"
            canonical_key = "OtherId"
            "#,
        );
        let out = generate_spoke(&ir, &source, &CodecTable::new(), "super::hub");
        assert!(out.contains("let mut extra_other_ref = 0usize;"), "{out}");
        assert!(
            out.contains("if self.other_ref.is_none() {\n                self.other_ref = Some(Box::new(item));\n            } else {\n                extra_other_ref += 1;\n            }"),
            "{out}"
        );
        assert!(
            out.contains("overflow(\"Invoice.OtherRef\", extra_other_ref);"),
            "{out}"
        );
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
            prefix: String::new(),
            always_present: false,
            order: 0,
            alias: None,
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
        let plan = super::plan_spoke_dedup(&spokes, &no_codecs(), "super::hub");

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
        let plan = super::plan_spoke_dedup(&spokes, &no_codecs(), "super::hub");

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
        let plan = super::plan_spoke_dedup(&spokes, &no_codecs(), "super::hub");

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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");

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
        let spoke = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
            let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
            assert!(out.contains("if values.len() > 1 {"), "{out}");
            assert!(out.contains("MULTIPLE_VALUES"), "{out}");
            assert!(out.contains(severity), "{policy}: {out}");
        }
    }

    #[test]
    fn test_reader_moves_unique_source_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        assert!(!out.contains("source.uuid.take()"), "{out}");
        assert!(out.contains("source.uuid.as_ref()"), "{out}");
        assert!(out.contains("source.id.take()"), "{out}");
        assert!(out.contains("source.alt.take()"), "{out}");
    }

    #[test]
    fn test_writer_moves_hub_values() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
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

    /// Byte offset of `needle` inside the generated `pub struct {name} {` block.
    fn offset_in_struct(out: &str, name: &str, needle: &str) -> usize {
        let header = format!("pub struct {name} {{");
        let start = out
            .find(&header)
            .unwrap_or_else(|| panic!("{header} in:\n{out}"));
        let body = &out[start..];
        let end = body.find("\n}\n").expect("struct closes");
        body[..end]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} inside {name}:\n{}", &body[..end]))
    }

    #[test]
    fn test_source_struct_fields_follow_declaration_order() {
        // UBL declares ID, DocumentCurrencyCode, LegalMonetaryTotal, InvoiceLine
        // in that order; the struct (and so the emitted XML) must too, not
        // alphabetically (`document_currency_code` < `id` < `invoice_line` <
        // `legal_monetary_total`).
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let id = offset_in_struct(&out, "Invoice", "pub id:");
        let currency = offset_in_struct(&out, "Invoice", "pub document_currency_code:");
        let totals = offset_in_struct(&out, "Invoice", "pub legal_monetary_total:");
        let lines = offset_in_struct(&out, "Invoice", "pub invoice_line:");
        assert!(
            id < currency && currency < totals && totals < lines,
            "{out}"
        );
    }

    #[test]
    fn test_source_struct_emits_attributes_before_element_text() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let attr = offset_in_struct(&out, "LegalMonetaryTotalPayableAmount", "pub currency_id:");
        let text = offset_in_struct(&out, "LegalMonetaryTotalPayableAmount", "pub value:");
        assert!(attr < text, "{out}");
    }

    const NAMESPACED_UBL: &str = r#"
        [meta]
        doc_format = "ubl-invoice"
        format_version = "2.1"
        mapping_version = "1.0"
        canonical_model = "canonical-invoice:1.0"
        root = "Invoice"
        root_ns = ""

        [meta.namespaces]
        "" = "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"
        cbc = "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
        cac = "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2"

        [meta.ns_defaults]
        leaf = "cbc"
        aggregate = "cac"

        [Invoice.ID]
        type = "identifier"
        canonical_key = "InvoiceNumber"

        [Invoice.LegalMonetaryTotal.PayableAmount]
        type = "decimal"
        canonical_key = "PayableAmount"

        [Invoice.LegalMonetaryTotal.PayableAmount.currencyID]
        xml = "@currencyID"
        type = "currency"
        canonical_key = "PayableAmountCurrency"
    "#;

    fn compiled_namespaced() -> (MappingIr, SourceModelMeta) {
        let (ir, source, diags) = build_ir(&[parse_mapping(NAMESPACED_UBL).expect("parses")]);
        assert!(diags.is_empty(), "{diags:?}");
        (ir, source)
    }

    #[test]
    fn test_namespaced_fields_get_split_renames() {
        let (ir, source) = compiled_namespaced();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // Written prefixed, read by local name.
        assert!(
            out.contains("#[serde(rename(serialize = \"cbc:ID\", deserialize = \"ID\"), default, skip_serializing_if = \"Option::is_none\")]"),
            "{out}"
        );
        assert!(
            out.contains("rename(serialize = \"cac:LegalMonetaryTotal\", deserialize = \"LegalMonetaryTotal\")"),
            "{out}"
        );
        assert!(
            out.contains(
                "rename(serialize = \"cbc:PayableAmount\", deserialize = \"PayableAmount\")"
            ),
            "{out}"
        );
        // Attributes and text are never prefixed: plain renames as before.
        assert!(out.contains("rename = \"@currencyID\""), "{out}");
        assert!(out.contains("rename = \"$text\""), "{out}");
    }

    #[test]
    fn test_namespaced_root_carries_xmlns_markers_and_prefixed_root_tag() {
        let (ir, source) = compiled_namespaced();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // One marker type per declared namespace, serializing as its URI.
        assert!(out.contains("pub struct XmlnsDefault;"), "{out}");
        assert!(out.contains("pub struct XmlnsCbc;"), "{out}");
        assert!(out.contains("pub struct XmlnsCac;"), "{out}");
        assert!(
            out.contains("s.serialize_str(\"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\")"),
            "{out}"
        );
        // The root struct declares them as attribute fields, before any element.
        let xmlns = offset_in_struct(
            &out,
            "Invoice",
            "#[serde(rename = \"@xmlns\", default)]\n    pub xmlns: XmlnsDefault,",
        );
        let cbc = offset_in_struct(
            &out,
            "Invoice",
            "#[serde(rename = \"@xmlns:cbc\", default)]\n    pub xmlns_cbc: XmlnsCbc,",
        );
        let id = offset_in_struct(&out, "Invoice", "pub id:");
        assert!(xmlns < id && cbc < id, "{out}");
        // Non-root structs carry none.
        assert!(
            !out[out.find("pub struct LegalMonetaryTotal {").unwrap()..].contains("xmlns_cbc"),
            "{out}"
        );
        // The emptiness predicate ignores the markers.
        assert!(!out.contains("self.xmlns"), "{out}");
        // Declaration + qualified root (the default namespace leaves it bare).
        assert!(
            out.contains("String::from(\"<?xml version=\\\"1.0\\\" encoding=\\\"UTF-8\\\"?>\\n\")"),
            "{out}"
        );
        assert!(
            out.contains("quick_xml::se::to_writer_with_root(&mut out, \"Invoice\", source)?;"),
            "{out}"
        );
    }

    #[test]
    fn test_prefixed_root_tag_and_no_markers_without_declarations() {
        let (ir, source) = compile_named("cii", "");
        let _ = ir;
        let mut source = source;
        source.namespaces.root_prefix = "rsm".into();
        let out = super::generate_source_module(&source);
        assert!(
            out.contains("to_writer_with_root(&mut out, \"rsm:Invoice\", source)?;"),
            "{out}"
        );
        assert!(!out.contains("namespace declarations"), "{out}");
        assert!(!out.contains("Xmlns"), "{out}");
    }

    #[test]
    fn test_codec_decodes_on_read_checks_wire_and_encodes_on_write() {
        let codecs = cii_codecs();
        let src = "[meta]\ndoc_format = \"cii\"\nformat_version = \"1\"\nmapping_version = \"1\"\ncanonical_model = \"c:1\"\nroot = \"Invoice\"\n\n[Invoice.IssueDateTime.DateTimeString]\ntype = \"date\"\ncanonical_key = \"IssueDate\"\ncodec = \"cii-date-102\"\n\n[Invoice.Paid]\ntype = \"boolean\"\ncanonical_key = \"Paid\"\ncodec = \"boolean-1-0\"\n";
        let (ir, source, diags) = build_ir_with(&[parse_mapping(src).expect("parses")], &codecs);
        assert!(diags.is_empty(), "{diags:?}");
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");

        // Source model: the dated element is a valued container with the wire
        // attribute as an attribute field.
        assert!(
            out.contains("pub struct IssueDateTimeDateTimeString {"),
            "{out}"
        );
        assert!(out.contains("rename = \"@format\""), "{out}");

        // Reader: decode through the codec into the canonical ISO form, with a
        // CODEC_INVALID diagnostic on mismatch; warn when the document's wire
        // attribute disagrees with the codec's.
        assert!(
            out.contains("match codec::decode_date(raw.trim(), \"YYYYMMDD\") {"),
            "{out}"
        );
        assert!(out.contains("CODEC_INVALID"), "{out}");
        assert!(out.contains("CODEC_WIRE_MISMATCH"), "{out}");
        assert!(
            out.contains("match codec::decode_bool(raw.trim(), \"1\", \"0\") {"),
            "{out}"
        );

        // Writer: encode through the codec and set the wire attribute next to
        // the value.
        assert!(
            out.contains("match codec::encode_date(value.as_str(), \"YYYYMMDD\") {"),
            "{out}"
        );
        assert!(
            out.contains(
                "source.issue_date_time.get_or_insert_default().date_time_string.get_or_insert_default().format = Some(CompactString::from(\"102\"));"
            ),
            "{out}"
        );
        assert!(
            out.contains("let rendered = codec::encode_bool(value.clone(), \"1\", \"0\");"),
            "{out}"
        );
    }

    #[test]
    fn test_root_derivation_borrows_the_root_key_inside_the_collection() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.DocumentCurrencyCode]
            type = "currency"
            canonical_key = "DocumentCurrency"

            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.LineExtensionAmount]
            type = "decimal"
            canonical_key = "LineNetAmount"

            [InvoiceLine.LineExtensionAmount.currencyID]
            xml = "@currencyID"
            type = "currency"
            clone_of = "$root.DocumentCurrency"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // The root primary no longer moves the key out: it is read again per line.
        assert!(!out.contains("main.document_currency.take()"), "{out}");
        assert_eq!(
            out.matches("if let Some(value) = &main.document_currency {")
                .count(),
            2,
            "root primary + the per-line clone: {out}"
        );
        assert!(
            out.contains("element0.line_extension_amount.get_or_insert_default().currency_id = Some(rendered);"),
            "{out}"
        );
        // Reader: the per-line copy is compared against the root value.
        assert!(
            out.contains("if main.document_currency.as_ref() != Some(&found) {"),
            "{out}"
        );
        assert!(out.contains("CLONE_MISMATCH"), "{out}");
    }

    #[test]
    fn test_attribute_of_a_valued_element_follows_the_elements_value() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // PayableAmountCurrency is written only when PayableAmount has its value;
        // the amount itself has no such guard.
        assert!(
            out.contains("if !rendered.is_empty() && Some(&source).and_then(|v0| v0.legal_monetary_total.as_ref()).and_then(|v1| v1.payable_amount.as_ref()).is_some_and(|owner| !owner.is_empty()) {"),
            "{out}"
        );
        let amount = out
            .find("// PayableAmount -> legal_monetary_total.payable_amount.value")
            .unwrap();
        let currency = out
            .find("// PayableAmountCurrency -> legal_monetary_total.payable_amount.currency_id")
            .unwrap();
        assert!(
            amount < currency,
            "the value is written before its attribute: {out}"
        );
        // Not a valued element: plain non-empty guard.
        assert!(out.contains("let rendered = value;\n        if !rendered.is_empty() {\n            source.id = Some(rendered);"), "{out}");
    }

    #[test]
    fn test_parent_derivation_reads_the_enclosing_item() {
        let (ir, _, source) = compile(
            r#"
            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId"

            [InvoiceLine.AllowanceCharge]
            type = "collection"
            canonical_key = "LineAllowances"

            [InvoiceLine.AllowanceCharge.Amount]
            type = "decimal"
            canonical_key = "LineAllowanceAmount"

            [InvoiceLine.AllowanceCharge.LineRef]
            type = "identifier"
            clone_of = "$parent.LineId"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        assert!(!out.contains("hub_item0.line_id.take()"), "borrowed: {out}");
        assert!(
            out.contains("if let Some(value) = &hub_item0.line_id {"),
            "{out}"
        );
        assert!(out.contains("element1.line_ref = Some(rendered);"), "{out}");
        assert!(
            out.contains("if item0.line_id.as_ref() != Some(&found) {"),
            "{out}"
        );
    }

    #[test]
    fn test_constant_with_an_owner_is_guarded_on_the_owners_content() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.Party.PartyTaxScheme.CompanyID]
            type = "identifier"
            canonical_key = "SellerVatId"

            [Invoice.Party.PartyTaxScheme.TaxScheme.ID]
            type = "identifier"
            constant = "VAT"

            [Invoice.UBLVersionID]
            type = "identifier"
            constant = "2.1"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // Owner = PartyTaxScheme (shared with CompanyID), not TaxScheme.
        let guard = "if Some(&source).and_then(|v0| v0.party.as_ref()).and_then(|v1| v1.party_tax_scheme.as_ref()).is_some_and(|owner| !owner.is_empty()) {";
        assert!(out.contains(guard), "{out}");
        let guard_at = out.find(guard).unwrap();
        let assign_at = out
            .find("source.party.get_or_insert_default().party_tax_scheme.get_or_insert_default().tax_scheme.get_or_insert_default().id = Some(CompactString::from(\"VAT\"));")
            .expect("constant assigned");
        let company_at = out.find("source.party.get_or_insert_default().party_tax_scheme.get_or_insert_default().company_id = Some(rendered);").expect("content written");
        assert!(
            company_at < guard_at && guard_at < assign_at,
            "constants come last: {out}"
        );
        // A root-level constant with no owner stays unconditional.
        assert!(
            out.contains("source.ubl_version_id = Some(CompactString::from(\"2.1\"));"),
            "{out}"
        );
        assert!(!out.contains("ubl_version_id.is_some_and"), "{out}");
    }

    #[test]
    fn test_required_structural_node_is_always_materialized() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.Transaction.Delivery]
            required = true

            [Invoice.Transaction.Delivery.ActualDate]
            type = "date"
            canonical_key = "ActualDeliveryDate"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        assert!(
            out.contains("let _ = source.transaction.get_or_insert_default().delivery.get_or_insert_default();"),
            "{out}"
        );
        assert!(
            !out.contains("let _ = source.transaction.get_or_insert_default();\n"),
            "only the flagged element: {out}"
        );
    }

    #[test]
    fn test_generation_is_deterministic() {
        let (ir, hub, source) = compiled();
        assert_eq!(generate_hub(&hub), generate_hub(&hub));
        assert_eq!(
            generate_spoke(&ir, &source, &no_codecs(), "super::hub"),
            generate_spoke(&ir, &source, &no_codecs(), "super::hub")
        );
    }
}
