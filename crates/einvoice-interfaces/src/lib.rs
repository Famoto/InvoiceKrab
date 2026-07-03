//! `einvoice-interfaces` — the public engine API (N–1–N transformation).
//!
//! Everything downstream of the spoke TOML is generated at build time: `build.rs`
//! scans the workspace `mappings/` directory and runs the `einvoice-dsl` compiler
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
//! - [`Spoke`] — selects which generated mapper to use.
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

use einvoice_transformator::result::MappingResult;

pub mod analysis;
pub mod cli;
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
    /// The spoke registry: one `mod <slug>` per `mappings/*.toml`, the `Spoke`
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
    /// XML, carrying through the writer's diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Serialize`] if the produced model cannot be
    /// serialized.
    pub fn from_hub(
        &self,
        spoke: Spoke,
        hub: &MainKey,
    ) -> Result<MappingResult<String>, EngineError> {
        Ok(generated::write(spoke, hub)?)
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
        let written = self.from_hub(to, &hub)?;
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
        <IssueDate>2026-06-27</IssueDate>
        <DocumentCurrencyCode>eur</DocumentCurrencyCode>
        <LegalMonetaryTotal>
            <PayableAmount currencyID="EUR">119.00</PayableAmount>
        </LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>2</InvoicedQuantity><Item><Name>Widget</Name></Item></InvoiceLine>
        <InvoiceLine><ID>2</ID><InvoicedQuantity>3</InvoicedQuantity><Item><Name>Gadget</Name></Item></InvoiceLine>
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

        assert!(xml.contains("<ID>INV-42</ID>"));
        assert!(!xml.contains("<AccountingSupplierParty>"), "{xml}");
        assert!(!xml.contains("<TaxAmount/>"), "{xml}");
    }

    #[test]
    fn test_writer_reports_missing_target_required_field() {
        let engine = Engine::new();
        let result = engine
            .transform(Spoke::UblInvoice, Spoke::XrechnungInvoice, UBL)
            .expect("well-formed");

        assert!(result.has_errors());
        assert!(result.diagnostics.iter().any(|d| {
            d.code == "REQUIRED_MISSING"
                && d.source_node == "Invoice.CustomizationID"
                && d.canonical_key.as_deref() == Some("SpecificationId")
        }));
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
        let xml = br#"<Invoice>
            <ID>INV-42</ID>
            <Note>first note</Note>
            <Note>  second note </Note>
            <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
            <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
            <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
        </Invoice>"#;
        let result = engine.to_hub(Spoke::UblInvoice, xml).expect("well-formed");
        assert!(!result.has_errors(), "{:?}", result.diagnostics);
        let hub = result.value.expect("hub");
        assert_eq!(
            hub.invoice_note.as_deref(),
            Some("first note\nsecond note"),
            "notes join in source order, each trimmed"
        );

        let out = engine
            .from_hub(Spoke::UblInvoice, &hub)
            .expect("renderable");
        let xml = out.value.expect("document");
        assert_eq!(xml.matches("<Note>").count(), 1, "{xml}");
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
