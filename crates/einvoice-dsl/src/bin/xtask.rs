//! `xtask`: the developer CLI over the mapping compiler.
//!
//! Thin command layer. Each command loads the `config/` directory (codecs,
//! then mappings) through the same [`einvoice_dsl::loader`] the
//! `einvoice-interfaces` build script uses — inheritance chains resolved,
//! disabled bases skipped, slugs from `[meta].doc_format` — compiles every
//! spoke through [`einvoice_dsl::compile`] with the loaded codecs, and renders
//! the result. What `check` accepts, the real
//! build accepts; there is no second loading path.
//!
//! # Commands
//!
//! - `check  <config-dir>` — compile the mappings; print every diagnostic (R9:
//!   all of them). Exits non-zero on any error-severity diagnostic. Loading
//!   already fails (exit 2) on a declared schema or sample file that does not
//!   exist under the workspace root, the config dir's parent (E100), or a
//!   sample no spoke reads (E101).
//! - `report <config-dir>` — print the canonical coverage matrix and the gap
//!   report.
//!
//! Run as `cargo run -p einvoice-dsl -- check config`.

use std::path::Path;
use std::process::ExitCode;

use einvoice_dsl::compile::{CompileOutput, SpokeInput, compile};
use einvoice_dsl::derive::check_derivations;
use einvoice_dsl::error::Severity;
use einvoice_dsl::loader::load_config;
use einvoice_dsl::report::{coverage_matrix, gap_report, render_coverage_markdown};

const USAGE: &str = "\
usage: xtask <command> <config-dir>

commands:
  check  <dir>   compile every spoke mapping (with <dir>/codecs); report all diagnostics
  report <dir>   print the canonical coverage matrix and gap report
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = run(&args);
    print!("{}", out.stdout);
    ExitCode::from(out.exit_code as u8)
}

/// The rendered output of a command plus its process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    /// Text to print to stdout.
    stdout: String,
    /// Process exit code (0 success, 2 usage/IO error, 1 command failure).
    exit_code: u8,
}

/// Parses `args` (program name stripped) and dispatches to a subcommand.
fn run(args: &[String]) -> CommandOutput {
    let Some(command) = args.first().map(String::as_str) else {
        return usage();
    };
    match command {
        "check" => with_spokes(args.get(1), |out| render_check(&out)),
        "report" => with_spokes(args.get(1), |out| render_report(&out)),
        _ => usage(),
    }
}

fn usage() -> CommandOutput {
    CommandOutput {
        stdout: USAGE.to_string(),
        exit_code: 2,
    }
}

/// Loads the config directory (codecs + mappings), compiles it, and hands the
/// result to `render`.
fn with_spokes(
    dir: Option<&String>,
    render: impl FnOnce(CompileOutput) -> CommandOutput,
) -> CommandOutput {
    let Some(dir) = dir else {
        return CommandOutput {
            stdout: format!("error: a config directory is required\n\n{USAGE}"),
            exit_code: 2,
        };
    };
    let loaded = match load_config(Path::new(dir)) {
        Ok(l) => l,
        Err(e) => {
            return CommandOutput {
                stdout: format!("error: {e}\n"),
                exit_code: 2,
            };
        }
    };
    let spokes: Vec<SpokeInput> = loaded
        .spokes
        .iter()
        .map(|s| SpokeInput {
            id: s.slug.clone(),
            chain: &s.chain,
        })
        .collect();
    let mut out = compile(&spokes, &loaded.codecs);
    out.diagnostics
        .extend(check_derivations(&out.hub, &loaded.derivations));
    render(out)
}

/// Renders every diagnostic, returning a non-zero exit code on errors (R9).
fn render_check(out: &CompileOutput) -> CommandOutput {
    let mut s = String::new();
    let (mut errors, mut warnings) = (0, 0);
    for d in &out.diagnostics {
        match d.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => {}
        }
        let node = d.source_node.as_deref().unwrap_or("-");
        s.push_str(&format!(
            "[{}] {} ({node}): {}\n",
            d.severity.as_str(),
            d.code,
            d.message
        ));
    }
    s.push_str(&format!(
        "\n{} spoke(s), {} canonical field(s); {errors} error(s), {warnings} warning(s)\n",
        out.irs.len(),
        out.hub.len()
    ));
    CommandOutput {
        stdout: s,
        exit_code: if out.has_errors() { 1 } else { 0 },
    }
}

