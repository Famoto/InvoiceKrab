//! End-to-end tests driving the [`einvoice_interfaces::cli::run`] entry point
//! with in-memory streams, plus the bundled spokes.
//!
//! These exercise the full path — argument parsing, format resolution,
//! auto-detection, the engine, and output rendering — without spawning a
//! process, by passing byte buffers as stdin/stdout/stderr.

use einvoice_interfaces::cli::run;

/// A minimal but valid UBL invoice covering the required canonical keys.
const UBL: &[u8] = br#"<Invoice>
    <ID>INV-42</ID>
    <IssueDate>2026-06-27</IssueDate>
    <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
    <LegalMonetaryTotal><PayableAmount currencyID="EUR">119.00</PayableAmount></LegalMonetaryTotal>
    <InvoiceLine><ID>1</ID><InvoicedQuantity>2</InvoicedQuantity><Item><Name>Widget</Name></Item></InvoiceLine>
</Invoice>"#;

fn invoke(args: &[&str], stdin: &[u8]) -> (i32, String, String) {
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut input = stdin;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(&argv, &mut input, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("utf8 stdout"),
        String::from_utf8(err).expect("utf8 stderr"),
    )
}

#[test]
fn test_transform_explicit_source_writes_xml_to_stdout() {
    let (code, out, err) = invoke(&["-", "ubl-invoice", "--from", "ubl-invoice"], UBL);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("<ID>INV-42</ID>"), "got: {out}");
    assert!(out.ends_with('\n'));
}

#[test]
fn test_transform_auto_detect_source_from_stdin() {
    let (code, out, _err) = invoke(&["-", "ubl-invoice"], UBL);
    assert_eq!(code, 0);
    assert!(out.contains("<DocumentCurrencyCode>EUR</DocumentCurrencyCode>"));
}

#[test]
fn test_unknown_target_format_exits_64() {
    let (code, out, err) = invoke(&["-", "no-such-format", "--from", "ubl-invoice"], UBL);
    assert_eq!(code, 64);
    assert!(out.is_empty());
    assert!(err.contains("unknown format"), "stderr: {err}");
}

#[test]
fn test_missing_input_file_exits_74() {
    let (code, _out, err) = invoke(
        &["/no/such/file.xml", "ubl-invoice", "--from", "ubl-invoice"],
        b"",
    );
    assert_eq!(code, 74);
    assert!(err.contains("io error"), "stderr: {err}");
}

#[test]
fn test_malformed_xml_exits_65() {
    let (code, out, err) = invoke(
        &["-", "ubl-invoice", "--from", "ubl-invoice"],
        b"not xml <<<",
    );
    assert_eq!(code, 65);
    assert!(out.is_empty());
    assert!(err.contains("engine error"), "stderr: {err}");
}

#[test]
fn test_missing_required_field_exits_65_with_diagnostics() {
    let bad = br#"<Invoice>
        <ID></ID>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <LegalMonetaryTotal><PayableAmount currencyID="EUR">1.00</PayableAmount></LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>1</InvoicedQuantity><Item><Name>X</Name></Item></InvoiceLine>
    </Invoice>"#;
    let (code, out, err) = invoke(&["-", "ubl-invoice", "--from", "ubl-invoice"], bad);
    assert_eq!(code, 65, "stderr: {err}");
    assert!(out.is_empty(), "no partial output on error");
    assert!(err.contains("REQUIRED_MISSING"), "stderr: {err}");
}

#[test]
fn test_list_formats_exits_zero() {
    let (code, out, _err) = invoke(&["--list"], b"");
    assert_eq!(code, 0);
    assert!(out.contains("ubl-invoice"));
}

#[test]
fn test_help_exits_zero() {
    let (code, out, _err) = invoke(&["--help"], b"");
    assert_eq!(code, 0);
    assert!(out.contains("USAGE"));
}

#[test]
fn test_out_flag_writes_to_file() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("krab-cli-test-{}.xml", std::process::id()));
    let path_str = path.to_str().expect("utf8 path");

    let (code, out, err) = invoke(
        &[
            "-",
            "ubl-invoice",
            "--from",
            "ubl-invoice",
            "--out",
            path_str,
        ],
        UBL,
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.is_empty(), "output went to file, not stdout");

    let written = std::fs::read_to_string(&path).expect("output file exists");
    assert!(written.contains("<ID>INV-42</ID>"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_analyze_emits_table_with_legend_and_exits_zero() {
    let (code, out, err) = invoke(&["--analyze"], b"");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("SOURCE"), "got: {out}");
    assert!(out.contains("STATE"), "got: {out}");
    assert!(out.contains("lossless"), "got: {out}");
    assert!(out.contains("legend:"), "got: {out}");
    // Every ordered pair of the bundled spokes appears (source x target).
    use einvoice_interfaces::Spoke;
    let data_rows = out
        .lines()
        .filter(|l| Spoke::ALL.iter().any(|s| l.starts_with(s.name())))
        .count();
    assert_eq!(data_rows, Spoke::ALL.len() * Spoke::ALL.len());
}

#[test]
fn test_analyze_scoped_to_one_source_exits_zero() {
    let (code, out, err) = invoke(&["--analyze", "ubl-invoice"], b"");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("ubl-invoice"), "got: {out}");
    assert!(out.contains("legend:"));
}

#[test]
fn test_analyze_unknown_source_exits_64() {
    let (code, out, err) = invoke(&["--analyze", "no-such-format"], b"");
    assert_eq!(code, 64);
    assert!(out.is_empty());
    assert!(err.contains("unknown format"), "stderr: {err}");
}

#[test]
fn test_keys_lists_hub_vocabulary_and_exits_zero() {
    let (code, out, err) = invoke(&["--keys"], b"");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("KEY"), "got: {out}");
    assert!(out.contains("SPOKES"), "got: {out}");
    assert!(out.contains("main keys across"), "got: {out}");
}

#[test]
fn test_keys_scoped_to_one_spoke_shows_sections_and_exits_zero() {
    let (code, out, err) = invoke(&["--keys", "ubl-invoice"], b"");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("main keys for ubl-invoice"), "got: {out}");
    assert!(out.contains("COVERED"), "got: {out}");
    assert!(out.contains("UNUSED"), "got: {out}");
}

#[test]
fn test_keys_unknown_format_exits_64() {
    let (code, out, err) = invoke(&["--keys", "no-such-format"], b"");
    assert_eq!(code, 64);
    assert!(out.is_empty());
    assert!(err.contains("unknown format"), "stderr: {err}");
}

#[test]
fn test_help_lists_keys_command() {
    let (code, out, _err) = invoke(&["--help"], b"");
    assert_eq!(code, 0);
    assert!(out.contains("--keys"), "got: {out}");
}
