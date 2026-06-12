//! Format resolution and source auto-detection.
//!
//! [`resolve_spoke`] maps a human-typed format name to a [`Spoke`]; when
//! `--from` is omitted, [`detect_source`] identifies the source format from the
//! document's own signature (root element, then `CustomizationID`) using only
//! the compile-time spoke registry.

use super::CliError;
use crate::Spoke;

/// Resolves a human-typed format name to a [`Spoke`], case-insensitively.
///
/// Matches either the full display name (`ubl-invoice:2.1`) or the bare
/// `doc_format` prefix before the version colon (`ubl-invoice`).
///
/// # Errors
///
/// Returns [`CliError::UnknownFormat`] (listing the known names) when `name`
/// matches no spoke.
pub fn resolve_spoke(name: &str) -> Result<Spoke, CliError> {
    let matches = |full: &str| {
        full.eq_ignore_ascii_case(name)
            || full
                .split_once(':')
                .is_some_and(|(prefix, _)| prefix.eq_ignore_ascii_case(name))
    };
    Spoke::ALL
        .iter()
        .copied()
        .find(|s| matches(s.name()))
        .ok_or_else(|| {
            CliError::UnknownFormat(format!(
                "{name:?} (known formats: {})",
                Spoke::ALL
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Detects the source spoke of `bytes` from the compile-time spoke registry.
///
/// Identification is by *signature*, not by trial-parsing: it reads the
/// document's root XML element once and narrows to the spokes whose registered
/// [`Spoke::root`] (from each mapping's `[meta].root`) matches it. When several
/// formats share that root (e.g. UBL, Peppol, and XRechnung are all rooted at
/// `Invoice`), it disambiguates by *specificity*: a spoke whose
/// [`Spoke::detect_markers`] appear in the document's `CustomizationID` (EN16931
/// BT-24 — the field where an invoice declares the specification/CIUS it
/// follows) wins over a base format that declares none. Both the roots and the
/// markers come from the generated registry, so nothing about the formats is
/// hardcoded here.
///
/// # Errors
///
/// Returns [`CliError::AmbiguousSource`] when the document has no recognized
/// root, or when detection cannot single one out; the caller should then pass
/// `--from`.
pub fn detect_source(bytes: &[u8]) -> Result<Spoke, CliError> {
    let Some(root) = root_element(bytes) else {
        return Err(CliError::AmbiguousSource(
            "could not detect the source format; pass --from <FORMAT>".into(),
        ));
    };

    // Narrow to the spokes whose registered root element matches the document's.
    let candidates: Vec<Spoke> = Spoke::ALL
        .iter()
        .copied()
        .filter(|s| s.root() == root)
        .collect();

    if let [only] = candidates.as_slice() {
        return Ok(*only);
    }
    if candidates.is_empty() {
        return Err(CliError::AmbiguousSource(format!(
            "could not detect the source format for root <{root}>; pass --from <FORMAT>"
        )));
    }

    // Several formats share this root: disambiguate by self-identification —
    // prefer spokes whose declared markers appear in the document's
    // CustomizationID; otherwise fall back to the base spokes that declare none.
    let customization = customization_id(bytes)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let marks = |spoke: &Spoke| {
        spoke
            .detect_markers()
            .iter()
            .any(|m| customization.contains(&m.to_ascii_lowercase()))
    };

    let specific: Vec<Spoke> = candidates.iter().copied().filter(marks).collect();
    let chosen: Vec<Spoke> = if specific.is_empty() {
        candidates
            .iter()
            .copied()
            .filter(|s| s.detect_markers().is_empty())
            .collect()
    } else {
        specific
    };

    match chosen.as_slice() {
        [only] => Ok(*only),
        _ => Err(CliError::AmbiguousSource(format!(
            "source format is ambiguous ({}); pass --from <FORMAT>",
            candidates
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Extracts the text of the document's `CustomizationID` element, if present.
///
/// Matches the element by its XML *local* name, so it works regardless of
/// namespace prefix (`cbc:CustomizationID`, `CustomizationID`, ...). Returns the
/// trimmed, unescaped text of the first such element, or `None` when the document
/// has none or cannot be scanned. This is the EN16931 BT-24 field used by
/// [`detect_source`] to recognize a format.
fn customization_id(bytes: &[u8]) -> Option<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut in_customization = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                in_customization = e.local_name().as_ref() == b"CustomizationID";
            }
            Ok(Event::Text(t)) if in_customization => {
                let text = t.unescape().ok()?.trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
            Ok(Event::End(_)) => in_customization = false,
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Extracts the *local* name of the document's root element, if any.
///
/// Reads only up to the first start (or empty) tag and returns its name with any
/// namespace prefix stripped (like [`customization_id`]), so `<rsm:Invoice>` and
/// `<Invoice>` both yield `Invoice`. This is the document's primary signature:
/// [`detect_source`] matches it against each spoke's [`Spoke::root`] from the
/// compile-time registry. Returns `None` when the bytes hold no element or cannot
/// be scanned.
fn root_element(bytes: &[u8]) -> Option<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return std::str::from_utf8(e.local_name().as_ref())
                    .ok()
                    .map(str::to_string);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A bare-name UBL invoice: valid under both spokes. Its CustomizationID is
    /// the plain EN16931 id, so it carries no XRechnung marker.
    const UBL: &[u8] = br#"<Invoice>
        <CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
        <ID>INV-1</ID>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
    </Invoice>"#;

    /// Same document carrying the XRechnung CustomizationID marker, plus the
    /// `TaxTotal` element the XRechnung source model requires to be present.
    const XRECHNUNG: &[u8] = br#"<Invoice>
        <CustomizationID>urn:cen.eu:en16931:2017#compliant#urn:xoev-de:kosit:standard:xrechnung_3.0</CustomizationID>
        <ID>INV-1</ID>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
        <TaxTotal><TaxAmount currencyID="EUR">0.00</TaxAmount></TaxTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
    </Invoice>"#;

    fn spoke_named(name: &str) -> Spoke {
        resolve_spoke(name).expect("bundled spoke")
    }

    #[test]
    fn test_detect_source_prefers_marker_match() {
        // The XRechnung marker is present → the CIUS wins over base UBL even
        // though both spokes parse the document cleanly.
        let detected = detect_source(XRECHNUNG).expect("detected");
        assert_eq!(detected, spoke_named("xrechnung-invoice"));
    }

    #[test]
    fn test_detect_source_falls_back_to_base_when_no_marker() {
        // No XRechnung marker → the markerless base format (UBL) is chosen
        // rather than reporting ambiguity.
        let detected = detect_source(UBL).expect("detected");
        assert_eq!(detected, spoke_named("ubl-invoice"));
    }

    #[test]
    fn test_detect_markers_only_on_declaring_spoke() {
        // Exactly the spokes that declare `[meta].detect` expose markers.
        assert_eq!(spoke_named("ubl-invoice").detect_markers(), &[] as &[&str]);
        assert!(
            spoke_named("xrechnung-invoice")
                .detect_markers()
                .contains(&"xrechnung")
        );
    }

    #[test]
    fn test_customization_id_extracts_prefixed_and_bare() {
        // Prefix-agnostic: matches by local name.
        let prefixed = br#"<Invoice xmlns:cbc="x"><cbc:CustomizationID> urn:xrechnung_3.0 </cbc:CustomizationID></Invoice>"#;
        assert_eq!(
            customization_id(prefixed).as_deref(),
            Some("urn:xrechnung_3.0")
        );
        assert_eq!(
            customization_id(UBL).as_deref(),
            Some("urn:cen.eu:en16931:2017")
        );
    }

    #[test]
    fn test_customization_id_absent_is_none() {
        assert_eq!(customization_id(b"<Invoice><ID>1</ID></Invoice>"), None);
        assert_eq!(customization_id(b"not xml <<<"), None);
    }

    #[test]
    fn test_detect_source_ignores_marker_outside_customization_id() {
        // The word "xrechnung" appears only in other elements, not in the
        // CustomizationID — it must NOT trip detection toward XRechnung.
        let doc = br#"<Invoice>
            <CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
            <Note>generated by xrechnung-exporter</Note>
            <ID>INV-1</ID>
            <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
            <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
            <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
        </Invoice>"#;
        let detected = detect_source(doc).expect("detected");
        assert_eq!(detected, spoke_named("ubl-invoice"));
    }

    #[test]
    fn test_spoke_root_from_compiletime_registry() {
        // The generated registry carries each spoke's root XML element, taken
        // from `[meta].root`; no document parsing is needed to know it.
        assert_eq!(spoke_named("ubl-invoice").root(), "Invoice");
        assert_eq!(spoke_named("fatturapa").root(), "FatturaElettronica");
        // `cii-invoice` is inherit-only (`disabled`); Factur-X inherits its tree
        // and is the emitted spoke carrying the `CrossIndustryInvoice` root.
        assert_eq!(
            spoke_named("facturx-invoice").root(),
            "CrossIndustryInvoice"
        );
    }

    #[test]
    fn test_detect_source_by_distinct_root() {
        // FatturaPA declares no CustomizationID markers; its unique root element
        // identifies it through the registry, even on a skeleton document that
        // would not read cleanly under the old trial-parse detection.
        let detected =
            detect_source(b"<FatturaElettronica></FatturaElettronica>").expect("detected");
        assert_eq!(detected, spoke_named("fatturapa"));
    }

    #[test]
    fn test_detect_source_unknown_root_is_ambiguous() {
        // A root element no spoke registers cannot be identified.
        let err = detect_source(b"<Unknown/>").unwrap_err();
        assert!(matches!(err, CliError::AmbiguousSource(_)));
    }

    #[test]
    fn test_resolve_spoke_known_name() {
        // Every bundled spoke must resolve from its own display name.
        for spoke in Spoke::ALL {
            assert_eq!(resolve_spoke(spoke.name()).expect("known"), *spoke);
        }
    }

    #[test]
    fn test_resolve_spoke_is_case_insensitive() {
        let name = Spoke::ALL[0].name().to_uppercase();
        assert_eq!(resolve_spoke(&name).expect("known"), Spoke::ALL[0]);
    }

    #[test]
    fn test_resolve_spoke_accepts_bare_doc_format_prefix() {
        // The display name carries a version (e.g. `ubl-invoice:2.1`); the bare
        // `ubl-invoice` prefix must resolve to the same spoke.
        for spoke in Spoke::ALL {
            if let Some((prefix, _)) = spoke.name().split_once(':') {
                assert_eq!(resolve_spoke(prefix).expect("prefix resolves"), *spoke);
            }
        }
    }

    #[test]
    fn test_resolve_spoke_unknown_name_errors() {
        let err = resolve_spoke("totally-made-up").expect_err("unknown");
        assert!(matches!(err, CliError::UnknownFormat(_)));
        assert_eq!(err.exit_code(), 64);
    }
}
