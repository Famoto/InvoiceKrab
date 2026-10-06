//! `einvoice-interfaces` — the public engine API (N–1–N transformation).
//!
//! Everything downstream of the spoke TOML is generated at build time: `build.rs`
//! loads the workspace `config/` directory (codecs, then mappings) and runs the `einvoice-dsl` compiler
//! over every `*.toml` it finds — no spoke is named here in code. It emits the
//! typed canonical hub (`MainKey`), one mapper module per spoke, and a generated
//! registry (`spokes.rs`) holding the [`Spoke`] enum and the read/write dispatch.
//! Each spoke's name is derived from its `[meta].doc_format`. This crate
//! `include!`s that generated code and exposes the [`Engine`]; at run time the
//! engine only ever calls generated code — there is no interpreter and no
//! hand-written model struct.
//!
//! # Structure
//!
//! - [`Engine`] — [`Engine::to_hub`] (source bytes → [`MainKey`]),
//!   [`Engine::from_hub`] ([`MainKey`] → target bytes), and [`Engine::transform`]
//!   (source bytes → target bytes through the hub — the N–1–N path).
//! - [`Spoke`] — selects which generated mapper to use; [`Spoke::contract`] is
//!   its embedded [`contract::TransformationContract`], [`Spoke::schema`] and
//!   [`Spoke::samples`] its schema-conformance declarations, which
//!   [`conformance::check`] turns into checks.
//! - [`MainKey`] — the generated typed canonical hub.
//! - [`EngineError`] — XML (de)serialization failures at the crate boundary.
//!
//! Mapping-level outcomes (missing required fields, type errors, fallbacks taken)
//! are not errors: they are carried as
//! [`MappingDiagnostic`](einvoice_transformator::result::MappingDiagnostic)s in
//! the returned [`MappingResult`]. An [`EngineError`] means the bytes could not
//! be parsed or rendered at all.
//!
//! ```no_run
//! use einvoice_interfaces::{Engine, Spoke};
//!
//! let engine = Engine::new();
//! let result = engine
//!     .transform(Spoke::UblInvoice, Spoke::UblInvoice, b"<Invoice>...</Invoice>")
//!     .expect("well-formed XML");
//! assert!(!result.has_errors());
//! ```

use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};

pub mod analysis;
pub mod cli;
pub mod conformance;
pub mod contract;
pub mod keys;
pub mod server;
mod table;

/// The generated canonical hub, spoke mappers, and registry, emitted by
/// `build.rs` into `OUT_DIR`. Generated code is allowed to trip style/unused
/// lints.
#[allow(clippy::all, unused)]
mod generated {
    /// The typed canonical hub (`MainKey` + item structs).
    pub mod hub {
        include!(concat!(env!("OUT_DIR"), "/hub.rs"));
    }
    /// The spoke registry: one `mod <slug>` per `config/mappings/*.toml`, the `Spoke`
    /// enum, and the `read`/`write` dispatch — all derived from the spokes'
    /// `[meta]` tables. Names nothing by hand.
    include!(concat!(env!("OUT_DIR"), "/spokes.rs"));
}

pub use generated::Spoke;
pub use generated::hub::MainKey;

