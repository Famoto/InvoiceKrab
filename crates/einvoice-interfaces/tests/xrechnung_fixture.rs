//! Integration test: read the real XRechnung 3.0.2 sample through the UBL spoke.
//!
//! The reference UBL mapping binds to namespace-*local* element names, so the
//! same mapper reads a fully namespaced (`cbc:`/`cac:`) XRechnung document and
//! ignores the many elements the minimal spoke does not model. This pins that
//! end-to-end behaviour against the checked-in fixture in `testfiles/`.
//!
//! What the schemas decide (element order, mandatory elements and attributes)
//! and whether every covered value survives a round trip are not asserted
//! here: the mappings declare the fixture as a sample, and
//! `tests/xsd_validation.rs` derives those checks for every spoke. What stays
//! are the values the XSDs leave open: the codes, wire attributes and
//! currencies the mappings pin.

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
    // EN 16931 scheme and unit attributes are carried (BT-34-1, BT-130).
    assert_eq!(hub.seller_electronic_address_scheme.as_deref(), Some("EM"));

    assert_eq!(hub.invoice_lines.len(), 2);
    assert_eq!(hub.invoice_lines[0].line_id.as_deref(), Some("1"));
    assert_eq!(
        hub.invoice_lines[0].quantity_unit_code.as_deref(),
        Some("HUR")
    );
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
fn test_cii_dates_are_written_in_the_format_102_wire_form() {
    // UBL ISO dates become CII format-102 dates, wire attribute included. The
    // XSD only requires *a* `format`; the codec pins its value and lexical
    // form. (Reading them back as ISO dates is the derived round trip.)
    let engine = Engine::new();
    let facturx = engine
        .transform(Spoke::UblInvoice, Spoke::FacturxInvoice, &xrechnung())
        .expect("fixture is well-formed XML");
    assert!(!facturx.has_errors(), "{:?}", facturx.diagnostics);
    let xml = facturx.value.expect("writer yields a document");
    assert!(
        xml.contains(r#"<udt:DateTimeString format="102">20260415</udt:DateTimeString>"#),
        "{xml}"
    );
    assert!(
        xml.contains(r#"<udt:DateTimeString format="102">20260515</udt:DateTimeString>"#),
        "due date: {xml}"
    );
}

#[test]
fn test_cii_date_codec_diagnostics_on_read() {
    let engine = Engine::new();
    // A date not in the codec's lexical form is a CODEC_INVALID error; a wire
    // attribute that disagrees with the codec is a CODEC_WIRE_MISMATCH warning.
    let doc = br#"<CrossIndustryInvoice>
        <ExchangedDocument>
            <ID>INV-1</ID>
            <IssueDateTime><DateTimeString format="610">2026-04-15</DateTimeString></IssueDateTime>
        </ExchangedDocument>
        <SupplyChainTradeTransaction>
            <IncludedSupplyChainTradeLineItem><AssociatedDocumentLineDocument><LineID>1</LineID></AssociatedDocumentLineDocument></IncludedSupplyChainTradeLineItem>
            <ApplicableHeaderTradeSettlement>
                <SpecifiedTradePaymentTerms><DueDateDateTime><DateTimeString format="102">20260515</DateTimeString></DueDateDateTime></SpecifiedTradePaymentTerms>
            </ApplicableHeaderTradeSettlement>
        </SupplyChainTradeTransaction>
    </CrossIndustryInvoice>"#;
    let result = engine
        .to_hub(Spoke::FacturxInvoice, doc)
        .expect("well-formed");
    let hub = result.value.expect("reader always yields a hub");
    assert_eq!(hub.issue_date, None, "undecodable date is not assigned");
    assert_eq!(
        hub.due_date.as_deref(),
        Some("2026-05-15"),
        "format 102 decodes"
    );
    assert!(
        result.diagnostics.iter().any(|d| {
            d.code == "CODEC_INVALID"
                && d.source_node
                    == "CrossIndustryInvoice.ExchangedDocument.IssueDateTime.DateTimeString"
        }),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| { d.code == "CODEC_WIRE_MISMATCH" && d.message.contains("610") }),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn test_ubl_amounts_carry_the_document_currency_and_tax_schemes_are_pinned() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::UblInvoice, &xrechnung())
        .expect("fixture is well-formed XML");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("writer yields a document");

    // Every emitted amount carries the document currency, derived through
    // `$root.DocumentCurrency` inside the lines and tax subtotals.
    for amount in [
        "<cbc:LineExtensionAmount currencyID=\"EUR\">1000.00</cbc:LineExtensionAmount>",
        "<cbc:TaxExclusiveAmount currencyID=\"EUR\">1000.00</cbc:TaxExclusiveAmount>",
        "<cbc:TaxableAmount currencyID=\"EUR\">1000.00</cbc:TaxableAmount>",
        "<cbc:PriceAmount currencyID=\"EUR\">80.00</cbc:PriceAmount>",
        "<cbc:LineExtensionAmount currencyID=\"EUR\">800.00</cbc:LineExtensionAmount>",
    ] {
        assert!(xml.contains(amount), "{amount}\n{xml}");
    }
    // Amounts the source does not carry are not conjured up by their currency.
    assert!(!xml.contains("AllowanceTotalAmount"), "{xml}");
    assert!(!xml.contains("PrepaidAmount"), "{xml}");

    // The mandatory TaxScheme/ID completes every tax scheme and category…
    assert_eq!(
        xml.matches("<cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme>")
            .count(),
        5,
        "seller + buyer PartyTaxScheme, one TaxSubtotal category, two line categories: {xml}"
    );
}

#[test]
fn test_tax_scheme_constant_is_not_written_without_its_owner() {
    // No PartyTaxScheme/CompanyID anywhere: the party's constant has nothing
    // to complete, so no PartyTaxScheme is emitted — while the VAT categories,
    // which have content, each get their TaxScheme.
    let engine = Engine::new();
    let doc = br#"<Invoice>
        <ID>INV-1</ID>
        <IssueDate>2026-04-15</IssueDate>
        <InvoiceTypeCode>380</InvoiceTypeCode>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <AccountingSupplierParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress>
          <PartyLegalEntity><RegistrationName>Seller</RegistrationName></PartyLegalEntity></Party></AccountingSupplierParty>
  <AccountingCustomerParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress>
          <PartyLegalEntity><RegistrationName>Buyer</RegistrationName></PartyLegalEntity></Party></AccountingCustomerParty>
        <TaxTotal><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxSubtotal><TaxableAmount currencyID="EUR">100.00</TaxableAmount><TaxAmount currencyID="EUR">19.00</TaxAmount>
          <TaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></TaxCategory></TaxSubtotal></TaxTotal>
        <LegalMonetaryTotal><LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount><TaxExclusiveAmount currencyID="EUR">100.00</TaxExclusiveAmount>
          <TaxInclusiveAmount currencyID="EUR">119.00</TaxInclusiveAmount><PayableAmount currencyID="EUR">119.00</PayableAmount></LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity unitCode="C62">1</InvoicedQuantity><LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount>
          <Item><Name>X</Name><ClassifiedTaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></ClassifiedTaxCategory></Item>
          <Price><PriceAmount currencyID="EUR">100.00</PriceAmount></Price></InvoiceLine>
    </Invoice>"#;
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::UblInvoice, doc)
        .expect("well-formed");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");
    assert!(
        xml.contains("<cbc:RegistrationName>Seller</cbc:RegistrationName>"),
        "{xml}"
    );
    assert!(!xml.contains("PartyTaxScheme"), "{xml}");
    assert_eq!(
        xml.matches("<cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme>")
            .count(),
        2,
        "only the subtotal and the line category carry a scheme: {xml}"
    );
}

#[test]
fn test_cii_output_pins_the_vat_type_code() {
    // The XSD requires a TypeCode in every tax group but leaves its value
    // open; the mapping pins `VAT`.
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::FacturxInvoice, &xrechnung())
        .expect("fixture is well-formed XML");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");
    assert_eq!(
        xml.matches("<ram:TypeCode>VAT</ram:TypeCode>").count(),
        3,
        "two line taxes + one header breakdown: {xml}"
    );
}
