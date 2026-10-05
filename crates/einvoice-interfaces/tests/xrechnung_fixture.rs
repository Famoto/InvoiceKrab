//! Integration test: read the real XRechnung 3.0.2 sample through the UBL spoke.
//!
//! The reference UBL mapping binds to namespace-*local* element names, so the
//! same mapper reads a fully namespaced (`cbc:`/`cac:`) XRechnung document and
//! ignores the many elements the minimal spoke does not model. This pins that
//! end-to-end behaviour against the checked-in fixture in `testfiles/`.

use einvoice_interfaces::{Engine, Spoke};
use quick_xml::Reader;
use quick_xml::events::Event;
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

/// Every start tag's *local* name, in document order.
fn start_tags(xml: &[u8]) -> Vec<String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut tags = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).expect("well-formed") {
            Event::Start(e) | Event::Empty(e) => {
                tags.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Event::Eof => return tags,
            _ => {}
        }
        buf.clear();
    }
}

/// Whether `needle` occurs in `hay` as a (not necessarily contiguous) subsequence.
fn is_subsequence(needle: &[String], hay: &[String]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

#[test]
fn test_transform_emits_elements_in_the_fixtures_schema_order() {
    // The fixture is a real, schema-valid XRechnung, so its element sequence
    // *is* the UBL sequence order. The writer emits a subset of those elements
    // (the mapped ones), nested the same way, so the emitted start tags must
    // read as a subsequence of the fixture's — anything alphabetical
    // (`AccountingCustomerParty` before `CustomizationID`, `TaxTotal` after
    // `LegalMonetaryTotal`) breaks this.
    let input = xrechnung();
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::UblInvoice, &input)
        .expect("fixture is well-formed XML");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("writer yields a document");

    let emitted = start_tags(xml.as_bytes());
    let fixture = start_tags(&input);
    assert!(
        is_subsequence(&emitted, &fixture),
        "emitted element order diverges from the schema order:\n{emitted:?}\nvs fixture\n{fixture:?}"
    );

    // And the top level, spelled out.
    let mut depth = 0usize;
    let mut top_level = Vec::new();
    let mut reader = Reader::from_str(&xml);
    loop {
        match reader.read_event().expect("well-formed") {
            Event::Start(e) => {
                if depth == 1 {
                    top_level.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                }
                depth += 1;
            }
            Event::Empty(e) => {
                if depth == 1 {
                    top_level.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                }
            }
            Event::End(_) => depth -= 1,
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!(
        top_level,
        [
            "CustomizationID",
            "ProfileID",
            "ID",
            "IssueDate",
            "DueDate",
            "InvoiceTypeCode",
            "DocumentCurrencyCode",
            "BuyerReference",
            "AccountingSupplierParty",
            "AccountingCustomerParty",
            "PaymentMeans",
            "TaxTotal",
            "LegalMonetaryTotal",
            "InvoiceLine",
            "InvoiceLine",
        ]
    );
}

#[test]
fn test_cii_dates_round_trip_through_the_format_102_codec() {
    // UBL ISO dates become CII format-102 dates with the wire attribute on the
    // way out, and come back as ISO dates on the way in.
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
    assert!(
        !xml.contains("2026-04-15"),
        "no ISO date leaks into CII: {xml}"
    );

    let back = engine
        .transform(Spoke::FacturxInvoice, Spoke::UblInvoice, xml.as_bytes())
        .expect("emitted Factur-X is well-formed");
    assert!(!back.has_errors(), "{:?}", back.diagnostics);
    let ubl = back.value.expect("document");
    assert!(
        ubl.contains("<cbc:IssueDate>2026-04-15</cbc:IssueDate>"),
        "{ubl}"
    );
    assert!(
        ubl.contains("<cbc:DueDate>2026-05-15</cbc:DueDate>"),
        "{ubl}"
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
    // No PartyTaxScheme/CompanyID anywhere: the constant has nothing to
    // complete, so no TaxScheme (and no PartyTaxScheme) is emitted.
    let engine = Engine::new();
    let doc = br#"<Invoice>
        <ID>INV-1</ID>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <AccountingSupplierParty><Party><PartyLegalEntity><RegistrationName>Seller</RegistrationName></PartyLegalEntity></Party></AccountingSupplierParty>
        <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
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
    assert!(!xml.contains("TaxScheme"), "{xml}");
    assert!(
        !xml.contains("ClassifiedTaxCategory"),
        "no category, no scheme: {xml}"
    );
}

#[test]
fn test_cii_output_pins_vat_type_codes_and_materializes_the_delivery_element() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::FacturxInvoice, &xrechnung())
        .expect("fixture is well-formed XML");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");
    assert!(
        xml.contains("<ram:ApplicableHeaderTradeAgreement>"),
        "{xml}"
    );
    // Mandatory even though the fixture has no delivery data.
    assert!(
        xml.contains("</ram:ApplicableHeaderTradeAgreement><ram:ApplicableHeaderTradeDelivery/><ram:ApplicableHeaderTradeSettlement>"),
        "{xml}"
    );
    // TypeCode precedes CategoryCode in every tax group, as the schema orders it.
    assert_eq!(
        xml.matches("<ram:TypeCode>VAT</ram:TypeCode>").count(),
        3,
        "two line taxes + one header breakdown: {xml}"
    );
    assert!(
        xml.contains("<ram:CalculatedAmount>190.00</ram:CalculatedAmount><ram:TypeCode>VAT</ram:TypeCode><ram:BasisAmount>1000.00</ram:BasisAmount>"),
        "{xml}"
    );
}
