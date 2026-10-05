//! Integration tests for `match` selectors and physical-element aliasing: one
//! XML element shared by several logical mapping nodes, told apart by a child
//! value (UBL `AdditionalDocumentReference` / `PartyTaxScheme`, CII
//! `AdditionalReferencedDocument` / `SpecifiedTaxRegistration`).

use einvoice_interfaces::{Engine, Spoke};

/// A bare-name UBL document carrying every aliased element: two invoiced-object
/// references (only the first may be read), one supporting document, and both
/// seller tax schemes.
const UBL: &[u8] = br#"<Invoice>
  <ID>INV-1</ID>
  <IssueDate>2026-04-15</IssueDate>
  <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
  <AdditionalDocumentReference><ID>OBJ-7</ID><DocumentTypeCode>130</DocumentTypeCode></AdditionalDocumentReference>
  <AdditionalDocumentReference><ID>DOC-1</ID><DocumentDescription>Timesheet</DocumentDescription></AdditionalDocumentReference>
  <AdditionalDocumentReference><ID>OBJ-8</ID><DocumentTypeCode>130</DocumentTypeCode></AdditionalDocumentReference>
  <AccountingSupplierParty><Party>
    <PartyTaxScheme><CompanyID>DE123456789</CompanyID><TaxScheme><ID>VAT</ID></TaxScheme></PartyTaxScheme>
    <PartyTaxScheme><CompanyID>201/113/40209</CompanyID><TaxScheme><ID>FC</ID></TaxScheme></PartyTaxScheme>
    <PartyLegalEntity><RegistrationName>Seller</RegistrationName></PartyLegalEntity>
  </Party></AccountingSupplierParty>
  <InvoiceLine><ID>1</ID></InvoiceLine>
</Invoice>"#;

/// One element's occurrences in document order, each flattened to its text.
fn elements(xml: &str, local: &str) -> Vec<String> {
    let open = format!("<cac:{local}>");
    let close = format!("</cac:{local}>");
    xml.split(&open)
        .skip(1)
        .map(|rest| rest.split(&close).next().unwrap_or("").to_string())
        .collect()
}

#[test]
fn test_reader_partitions_aliased_elements_by_selector() {
    let engine = Engine::new();
    let result = engine.to_hub(Spoke::UblInvoice, UBL).expect("well-formed");
    assert!(!result.has_errors(), "{:?}", result.diagnostics);
    let hub = result.value.expect("hub");

    assert_eq!(hub.invoiced_object_identifier.as_deref(), Some("OBJ-7"));
    assert_eq!(
        hub.supporting_documents.len(),
        1,
        "only the selector-less references are supporting documents"
    );
    assert_eq!(
        hub.supporting_documents[0]
            .supporting_document_reference
            .as_deref(),
        Some("DOC-1")
    );
    assert_eq!(hub.seller_vat_identifier.as_deref(), Some("DE123456789"));
    assert_eq!(
        hub.seller_tax_registration_identifier.as_deref(),
        Some("201/113/40209")
    );

    // The second `130` reference is surplus for a single-valued node.
    let overflow: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "MATCH_MULTIPLE")
        .collect();
    assert_eq!(overflow.len(), 1, "{:?}", result.diagnostics);
    assert_eq!(overflow[0].source_node, "Invoice.InvoicedObjectReference");
    assert!(overflow[0].message.contains("1 more"));
}

#[test]
fn test_ubl_writer_merges_logical_nodes_with_their_discriminators() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::UblInvoice, UBL)
        .expect("well-formed");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");

    // Supporting documents (no selector) first, then the object reference with
    // its discriminator written from the selector — nothing else carries one.
    let refs = elements(&xml, "AdditionalDocumentReference");
    assert_eq!(refs.len(), 2, "{xml}");
    assert!(
        refs[0].contains("<cbc:ID>DOC-1</cbc:ID>") && !refs[0].contains("DocumentTypeCode"),
        "{xml}"
    );
    assert!(
        refs[1].contains("<cbc:ID>OBJ-7</cbc:ID><cbc:DocumentTypeCode>130</cbc:DocumentTypeCode>"),
        "{xml}"
    );

    // Both tax schemes, VAT before FC, each with its scheme id written back.
    let schemes = elements(&xml, "PartyTaxScheme");
    assert_eq!(schemes.len(), 2, "{xml}");
    assert!(
        schemes[0].contains("<cbc:CompanyID>DE123456789</cbc:CompanyID><cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme>"),
        "{xml}"
    );
    assert!(
        schemes[1].contains("<cbc:CompanyID>201/113/40209</cbc:CompanyID><cac:TaxScheme><cbc:ID>FC</cbc:ID></cac:TaxScheme>"),
        "{xml}"
    );
}

