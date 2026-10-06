# Changelog

All notable changes to KrabInvoice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
[Semantic Versioning](https://semver.org/) as defined in
[README › Versioning and stability](README.md#versioning-and-stability).
Changes to the mapping DSL are listed under **DSL** in each release.

## [Unreleased]

### DSL

- **Changed** the schema files a mapping declares (`[meta.schema]` `xsd` and
  `catalog`) need not exist at build time: nothing is built from them. Their
  paths must still stay in the workspace (E100), and the samples
  (`[[meta.samples]]`, `refuses`) must still exist. Without a declared schema
  file, `krab-cli --check` and the XSD test skip schema validation with a
  notice, as they do without `xmllint`.

### Changed

- The third-party XSDs are no longer in the repository:
  `scripts/fetch-schemas.sh` downloads them, pinned to a commit and verified
  against `testfiles/xsd/SHA256SUMS`, and CI runs it. The OASIS UBL 2.1
  example invoice (`testfiles/UBL-Invoice-2.1.xml`) is removed;
  `testfiles/en16931-full-ubl.xml` remains the UBL sample.

- Licensing: the KrabInvoice Configuration Exception (`LICENSE-EXCEPTION`),
  an additional permission under section 7 of the AGPL. Building with your
  own configuration directory is not a modification of KrabInvoice, and that
  configuration and the code generated from it need not be part of the
  Corresponding Source.
- Licensing: the repository is [REUSE](https://reuse.software/) 3.3
  compliant. `REUSE.toml` records every file's copyright and license,
  including the vendored third-party schemas, and `LICENSES/` holds the
  license texts; CI and pre-commit run `reuse lint`.
- Licensing: the sample invoices based on the KoSIT XRechnung test suite
  (`testfiles/en16931-full-*.xml`, `testfiles/xrechnung-3.0.2-beispiel.xml`)
  are marked Apache-2.0, as their source is, and note that KrabInvoice
  modified them.

### Documentation

- README: "Using KrabInvoice" — the web API with your own configuration as
  the recommended deployment, and changes to KrabInvoice go upstream or are
  published.

## [1.0.0] — 2026-10-06

The first stable release: the mapping DSL, the library API, the `krab-cli`
command line and the `krab-server` HTTP API are now covered by semantic
versioning. The bundled mappings remain demos (see the README's disclaimer).

### DSL

- **Added** `check` on `sum` / `add` rules in `derivations.toml`
  (`"warning"` by default, `"error"` or `"off"`): a total the document carries
  is recomputed by its rule, and a contradiction is reported as
  `VALUE_INCONSISTENT` (#44). Derivation sums and arithmetic use checked
  decimal operations: an overflowing result derives nothing and an
  overflowing check is reported, instead of panicking.
- **Added** `skip_negative` on `add` rules: a negative result derives
  nothing. The bundled BR-CO-16-for-the-paid-amount rule uses it, so a wrong
  amount due is reported rather than balanced by a negative prepayment.
- **Added** several versions of one format: mappings may share a `doc_format`
  with different `format_version`s; their slugs and `Spoke` variants are then
  version-qualified (`XrechnungInvoiceV3_0_2`), and a sample `source` naming
  the shared bare `doc_format` is ambiguous (E101) (#24).
- **Added** `KRAB_CONFIG_DIR`: the build compiles the configuration directory
  it names (absolute path), so mappings can live outside this repository; an
  empty value selects the bundled `config/` (#23).

### Added

- Source documents in UTF-16, ISO-8859-1, windows-1252 and US-ASCII are
  transcoded to UTF-8 before reading (`EngineError::Encoding` for others)
  (#45).
- `krab-cli --version` / `-V`, `krab-server --version` and `GET /version`:
  the engine version and every compiled mapping with its `mapping_version`;
  `Spoke::mapping_version()`.
- `krab-server`: `KRAB_REQUEST_TIMEOUT_SECS` (default 300, `408`) bounds the
  whole request body, `KRAB_QUEUE_TIMEOUT_SECS` (default 60, `503` +
  `Retry-After`) the wait for a memory reservation (#47).
- Dockerfile: `--build-arg KRAB_CONFIG_DIR=<dir>` compiles your own mappings;
  `linux/arm64` builds; cached dependency builds.
- CI: dependency policy (`cargo deny`), the minimum supported Rust version,
  a build with a custom two-version configuration, and the Docker images with
  a smoke test; third-party actions pinned by commit.
- `SECURITY.md`, this changelog, and the README sections *Disclaimer*,
  *Using your own mappings* and *Versioning and stability*.

### Changed

- A format name that matches several compiled formats — a bare name
  (`xrechnung-invoice`) shared by several versions, a display name equal to
  another's bare prefix, or display names differing only in case — is refused
  with the list of matches (exit 64 / `400`) instead of resolving to the first
  one (#24).
- `krab-server` query parameters are URL-decoded (`to=xrechnung-invoice%3A3.0.2`),
  and a repeated parameter is a `400` (#43).
- Minimum supported Rust version: 1.88 (`rust-version`).
- The workspace crates are `publish = false` and declare
  `license = "AGPL-3.0-or-later"`.
- Licensing: KrabInvoice is AGPL-3.0-or-later, except the `config/`
  directory (demo mappings, codecs, calculation rules), which is now
  CC0-1.0 (`config/LICENSE`), so mappings derived from the demos carry
  whatever license their authors choose.
- `crossbeam-epoch` (benchmark dependency) updated past RUSTSEC-2026-0204.

### Fixed

- `krab-server --healthcheck` probes the bound address instead of always
  `127.0.0.1` (#46).
- A slow client trickling its upload could hold its memory reservation, up to
  the whole budget, indefinitely (#47).
- A carried total that contradicts the derived ones (the engine itself could
  write e.g. a total with VAT unequal to total without VAT + VAT) was written
  silently (#44).

## [0.1.0]

Development versions before the first release.

[Unreleased]: https://github.com/Famoto/InvoiceKrab/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Famoto/InvoiceKrab/releases/tag/v1.0.0
