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

#[test]
fn test_fatturapa_writes_document_allowances_and_charges_as_numbered_lines() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::Fatturapa, &full_ubl())
        .expect("well-formed source");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("written document");
    println!("PATCH_REACHED");
    std::io::stdout().flush().expect("flush stdout");
    // BG-20 / BG-21 follow the three invoice lines as sconto (SC) and altra
    // spesa accessoria (AC) lines: an allowance negative, numbered on.
    assert!(
        xml.contains("<DettaglioLinee><NumeroLinea>4</NumeroLinea><TipoCessionePrestazione>SC</TipoCessionePrestazione><Descrizione>Framework contract rebate</Descrizione><PrezzoUnitario>-25.50</PrezzoUnitario><PrezzoTotale>-25.50</PrezzoTotale><AliquotaIVA>19.00</AliquotaIVA></DettaglioLinee>"),
        "{xml}"
    );
    assert!(
        xml.contains("<DettaglioLinee><NumeroLinea>5</NumeroLinea><TipoCessionePrestazione>AC</TipoCessionePrestazione><Descrizione>Freight</Descrizione><PrezzoUnitario>30.00</PrezzoUnitario><PrezzoTotale>30.00</PrezzoTotale><AliquotaIVA>19.00</AliquotaIVA></DettaglioLinee>"),
        "{xml}"
    );
    // SdI 00422: the 19 % lines now add up to the summary's taxable amount
    // (147.00 + 510.00 - 25.50 + 30.00).
    assert!(
        xml.contains(
            "<AliquotaIVA>19.00</AliquotaIVA><ImponibileImporto>661.50</ImponibileImporto>"
        ),
        "{xml}"
    );

    // Read back, the SC / AC lines are document allowances and charges again,
    // not invoice lines, and BT-109 counts them.
    let back = engine
        .transform(Spoke::Fatturapa, Spoke::UblInvoice, xml.as_bytes())
        .expect("well-formed output");
    assert!(!back.has_errors(), "{:?}", back.diagnostics);
    let ubl = back.value.expect("written document");
    assert_eq!(ubl.matches("<cac:InvoiceLine>").count(), 3, "{ubl}");
    assert!(
        ubl.contains(r#"<cac:AllowanceCharge><cbc:ChargeIndicator>false</cbc:ChargeIndicator><cbc:AllowanceChargeReason>Framework contract rebate</cbc:AllowanceChargeReason><cbc:Amount currencyID="EUR">25.50</cbc:Amount>"#),
        "{ubl}"
    );
    assert!(
        ubl.contains(r#"<cac:AllowanceCharge><cbc:ChargeIndicator>true</cbc:ChargeIndicator><cbc:AllowanceChargeReason>Freight</cbc:AllowanceChargeReason><cbc:Amount currencyID="EUR">30.00</cbc:Amount>"#),
        "{ubl}"
    );
    assert!(
        ubl.contains(r#"<cbc:TaxExclusiveAmount currencyID="EUR">901.50</cbc:TaxExclusiveAmount>"#),
        "{ubl}"
    );
}

#[test]
fn test_fatturapa_reads_an_allowance_or_charge_line_by_its_total() {
    // A producer may state an AC line with a quantity: its total, which
    // DatiRiepilogo counts, is the charge amount, not the unit price.
    let engine = Engine::new();
    let xml = engine
        .transform(Spoke::UblInvoice, Spoke::Fatturapa, &full_ubl())
        .expect("well-formed source")
        .value
        .expect("written document")
        .replace(
            "<Descrizione>Freight</Descrizione><PrezzoUnitario>30.00</PrezzoUnitario>",
            "<Descrizione>Freight</Descrizione><Quantita>2.00</Quantita><PrezzoUnitario>15.00</PrezzoUnitario>",
        );
    assert!(xml.contains("<Quantita>2.00</Quantita>"), "{xml}");
    let back = engine
        .transform(Spoke::Fatturapa, Spoke::UblInvoice, xml.as_bytes())
        .expect("well-formed document");
    assert!(!back.has_errors(), "{:?}", back.diagnostics);
    let ubl = back.value.expect("written document");
    assert!(
        ubl.contains(r#"<cbc:AllowanceChargeReason>Freight</cbc:AllowanceChargeReason><cbc:Amount currencyID="EUR">30.00</cbc:Amount>"#),
        "{ubl}"
    );
}