#[test]
fn test_peppol_restates_only_the_tax_registration_selector() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::PeppolBisBilling, UBL)
        .expect("well-formed");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");
    let schemes = elements(&xml, "PartyTaxScheme");
    assert_eq!(schemes.len(), 2, "{xml}");
    assert!(schemes[0].contains("<cbc:ID>VAT</cbc:ID>"), "{xml}");
    assert!(
        schemes[1].contains("<cbc:CompanyID>201/113/40209</cbc:CompanyID><cac:TaxScheme><cbc:ID>TAX</cbc:ID></cac:TaxScheme>"),
        "Peppol tags BT-32 with TAX: {xml}"
    );

    // Reading Peppol back: `FC` is no longer the registration scheme there.
    let back = engine
        .to_hub(Spoke::PeppolBisBilling, xml.as_bytes())
        .expect("well-formed")
        .value
        .expect("hub");
    assert_eq!(
        back.seller_tax_registration_identifier.as_deref(),
        Some("201/113/40209")
    );
    let fc = engine
        .to_hub(Spoke::PeppolBisBilling, UBL)
        .expect("well-formed");
    assert_eq!(
        fc.value.expect("hub").seller_tax_registration_identifier,
        None,
        "an FC scheme matches no Peppol node and is ignored"
    );
}

#[test]
fn test_cii_writes_and_reads_type_code_and_scheme_id_discriminators() {
    let engine = Engine::new();
    let out = engine
        .transform(Spoke::UblInvoice, Spoke::FacturxInvoice, UBL)
        .expect("well-formed");
    assert!(!out.has_errors(), "{:?}", out.diagnostics);
    let xml = out.value.expect("document");

    // Tax registrations: the attribute is the discriminator.
    assert!(
        xml.contains(r#"<ram:SpecifiedTaxRegistration><ram:ID schemeID="VA">DE123456789</ram:ID></ram:SpecifiedTaxRegistration><ram:SpecifiedTaxRegistration><ram:ID schemeID="FC">201/113/40209</ram:ID></ram:SpecifiedTaxRegistration>"#),
        "{xml}"
    );
    // Referenced documents: supporting documents carry the constant 916, the
    // object reference its selector value, TypeCode in sequence position.
    assert!(
        xml.contains("<ram:AdditionalReferencedDocument><ram:IssuerAssignedID>DOC-1</ram:IssuerAssignedID><ram:TypeCode>916</ram:TypeCode><ram:Name>Timesheet</ram:Name></ram:AdditionalReferencedDocument>"),
        "{xml}"
    );
    assert!(
        xml.contains("<ram:AdditionalReferencedDocument><ram:IssuerAssignedID>OBJ-7</ram:IssuerAssignedID><ram:TypeCode>130</ram:TypeCode></ram:AdditionalReferencedDocument>"),
        "{xml}"
    );

    // A CII document with all three type codes partitions on read; the tender
    // reference lands on the same key UBL fills from OriginatorDocumentReference.
    let cii = br#"<CrossIndustryInvoice>
      <ExchangedDocument><ID>INV-2</ID></ExchangedDocument>
      <SupplyChainTradeTransaction>
        <IncludedSupplyChainTradeLineItem><AssociatedDocumentLineDocument><LineID>1</LineID></AssociatedDocumentLineDocument></IncludedSupplyChainTradeLineItem>
        <ApplicableHeaderTradeAgreement>
          <AdditionalReferencedDocument><IssuerAssignedID>SUP-1</IssuerAssignedID><TypeCode>916</TypeCode></AdditionalReferencedDocument>
          <AdditionalReferencedDocument><IssuerAssignedID>TENDER-9</IssuerAssignedID><TypeCode>50</TypeCode></AdditionalReferencedDocument>
          <AdditionalReferencedDocument><IssuerAssignedID>OBJ-3</IssuerAssignedID><TypeCode>130</TypeCode></AdditionalReferencedDocument>
          <AdditionalReferencedDocument><IssuerAssignedID>SUP-2</IssuerAssignedID></AdditionalReferencedDocument>
        </ApplicableHeaderTradeAgreement>
      </SupplyChainTradeTransaction>
    </CrossIndustryInvoice>"#;
    let result = engine
        .to_hub(Spoke::FacturxInvoice, cii)
        .expect("well-formed");
    assert!(!result.has_errors(), "{:?}", result.diagnostics);
    let hub = result.value.expect("hub");
    assert_eq!(hub.tender_or_lot_reference.as_deref(), Some("TENDER-9"));
    assert_eq!(hub.invoiced_object_identifier.as_deref(), Some("OBJ-3"));
    let supporting: Vec<_> = hub
        .supporting_documents
        .iter()
        .map(|d| d.supporting_document_reference.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        supporting,
        ["SUP-1", "SUP-2"],
        "the selector-less collection takes every unclaimed reference"
    );

    // And UBL gets the tender reference where UBL keeps it.
    let ubl = engine
        .transform(Spoke::FacturxInvoice, Spoke::UblInvoice, cii)
        .expect("well-formed")
        .value
        .expect("document");
    assert!(
        ubl.contains("<cac:OriginatorDocumentReference><cbc:ID>TENDER-9</cbc:ID></cac:OriginatorDocumentReference>"),
        "{ubl}"
    );
}
