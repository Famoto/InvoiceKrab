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
//! - [`MainKey`] — the generated typed canonical hub. A transform keeps it
//!   packed in between (the generated `CompactHub`: the root collections'
//!   items as records), and builds a [`MainKey`] only for
//!   [`Engine::to_hub`] and from [`Engine::from_hub`]'s argument.
//! - [`EngineError`] — XML (de)serialization failures at the crate boundary.
//!
//! Mapping-level outcomes (missing required fields, type errors, fallbacks taken)
//! are not errors: they are carried as
//! [`MappingDiagnostic`](einvoice_transformator::result::MappingDiagnostic)s in
//! the returned [`MappingResult`]. An [`EngineError`] means the bytes are not
//! a document of the source format (its [`identity`], checked on every read)
//! or could not be parsed or rendered at all.
//!
//! ```no_run
//! use einvoice_interfaces::{Engine, Spoke};
//!
//! let engine = Engine::new();
//! let result = engine
//!     .transform(Spoke::UblInvoice, Spoke::UblInvoice, br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2">...</Invoice>"#)
//!     .expect("well-formed XML");
//! assert!(!result.has_errors());
//! ```

use std::borrow::Cow;

use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};

pub mod analysis;
pub mod cli;
pub mod conformance;
pub mod contract;
pub mod encoding;
pub mod identity;
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
use generated::hub::CompactHub;
pub use generated::hub::MainKey;

