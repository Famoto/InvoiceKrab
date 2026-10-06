//! Schema conformance of the bundled mappings: the acceptance test for
//! namespaces, element order, the mandatory elements and attributes the
//! mappings pin, and the values they carry through the hub.
//!
//! Nothing here names a format, a fixture or an XSD. Each mapping declares the
//! schema its documents must satisfy (`[meta.schema]`, inherited by a CIUS)
//! and the samples that prove it (`[[meta.samples]]`); the build embeds them
//! in the registry, and [`conformance::check`] derives the checks: every sample
//! validates against its reader's XSD, and written by every spoke with a
//! schema it validates against that spoke's XSD (up to the spoke's
//! `known_gaps`, none of which may be stale) and round-trips every canonical
//! key the spoke covers. `krab-cli --check` runs the same checks.
//!
//! Where a spoke names business-rule sets (`[meta.schema].schematron`), every
//! such document must also satisfy those official Schematron rules.
//!
//! Without `xmllint` on `PATH` the schema checks are skipped with a notice and
//! the round trips still run; likewise the rule checks without the rules
//! (`scripts/fetch-schematron.sh`) or `java`. CI installs `libxml2-utils` and
//! fetches the rules, so there every check always runs.

use std::path::Path;

use einvoice_interfaces::Spoke;
use einvoice_interfaces::conformance::{self, Schematron, Xmllint};

#[test]
fn test_bundled_mappings_satisfy_their_declared_schemas_and_samples() {
    let xmllint = Xmllint::detect();
    if xmllint.is_none() {
        // Locally the schema checks are optional; on CI they are the schema
        // gate, so a missing install step must fail loudly instead of
        // passing silently.
        assert!(
            std::env::var_os("CI").is_none(),
            "xmllint is required on CI for XSD validation (install libxml2-utils)"
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schematron = Schematron::detect(&root);
    if let Err(why) = &schematron {
        assert!(
            std::env::var_os("CI").is_none(),
            "the business rules are required on CI ({why}): run scripts/fetch-schematron.sh"
        );
    }
    let report = conformance::check(&root, xmllint, schematron.as_ref().map_err(String::as_str));
    // The full report (known gaps, dropped keys) shows with `--nocapture`.
    eprintln!("{}", report.render());

    // The matrix is derived, so make sure it is not vacuous: the mappings
    // declare samples, and every sample reaches every spoke with a schema.
    assert!(
        !report.samples.is_empty(),
        "no mapping declares a [[meta.samples]] document"
    );
    assert_eq!(
        report.targets.len(),
        Spoke::ALL.iter().filter(|s| s.schema().is_some()).count()
    );
    assert!(report.is_ok(), "\n{}", report.render());
    for sample in &report.samples {
        assert_eq!(sample.pairs.len(), report.targets.len(), "{}", sample.file);
    }
}