/// Renders the coverage matrix + gap report.
fn render_report(out: &CompileOutput) -> CommandOutput {
    let matrix = coverage_matrix(out);
    let gaps = gap_report(out);
    let mut s = String::from("## Canonical coverage matrix\n\n");
    s.push_str(&render_coverage_markdown(&matrix));
    s.push_str("\n## Gaps (spoke does not map field)\n\n");
    if gaps.is_empty() {
        s.push_str("none\n");
    } else {
        for g in &gaps {
            s.push_str(&format!("- {} is missing `{}`\n", g.spoke, g.field.label()));
        }
    }
    // A report never fails the build; surface errors via `check`.
    CommandOutput {
        stdout: s,
        exit_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use einvoice_dsl::parse::parse_mapping;

    fn output(bodies: &[(&str, &str)]) -> CompileOutput {
        let chains: Vec<(String, Vec<einvoice_dsl::ParsedMapping>)> = bodies
            .iter()
            .enumerate()
            .map(|(i, (id, body))| {
                let src = format!(
                    r#"
                    [meta]
                    doc_format = "f"
                    format_version = "1"
                    mapping_version = "1"
                    source_model = "s{i}:1"
                    canonical_model = "c:1"
                    root = "Doc"
                    {body}
                "#
                );
                ((*id).to_string(), vec![parse_mapping(&src).unwrap()])
            })
            .collect();
        let spokes: Vec<SpokeInput> = chains
            .iter()
            .map(|(id, chain)| SpokeInput {
                id: id.clone(),
                chain,
            })
            .collect();
        compile(&spokes, &Default::default())
    }

    #[test]
    fn test_check_clean_exits_zero() {
        let out = output(&[(
            "ubl",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        )]);
        let rendered = render_check(&out);
        assert_eq!(rendered.exit_code, 0);
        assert!(rendered.stdout.contains("0 error(s)"));
    }

    #[test]
    fn test_check_invalid_node_exits_one_and_reports_code() {
        // `join_with` without `multiple = "join"` is an error-severity
        // diagnostic (E040).
        let out = output(&[(
            "ubl",
            r#"[Doc.X]
            type = "string"
            join_with = ", ""#,
        )]);
        let rendered = render_check(&out);
        assert_eq!(rendered.exit_code, 1);
        assert!(rendered.stdout.contains("E040"));
    }

    #[test]
    fn test_report_renders_matrix_and_gaps() {
        let out = output(&[
            (
                "a",
                r#"[Doc.ID]
                type = "identifier"
                canonical_key = "InvoiceNumber""#,
            ),
            (
                "b",
                r#"[Doc.ID]
                type = "string""#,
            ),
        ]);
        let rendered = render_report(&out);
        assert!(rendered.stdout.contains("coverage matrix"));
        assert!(rendered.stdout.contains("InvoiceNumber"));
        // Spoke b maps nothing canonical, so it gaps InvoiceNumber.
        assert!(rendered.stdout.contains("b is missing"));
    }

    #[test]
    fn test_no_args_is_usage() {
        assert_eq!(run(&[]).exit_code, 2);
    }

    #[test]
    fn test_check_real_config_dir_uses_shared_loader() {
        // End-to-end over the workspace `config/`: codecs load, inheritance
        // chains resolve (xrechnung/peppol/facturx inherit their bases) and the
        // disabled CII base emits no spoke — exactly what the build script
        // compiles.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let rendered = with_spokes(Some(&dir.display().to_string()), |out| {
            assert!(!out.has_errors(), "{:?}", out.diagnostics);
            assert!(
                !out.irs.contains_key("cii_invoice"),
                "disabled base must not emit a spoke"
            );
            let xr = &out.irs["xrechnung_invoice"];
            assert!(
                xr.nodes.len() > 10,
                "inherited base nodes must be folded in, got {}",
                xr.nodes.len()
            );
            render_check(&out)
        });
        assert_eq!(rendered.exit_code, 0, "{}", rendered.stdout);
    }
}
