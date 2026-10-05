//! Schema validation of emitted documents: the acceptance test for namespaces,
//! element order, and the mandatory elements and attributes the mappings pin.
//!
//! Each case transforms a checked-in fixture into a target spoke and validates
//! the result against that format's XSD (vendored under `testfiles/xsd/`) with
//! `xmllint`. Every reported schema error must match a line of the case's
//! allowlist (`tests/xsd_allowlist/<case>.txt`, substring patterns, `#`
//! comments), and every allowlist line must still match some error — a fix
//! that clears an error also has to delete its allowlist line, so the lists
//! only ever shrink. An empty allowlist means the document must validate.
//!
//! Without `xmllint` on `PATH` the cases are skipped with a notice (CI installs
//! `libxml2-utils`, so there they always run).

use std::path::{Path, PathBuf};
use std::process::Command;

use einvoice_interfaces::{Engine, Spoke};

struct Case {
    /// Allowlist file stem and temp-file label.
    name: &'static str,
    fixture: &'static str,
    source: Spoke,
    target: Spoke,
    /// Root XSD, relative to the workspace root.
    schema: &'static str,
    /// XML catalog resolving the schema's remote imports offline, if any.
    catalog: Option<&'static str>,
}

const UBL_XSD: &str = "testfiles/xsd/ubl-2.1/maindoc/UBL-Invoice-2.1.xsd";
const FACTURX_XSD: &str = "testfiles/xsd/facturx-en16931/Factur-X_EN16931.xsd";
const FATTURAPA_XSD: &str =
    "testfiles/xsd/fatturapa-1.2.2/Schema_del_file_xml_FatturaPA_v1.2.2.xsd";
const FATTURAPA_CATALOG: &str = "testfiles/xsd/fatturapa-1.2.2/catalog.xml";
const XRECHNUNG_FIXTURE: &str = "testfiles/xrechnung-3.0.2-beispiel.xml";

const CASES: &[Case] = &[
    Case {
        name: "xrechnung-to-ubl-invoice",
        fixture: XRECHNUNG_FIXTURE,
        source: Spoke::UblInvoice,
        target: Spoke::UblInvoice,
        schema: UBL_XSD,
        catalog: None,
    },
    Case {
        name: "xrechnung-to-xrechnung-invoice",
        fixture: XRECHNUNG_FIXTURE,
        source: Spoke::UblInvoice,
        target: Spoke::XrechnungInvoice,
        schema: UBL_XSD,
        catalog: None,
    },
    Case {
        name: "xrechnung-to-peppol-bis-billing",
        fixture: XRECHNUNG_FIXTURE,
        source: Spoke::UblInvoice,
        target: Spoke::PeppolBisBilling,
        schema: UBL_XSD,
        catalog: None,
    },
    Case {
        name: "xrechnung-to-facturx-invoice",
        fixture: XRECHNUNG_FIXTURE,
        source: Spoke::UblInvoice,
        target: Spoke::FacturxInvoice,
        schema: FACTURX_XSD,
        catalog: None,
    },
    Case {
        name: "xrechnung-to-fatturapa",
        fixture: XRECHNUNG_FIXTURE,
        source: Spoke::UblInvoice,
        target: Spoke::Fatturapa,
        schema: FATTURAPA_XSD,
        catalog: Some(FATTURAPA_CATALOG),
    },
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn xmllint_available() -> bool {
    Command::new("xmllint").arg("--version").output().is_ok()
}

/// Runs the transform and returns the schema errors `xmllint` reports (the
/// message after `Schemas validity error : `), in order.
fn schema_errors(case: &Case) -> Vec<String> {
    let root = workspace_root();
    let input = std::fs::read(root.join(case.fixture)).expect("read fixture");
    let out = Engine::new()
        .transform(case.source, case.target, &input)
        .expect("fixture is well-formed XML");
    assert!(!out.has_errors(), "{}: {:?}", case.name, out.diagnostics);
    let xml = out.value.expect("writer yields a document");

    let path =
        std::env::temp_dir().join(format!("krab-xsd-{}-{}.xml", case.name, std::process::id()));
    std::fs::write(&path, &xml).expect("write temp document");

    let mut cmd = Command::new("xmllint");
    cmd.args(["--noout", "--nonet", "--schema"])
        .arg(root.join(case.schema))
        .arg(&path);
    if let Some(catalog) = case.catalog {
        cmd.env("XML_CATALOG_FILES", root.join(catalog));
    }
    let output = cmd.output().expect("run xmllint");
    let _ = std::fs::remove_file(&path);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let errors: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.split_once("Schemas validity error : "))
        .map(|(_, msg)| msg.trim().to_string())
        .collect();
    assert!(
        output.status.success() || !errors.is_empty(),
        "{}: xmllint failed without schema errors:\n{stderr}\n{xml}",
        case.name
    );
    errors
}

/// The case's allowlist patterns (substring matches), comments and blanks dropped.
fn allowlist(case: &Case) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/xsd_allowlist")
        .join(format!("{}.txt", case.name));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[test]
fn test_emitted_documents_validate_against_their_xsd_up_to_the_allowlist() {
    if !xmllint_available() {
        // Locally the harness is optional; on CI it is the schema gate, so a
        // missing install step must fail loudly instead of passing silently.
        assert!(
            std::env::var_os("CI").is_none(),
            "xmllint is required on CI for XSD validation (install libxml2-utils)"
        );
        eprintln!("xmllint not found on PATH; skipping XSD validation");
        return;
    }
    let mut failures = Vec::new();
    for case in CASES {
        let errors = schema_errors(case);
        let allowed = allowlist(case);
        let unexpected: Vec<&String> = errors
            .iter()
            .filter(|e| !allowed.iter().any(|a| e.contains(a.as_str())))
            .collect();
        let stale: Vec<&String> = allowed
            .iter()
            .filter(|a| !errors.iter().any(|e| e.contains(a.as_str())))
            .collect();
        if !unexpected.is_empty() {
            failures.push(format!(
                "{}: {} schema error(s) not covered by tests/xsd_allowlist/{}.txt:\n  {}",
                case.name,
                unexpected.len(),
                case.name,
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            ));
        }
        if !stale.is_empty() {
            failures.push(format!(
                "{}: allowlist line(s) no longer match any error (delete them):\n  {}",
                case.name,
                stale
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
