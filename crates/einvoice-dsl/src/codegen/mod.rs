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
//!   source record's element functions (XML read/write over a packed record
//!   laid out by the mapping), `from_xml`/`to_xml`, and the `read` (source →
//!   `MainKey`) / `write` (`MainKey` → source) mappers.
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
//! [`naming`] (identifier/type helpers), [`hub`], [`source`] (record layout + XML I/O),
//! [`read`], [`write`], [`access`] (source-path expressions), [`plan`] (IR
//! classification), and [`diag`] (diagnostic emission).
//!
//! # Behavior
//!
//! The generators are **pure and deterministic**: text in, text out, with all
//! `BTreeMap`s iterated in sorted order so identical inputs yield byte-identical
//! output. The one deliberate exception is the field order *inside* a generated
//! source element: fields follow the mapping's declaration order (attributes
//! first), because the writer emits in that order and the emitted XML must
//! follow the schema's sequence. The emitted reader, per node: reads the source field, applies
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

pub use hub::{generate_hub, generate_hub_with};

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
///    share one `shared_<n>` structs module (compiling the XML reader/writer
///    of `inherits` families once) and get mappers-only modules.
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

/// Emits the source records and the root's `from_xml`/`to_xml`.
fn source_section(out: &mut String, source: &SourceModelMeta) {
    source::generate_source_structs(out, source);
    out.push('\n');
    source::generate_xml_io(out, &source.root, &source.namespaces);
}

