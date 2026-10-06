# XSD schemas

The schemas are not part of the repository: their publishers' terms are
their own. `scripts/fetch-schemas.sh` downloads them into this directory from
the GitHub mirrors below, pinned to a commit (the official hosts were
unreachable when they were chosen, 2026-10-04), and verifies every file
against `SHA256SUMS`. Files are byte-for-byte as found in the mirror; nothing
is edited.

| Directory | Content | Mirror | Commit |
|---|---|---|---|
| ubl-2.1/ | OASIS UBL 2.1 OS (04 Nov 2013): all 14 common modules, maindoc Invoice + CreditNote | akretion/factur-x, src/facturx/xsd_and_schematron/ubl-2.1 | 6ce07409b918bb08766cf90a80546685e218d733 |
| facturx-en16931/ | Factur-X 1.09 (label inside the files), EN16931 profile, 4 XSDs | akretion/factur-x, src/facturx/xsd_and_schematron/facturx-en16931 | 6ce07409b918bb08766cf90a80546685e218d733 |
| fatturapa-1.2.2/ | Schema_del_file_xml_FatturaPA_v1.2.2.xsd + xmldsig-core-schema.xsd | OCA/l10n-italy (branch 14.0), l10n_it_account/tools/xsd | 4e06dda6a78efa8e36e28bbdbf95cdc00dd62838 |

Upstream originals:
- http://docs.oasis-open.org/ubl/os-UBL-2.1/xsd/
- https://fnfe-mpe.org/factur-x/ (Factur-X / ZUGFeRD distribution)
- https://www.fatturapa.gov.it/ (Schema FatturaPA v1.2.2)

Notes:
- The FatturaPA schema imports xmldsig via an absolute URL
  (http://www.w3.org/TR/2002/REC-xmldsig-core-20020212/xmldsig-core-schema.xsd).
  `fatturapa-1.2.2/catalog.xml` (ours, not upstream) is the XML catalog that maps
  it to the local `xmldsig-core-schema.xsd`. `config/mappings/fatturapa.toml`
  declares it as `[meta.schema].catalog`, the conformance checks pass it via
  `XML_CATALOG_FILES`, and `xmllint --nonet` then validates offline.
- Not yet compared against the official downloads; SHA256SUMS lets you do that.
- Only Invoice and CreditNote were taken from the UBL maindoc set.
- Terms, as the files state them: the UBL modules carry the OASIS copyright
  notice (the CCTS module the UN/CEFACT one), the xmldsig schemas the W3C
  Software License (1998-07-20); the XAdES (ETSI), Factur-X and FatturaPA
  schemas state none. None of them is licensed by KrabInvoice.
