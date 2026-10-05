# Vendored XSD schemas

Fetched 2026-10-04 from GitHub mirrors (the official hosts were unreachable).
Files are byte-for-byte as found in the mirror; nothing was edited.

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