/// A failure at the crate boundary: the bytes could not be parsed or rendered.
///
/// Mapping-level issues (missing fields, bad types) are diagnostics in the
/// [`MappingResult`], not [`EngineError`]s.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Source XML could not be deserialized into the typed model.
    #[error("source deserialization failed: {0}")]
    Deserialize(#[from] quick_xml::DeError),
    /// The produced model could not be serialized back to XML.
    #[error("target serialization failed: {0}")]
    Serialize(#[from] quick_xml::SeError),
}

/// The transformation engine. Stateless and cheap to construct; all mapping logic
/// lives in the generated code linked at build time.
#[derive(Debug, Clone, Copy, Default)]
pub struct Engine;

impl Engine {
    /// Creates an engine.
    pub fn new() -> Self {
        Engine
    }

    /// Deserializes `bytes` of `spoke` and runs its generated reader, producing
    /// the typed canonical hub plus any mapping diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Deserialize`] if `bytes` is not a well-formed
    /// document for `spoke`.
    pub fn to_hub(
        &self,
        spoke: Spoke,
        bytes: &[u8],
    ) -> Result<MappingResult<MainKey>, EngineError> {
        Ok(generated::read(spoke, bytes)?)
    }

    /// Runs `spoke`'s generated writer over `hub` and serializes the result to
    /// XML, carrying through the writer's diagnostics. EN 16931 totals the hub
    /// lacks are first derived by their calculation rules
    /// ([`MainKey::derive_missing`]), each reported as a `VALUE_DERIVED` info
    /// diagnostic; values the hub carries are never replaced.
    ///
    /// Consumes the hub: the writer moves its values into the target document
    /// instead of cloning them, so the hub's memory is released as the target
    /// is built — which matters for multi-hundred-MB documents.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Serialize`] if the produced model cannot be
    /// serialized.
    pub fn from_hub(
        &self,
        spoke: Spoke,
        mut hub: MainKey,
    ) -> Result<MappingResult<String>, EngineError> {
        // EN 16931 totals the hub lacks are computed by their calculation
        // rules (`config/derivations.toml`) before the writer runs.
        let derived: Vec<MappingDiagnostic> = hub
            .derive_missing()
            .into_iter()
            .map(|(label, rule)| {
                let mut d = MappingDiagnostic::new(
                    Severity::Info,
                    "VALUE_DERIVED",
                    rule,
                    format!("`{label}` was absent and is derived by {rule}"),
                );
                d.canonical_key = Some(label.to_string());
                d
            })
            .collect();
        let mut written = generated::write(spoke, hub)?;
        if !derived.is_empty() {
            written.diagnostics.splice(0..0, derived);
        }
        Ok(written)
    }

    /// Transforms `bytes` from the `from` spoke to the `to` spoke through the
    /// canonical hub (the N–1–N path). Diagnostics from the read half and the
    /// write half are concatenated in order.
    ///
    /// # Errors
    ///
    /// Returns an [`EngineError`] if the source cannot be deserialized or the
    /// target cannot be serialized.
    pub fn transform(
        &self,
        from: Spoke,
        to: Spoke,
        bytes: &[u8],
    ) -> Result<MappingResult<String>, EngineError> {
        let read = self.to_hub(from, bytes)?;
        let Some(hub) = read.value else {
            return Ok(MappingResult::new(None, read.diagnostics));
        };
        let written = self.from_hub(to, hub)?;
        let mut diagnostics = read.diagnostics;
        diagnostics.extend(written.diagnostics);
        Ok(MappingResult::new(written.value, diagnostics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr as _;

    const UBL: &[u8] = br#"<Invoice>
        <ID>INV-42</ID>
        <IssueDate>2026-06-27</IssueDate><InvoiceTypeCode>380</InvoiceTypeCode>
        <DocumentCurrencyCode>eur</DocumentCurrencyCode>
        <AccountingSupplierParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress><PartyLegalEntity><RegistrationName>Seller GmbH</RegistrationName></PartyLegalEntity></Party></AccountingSupplierParty><AccountingCustomerParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress><PartyLegalEntity><RegistrationName>Buyer AG</RegistrationName></PartyLegalEntity></Party></AccountingCustomerParty><TaxTotal><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxSubtotal><TaxableAmount currencyID="EUR">100.00</TaxableAmount><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></TaxCategory></TaxSubtotal></TaxTotal>
        <LegalMonetaryTotal>
            <LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount><TaxExclusiveAmount currencyID="EUR">100.00</TaxExclusiveAmount><TaxInclusiveAmount currencyID="EUR">119.00</TaxInclusiveAmount><PayableAmount currencyID="EUR">119.00</PayableAmount>
        </LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity unitCode="C62">2</InvoicedQuantity><LineExtensionAmount currencyID="EUR">50.00</LineExtensionAmount><Item><Name>Widget</Name><ClassifiedTaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></ClassifiedTaxCategory></Item><Price><PriceAmount currencyID="EUR">25.00</PriceAmount></Price></InvoiceLine>
        <InvoiceLine><ID>2</ID><InvoicedQuantity unitCode="C62">3</InvoicedQuantity><LineExtensionAmount currencyID="EUR">50.00</LineExtensionAmount><Item><Name>Gadget</Name><ClassifiedTaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></ClassifiedTaxCategory></Item><Price><PriceAmount currencyID="EUR">25.00</PriceAmount></Price></InvoiceLine>
    </Invoice>"#;

    #[test]
    fn test_to_hub_populates_typed_fields() {
        let engine = Engine::new();
        let result = engine.to_hub(Spoke::UblInvoice, UBL).expect("well-formed");
        assert!(!result.has_errors(), "{:?}", result.diagnostics);
        let hub = result.value.expect("reader always yields a hub");
        // `normalize = ["trim","uppercase"]` upcased the lowercase currency.
        assert_eq!(hub.document_currency.as_deref(), Some("EUR"));
        assert_eq!(hub.invoice_number.as_deref(), Some("INV-42"));
        assert_eq!(
            hub.payable_amount,
            Some(Decimal::from_str("119.00").unwrap())
        );
        assert_eq!(hub.invoice_lines.len(), 2);
        assert_eq!(hub.invoice_lines[1].item_name.as_deref(), Some("Gadget"));
    }

    #[test]
    fn test_transform_roundtrips_through_hub() {
        let engine = Engine::new();
        let out = engine
            .transform(Spoke::UblInvoice, Spoke::UblInvoice, UBL)
            .expect("well-formed");
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let xml = out.value.expect("writer yields a document");
        // The transformed document re-parses and preserves the canonical values.
        let again = engine
            .to_hub(Spoke::UblInvoice, xml.as_bytes())
            .expect("re-parse")
            .value
            .expect("hub");
        assert_eq!(again.invoice_number.as_deref(), Some("INV-42"));
        assert_eq!(again.document_currency.as_deref(), Some("EUR"));
        assert_eq!(again.invoice_lines.len(), 2);
    }

    #[test]
    fn test_transform_omits_empty_target_containers() {
        let engine = Engine::new();
        let out = engine
            .transform(Spoke::UblInvoice, Spoke::UblInvoice, UBL)
            .expect("well-formed");
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let xml = out.value.expect("writer yields a document");

        assert!(xml.contains("<cbc:ID>INV-42</cbc:ID>"), "{xml}");
        // Containers the source fills nothing in are not emitted, even where a
        // pinned constant (`PartyTaxScheme/TaxScheme/ID`, `CardAccount/NetworkID`)
        // could otherwise have conjured them.
        for empty in [
            "<cac:PayeeParty",
            "<cac:Delivery",
            "<cac:PartyTaxScheme",
            "<cac:CardAccount",
        ] {
            assert!(!xml.contains(empty), "{empty} in {xml}");
        }
        assert!(!xml.contains("<cbc:TaxAmount/>"), "{xml}");
    }

    #[test]
    fn test_writer_emits_children_in_mapping_declaration_order() {
        // The UBL mapping declares ID, IssueDate, DocumentCurrencyCode,
        // LegalMonetaryTotal, InvoiceLine in schema order; the emitted document
        // must follow it, not the alphabetical order of the generated fields.
        let engine = Engine::new();
        let out = engine
            .transform(Spoke::UblInvoice, Spoke::UblInvoice, UBL)
            .expect("well-formed");
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let xml = out.value.expect("writer yields a document");
        let at = |needle: &str| {
            xml.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {xml}"))
        };
        let id = at("<cbc:ID>INV-42</cbc:ID>");
        let issue = at("<cbc:IssueDate>");
        let currency = at("<cbc:DocumentCurrencyCode>");
        let totals = at("<cac:LegalMonetaryTotal>");
        let line = at("<cac:InvoiceLine>");
        assert!(
            id < issue && issue < currency && currency < totals && totals < line,
            "{xml}"
        );
    }

    #[test]
    fn test_writer_reports_missing_target_required_field() {
        let engine = Engine::new();
        let result = engine
            .transform(Spoke::UblInvoice, Spoke::XrechnungInvoice, UBL)
            .expect("well-formed");

        // XRechnung pins its own specification identifier (BT-24) but needs
        // the business process (BT-23) from the source, which this one lacks.
        assert!(result.has_errors());
        assert!(result.diagnostics.iter().any(|d| {
            d.code == "REQUIRED_MISSING"
                && d.source_node == "Invoice.ProfileID"
                && d.canonical_key.as_deref() == Some("BusinessProcessType")
        }));
    }

    #[test]
    fn test_from_hub_derives_missing_totals_and_reports_them() {
        // A hub without the sum of line net amounts (BT-106) or the total
        // without VAT (BT-109), as FatturaPA yields: the engine derives both by
        // BR-CO-10 / BR-CO-13, leaves the present total with VAT alone, and
        // says so.
        let engine = Engine::new();
        let mut hub = engine
            .to_hub(Spoke::UblInvoice, UBL)
            .unwrap()
            .value
            .unwrap();
        hub.sum_of_invoice_line_net_amount = None;
        hub.invoice_total_without_vat = None;
        hub.invoice_lines[0].line_net_amount = Some(Decimal::from_str("40.00").unwrap());
        hub.invoice_lines[1].line_net_amount = Some(Decimal::from_str("60").unwrap());
        let out = engine.from_hub(Spoke::UblInvoice, hub).unwrap();
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let xml = out.value.unwrap();
        assert!(
            xml.contains(
                r#"<cbc:LineExtensionAmount currencyID="EUR">100.00</cbc:LineExtensionAmount>"#
            ),
            "{xml}"
        );
        assert!(
            xml.contains(
                r#"<cbc:TaxExclusiveAmount currencyID="EUR">100.00</cbc:TaxExclusiveAmount>"#
            ),
            "{xml}"
        );
        assert!(
            xml.contains(
                r#"<cbc:TaxInclusiveAmount currencyID="EUR">119.00</cbc:TaxInclusiveAmount>"#
            ),
            "{xml}"
        );
        let derived: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "VALUE_DERIVED")
            .map(|d| {
                (
                    d.canonical_key.as_deref().unwrap_or(""),
                    d.source_node.as_str(),
                )
            })
            .collect();
        assert_eq!(
            derived,
            [
                ("SumOfInvoiceLineNetAmount", "BR-CO-10"),
                ("InvoiceTotalWithoutVat", "BR-CO-13")
            ]
        );
    }

    #[test]
    fn test_missing_required_id_is_a_diagnostic_not_an_error() {
        let engine = Engine::new();
        let xml = br#"<Invoice>
            <ID></ID>
            <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
            <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
            <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
        </Invoice>"#;
        let result = engine.to_hub(Spoke::UblInvoice, xml).expect("well-formed");
        // Empty ID is `empty_as_missing` → required-missing diagnostic.
        assert!(result.has_errors());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "REQUIRED_MISSING" && d.source_node == "Invoice.ID")
        );
    }

    #[test]
    fn test_repeated_note_joins_into_one_canonical_value() {
        // BG-1: `cbc:Note` repeats in UBL; `multiple = "join"` collapses the
        // values into the single canonical `InvoiceNote`, and the writer emits
        // the joined value back as one element.
        let engine = Engine::new();
        let xml = String::from_utf8(UBL.to_vec()).unwrap().replace(
            "<DocumentCurrencyCode>",
            "<Note>first note</Note><Note>  second note </Note><DocumentCurrencyCode>",
        );
        let result = engine
            .to_hub(Spoke::UblInvoice, xml.as_bytes())
            .expect("well-formed");
        assert!(!result.has_errors(), "{:?}", result.diagnostics);
        let hub = result.value.expect("hub");
        assert_eq!(
            hub.invoice_note.as_deref(),
            Some("first note\nsecond note"),
            "notes join in source order, each trimmed"
        );

        let out = engine.from_hub(Spoke::UblInvoice, hub).expect("renderable");
        let xml = out.value.expect("document");
        assert_eq!(xml.matches("<cbc:Note>").count(), 1, "{xml}");
    }

    #[test]
    fn test_absent_required_element_is_a_diagnostic_not_an_engine_error() {
        // The required `<ID>` element is missing entirely (not just empty). The
        // document must still parse; the reader reports REQUIRED_MISSING.
        let engine = Engine::new();
        let xml = br#"<Invoice>
            <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
            <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
            <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
        </Invoice>"#;
        let result = engine.to_hub(Spoke::UblInvoice, xml).expect("must parse");
        assert!(result.has_errors());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "REQUIRED_MISSING" && d.source_node == "Invoice.ID"),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn test_malformed_xml_is_an_engine_error() {
        let engine = Engine::new();
        let err = engine
            .to_hub(Spoke::UblInvoice, b"not xml <<<")
            .unwrap_err();
        assert!(matches!(err, EngineError::Deserialize(_)));
    }
}
