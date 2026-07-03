//! The IO entry point: dispatch a parsed command and wire up the streams.
//!
//! [`run`] is the boundary the binary calls; everything below it performs the
//! chosen [`Command`] — reading input, driving the engine, and writing output —
//! through injected readers/writers so the whole flow is testable without the
//! real process streams.

use std::io::Write as _;

use super::{
    Args, CliError, Command, detect_source, format_list, parse_args, render_diagnostics,
    resolve_spoke, usage,
};
use crate::{Engine, Spoke};
use einvoice_transformator::result::{MappingDiagnostic, MappingResult};

/// Runs the CLI with the given argv (excluding the program name), reading and
/// writing through `stdin`/`stdout`/`stderr`.
///
/// Returns the process exit code. All output goes through the provided writers so
/// the function is testable without touching the real process streams.
///
/// # Errors
///
/// Failures are reported to `stderr` and reflected in the returned exit code; the
/// function itself does not return a `Result`.
pub fn run(
    args: &[String],
    stdin: &mut dyn std::io::Read,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) -> i32 {
    match dispatch(args, stdin, stdout) {
        Ok(diagnostics) => {
            let rendered = render_diagnostics(&diagnostics);
            if !rendered.is_empty() {
                let _ = write!(stderr, "{rendered}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(stderr, "{e}");
            e.exit_code()
        }
    }
}

/// The fallible core of [`run`]: performs the parsed command, returning the
/// diagnostics to surface on success.
fn dispatch(
    args: &[String],
    stdin: &mut dyn std::io::Read,
    stdout: &mut dyn std::io::Write,
) -> Result<Vec<MappingDiagnostic>, CliError> {
    match parse_args(args)? {
        Command::Help => {
            write_all(stdout, usage().as_bytes())?;
            write_all(stdout, b"\n")?;
            Ok(Vec::new())
        }
        Command::ListFormats => {
            write_all(stdout, format_list().as_bytes())?;
            write_all(stdout, b"\n")?;
            Ok(Vec::new())
        }
        Command::Analyze(source) => {
            write_all(stdout, analyze_table(source.as_deref())?.as_bytes())?;
            Ok(Vec::new())
        }
        Command::Keys(format) => {
            write_all(stdout, keys_output(format.as_deref())?.as_bytes())?;
            Ok(Vec::new())
        }
        Command::Transform(args) => transform(&args, stdin, stdout),
    }
}

/// Builds the `--analyze` table: every transform's loss/error state, scoped to a
/// single source when `source` is given (else the full source x target matrix).
/// Also served verbatim by `krab-server` as `GET /analyze`.
///
/// # Errors
///
/// Returns [`CliError::UnknownFormat`] when `source` names no spoke.
pub fn analyze_table(source: Option<&str>) -> Result<String, CliError> {
    let sources: Vec<Spoke> = match source {
        Some(name) => vec![resolve_spoke(name)?],
        None => Spoke::ALL.to_vec(),
    };
    let reports = crate::analysis::analyze_all(&sources, Spoke::ALL);
    Ok(crate::analysis::render_table(&reports))
}

/// Builds the `--keys` output: the whole hub vocabulary when `format` is `None`,
/// or one spoke's covered-vs-unused authoring view when a format is given.
///
/// # Errors
///
/// Returns [`CliError::UnknownFormat`] when `format` names no spoke.
fn keys_output(format: Option<&str>) -> Result<String, CliError> {
    match format {
        None => Ok(crate::keys::render_hub_keys(&crate::keys::hub_keys())),
        Some(name) => {
            let spoke = resolve_spoke(name)?;
            Ok(crate::keys::render_spoke_keys(&crate::keys::spoke_keys(
                spoke,
            )))
        }
    }
}

/// Executes a transform: read input, resolve formats, run the engine, write out.
fn transform(
    args: &Args,
    stdin: &mut dyn std::io::Read,
    stdout: &mut dyn std::io::Write,
) -> Result<Vec<MappingDiagnostic>, CliError> {
    let bytes = read_input(&args.input, stdin)?;
    let engine = Engine::new();

    let to = resolve_spoke(&args.target_format)?;
    let from = match &args.source_format {
        Some(name) => resolve_spoke(name)?,
        None => detect_source(&bytes)?,
    };

    let result: MappingResult<String> = engine.transform(from, to, &bytes)?;

    match result.value {
        Some(mut xml) if !result.has_errors() => {
            if !xml.ends_with('\n') {
                xml.push('\n');
            }
            match &args.output {
                Some(path) => write_output_file(path, &xml)?,
                None => write_all(stdout, xml.as_bytes())?,
            }
            Ok(result.diagnostics)
        }
        _ => Err(CliError::Mapping(format!(
            "transformation failed with errors:\n{}",
            render_diagnostics(&result.diagnostics).trim_end()
        ))),
    }
}

/// Reads `input` fully — from stdin when it is `-`, otherwise from the file path.
fn read_input(input: &str, stdin: &mut dyn std::io::Read) -> Result<Vec<u8>, CliError> {
    if input == "-" {
        let mut buf = Vec::new();
        stdin
            .read_to_end(&mut buf)
            .map_err(|e| CliError::Io(format!("reading stdin: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read(input).map_err(|e| CliError::Io(format!("reading {input:?}: {e}")))
    }
}

/// Writes all of `bytes` to `w`, mapping failures to [`CliError::Io`].
fn write_all(w: &mut dyn std::io::Write, bytes: &[u8]) -> Result<(), CliError> {
    w.write_all(bytes)
        .map_err(|e| CliError::Io(format!("writing output: {e}")))
}

/// Writes `xml` to `path`, creating or truncating it.
///
/// Used by `main.rs` when `--out` is given; kept here so the IO policy lives in
/// one place.
pub fn write_output_file(path: &str, xml: &str) -> Result<(), CliError> {
    let mut f =
        std::fs::File::create(path).map_err(|e| CliError::Io(format!("creating {path:?}: {e}")))?;
    f.write_all(xml.as_bytes())
        .map_err(|e| CliError::Io(format!("writing {path:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keys_output_full_vocabulary_lists_keys() {
        let out = keys_output(None).expect("ok");
        assert!(out.contains("KEY"));
        assert!(out.contains("main keys across"));
    }

    #[test]
    fn test_keys_output_scoped_shows_covered_and_unused() {
        let out = keys_output(Some("ubl-invoice")).expect("known");
        assert!(out.contains("main keys for"));
        assert!(out.contains("COVERED"));
        assert!(out.contains("UNUSED"));
    }

    #[test]
    fn test_keys_output_unknown_format_is_unknown_format() {
        let err = keys_output(Some("totally-made-up")).expect_err("unknown");
        assert!(matches!(err, CliError::UnknownFormat(_)));
    }

    #[test]
    fn test_analyze_table_unknown_source_is_unknown_format() {
        let err = analyze_table(Some("totally-made-up")).expect_err("unknown");
        assert!(matches!(err, CliError::UnknownFormat(_)));
    }

    #[test]
    fn test_analyze_table_scoped_lists_only_that_source() {
        let table = analyze_table(Some("ubl-invoice")).expect("known");
        assert!(table.contains("legend:"));
        // No spoke other than the scoped one appears in the SOURCE column (every
        // data row starts with the source name).
        let ubl = resolve_spoke("ubl-invoice").expect("known").name();
        for spoke in Spoke::ALL {
            if spoke.name() != ubl {
                assert!(
                    !table.lines().any(|l| l.starts_with(spoke.name())),
                    "{} should not be a source row",
                    spoke.name()
                );
            }
        }
    }
}