/// A failure at the crate boundary: the bytes are not a document of the source
/// format, or could not be parsed or rendered.
///
/// Mapping-level issues (missing fields, bad types) are diagnostics in the
/// [`MappingResult`], not [`EngineError`]s.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The source document's character encoding cannot be decoded
    /// ([`encoding::to_utf8`]).
    #[error("source encoding: {0}")]
    Encoding(#[from] encoding::EncodingError),
    /// The source document does not have the source format's identity (root
    /// namespace and name, profile, version, identity attributes).
    #[error("source is not a {format} document: {error}")]
    Identity {
        /// The source format's display name.
        format: &'static str,
        /// What the document lacks.
        error: identity::IdentityError,
    },
    /// Source XML could not be deserialized into the typed model.
    #[error("source deserialization failed: {}", deserialize_problem(.0))]
    Deserialize(#[from] quick_xml::DeError),
    /// The produced model could not be serialized back to XML.
    #[error("target serialization failed: {0}")]
    Serialize(#[from] quick_xml::SeError),
}

/// A deserialization error in the document's terms: serde's ``duplicate field
/// `X` `` is an element the mapping reads once and the document repeats (#50).
fn deserialize_problem(error: &quick_xml::DeError) -> String {
    let repeated = match error {
        quick_xml::DeError::Custom(message) => message
            .strip_prefix("duplicate field `")
            .and_then(|rest| rest.strip_suffix('`')),
        _ => None,
    };
    match repeated {
        Some(element) => format!(
            "element `{element}` occurs more than once, but the mapping reads it as a single \
             element (a document holding several invoices, or a repeated element the mapping \
             does not declare as a collection)"
        ),
        None => error.to_string(),
    }
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

    /// Verifies that `bytes` is a document of `spoke` ([`Spoke::identity`]),
    /// deserializes it and runs its generated reader, producing the typed
    /// canonical hub plus any mapping diagnostics. The identity is checked on
    /// every read, whether `spoke` was auto-detected or chosen by the caller.
    /// A document in another supported encoding than UTF-8 is transcoded
    /// first ([`encoding::to_utf8`]).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Encoding`] if the document's encoding cannot be
    /// decoded, [`EngineError::Identity`] if `bytes` does not have `spoke`'s
    /// identity, and [`EngineError::Deserialize`] if it is not a well-formed
    /// document for `spoke`.
    pub fn to_hub(
        &self,
        spoke: Spoke,
        bytes: &[u8],
    ) -> Result<MappingResult<MainKey>, EngineError> {
        Ok(into_main(self.read(spoke, encoding::to_utf8(bytes)?)?))
    }

    /// [`Engine::to_hub`] taking the document by value: its bytes are freed
    /// as soon as they are parsed, before the reader builds the hub, so the
    /// raw document and the hub are never in memory together.
    ///
    /// # Errors
    ///
    /// As [`Engine::to_hub`].
    pub fn to_hub_owned(
        &self,
        spoke: Spoke,
        bytes: Vec<u8>,
    ) -> Result<MappingResult<MainKey>, EngineError> {
        Ok(into_main(self.read_owned(spoke, bytes)?))
    }

    /// [`Engine::to_hub_owned`] into the packed hub.
    fn read_owned(
        &self,
        spoke: Spoke,
        bytes: Vec<u8>,
    ) -> Result<MappingResult<CompactHub>, EngineError> {
        let utf8 = match encoding::to_utf8(&bytes)? {
            Cow::Owned(transcoded) => {
                drop(bytes);
                transcoded
            }
            Cow::Borrowed(_) => bytes,
        };
        self.read(spoke, Cow::Owned(utf8))
    }

    /// [`Engine::to_hub`] into the packed hub.
    fn read(
        &self,
        spoke: Spoke,
        bytes: Cow<'_, [u8]>,
    ) -> Result<MappingResult<CompactHub>, EngineError> {
        spoke
            .identity()
            .check(&bytes)
            .map_err(|error| EngineError::Identity {
                format: spoke.name(),
                error,
            })?;
        Ok(generated::read(spoke, bytes)?)
    }

    /// Runs `spoke`'s generated writer over `hub` and serializes the result to
    /// XML, carrying through the writer's diagnostics. EN 16931 totals the hub
    /// lacks are first derived by their calculation rules
    /// ([`MainKey::derive_missing`]), each reported as a `VALUE_DERIVED` info
    /// diagnostic; values the hub carries are never replaced. A carried total
    /// that contradicts its rule ([`MainKey::check_derived`]) is reported as
    /// `VALUE_INCONSISTENT`, a warning or an error as the rule's `check` says.
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
        hub: MainKey,
    ) -> Result<MappingResult<String>, EngineError> {
        self.from_hub_with_capacity(spoke, hub, 0)
    }

    /// [`Engine::from_hub`] writing into a buffer reserved for `capacity`
    /// bytes, so a large output is not built by repeated doubling. The hint
    /// only sizes the first allocation; the output may be shorter or longer.
    ///
    /// # Errors
    ///
    /// As [`Engine::from_hub`].
    pub fn from_hub_with_capacity(
        &self,
        spoke: Spoke,
        hub: MainKey,
        capacity: usize,
    ) -> Result<MappingResult<String>, EngineError> {
        self.write(spoke, hub.into(), capacity)
    }

    /// [`Engine::from_hub_with_capacity`] from the packed hub.
    fn write(
        &self,
        spoke: Spoke,
        mut hub: CompactHub,
        capacity: usize,
    ) -> Result<MappingResult<String>, EngineError> {
        // EN 16931 totals the hub lacks are computed by their calculation
        // rules (`config/derivations.toml`) before the writer runs.
        let mut checks: Vec<MappingDiagnostic> = hub
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
        // Totals the hub carries are checked against the same rules, derived
        // operands included: a contradiction is never written silently.
        checks.extend(hub.check_derived().into_iter().map(
            |(label, rule, is_error, carried, computed)| {
                let severity = if is_error {
                    Severity::Error
                } else {
                    Severity::Warning
                };
                let mut d = MappingDiagnostic::new(
                    severity,
                    "VALUE_INCONSISTENT",
                    rule,
                    match computed {
                        Some(computed) => {
                            format!("`{label}` is {carried} but {rule} computes {computed}")
                        }
                        None => format!(
                            "`{label}` is {carried} but {rule} cannot be computed: \
                             the result exceeds the decimal range"
                        ),
                    },
                );
                d.canonical_key = Some(label.to_string());
                d
            },
        ));
        let mut written = generated::write(spoke, hub, capacity)?;
        if !checks.is_empty() {
            written.diagnostics.splice(0..0, checks);
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
        let hint = output_hint(bytes.len());
        self.write_read(to, self.read(from, encoding::to_utf8(bytes)?)?, hint)
    }

    /// [`Engine::transform`] taking the document by value, freed once parsed
    /// ([`Engine::to_hub_owned`]).
    ///
    /// # Errors
    ///
    /// As [`Engine::transform`].
    pub fn transform_owned(
        &self,
        from: Spoke,
        to: Spoke,
        bytes: Vec<u8>,
    ) -> Result<MappingResult<String>, EngineError> {
        let hint = output_hint(bytes.len());
        self.write_read(to, self.read_owned(from, bytes)?, hint)
    }

    /// The write half of a transform: writes the hub `read` produced as `to`,
    /// after the read half's diagnostics.
    fn write_read(
        &self,
        to: Spoke,
        read: MappingResult<CompactHub>,
        hint: usize,
    ) -> Result<MappingResult<String>, EngineError> {
        let Some(hub) = read.value else {
            return Ok(MappingResult::new(None, read.diagnostics));
        };
        let written = self.write(to, hub, hint)?;
        let mut diagnostics = read.diagnostics;
        diagnostics.extend(written.diagnostics);
        Ok(MappingResult::new(written.value, diagnostics))
    }
}

/// A read into the packed hub as the public [`MainKey`].
fn into_main(read: MappingResult<CompactHub>) -> MappingResult<MainKey> {
    MappingResult::new(read.value.map(MainKey::from), read.diagnostics)
}

/// The output buffer to reserve for a document read from `input_len` bytes:
/// 1.5× covers the largest measured growth (Factur-X from UBL, about 1.37×).
// ponytail: one ratio for every route; per-spoke ratios if a route shrinks a lot.
pub(crate) fn output_hint(input_len: usize) -> usize {
    input_len.saturating_add(input_len / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr as _;

    const UBL: &[u8] = br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"><CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
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
    fn test_from_hub_reports_a_carried_total_that_contradicts_its_rule() {
        // The source states an amount due that BR-CO-16 does not give: it is
        // written as carried (never replaced), but not silently. The paid
        // amount is stated (zero): without it, BR-CO-16 solved for the paid
        // amount would derive one that makes any amount due consistent.
        let engine = Engine::new();
        let mut hub = engine
            .to_hub(Spoke::UblInvoice, UBL)
            .unwrap()
            .value
            .unwrap();
        hub.payable_amount = Some(Decimal::from_str("999.99").unwrap());
        hub.paid_amount = Some(Decimal::ZERO);
        let out = engine.from_hub(Spoke::UblInvoice, hub).unwrap();
        let inconsistent: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "VALUE_INCONSISTENT")
            .collect();
        assert_eq!(inconsistent.len(), 1, "{:?}", out.diagnostics);
        let d = inconsistent[0];
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.source_node, "BR-CO-16");
        assert_eq!(d.canonical_key.as_deref(), Some("PayableAmount"));
        assert!(
            d.message.contains("999.99") && d.message.contains("119"),
            "{}",
            d.message
        );
        assert!(!out.has_errors(), "a warning keeps the output");
        assert!(out.value.unwrap().contains(">999.99<"));
    }

    #[test]
    fn test_from_hub_checks_carried_totals_against_derived_operands() {
        // A line sum that is derived from the lines (BR-CO-10) and a total
        // with VAT that is carried must still agree (BR-CO-13, BR-CO-15): the
        // engine never mixes a derived and a carried value silently.
        let engine = Engine::new();
        let mut hub = engine
            .to_hub(Spoke::UblInvoice, UBL)
            .unwrap()
            .value
            .unwrap();
        hub.sum_of_invoice_line_net_amount = None;
        hub.invoice_total_without_vat = None;
        hub.invoice_lines[0].line_net_amount = Some(Decimal::from_str("5000.00").unwrap());
        let out = engine.from_hub(Spoke::UblInvoice, hub).unwrap();
        let rules: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "VALUE_INCONSISTENT")
            .map(|d| d.source_node.as_str())
            .collect();
        assert_eq!(rules, ["BR-CO-15"], "{:?}", out.diagnostics);
    }

    #[test]
    fn test_wrong_amount_due_without_paid_amount_is_reported() {
        // No paid amount: BR-CO-16 solved for it would infer -880.99, which
        // would make the amount due consistent with itself. A negative
        // prepayment is never derived (`skip_negative`), so the amount due
        // is checked against the totals and reported.
        let engine = Engine::new();
        let mut hub = engine
            .to_hub(Spoke::UblInvoice, UBL)
            .unwrap()
            .value
            .unwrap();
        hub.payable_amount = Some(Decimal::from_str("999.99").unwrap());
        hub.paid_amount = None;
        let out = engine.from_hub(Spoke::UblInvoice, hub).unwrap();
        assert!(
            out.diagnostics
                .iter()
                .all(|d| d.canonical_key.as_deref() != Some("PaidAmount")),
            "no paid amount is derived: {:?}",
            out.diagnostics
        );
        let rules: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "VALUE_INCONSISTENT")
            .map(|d| d.source_node.as_str())
            .collect();
        assert_eq!(rules, ["BR-CO-16"], "{:?}", out.diagnostics);
    }

    #[test]
    fn test_totals_beyond_the_decimal_range_are_reported_not_panicking() {
        // Two line amounts at the decimal maximum: their sum overflows. The
        // engine must not panic (decimal `+` would): the sum is not derived,
        // and the carried total cannot be confirmed.
        let engine = Engine::new();
        let mut hub = engine
            .to_hub(Spoke::UblInvoice, UBL)
            .unwrap()
            .value
            .unwrap();
        for line in &mut hub.invoice_lines {
            line.line_net_amount = Some(Decimal::MAX);
        }
        let out = engine.from_hub(Spoke::UblInvoice, hub).unwrap();
        let overflow = out
            .diagnostics
            .iter()
            .find(|d| d.code == "VALUE_INCONSISTENT" && d.source_node == "BR-CO-10")
            .unwrap_or_else(|| panic!("BR-CO-10 reported: {:?}", out.diagnostics));
        assert!(
            overflow.message.contains("decimal range"),
            "{}",
            overflow.message
        );
    }

    #[test]
    fn test_consistent_document_reports_no_inconsistency() {
        let out = Engine::new()
            .transform(Spoke::UblInvoice, Spoke::UblInvoice, UBL)
            .unwrap();
        assert!(
            out.diagnostics
                .iter()
                .all(|d| d.code != "VALUE_INCONSISTENT"),
            "{:?}",
            out.diagnostics
        );
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
        let xml = br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"><CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
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
    fn test_to_hub_reads_a_latin1_document() {
        // A declared ISO-8859-1 document is transcoded before the identity
        // check and the read; `\xE8` is `è` in Latin-1, not valid UTF-8.
        let mut xml = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>".to_vec();
        xml.extend(
            String::from_utf8(UBL.to_vec())
                .unwrap()
                .replace(
                    "<DocumentCurrencyCode>",
                    "<Note>Caff@</Note><DocumentCurrencyCode>",
                )
                .bytes()
                .map(|b| if b == b'@' { 0xE8 } else { b }),
        );
        let engine = Engine::new();
        let borrowed = engine
            .to_hub(Spoke::UblInvoice, &xml)
            .expect("Latin-1 is decoded");
        assert!(!borrowed.has_errors(), "{:?}", borrowed.diagnostics);
        assert_eq!(
            borrowed.value.expect("hub").invoice_note.as_deref(),
            Some("Caff\u{E8}")
        );
        // The owned path reads the same, transcoded or not.
        for doc in [xml, UBL.to_vec()] {
            let owned = engine
                .to_hub_owned(Spoke::UblInvoice, doc.clone())
                .expect("owned");
            let borrowed = engine.to_hub(Spoke::UblInvoice, &doc).expect("borrowed");
            assert_eq!(owned.value, borrowed.value);
        }
    }

    #[test]
    fn test_to_hub_unsupported_encoding_is_an_encoding_error() {
        let mut xml = b"<?xml version=\"1.0\" encoding=\"EBCDIC-US\"?>".to_vec();
        xml.extend_from_slice(UBL);
        assert!(matches!(
            Engine::new().to_hub(Spoke::UblInvoice, &xml),
            Err(EngineError::Encoding(_))
        ));
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
        let xml = br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"><CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
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
        assert!(matches!(
            err,
            EngineError::Identity {
                error: identity::IdentityError::NotXml(_),
                ..
            }
        ));
        // Past an intact identity header, malformed content still fails to
        // deserialize.
        let broken = String::from_utf8(UBL.to_vec())
            .unwrap()
            .replace("</LegalMonetaryTotal>", "</Wrong>");
        let err = engine
            .to_hub(Spoke::UblInvoice, broken.as_bytes())
            .unwrap_err();
        assert!(matches!(err, EngineError::Deserialize(_)), "{err}");
    }

    #[test]
    fn test_explicit_source_format_checks_root_namespace() {
        // The issue's reproduction: correct local name, wrong namespace URI —
        // refused even though the caller named the format explicitly.
        let engine = Engine::new();
        let spoofed = String::from_utf8(UBL.to_vec()).unwrap().replace(
            "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2",
            "urn:example:not-ubl",
        );
        let err = engine
            .to_hub(Spoke::UblInvoice, spoofed.as_bytes())
            .unwrap_err();
        let EngineError::Identity { format, error } = err else {
            panic!("expected an identity error, got {err}");
        };
        assert_eq!(format, Spoke::UblInvoice.name());
        assert!(
            matches!(error, identity::IdentityError::Root { .. }),
            "{error}"
        );
        // A prefix is irrelevant; only the URI it is bound to counts.
        let prefixed = String::from_utf8(UBL.to_vec())
            .unwrap()
            .replacen("<Invoice xmlns=", "<inv:Invoice xmlns:inv=", 1)
            .replace("</Invoice>", "</inv:Invoice>");
        let result = engine
            .to_hub(Spoke::UblInvoice, prefixed.as_bytes())
            .expect("prefixed root in the UBL namespace");
        assert!(!result.has_errors(), "{:?}", result.diagnostics);
    }

    #[test]
    fn test_explicit_source_format_checks_profile_and_version() {
        let engine = Engine::new();
        let ubl = String::from_utf8(UBL.to_vec()).unwrap();
        // Another format's profile is refused by the base UBL spoke.
        let peppol = ubl.replace(
            "urn:cen.eu:en16931:2017<",
            "urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0<",
        );
        let err = engine
            .to_hub(Spoke::UblInvoice, peppol.as_bytes())
            .unwrap_err();
        assert!(
            err.to_string().contains("unsupported profile identifier"),
            "{err}"
        );
        // ... but read by the spoke it names.
        assert!(
            engine
                .to_hub(Spoke::PeppolBisBilling, peppol.as_bytes())
                .is_ok()
        );
        // A missing profile, and an unsupported version, are refused.
        let missing = ubl.replace(
            "<CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>",
            "",
        );
        let err = engine
            .to_hub(Spoke::UblInvoice, missing.as_bytes())
            .unwrap_err();
        assert!(
            err.to_string().contains("declares no profile identifier"),
            "{err}"
        );
        let versioned = |v: &str| {
            ubl.replacen(
                "<CustomizationID>",
                &format!("<UBLVersionID>{v}</UBLVersionID><CustomizationID>"),
                1,
            )
        };
        assert!(
            engine
                .to_hub(Spoke::UblInvoice, versioned("2.1").as_bytes())
                .is_ok()
        );
        let err = engine
            .to_hub(Spoke::UblInvoice, versioned("9.9").as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("unsupported version"), "{err}");
    }

    /// Markup characters in free-text values survive the generated writers
    /// and readers: every sample read, its free-text fields spiced, written
    /// as every target and read back as that target.
    #[test]
    fn test_markup_round_trips_through_every_writer_and_reader() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let engine = Engine::new();
        let raw = "a & b < c > d \"e\" 'f'\r\n\tü";
        let mut pairs = 0;
        for &reader in Spoke::ALL {
            for &file in reader.samples() {
                let bytes = std::fs::read(root.join(file)).expect(file);
                let mut hub = engine
                    .to_hub(reader, &bytes)
                    .expect(file)
                    .value
                    .expect(file);
                hub.invoice_note = Some(raw.into());
                hub.seller_name = Some(raw.into());
                hub.buyer_name = Some(raw.into());
                if let Some(line) = hub.invoice_lines.first_mut() {
                    line.item_name = Some(raw.into());
                }
                for &target in Spoke::ALL {
                    let xml = generated::write(target, hub.clone().into(), 0)
                        .expect(file)
                        .value
                        .expect(file);
                    let back = engine
                        .to_hub(target, xml.as_bytes())
                        .unwrap_or_else(|e| panic!("{file} as {}: {e}", target.name()))
                        .value
                        .expect(file);
                    let line = back.invoice_lines.first().and_then(|l| l.item_name.clone());
                    for value in [back.seller_name, back.buyer_name, line] {
                        assert_eq!(value.as_deref(), Some(raw), "{file} as {}", target.name());
                    }
                    pairs += 1;
                }
            }
        }
        assert!(pairs > 0);
    }

    /// The generated reader on edge cases of XML, each a UBL document
    /// changed in one place. The outcomes are quick-xml's serde
    /// deserializer's, recorded when it was replaced.
    #[test]
    fn test_reader_edge_cases_read_as_serde_read_them() {
        let ubl = std::str::from_utf8(UBL).expect("utf-8");
        let id = "<ID>INV-42</ID>";
        let amount = r#"<TaxAmount currencyID="EUR">19.00</TaxAmount><TaxSubtotal>"#;
        let xsi = r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance""#;
        let ok = r#"Some("INV-42") None []"#;
        let missing = r#"None None ["REQUIRED_MISSING"]"#;
        let empty_root = format!("None None {:?}", ["REQUIRED_MISSING"; 14]);
        let cases: Vec<(&str, String, &str)> = vec![
            ("references", ubl.replace(id, "<ID>A&amp;B &#x41;&#65; &lt;x&gt;</ID>"), r#"Some("A&B AA <x>") None []"#),
            ("CDATA", ubl.replace(id, "<ID>INV<![CDATA[<42>]]> x</ID>"), r#"Some("INV<42> x") None []"#),
            ("comment and PI in text", ubl.replace(id, "<ID>INV<!-- c -->-4<?pi x?>2</ID>"), ok),
            ("whitespace in text", ubl.replace(id, "<ID>\r\n  INV-42 \t</ID>"), ok),
            ("empty element", ubl.replace(id, "<ID/>"), missing),
            ("empty pair", ubl.replace(id, "<ID></ID>"), missing),
            ("nil", ubl.replace(id, &format!(r#"<ID {xsi} xsi:nil="true">x</ID>"#)), missing),
            ("nil optional", ubl.replace(id, &format!(r#"{id}<Note {xsi} xsi:nil="true"/>"#)), ok),
            ("duplicate element", ubl.replace(id, "<ID>A</ID><ID>B</ID>"), "duplicate field `ID`"),
            ("repeat after another element", ubl.replace("<InvoiceLine><ID>2</ID>", "<Note>x</Note><InvoiceLine><ID>2</ID>"), "duplicate field `InvoiceLine`"),
            ("unknown elements", ubl.replace(id, &format!(r#"<Unknown a="1"><Deep>{id}</Deep></Unknown><Other/>{id}"#)), ok),
            ("other prefix", ubl.replace(id, r#"<x:ID xmlns:x="urn:x">INV-42</x:ID>"#), ok),
            ("attribute references", ubl.replacen(amount, r#"<TaxAmount currencyID="E&amp;U&#x52;">19.00</TaxAmount><TaxSubtotal>"#, 1), r#"Some("INV-42") None ["TYPE_INVALID"]"#),
            ("prefixed attribute twice", ubl.replacen(amount, r#"<TaxAmount currencyID="EUR" x:currencyID="USD" xmlns:x="urn:x">19.00</TaxAmount><TaxSubtotal>"#, 1), "duplicate field `@currencyID`"),
            ("prolog", format!("<?xml version=\"1.0\"?>\n<!DOCTYPE Invoice>\n<!-- c -->\n{ubl}"), ok),
            ("BOM", format!("\u{feff}{ubl}"), ok),
            ("CRLF", ubl.replace('\n', "\r\n"), ok),
            ("empty root", "<Invoice/>".to_string(), &empty_root),
            ("element in text", ubl.replace(id, "<ID>INV<b>42</b></ID>"), "cannot deserialize primitive type from mixed content, found unexpected tag <b>"),
            ("unknown entity", ubl.replace(id, "<ID>&foo;</ID>"), "at 0..3: unrecognized entity `foo`"),
            ("truncated", ubl[..ubl.len() / 2].to_string(), "ill-formed document: start tag not closed: `</Invoice>` not found before end of input"),
            ("text before the root", format!("garbage{ubl}"), r#"invalid type: string "garbage", expected struct Invoice"#),
            ("blank before the root", format!("\n \t{ubl}"), ok),
            ("repeat under one prefix", ubl.replace(id, &format!(r#"{id}<x:Note xmlns:x="urn:x">a</x:Note><x:Note xmlns:x="urn:x">b</x:Note>"#)), r#"Some("INV-42") Some("a\nb") []"#),
            ("repeat under another prefix", ubl.replace(id, &format!(r#"{id}<Note>a</Note><x:Note xmlns:x="urn:x">b</x:Note>"#)), "duplicate field `Note`"),
            ("unknown entity in an ignored attribute", ubl.replacen(amount, r#"<TaxAmount currencyID="EUR" x="&foo;">19.00</TaxAmount><TaxSubtotal>"#, 1), ok),
        ];
        for (name, doc, want) in cases {
            assert!(doc != ubl, "edge case `{name}` changed nothing");
            let got = match generated::read(Spoke::UblInvoice, doc.as_bytes().into()) {
                Ok(read) => {
                    let hub = read.value.expect("reader always yields a hub");
                    let codes: Vec<_> = read
                        .diagnostics
                        .iter()
                        .map(|d| d.code.to_string())
                        .collect();
                    format!("{:?} {:?} {codes:?}", hub.invoice_number, hub.invoice_note)
                }
                Err(e) => e.to_string(),
            };
            assert_eq!(got, want, "edge case `{name}`");
        }
    }
}
