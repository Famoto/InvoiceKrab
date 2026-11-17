//! Integration tests for business groups EN 16931 allows to repeat but that
//! were once mapped as single elements: preceding invoice references (BG-3,
//! UBL `BillingReference`, CII `InvoiceReferencedDocument`, FatturaPA
//! `DatiFattureCollegate`) and item classifications (BT-158, UBL
//! `CommodityClassification`, CII `DesignatedProductClassification`).
//!
//! A document repeating one must be read in full, and every writer must emit
//! every occurrence, in document order.

use std::io::Write;

use einvoice_interfaces::{Engine, Spoke};

/// A bare-name UBL invoice with two preceding invoices and one line item
/// carrying two classifications, with what every writer requires.
const UBL: &[u8] = br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"><CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
  <ID>INV-1</ID>
  <IssueDate>2026-04-15</IssueDate>
  <InvoiceTypeCode>380</InvoiceTypeCode>
  <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
  <BillingReference><InvoiceDocumentReference><ID>PRE-1</ID><IssueDate>2026-01-10</IssueDate></InvoiceDocumentReference></BillingReference>
  <BillingReference><InvoiceDocumentReference><ID>PRE-2</ID></InvoiceDocumentReference></BillingReference>
  <AccountingSupplierParty><Party>
    <EndpointID schemeID="0088">4000001000012</EndpointID>
    <PostalAddress><StreetName>Hauptstr. 1</StreetName><CityName>Berlin</CityName><PostalZone>10115</PostalZone><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress>
    <PartyTaxScheme><CompanyID>DE123456789</CompanyID><TaxScheme><ID>VAT</ID></TaxScheme></PartyTaxScheme>
    <PartyLegalEntity><RegistrationName>Seller</RegistrationName></PartyLegalEntity>
  </Party></AccountingSupplierParty>
  <AccountingCustomerParty><Party><EndpointID schemeID="0088">4000001000029</EndpointID><PostalAddress><StreetName>Hauptstr. 1</StreetName><CityName>Berlin</CityName><PostalZone>10115</PostalZone><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress>
    <PartyLegalEntity><RegistrationName>Buyer</RegistrationName></PartyLegalEntity></Party></AccountingCustomerParty>
  <PaymentMeans><PaymentMeansCode>30</PaymentMeansCode></PaymentMeans>
  <TaxTotal><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxSubtotal><TaxableAmount currencyID="EUR">100.00</TaxableAmount><TaxAmount currencyID="EUR">19.00</TaxAmount>
    <TaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></TaxCategory></TaxSubtotal></TaxTotal>
  <LegalMonetaryTotal><LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount><TaxExclusiveAmount currencyID="EUR">100.00</TaxExclusiveAmount>
    <TaxInclusiveAmount currencyID="EUR">119.00</TaxInclusiveAmount><PayableAmount currencyID="EUR">119.00</PayableAmount></LegalMonetaryTotal>
  <InvoiceLine><ID>1</ID><InvoicedQuantity unitCode="C62">1</InvoicedQuantity><LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount>
    <Item><Name>X</Name>
      <CommodityClassification><ItemClassificationCode listID="STI">1001</ItemClassificationCode></CommodityClassification>
      <CommodityClassification><ItemClassificationCode listID="CV" listVersionID="2008">03222000</ItemClassificationCode></CommodityClassification>
      <ClassifiedTaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></ClassifiedTaxCategory></Item>
    <Price><PriceAmount currencyID="EUR">100.00</PriceAmount></Price></InvoiceLine>
</Invoice>"#;

/// Transforms `bytes` from `from` into `to` and returns the written document.
fn write(from: Spoke, to: Spoke, bytes: &[u8]) -> String {
    let out = Engine::new()
        .transform(from, to, bytes)
        .expect("well-formed source");
    println!("PATCH_REACHED");
    std::io::stdout().flush().expect("flush stdout");
    assert!(!out.has_errors(), "{to:?}: {:?}", out.diagnostics);
    out.value.expect("written document")
}

/// Asserts each of `needles` occurs exactly once in `xml`, in the given order.
fn assert_in_order(xml: &str, needles: &[&str]) {
    let mut last = 0;
    for needle in needles {
        assert_eq!(xml.matches(needle).count(), 1, "{needle} once in: {xml}");
        let at = xml.find(needle).expect("counted above");
        assert!(at >= last, "{needle} out of order in: {xml}");
        last = at;
    }
}

#[test]
fn test_ubl_repeated_billing_references_and_classifications_round_trip() {
    let xml = write(Spoke::UblInvoice, Spoke::UblInvoice, UBL);
    assert_eq!(xml.matches("<cac:BillingReference>").count(), 2, "{xml}");
    assert_in_order(&xml, &["PRE-1", "2026-01-10", "PRE-2"]);
    assert_eq!(
        xml.matches("<cac:CommodityClassification>").count(),
        2,
        "{xml}"
    );
    assert_in_order(
        &xml,
        &[
            r#"<cbc:ItemClassificationCode listID="STI">1001</cbc:ItemClassificationCode>"#,
            r#"<cbc:ItemClassificationCode listID="CV" listVersionID="2008">03222000</cbc:ItemClassificationCode>"#,
        ],
    );
}

#[test]
fn test_cii_writes_and_reads_back_every_repeated_reference_and_classification() {
    let cii = write(Spoke::UblInvoice, Spoke::FacturxInvoice, UBL);
    assert_eq!(
        cii.matches("<ram:InvoiceReferencedDocument>").count(),
        2,
        "{cii}"
    );
    assert_in_order(&cii, &["PRE-1", "20260110", "PRE-2"]);
    assert_eq!(
        cii.matches("<ram:DesignatedProductClassification>").count(),
        2,
        "{cii}"
    );
    assert_in_order(&cii, &[">1001<", ">03222000<"]);

    // And back: the CII reader keeps both, too.
    let ubl = write(Spoke::FacturxInvoice, Spoke::UblInvoice, cii.as_bytes());
    assert_in_order(
        &ubl,
        &["PRE-1", "2026-01-10", "PRE-2", ">1001<", ">03222000<"],
    );
}

#[test]
fn test_fatturapa_writes_every_preceding_invoice() {
    let xml = write(Spoke::UblInvoice, Spoke::Fatturapa, UBL);
    assert_eq!(xml.matches("<DatiFattureCollegate>").count(), 2, "{xml}");
    assert_in_order(&xml, &["PRE-1", "2026-01-10", "PRE-2"]);

    let ubl = write(Spoke::Fatturapa, Spoke::UblInvoice, xml.as_bytes());
    assert_in_order(&ubl, &["PRE-1", "2026-01-10", "PRE-2"]);
}