/// Emits the imports the `read`/`write` mappers need (the source records
/// name what they use by full path).
fn mapper_imports(out: &mut String, hub_module: &str) {
    out.push_str("use std::str::FromStr as _;\n");
    out.push_str("use compact_str::{CompactString, ToCompactString as _};\n");
    out.push_str("use rust_decimal::Decimal;\n");
    out.push_str(
        "use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};\n",
    );
    out.push_str("use einvoice_transformator::{codec, normalize, validate};\n");
    out.push_str("use einvoice_transformator::record::{Draft, Index};\n");
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
    use super::{generate_hub, generate_hub_with, generate_spoke};
    use crate::codec::CodecTable;
    use crate::hub::{CanonicalModel, derive_hub};
    use crate::ir::{MappingIr, build_ir, build_ir_with};
    use crate::parse::parse_mapping;
    use crate::source_model::SourceModelMeta;

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
        // The root record over one packed buffer; each element read by its
        // local name into its id (absent elements cost nothing; required is
        // a reader diagnostic).
        assert!(
            out.contains("pub struct Invoice(pub ::einvoice_transformator::record::Values);"),
            "{out}"
        );
        assert!(out.contains("\"ID\" => { if seen[0]"), "{out}");
        assert!(out.contains("values.push(base + 1, &text);"), "{out}");
        // The optional currencyID attribute, matched without its `@`.
        assert!(out.contains("\"currencyID\" => {"), "{out}");
        // xml io + mappers
        assert!(out.contains("pub fn from_xml"), "{out}");
        assert!(out.contains("pub fn to_xml"), "{out}");
        assert!(
            out.contains("pub fn read(source: Invoice) -> MappingResult<MainKey>"),
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
        // The normalize chain copies the value once into an inline string and
        // threads it by value (no per-op re-borrow / reallocation).
        assert!(
            out.contains("source.get(2).map(CompactString::from).map(normalize::trim).map(normalize::uppercase)"),
            "{out}"
        );
        assert!(!out.contains("normalize::trim(&s)"), "{out}");
        // Collection loops use per-depth variable names (`item0`, `element0`) and
        // reserve the hub collection up front from the counted items.
        assert!(out.contains("let elements0 = source.items(7);"), "{out}");
        assert!(
            out.contains("let count0 = elements0.clone().count();"),
            "{out}"
        );
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
        // Self-contained: records and XML I/O, naming the runtime by full path …
        assert!(!out.contains("\nuse "), "{out}");
        assert!(
            out.contains("pub struct Invoice(pub ::einvoice_transformator::record::Values);"),
            "{out}"
        );
        assert!(out.contains("pub struct InvoiceLine;"), "{out}");
        assert!(out.contains("pub fn from_xml"), "{out}");
        assert!(out.contains("pub fn to_xml"), "{out}");
        assert!(
            out.contains("pub fn read_xml<'a>(r: &mut ::einvoice_transformator::xml::Reader<'a>"),
            "{out}"
        );
        assert!(out.contains("pub fn write_xml(rec: &::einvoice_transformator::record::Index<'_>, base: u32, tag: &str"), "{out}");
        // … and no mappers.
        assert!(!out.contains("pub fn read("), "{out}");
        assert!(!out.contains("pub fn write("), "{out}");
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
        assert!(!out.contains("pub struct Invoice"), "{out}");
        assert!(!out.contains("pub fn from_xml"), "{out}");
        // Mappers are present.
        assert!(
            out.contains("pub fn read(source: Invoice) -> MappingResult<MainKey>"),
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
        for needle in ["pub struct Invoice(", "pub fn from_xml", "pub fn to_xml"] {
            assert!(
                monolith.contains(needle) && src.contains(needle),
                "{needle}"
            );
        }
        for needle in [
            "pub fn read(source: Invoice)",
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
        // Values are rendered once, skipped if empty, then set under their
        // id; an interior element is present once a value inside it is set.
        assert!(out.contains("source.set(6, &rendered);"), "{out}");
        assert!(out.contains("source.set(1, &rendered);"), "{out}");
        // An item is built in a record of its own and nested when non-empty.
        assert!(
            out.contains("let mut element0 = Draft::new(InvoiceLine::SIZE);"),
            "{out}"
        );
        assert!(
            out.contains("if !InvoiceLine::is_empty(element0.rec(), 0) {"),
            "{out}"
        );
        assert!(
            out.contains("source.push_record(7, element0.rec());"),
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
        // One physical element on the wire (id 2); the writer lays the
        // repeated logical field out under id 1 and the single one inline
        // at 3..6.
        assert!(
            out.contains("for item in rec.items(base + 2) { AdditionalDocumentReference::write_xml(&::einvoice_transformator::record::Index::new(item, AdditionalDocumentReference::SIZE), 0, \"AdditionalDocumentReference\", out); }"),
            "{out}"
        );
        // class: selector first, default bucket last; demux reports overflow.
        assert!(
            out.contains("pub fn class_all_additional_document_reference(item: ::einvoice_transformator::record::Rec<'_>) -> usize {\n        if item.get(2).is_some_and(|v| v.trim() == \"130\") { return 1; }\n        0\n    }"),
            "{out}"
        );
        assert!(
            out.contains(
                "if counts[1] > 1 { overflow(\"Invoice.InvoicedObjectReference\", counts[1] - 1); }"
            ),
            "{out}"
        );
        // mux: logical items move into the physical element, the
        // discriminator written from the selector.
        assert!(
            out.contains("for item in rec.items(1) { out.push_record(2, item); }"),
            "{out}"
        );
        assert!(
            out.contains("let mut item = rec.extract(3, 6);\n            item.set(2, \"130\");\n            out.push_record(2, item.rec());"),
            "{out}"
        );
        // The mappers call them on the root.
        assert!(
            out.contains("Invoice::demux(source.rec(), 0, &mut |node, extra| {"),
            "{out}"
        );
        assert!(out.contains("\"MATCH_MULTIPLE\""), "{out}");
        assert!(
            out.contains("    let source = Invoice::mux(source.rec());\n"),
            "{out}"
        );
        // The nodes map through the logical fields: read by picking the
        // physical items, written into the writer's own layout.
        assert!(
            out.contains("source.items(2).find(|i| Invoice::class_all_additional_document_reference(*i) == 1).and_then(|v0| v0.get(1))"),
            "{out}"
        );
        assert!(
            out.contains("source.items(2).filter(move |i| Invoice::class_all_additional_document_reference(*i) == 0)"),
            "{out}"
        );
        assert!(out.contains("source.set(4, &rendered);"), "{out}");
        assert!(
            out.contains("source.push_record(1, element0.rec());"),
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
            out.contains("if !rendered.is_empty() && !Amount::is_empty(source.rec(), "),
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
        // The default bucket takes what no selector claims ...
        assert!(out.contains("{ return 0; }\n        1\n    }"), "{out}");
        // ... reads the first of them, and reports the rest.
        assert!(
            out.contains("if counts[1] > 1 { overflow(\"Invoice.OtherRef\", counts[1] - 1); }"),
            "{out}"
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
        assert!(text.contains("pub struct Invoice("), "{text}");
        // The first spoke imports the shared structs; the second spoke's
        // module body is byte-identical: aliased, no file.
        let first = emitted(&plan.modules[0]);
        assert!(first.contains("pub use super::shared_0::*;"), "{first}");
        assert!(!first.contains("pub struct Invoice("), "{first}");
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
            assert!(code.contains("pub struct Invoice("), "{code}");
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
        assert!(
            out.contains("pub fn is_empty(rec: ::einvoice_transformator::record::Rec<'_>, base: u32) -> bool {"),
            "{out}"
        );
        // The root's 8 ids: an empty value or mark is empty, an item (under
        // the lines' id 7) is not, even an empty one.
        assert!(
            out.contains("local >= 8 || (payload.is_empty() && !matches!(local, 7))"),
            "{out}"
        );
        // Interior elements are laid out inline: `LegalMonetaryTotal` at
        // 3..7, written only when an id in that range is there.
        assert!(
            out.contains("if rec.any(base + 3, base + 7) { LegalMonetaryTotal::write_xml(rec, base + 3, \"LegalMonetaryTotal\", out); }"),
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

        // Spoke: nested read/write loops over the nested items, keyed by depth.
        let spoke = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        assert!(
            spoke.contains("let elements1 = element0.items(1);"),
            "{spoke}"
        );
        assert!(
            spoke.contains("for (idx1, element1) in elements1.enumerate()"),
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
            spoke.contains("element0.push_record(1, element1.rec());"),
            "{spoke}"
        );
        assert!(
            spoke.contains("if !InvoiceLineAllowanceCharge::is_empty(element1.rec(), 0)"),
            "{spoke}"
        );
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
        // Source record: a repeated scalar, one entry per value.
        assert!(
            out.contains("for value in rec.all(base + 1) { ::einvoice_transformator::xml::element(out, \"Note\", value); }"),
            "{out}"
        );
        // Reader: collect the normalized values, join with the separator.
        assert!(
            out.contains("source.all(1).filter_map(|s| Some(s).map(CompactString::from)"),
            "{out}"
        );
        // Slice join yields a `String`; `.into()` moves it into the inline
        // string type (O(1) for heap-sized joins).
        assert!(out.contains("Some(values.join(\"\\n\").into())"), "{out}");
        // Writer: the joined canonical value is pushed as one element.
        assert!(out.contains("source.push(1, &rendered);"), "{out}");
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
    fn test_reader_borrows_the_source_record() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // The reader views the record and copies each value it reads.
        assert!(
            out.contains("pub fn read(source: Invoice) -> MappingResult<MainKey>"),
            "{out}"
        );
        assert!(
            out.contains("let source = Index::new(document.0.rec(), Invoice::SIZE);"),
            "{out}"
        );
        // A value inside an interior element is read straight by its id.
        assert!(
            out.contains(
                "let value: Option<CompactString> = source.get(6).map(CompactString::from);"
            ),
            "{out}"
        );
        assert!(
            out.contains("for (idx0, element0) in elements0.enumerate()"),
            "{out}"
        );
    }

    #[test]
    fn test_reader_reads_a_shared_fallback_path_twice() {
        // Two primaries share `Invoice.UUID` (id 3) as a fallback: a borrowed
        // record reads it for both.
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
        assert_eq!(out.matches("value = source.get(3)").count(), 2, "{out}");
        assert!(
            out.contains("source.get(1).map(CompactString::from)"),
            "{out}"
        );
        assert!(
            out.contains("source.get(2).map(CompactString::from)"),
            "{out}"
        );
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
        // A collection under an interior container (the Factur-X shape): its
        // items nest under the interior's ids; the writer marks the wrapper
        // present only when there are items to push (as the typed writer
        // materialized it to reserve the items).
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
        // Reader: the items under `Wrap`'s `Line` id (wrapper at 2..4).
        assert!(out.contains("let elements0 = source.items(3);"), "{out}");
        // Writer: the wrapper is marked only when items exist.
        assert!(
            out.contains("if !hub_items0.is_empty() {\n        source.mark(2);\n    }"),
            "{out}"
        );
        assert!(
            out.contains("source.push_record(3, element0.rec());"),
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
        assert!(out.contains("source.set(2, \"2.1\");"), "{out}");
        // Reader never touches the field (id 2): write-only node.
        assert!(!out.contains("source.get(2)"), "{out}");
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
        assert!(
            out.contains("source.get(1).map(CompactString::from)"),
            "{out}"
        );
        assert!(out.contains("main.specification_id = Some(raw);"), "{out}");
        // Writer emits the constant and never consults the hub value.
        assert!(
            out.contains("source.set(1, \"urn:cen.eu:en16931:2017\");"),
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
            .find("if !InvoiceLine::is_empty(element0.rec(), 0) {")
            .expect("non-empty guard exists");
        let assign = out
            .find("element0.set(2, \"380\");")
            .expect("constant assigned on the element");
        let push = out
            .find("source.push_record(1, element0.rec());")
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
        assert!(out.contains("source.set(1, &rendered);"), "{out}");
        assert!(out.contains("source.set(2, &rendered);"), "{out}");
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
        assert!(out.contains("element0.set(2, &rendered);"), "{out}");
        // Reader: per-item mismatch check.
        assert!(out.contains("CLONE_MISMATCH"), "{out}");
    }

    /// Byte offset of `needle` inside the generated `impl {name} {` block.
    fn offset_in_struct(out: &str, name: &str, needle: &str) -> usize {
        let header = format!("impl {name} {{");
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
        // in that order; the layout and the emitted XML must too, not
        // alphabetically (`document_currency_code` < `id` < `invoice_line` <
        // `legal_monetary_total`).
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let id = offset_in_struct(
            &out,
            "Invoice",
            "rec.get(base + 1) { ::einvoice_transformator::xml::element(out, \"ID\"",
        );
        let currency = offset_in_struct(
            &out,
            "Invoice",
            "rec.get(base + 2) { ::einvoice_transformator::xml::element(out, \"DocumentCurrencyCode\"",
        );
        let totals = offset_in_struct(&out, "Invoice", "rec.any(base + 3, base + 7)");
        let lines = offset_in_struct(&out, "Invoice", "rec.items(base + 7)");
        assert!(
            id < currency && currency < totals && totals < lines,
            "{out}"
        );
    }

    #[test]
    fn test_source_struct_emits_attributes_before_element_text() {
        let (ir, _, source) = compiled();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let attr = offset_in_struct(
            &out,
            "LegalMonetaryTotalPayableAmount",
            "\" currencyID=\\\"\"",
        );
        let text = offset_in_struct(
            &out,
            "LegalMonetaryTotalPayableAmount",
            "::xml::text(out, value)",
        );
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
            out.contains("::einvoice_transformator::xml::element(out, \"cbc:ID\", value);"),
            "{out}"
        );
        assert!(out.contains("\"ID\" => {"), "{out}");
        assert!(out.contains("\"cac:LegalMonetaryTotal\", out);"), "{out}");
        assert!(out.contains("\"LegalMonetaryTotal\" => {"), "{out}");
        assert!(out.contains("\"cbc:PayableAmount\", out);"), "{out}");
        assert!(out.contains("\"PayableAmount\" => {"), "{out}");
        // Attributes and text are never prefixed.
        assert!(out.contains("\"currencyID\" => {"), "{out}");
        assert!(out.contains("out.push_str(\" currencyID=\\\"\");"), "{out}");
        assert!(
            out.contains("::einvoice_transformator::xml::text(out, value);"),
            "{out}"
        );
    }

    #[test]
    fn test_namespaced_root_carries_xmlns_markers_and_prefixed_root_tag() {
        let (ir, source) = compiled_namespaced();
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // The root writes one declaration per namespace, before any element.
        let xmlns = offset_in_struct(&out, "Invoice", "out.push_str(\" xmlns=\\\"\");");
        let cbc = offset_in_struct(&out, "Invoice", "out.push_str(\" xmlns:cbc=\\\"\");");
        let id = offset_in_struct(&out, "Invoice", "\"cbc:ID\"");
        assert!(xmlns < id && cbc < id, "{out}");
        assert!(
            out.contains("::einvoice_transformator::xml::attr(out, \"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\");"),
            "{out}"
        );
        // Non-root structs carry none.
        assert!(
            !out[out.find("impl LegalMonetaryTotal {").unwrap()..].contains("xmlns"),
            "{out}"
        );
        // Declaration + qualified root (the default namespace leaves it bare).
        assert!(
            out.contains("String::with_capacity(capacity);")
                && out.contains(
                    "out.push_str(\"<?xml version=\\\"1.0\\\" encoding=\\\"UTF-8\\\"?>\\n\");"
                ),
            "{out}"
        );
        assert!(
            out.contains("Invoice::write_xml(&::einvoice_transformator::record::Index::new(source.0.rec(), Invoice::SIZE), 0, \"Invoice\", &mut out);"),
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
            out.contains("::SIZE), 0, \"rsm:Invoice\", &mut out);"),
            "{out}"
        );
        assert!(!out.contains("xmlns"), "{out}");
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
        assert!(out.contains("impl IssueDateTimeDateTimeString {"), "{out}");
        assert!(out.contains("\"format\" => {"), "{out}");

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
        assert!(out.contains("source.set(3, \"102\");"), "{out}");
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
            out.contains("if !rendered.is_empty() && !InvoiceLineLineExtensionAmount::is_empty(element0.rec(), 1) {\n                element0.set(2, &rendered);"),
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
            out.contains("if !rendered.is_empty() && !LegalMonetaryTotalPayableAmount::is_empty(source.rec(), 4) {\n            source.set(5, &rendered);"),
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
        assert!(out.contains("let rendered = value;\n        if !rendered.is_empty() {\n            source.set(1, &rendered);"), "{out}");
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
        assert!(out.contains("element1.set(2, &rendered);"), "{out}");
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
        let guard = "if !PartyPartyTaxScheme::is_empty(source.rec(), 3) {";
        assert!(out.contains(guard), "{out}");
        let guard_at = out.find(guard).unwrap();
        let assign_at = out
            .find("source.set(6, \"VAT\");")
            .expect("constant assigned");
        let company_at = out
            .find("source.set(4, &rendered);")
            .expect("content written");
        assert!(
            company_at < guard_at && guard_at < assign_at,
            "constants come last: {out}"
        );
        // A root-level constant with no owner stays unconditional.
        assert!(
            out.contains("// constant -> ubl_version_id\n    source.set(7, \"2.1\");"),
            "{out}"
        );
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
            out.contains("// always present -> transaction.delivery\n    source.mark(3);"),
            "{out}"
        );
        assert_eq!(
            out.matches(".mark(").count(),
            1,
            "only the flagged element: {out}"
        );
    }

    #[test]
    fn test_constant_in_an_always_present_element_is_written_unconditionally() {
        // CII's document context: the guideline ID is pinned, the business
        // process is mapped, and the context element is mandatory. With no
        // business process the context still exists, so the pinned ID must be
        // written there rather than waiting for content that never comes.
        let (ir, _, source) = compile(
            r#"
            [Invoice.Context]
            required = true

            [Invoice.Context.Process.ID]
            type = "identifier"
            canonical_key = "BusinessProcessType"

            [Invoice.Context.Guideline.ID]
            type = "identifier"
            constant = "urn:cen.eu:en16931:2017"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let block = &out[out
            .find("// constant -> context.guideline.id")
            .expect("constant block")..];
        let block = &block[..block.find("\n\n").unwrap_or(block.len())];
        assert!(!block.contains("is_some_and"), "unguarded: {block}");
    }

    #[test]
    fn test_default_fills_the_key_when_the_source_has_none() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.Natura]
            type = "string"
            canonical_key = "VatCategoryCode"
            required = true
            default = "S"

            [Invoice.Rate]
            type = "decimal"
            canonical_key = "VatCategoryRate"
            default = "0.00"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let read = &out
            [out.find("pub fn read(").expect("reader")..out.find("pub fn write(").expect("writer")];
        assert!(
            read.contains("main.vat_category_code = Some(CompactString::from(\"S\"));"),
            "{read}"
        );
        assert!(
            read.contains("main.vat_category_rate = Some(Decimal::from_str(\"0.00\")"),
            "{read}"
        );
        assert!(
            !read.contains("REQUIRED_MISSING"),
            "a default always fills: {read}"
        );
    }

    /// One codec of every non-pattern kind, as a FatturaPA-like spoke uses them.
    fn fatturapa_like() -> (MappingIr, SourceModelMeta, CodecTable) {
        let codecs: CodecTable = crate::codec::parse_codecs(
            r#"
            [codec.doc-type]
            for_type = "string"
            values = [["380", "TD01"], ["381", "TD04"], ["380", "TD06"], ["S", ""]]

            [codec.amount-2]
            for_type = "decimal"
            fraction_digits = [2, 2]

            [codec.text]
            for_type = "string"
            charset = "latin-1"

            [codec.vat-split]
            for_type = "identifier"
            split = { at = 2, into = ["IdPaese", "IdCodice"] }
            "#,
        )
        .expect("codecs parse")
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
        let src = r#"[meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Invoice"

            [Invoice.TipoDocumento]
            type = "string"
            canonical_key = "InvoiceTypeCode"
            codec = "doc-type"

            [Invoice.Importo]
            type = "decimal"
            canonical_key = "PayableAmount"
            codec = "amount-2"

            [Invoice.Descrizione]
            type = "string"
            canonical_key = "ItemName"
            codec = "text"

            [Invoice.Cedente.IdFiscaleIVA]
            type = "identifier"
            canonical_key = "SellerVatIdentifier"
            codec = "vat-split"
            required = true
        "#;
        let (ir, source, diags) = build_ir_with(&[parse_mapping(src).expect("parses")], &codecs);
        assert!(diags.is_empty(), "{diags:?}");
        (ir, source, codecs)
    }

    #[test]
    fn test_alternatives_choose_on_write_and_combine_on_read() {
        let src = "[meta]\ndoc_format = \"x\"\nformat_version = \"1\"\nmapping_version = \"1\"\n\
                   canonical_model = \"c:1\"\nroot = \"Doc\"\n\
                   [Doc.S]\ntype = \"string\"\ncanonical_key = \"S\"\n\
                   [Doc.Code]\ntype = \"identifier\"\ncanonical_key = \"Addr\"\n\
                   when = { S = [\"A\", \"B\"] }\nplaceholder = \"0000000\"\n\
                   [Doc.Mail]\ntype = \"identifier\"\ncanonical_key = \"Addr\"\nwhen = { S = \"M\" }\n\
                   [Doc.Flag]\ntype = \"string\"\nconstant = \"1\"\nwhen = { S = { present = true } }\n";
        let codecs = CodecTable::new();
        let (ir, source, diags) = build_ir_with(&[parse_mapping(src).expect("parses")], &codecs);
        assert!(diags.is_empty(), "{diags:?}");
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");
        // Write: decided up front, NO_TARGET without a default, guarded blocks.
        assert!(
            out.contains(
                "let choice_1: usize = if matches!(main.s.as_deref(), Some(\"A\" | \"B\")) { 0 } \
                 else if main.s.as_deref() == Some(\"M\") { 1 } else { usize::MAX };"
            ),
            "{out}"
        );
        assert!(out.contains("if choice_1 == usize::MAX && main.addr.is_some() {"));
        assert!(out.contains("\"NO_TARGET\""));
        assert!(out.contains("if choice_1 == 1 {"));
        assert!(out.contains("let when_3: bool = main.s.is_some();"));
        assert!(out.contains("if when_3 {"));
        // Read: placeholder is no value; first with a value wins, fills `when`.
        assert!(out.contains("let value = value.filter(|v| !matches!(v.trim(), \"0000000\"));"));
        assert!(out.contains("main.s = Some(CompactString::from(\"M\"));"));
        assert!(out.contains("\"MULTIPLE_SOURCES\""));
    }

    #[test]
    fn test_codec_settings_round_and_limit_in_generated_code() {
        let codecs: CodecTable = crate::codec::parse_codecs(
            "[codec.amt]\nfor_type = \"decimal\"\nfraction_digits = [2, 2]\n\
             [codec.amt-down]\nfor_type = \"decimal\"\nfraction_digits = [2, 2]\nrounding = \"down\"\n\
             [codec.text-5]\nfor_type = \"string\"\nmax_length = 5\n",
        )
        .expect("codecs parse")
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
        let gen_with = |defaults: &str| {
            let src = format!(
                "[meta]\ndoc_format = \"x\"\nformat_version = \"1\"\nmapping_version = \"1\"\n\
                 canonical_model = \"c:1\"\nroot = \"Doc\"\n{defaults}\n\
                 [Doc.A]\ntype = \"decimal\"\ncanonical_key = \"A\"\ncodec = \"amt\"\n\
                 [Doc.B]\ntype = \"decimal\"\ncanonical_key = \"B\"\ncodec = \"amt-down\"\n\
                 [Doc.C]\ntype = \"string\"\ncanonical_key = \"C\"\ncodec = \"text-5\"\n"
            );
            let (ir, source, diags) =
                build_ir_with(&[parse_mapping(&src).expect("parses")], &codecs);
            assert!(diags.is_empty(), "{diags:?}");
            generate_spoke(&ir, &source, &codecs, "super::hub")
        };
        let plain = gen_with("");
        assert!(
            plain.contains("codec::format_fraction(&value.to_string(), 2, 2)"),
            "no rounding by default: {plain}"
        );
        assert!(
            plain.contains("RoundingStrategy::ToZero"),
            "codec's own mode"
        );
        assert!(
            plain.contains("rendered.chars().count() > 5"),
            "write limit"
        );
        assert!(plain.contains("raw.chars().count() > 5"), "read warning");
        let defaulted = gen_with("[meta.defaults]\nrounding = \"half_even\"\n");
        assert!(
            defaulted.contains(
                "round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointNearestEven)"
            ),
            "mapping default: {defaulted}"
        );
        assert!(defaulted.contains("RoundingStrategy::ToZero"), "codec wins");
    }

    #[test]
    fn test_split_codec_synthesizes_its_two_part_elements() {
        let (ir, source, codecs) = fatturapa_like();
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {out}"))
        };
        assert!(
            at("element(out, \"IdPaese\", value)") < at("element(out, \"IdCodice\", value)"),
            "head before tail"
        );
        assert!(out.contains("\"IdFiscaleIVA\" => {"), "{out}");
    }

    #[test]
    fn test_value_codecs_decode_on_read() {
        let (ir, source, codecs) = fatturapa_like();
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");
        let read = &out[out.find("pub fn read(").unwrap()..out.find("pub fn write(").unwrap()];
        // Code table, first pair per wire value: TD06 reads as 380 too, and the
        // empty wire value of `S` is never read.
        assert!(read.contains(r#""TD01" => Some("380"),"#), "{read}");
        assert!(read.contains(r#""TD06" => Some("380"),"#), "{read}");
        assert!(!read.contains(r#""" => Some("S")"#), "{read}");
        assert!(
            read.contains("Decimal::from_str(raw.trim()).ok()"),
            "{read}"
        );
        // The split value is read as its two parts joined.
        assert!(read.contains("codec::join_parts("), "{read}");
        assert!(
            read.contains("codec::join_parts(source.get(6), source.get(7))"),
            "{read}"
        );
    }

    #[test]
    fn test_value_codecs_encode_on_write() {
        let (ir, source, codecs) = fatturapa_like();
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");
        let write = &out[out.find("pub fn write(").unwrap()..];
        // First pair per canonical value: 380 writes TD01, never TD06.
        assert!(
            write.contains(r#""380" => CompactString::from("TD01"),"#),
            "{write}"
        );
        assert!(!write.contains(r#"CompactString::from("TD06")"#), "{write}");
        assert!(
            write.contains(r#""S" => CompactString::from(""),"#),
            "{write}"
        );
        assert!(write.contains("codec::format_fraction("), "{write}");
        assert!(write.contains("codec::to_latin1("), "{write}");
        assert!(
            write.contains("codec::split_at(value.as_str(), 2)"),
            "{write}"
        );
        assert!(
            write.contains("REQUIRED_MISSING"),
            "required split node: {write}"
        );
    }

    #[test]
    fn test_generate_hub_with_derivations_emits_the_rules() {
        let (_, hub, _) = compiled();
        let rules = crate::derive::parse_derivations(
            r#"
            [[derive]]
            key = "PayableAmount"
            rule = "BR-CO-16"
            add = ["PayableAmount"]
            "#,
        )
        .unwrap();
        let out = generate_hub_with(&hub, &rules);
        assert!(out.contains("pub fn derive_missing(&mut self)"), "{out}");
        assert!(
            out.contains(r#"("PayableAmount", "BR-CO-16", &["PayableAmount"]),"#),
            "{out}"
        );
        assert!(
            out.contains("if self.payable_amount.is_none() && self.payable_amount.is_some() {"),
            "{out}"
        );
        assert!(out.contains("pub fn check_derived("), "{out}");
        assert!(
            !out.contains("unwrap_or_default() +") && !out.contains("unwrap_or_default() -"),
            "no unchecked decimal arithmetic: {out}"
        );
        assert!(
            out.contains(
                "if let Some(carried) = self.payable_amount && self.payable_amount.is_some() {"
            ),
            "a checked `add` rule recomputes its carried target: {out}"
        );
        assert!(
            out.contains(
                r#"mismatches.push(("PayableAmount", "BR-CO-16", false, carried, computed));"#
            ),
            "the default check level is a warning: {out}"
        );
        // No rules: the table is empty and nothing is derived.
        let plain = generate_hub(&hub);
        assert!(plain.contains("pub const DERIVATIONS"), "{plain}");
        assert!(!plain.contains("derived.push"), "{plain}");
        syn::parse_file(&out).expect("valid Rust");
    }

    #[test]
    fn test_generate_hub_value_rule_fills_nested_items_and_compiles() {
        let (_, hub, _) = compiled();
        let rules = crate::derive::parse_derivations(
            r#"
            [[derive]]
            key = "InvoiceLines/Quantity"
            rule = "TEST"
            value = "1"

            [[derive]]
            key = "PayableAmount"
            rule = "BR-CO-16"
            add = ["PayableAmount"]
            requires = ["PayableAmount"]
            skip_zero = true
            skip_negative = true
            check = "off"
            "#,
        )
        .unwrap();
        // The second rule needs itself (E113), but both targets are valid.
        assert!(
            crate::derive::check_derivations(&hub, &rules)
                .iter()
                .all(|d| d.code != "E110" && d.code != "E114")
        );
        let out = generate_hub_with(&hub, &rules);
        assert!(out.contains("for i0 in &mut self.invoice_lines {"), "{out}");
        assert!(out.contains("if i0.quantity.is_none() {"), "{out}");
        assert!(
            out.contains("i0.quantity = Some(<Decimal as std::str::FromStr>::from_str(\"1\")"),
            "{out}"
        );
        assert!(
            out.contains("if !value.is_zero() && !value.is_sign_negative() {"),
            "{out}"
        );
        assert!(
            out.contains("if let Some(value) = Some(self.payable_amount.unwrap_or_default()) {"),
            "arithmetic yields an Option, `None` on overflow: {out}"
        );
        assert!(
            !out.contains("mismatches.push("),
            "neither a `value` rule nor a `check = \"off\"` rule is checked: {out}"
        );
        syn::parse_file(&out).expect("valid Rust");
    }

    #[test]
    fn test_generate_hub_copy_and_number_rules_fill_items_and_compile() {
        let mapping = parse_mapping(
            r#"
            [meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Doc"
            [Doc.L]
            type = "collection"
            canonical_key = "Lines"
            [Doc.L.Id]
            type = "identifier"
            canonical_key = "LineId"
            [Doc.C]
            type = "collection"
            canonical_key = "Charges"
            [Doc.C.Is]
            type = "boolean"
            canonical_key = "IsCharge"
            [Doc.C.A]
            type = "decimal"
            canonical_key = "Amount"
            [Doc.C.S]
            type = "decimal"
            canonical_key = "Signed"
            [Doc.C.N]
            type = "identifier"
            canonical_key = "Number"
            "#,
        )
        .unwrap();
        let (ir, _, diags) = build_ir(&[mapping]);
        assert!(diags.is_empty(), "{diags:?}");
        let hub = derive_hub(std::slice::from_ref(&ir)).0;
        let rules = crate::derive::parse_derivations(
            r#"
            [[derive]]
            key = "Charges/Signed"
            rule = "SIGN"
            copy = "Amount"
            negate_where = { IsCharge = "false" }

            [[derive]]
            key = "Charges/Number"
            rule = "NUM"
            number_after = "Lines/LineId"
            "#,
        )
        .unwrap();
        let diags = crate::derive::check_derivations(&hub, &rules);
        assert!(diags.is_empty(), "{diags:?}");
        let out = generate_hub_with(&hub, &rules);

        // Copy: only into an absent target, negated where the filter holds.
        assert!(
            out.contains("if i0.signed.is_none() && let Some(value) = i0.amount {"),
            "{out}"
        );
        assert!(
            out.contains("let negate = i0.is_charge == Some(false);"),
            "{out}"
        );
        assert!(
            out.contains(
                "i0.signed = Some(if negate && !value.is_zero() { -value } else { value });"
            ),
            "a zero stays unsigned (no `-0.00`): {out}"
        );
        // Number: after the largest whole number of either collection.
        assert!(out.contains("for i0 in &self.lines {"), "{out}");
        assert!(
            out.contains("i0.line_id.as_deref().and_then(|v| v.trim().parse::<u64>().ok())"),
            "{out}"
        );
        assert!(
            out.contains("i0.number.as_deref().and_then(|v| v.trim().parse::<u64>().ok())"),
            "{out}"
        );
        assert!(
            out.contains("i0.number = Some(CompactString::from(next.to_string()));"),
            "{out}"
        );
        assert!(out.contains("next = next.saturating_add(1);"), "{out}");
        assert!(
            out.contains(r#"derived.push(("Charges/Number", "NUM"));"#),
            "{out}"
        );
        assert!(
            !out.contains("mismatches.push("),
            "fills are not checked: {out}"
        );
        syn::parse_file(&out).expect("valid Rust");
    }

    #[test]
    fn test_generate_hub_sum_rules_filter_and_check_with_checked_arithmetic() {
        let mapping = parse_mapping(
            r#"
            [meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Doc"
            [Doc.A]
            type = "decimal"
            canonical_key = "LineTotal"
            [Doc.B]
            type = "decimal"
            canonical_key = "Allowances"
            [Doc.T]
            type = "decimal"
            canonical_key = "NetTotal"
            [Doc.L]
            type = "collection"
            canonical_key = "Lines"
            [Doc.L.X]
            type = "decimal"
            canonical_key = "Amount"
            [Doc.L.C]
            type = "boolean"
            canonical_key = "IsCharge"
            [Doc.L.K]
            type = "string"
            canonical_key = "Kind"
            [Doc.L.R]
            type = "decimal"
            canonical_key = "Rate"
            "#,
        )
        .unwrap();
        let (ir, _, diags) = build_ir(&[mapping]);
        assert!(diags.is_empty(), "{diags:?}");
        let hub = derive_hub(std::slice::from_ref(&ir)).0;
        let rules = crate::derive::parse_derivations(
            r#"
            [[derive]]
            key = "LineTotal"
            rule = "SUM"
            sum = "Lines/Amount"
            where = { IsCharge = "false", Kind = "net" }
            check = "error"

            [[derive]]
            key = "NetTotal"
            rule = "NET"
            add = ["LineTotal"]
            subtract = ["Allowances"]

            [[derive]]
            key = "Lines/Rate"
            rule = "RATE"
            value = "0.5"
            "#,
        )
        .unwrap();
        let diags = crate::derive::check_derivations(&hub, &rules);
        assert!(diags.is_empty(), "{diags:?}");
        let out = generate_hub_with(&hub, &rules);

        // The `where` filter compares a boolean natively and a string by `as_deref`.
        assert!(out.contains("if item.is_charge != Some(false) {"), "{out}");
        assert!(
            out.contains(r#"if item.kind.as_deref() != Some("net") {"#),
            "{out}"
        );
        // Summation is checked: an overflow stops the loop and derives nothing.
        assert!(
            out.contains("match sum.unwrap_or_default().checked_add(value) {"),
            "{out}"
        );
        assert!(
            out.contains("if !overflow && let Some(value) = sum {"),
            "{out}"
        );
        // The sum is recomputed against a carried target, at error level.
        assert!(
            out.contains("if let Some(carried) = self.line_total {"),
            "{out}"
        );
        assert!(
            out.contains(r#"mismatches.push(("LineTotal", "SUM", true, carried, computed));"#),
            "{out}"
        );
        assert!(
            out.contains("let computed = None;"),
            "overflow is reported: {out}"
        );
        // `subtract` is a checked subtraction, never a bare `-`.
        assert!(
            out.contains(
                ".and_then(|v: Decimal| v.checked_sub(self.allowances.unwrap_or_default()))"
            ),
            "{out}"
        );
        // A decimal literal is parsed, not spliced in as a float.
        assert!(
            out.contains(r#"i0.rate = Some(<Decimal as std::str::FromStr>::from_str("0.5")"#),
            "{out}"
        );
        syn::parse_file(&out).expect("valid Rust");
    }

    #[test]
    fn test_mux_list_discriminator_replaces_value_outside_list() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.Means]
            xml = "PaymentMeans"
            match = { Code = ["50", "130"] }

            [Invoice.Means.Code]
            type = "string"
            canonical_key = "MeansCode"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        // A slot value outside the list (`999`) would not demux back in.
        assert!(
            out.contains(
                "if !item.get(1).is_some_and(|v| matches!(v.trim(), \"50\" | \"130\")) \
                 { item.set(1, \"50\"); }"
            ),
            "{out}"
        );
    }

    #[test]
    fn test_read_placeholder_primary_falls_back() {
        let (ir, _, source) = compile(
            r#"
            [Invoice.Code]
            type = "identifier"
            canonical_key = "Addr"
            placeholder = "0000000"
            fallbacks = ["Invoice.Mail"]

            [Invoice.Mail]
            type = "identifier"
            "#,
        );
        let out = generate_spoke(&ir, &source, &no_codecs(), "super::hub");
        let filter = "value = value.filter(|v| !matches!(v.trim(), \"0000000\"));";
        let first = out
            .find(filter)
            .expect("placeholder filtered before fallbacks");
        let fallback = out.find("if value.is_none() {").expect("fallback");
        assert!(first < fallback, "{out}");
        assert!(
            out.contains(&format!("let {filter}")),
            "fallback values too: {out}"
        );
    }

    #[test]
    fn test_read_clone_over_length_warns() {
        let codecs: CodecTable =
            crate::codec::parse_codecs("[codec.short]\nfor_type = \"string\"\nmax_length = 3\n")
                .expect("codecs")
                .into_iter()
                .map(|c| (c.id.clone(), c))
                .collect();
        let src = "[meta]\ndoc_format = \"f\"\nformat_version = \"1\"\nmapping_version = \"1\"\n\
                   canonical_model = \"c:1\"\nroot = \"Invoice\"\n\
                   [Invoice.Name]\ntype = \"string\"\ncanonical_key = \"Name\"\n\
                   [Invoice.Copy]\ntype = \"string\"\nclone_of = \"Name\"\ncodec = \"short\"\n";
        let (ir, source, diags) = build_ir_with(&[parse_mapping(src).expect("parses")], &codecs);
        assert!(diags.is_empty(), "{diags:?}");
        let out = generate_spoke(&ir, &source, &codecs, "super::hub");
        let clone = &out[out.find("Invoice.Copy: copy of Name").expect("clone block")..];
        let clone = &clone[..clone.find("CLONE_MISMATCH").expect("compare")];
        assert!(clone.contains("if raw.chars().count() > 3 {"), "{clone}");
        assert!(clone.contains("\"VALUE_TOO_LONG\""), "{clone}");
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
