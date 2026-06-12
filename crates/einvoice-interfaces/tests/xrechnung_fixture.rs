//! Integration test: read the real XRechnung 3.0.2 sample through the UBL spoke.
//!
//! The reference UBL mapping binds to namespace-*local* element names, so the
//! same mapper reads a fully namespaced (`cbc:`/`cac:`) XRechnung document and
//! ignores the many elements the minimal spoke does not model. This pins that
//! end-to-end behaviour against the checked-in fixture in `testfiles/`.

use einvoice_interfaces::{Engine, Spoke};
use rust_decimal::Decimal;
use std::str::FromStr as _;

/// The checked-in XRechnung 3.0.2 sample, relative to this crate's manifest.
fn xrechnung() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testfiles/xrechnung-3.0.2-beispiel.xml"
    );
    std::fs::read(path).expect("read XRechnung fixture")
}

#[test]
fn test_to_hub_reads_namespaced_xrechnung() {
    let engine = Engine::new();
    let result = engine
        .to_hub(Spoke::UblInvoice, &xrechnung())
        .expect("fixture is well-formed XML");

    assert!(!result.has_errors(), "{:?}", result.diagnostics);
    let hub = result.value.expect("reader always yields a hub");

    assert_eq!(hub.invoice_number.as_deref(), Some("RE-2026-04-0042"));
    assert_eq!(hub.issue_date.as_deref(), Some("2026-04-15"));
    assert_eq!(hub.document_currency.as_deref(), Some("EUR"));
    assert_eq!(
        hub.payable_amount,
        Some(Decimal::from_str("1190.00").unwrap())
    );
    assert_eq!(hub.payable_amount_currency.as_deref(), Some("EUR"));

    assert_eq!(hub.invoice_lines.len(), 2);
    assert_eq!(hub.invoice_lines[0].line_id.as_deref(), Some("1"));
    assert_eq!(
        hub.invoice_lines[0].item_name.as_deref(),
        Some("Beratungsleistung — Senior Consultant")
    );
    assert_eq!(
        hub.invoice_lines[1].item_name.as_deref(),
        Some("Schulungs-Workshop (Pauschale)")
    );
}

#[test]
fn test_transform_xrechnung_to_ubl_preserves_canonical_values() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::UblInvoice, &xrechnung())
        .expect("fixture is well-formed XML");

    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("writer yields a document");

    // The emitted UBL re-parses and preserves the canonical values.
    let again = engine
        .to_hub(Spoke::UblInvoice, xml.as_bytes())
        .expect("re-parse emitted UBL")
        .value
        .expect("hub");
    assert_eq!(again.invoice_number.as_deref(), Some("RE-2026-04-0042"));
    assert_eq!(again.document_currency.as_deref(), Some("EUR"));
    assert_eq!(
        again.payable_amount,
        Some(Decimal::from_str("1190.00").unwrap())
    );
    assert_eq!(again.invoice_lines.len(), 2);
}
