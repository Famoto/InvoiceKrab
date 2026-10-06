//! Format resolution and source auto-detection.
//!
//! [`resolve_spoke`] maps a human-typed format name to a [`Spoke`]; when
//! `--from` is omitted, [`detect_source`] identifies the source format as the
//! one spoke whose [`Spoke::identity`] the document has — the same check every
//! read performs — using only the compile-time spoke registry.

use super::CliError;
use crate::Spoke;
use crate::identity::IdentityError;

/// Resolves a human-typed format name to a [`Spoke`], case-insensitively.
///
/// Matches either the full display name (`ubl-invoice:2.1`) or the bare
/// `doc_format` prefix before the version colon (`ubl-invoice`). A bare
/// prefix shared by several compiled versions of a format is refused rather
/// than guessed: which version an invoice is written as must not change when
/// a mapping for a newer one is added.
///
/// # Errors
///
/// Returns [`CliError::UnknownFormat`] (listing the known names) when `name`
/// matches no spoke, and [`CliError::Usage`] (listing the versions) when it is
/// a bare prefix of several.
pub fn resolve_spoke(name: &str) -> Result<Spoke, CliError> {
    let names: Vec<&str> = Spoke::ALL.iter().map(|s| s.name()).collect();
    resolve_name(name, &names).map(|index| Spoke::ALL[index])
}

