//! Integration tests for the document totals FatturaPA carries differently
//! from EN 16931: the rounding amount (BT-114) is `DatiGeneraliDocumento/
//! Arrotondamento`, and the paid amount (BT-113), which FatturaPA lacks, is
//! derived by BR-CO-16 when the hub is written: from the total with VAT, the
//! rounding and the amount due.

use std::io::Write;
use std::path::PathBuf;

use einvoice_interfaces::{Engine, Spoke};

/// The full EN 16931 UBL sample: a 100.00 prepayment and a 0.01 rounding.
fn full_ubl() -> Vec<u8> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testfiles/en16931-full-ubl.xml");
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn test_fatturapa_rounding_amount_round_trips_and_keeps_the_paid_amount() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::Fatturapa, &full_ubl())
        .expect("well-formed source");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("written document");
    println!("PATCH_REACHED");
    std::io::stdout().flush().expect("flush stdout");
    assert!(
        xml.contains("<ImportoTotaleDocumento>1027.19</ImportoTotaleDocumento><Arrotondamento>0.01</Arrotondamento>"),
        "BT-114 follows BT-112 in DatiGeneraliDocumento: {xml}"
    );

    // Read back, the hub carries the rounding, and the writer derives the paid
    // amount the FatturaPA document leaves out.
    let hub = engine
        .to_hub(Spoke::Fatturapa, xml.as_bytes())
        .expect("well-formed output")
        .value
        .expect("hub");
    assert_eq!(
        hub.rounding_amount.map(|d| d.to_string()).as_deref(),
        Some("0.01")
    );
    let back = engine
        .transform(Spoke::Fatturapa, Spoke::UblInvoice, xml.as_bytes())
        .expect("well-formed output");
    assert!(!back.has_errors(), "{:?}", back.diagnostics);
    let ubl = back.value.expect("written document");
    assert!(
        ubl.contains(r#"<cbc:PrepaidAmount currencyID="EUR">100.00</cbc:PrepaidAmount><cbc:PayableRoundingAmount currencyID="EUR">0.01</cbc:PayableRoundingAmount><cbc:PayableAmount currencyID="EUR">927.20</cbc:PayableAmount>"#),
        "BR-CO-16: 1027.19 + 0.01 - 927.20 is paid, the rounding not taken for a prepayment: {ubl}"
    );
}