/// The index in `names` of the display name `name` selects (see
/// [`resolve_spoke`]).
///
/// Every display name `name` matches counts — case-insensitively, in full or
/// as the bare prefix before the version colon — so a name is refused as
/// ambiguous whenever it could mean two spokes: two versions sharing a
/// prefix, a mapping whose display name *is* that bare prefix next to a
/// versioned one, or two display names differing only in case.
fn resolve_name(name: &str, names: &[&str]) -> Result<usize, CliError> {
    let versions: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, full)| {
            full.eq_ignore_ascii_case(name)
                || full
                    .split_once(':')
                    .is_some_and(|(prefix, _)| prefix.eq_ignore_ascii_case(name))
        })
        .map(|(index, _)| index)
        .collect();
    match versions.as_slice() {
        [only] => Ok(*only),
        [] => Err(CliError::UnknownFormat(format!(
            "{name:?} (known formats: {})",
            names.join(", ")
        ))),
        many => Err(CliError::Usage(format!(
            "format {name:?} names several formats; name one: {}",
            many.iter()
                .map(|&index| names[index])
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Detects the source spoke of `bytes` from the compile-time spoke registry.
///
/// Identification is by *identity*, not by trial-parsing or heuristics: a
/// spoke matches when the document's root element has its namespace URI and
/// local name and the document declares one of its exact profile identifiers,
/// a supported version and its mandatory identity attributes
/// ([`Identity::check`](crate::identity::Identity::check)) — the check
/// [`Engine::to_hub`](crate::Engine::to_hub) repeats on every read. The build
/// guarantees that spokes sharing a root accept disjoint profiles (`E121`), so
/// at most one spoke matches.
///
/// # Errors
///
/// Returns [`CliError::AmbiguousSource`] when no spoke (or, against the build
/// guarantee, more than one) matches; it names why the spokes sharing the
/// document's root refused it.
pub fn detect_source(bytes: &[u8]) -> Result<Spoke, CliError> {
    // Identities are matched on UTF-8. An undecodable encoding is left as is:
    // detection still works on ASCII-compatible bytes, and the read that
    // follows reports the encoding error itself.
    let decoded = crate::encoding::to_utf8(bytes);
    let bytes = decoded.as_deref().unwrap_or(bytes);
    let checked: Vec<(Spoke, Result<(), IdentityError>)> = Spoke::ALL
        .iter()
        .map(|&s| (s, s.identity().check(bytes)))
        .collect();

    let matching: Vec<Spoke> = checked
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(s, _)| *s)
        .collect();
    match matching.as_slice() {
        [only] => return Ok(*only),
        [] => {}
        many => {
            return Err(CliError::AmbiguousSource(format!(
                "source format is ambiguous ({}); pass --from <FORMAT>",
                many.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
            )));
        }
    }

    // Nothing matched: explain through the spokes that share the document's
    // root (they refused its profile, version or attributes), or else name the
    // root no spoke reads.
    let mut refusals = Vec::new();
    let mut found_root = None;
    for (spoke, result) in &checked {
        match result {
            Err(IdentityError::Root { found, .. }) => found_root = Some(found.clone()),
            Err(IdentityError::NotXml(why)) => {
                return Err(CliError::AmbiguousSource(format!(
                    "could not detect the source format: not an XML document ({why})"
                )));
            }
            Err(e) => refusals.push(format!("{}: {e}", spoke.name())),
            Ok(()) => {}
        }
    }
    Err(CliError::AmbiguousSource(if refusals.is_empty() {
        format!(
            "could not detect the source format: no supported format has the root element {}",
            found_root.unwrap_or_default()
        )
    } else {
        format!(
            "could not detect the source format: the document has no supported identity ({})",
            refusals.join("; ")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const UBL_NS: &str = "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2";
    const EN16931: &str = "urn:cen.eu:en16931:2017";
    const XRECHNUNG_ID: &str =
        "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0";

    /// A UBL invoice header in namespace `ns` declaring `customization`.
    fn ubl(ns: &str, customization: &str) -> Vec<u8> {
        format!(
            r#"<Invoice xmlns="{ns}" xmlns:cbc="urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2">
                <cbc:CustomizationID>{customization}</cbc:CustomizationID>
                <cbc:ID>INV-1</cbc:ID>
            </Invoice>"#
        )
        .into_bytes()
    }

    fn spoke_named(name: &str) -> Spoke {
        resolve_spoke(name).expect("bundled spoke")
    }

    #[test]
    fn test_detect_source_by_exact_profile() {
        assert_eq!(
            detect_source(&ubl(UBL_NS, EN16931)).expect("detected"),
            spoke_named("ubl-invoice")
        );
        assert_eq!(
            detect_source(&ubl(UBL_NS, XRECHNUNG_ID)).expect("detected"),
            spoke_named("xrechnung-invoice")
        );
        assert_eq!(
            detect_source(&ubl(
                UBL_NS,
                "urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0"
            ))
            .expect("detected"),
            spoke_named("peppol-bis-billing")
        );
    }

    #[test]
    fn test_detect_source_rejects_substring_markers() {
        // The old heuristic took any CustomizationID *containing* `xrechnung`
        // (or `peppol`); an identifier that merely mentions one is no
        // supported profile, so nothing is detected.
        for spoofed in [
            "urn:evil:xrechnung",
            "URN:CEN.EU:EN16931:2017#COMPLIANT#URN:XEINKAUF.DE:KOSIT:XRECHNUNG_3.0",
            "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0#extra",
            "urn:cen.eu:en16931:2017-peppol",
        ] {
            let err = detect_source(&ubl(UBL_NS, spoofed)).unwrap_err();
            assert!(matches!(err, CliError::AmbiguousSource(_)), "{spoofed}");
            assert!(err.to_string().contains("unsupported profile"), "{err}");
        }
    }

    #[test]
    fn test_detect_source_rejects_wrong_or_missing_root_namespace() {
        // Correct local name and profile, but not the UBL Invoice namespace.
        for ns in [
            "urn:evil",
            "urn:oasis:names:specification:ubl:schema:xsd:CreditNote-2",
        ] {
            let err = detect_source(&ubl(ns, EN16931)).unwrap_err();
            assert!(
                err.to_string().contains("no supported format has the root"),
                "{err}"
            );
        }
        let bare = format!("<Invoice><CustomizationID>{EN16931}</CustomizationID></Invoice>");
        assert!(detect_source(bare.as_bytes()).is_err());
    }

    #[test]
    fn test_detect_source_ignores_identifier_outside_the_profile_path() {
        // A CustomizationID nested elsewhere is not the document's BT-24.
        let doc = format!(
            r#"<Invoice xmlns="{UBL_NS}"><Note><CustomizationID>{EN16931}</CustomizationID></Note></Invoice>"#
        );
        let err = detect_source(doc.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("declares no profile identifier"),
            "{err}"
        );
    }

    #[test]
    fn test_detect_source_fatturapa_by_root_and_versione() {
        let doc = |versione: &str| {
            format!(
                r#"<p:FatturaElettronica xmlns:p="http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2" {versione}/>"#
            )
        };
        assert_eq!(
            detect_source(doc(r#"versione="FPR12""#).as_bytes()).expect("detected"),
            spoke_named("fatturapa")
        );
        for bad in ["", r#"versione="FPX99""#] {
            let err = detect_source(doc(bad).as_bytes()).unwrap_err();
            assert!(err.to_string().contains("versione"), "{err}");
        }
    }

    #[test]
    fn test_spoke_identity_from_compiletime_registry() {
        let ubl = spoke_named("ubl-invoice").identity();
        assert_eq!(ubl.namespace, Some(UBL_NS));
        assert_eq!(ubl.root, "Invoice");
        assert_eq!(ubl.profile, Some("CustomizationID"));
        assert_eq!(ubl.profiles, &[EN16931]);
        assert_eq!(spoke_named("fatturapa").root(), "FatturaElettronica");
        // `cii-invoice` is inherit-only (`disabled`); Factur-X inherits its tree
        // and its root namespace.
        let facturx = spoke_named("facturx-invoice").identity();
        assert_eq!(facturx.root, "CrossIndustryInvoice");
        assert_eq!(
            facturx.namespace,
            Some("urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100")
        );
    }

    #[test]
    fn test_detect_source_unknown_root_or_not_xml_is_ambiguous() {
        for doc in [&b"<Unknown/>"[..], b"not xml <<<", b""] {
            let err = detect_source(doc).unwrap_err();
            assert!(matches!(err, CliError::AmbiguousSource(_)));
        }
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

    const VERSIONS: [&str; 3] = [
        "xrechnung-invoice:3.0.2",
        "xrechnung-invoice:3.1",
        "ubl-invoice:2.1",
    ];

    #[test]
    fn test_resolve_name_full_name_selects_one_of_several_versions() {
        assert_eq!(
            resolve_name("XRECHNUNG-invoice:3.1", &VERSIONS).expect("full"),
            1
        );
        assert_eq!(
            resolve_name("ubl-invoice", &VERSIONS).expect("one version"),
            2
        );
    }

    #[test]
    fn test_resolve_name_bare_prefix_of_several_versions_is_ambiguous() {
        let err = resolve_name("xrechnung-invoice", &VERSIONS).expect_err("ambiguous");
        assert_eq!(err.exit_code(), 64);
        let text = err.to_string();
        assert!(
            text.contains("xrechnung-invoice:3.0.2, xrechnung-invoice:3.1"),
            "lists the versions: {text}"
        );
    }

    #[test]
    fn test_resolve_name_bare_display_name_next_to_a_version_is_ambiguous() {
        // A mapping whose display name is the bare `foo` (its `source_model`)
        // and a `foo:2`: `foo` could mean either, so it means neither.
        let err = resolve_name("foo", &["foo", "foo:2"]).expect_err("ambiguous");
        assert!(err.to_string().contains("foo, foo:2"), "{err}");
        assert_eq!(resolve_name("foo:2", &["foo", "foo:2"]).expect("full"), 1);
    }

    #[test]
    fn test_resolve_name_names_differing_only_in_case_are_ambiguous() {
        let err = resolve_name("foo:1", &["Foo:1", "foo:1"]).expect_err("ambiguous");
        assert_eq!(err.exit_code(), 64);
    }

    #[test]
    fn test_resolve_spoke_unknown_name_errors() {
        let err = resolve_spoke("totally-made-up").expect_err("unknown");
        assert!(matches!(err, CliError::UnknownFormat(_)));
        assert_eq!(err.exit_code(), 64);
    }
}
