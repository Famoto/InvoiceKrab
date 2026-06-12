//! Human-readable text output: usage, the format list, and diagnostics.
//!
//! These build the strings [`run`](super::run) prints; keeping them pure makes
//! the wording testable without capturing process streams.

use crate::Spoke;
use einvoice_transformator::result::{MappingDiagnostic, Severity};

/// One-line usage summary, with the formats this build knows about.
pub fn usage() -> String {
    format!(
        "krab-invoice — transform an e-invoice XML document between formats\n\
         \n\
         USAGE:\n    \
         krab-invoice <INPUT> <TARGET-FORMAT> [--from <SOURCE-FORMAT>] [--out <FILE>]\n    \
         krab-invoice --analyze [SOURCE-FORMAT]\n    \
         krab-invoice --keys [FORMAT]\n    \
         krab-invoice --list\n    \
         krab-invoice --help\n\
         \n\
         ARGS:\n    \
         <INPUT>            Source XML file, or `-` to read stdin\n    \
         <TARGET-FORMAT>    Format to emit (see --list)\n\
         \n\
         OPTIONS:\n    \
         --from <FORMAT>    Source format; auto-detected when omitted\n    \
         --out <FILE>       Write to FILE instead of stdout\n    \
         --analyze          Report each transform's loss/error state\n    \
         --keys [FORMAT]    Show canonical main keys (a mapping-authoring aid);\n                       \
         with FORMAT, that spoke's covered vs. unused keys\n    \
         --list             List available formats\n    \
         -h, --help         Show this help\n\
         \n\
         FORMATS:\n{}",
        format_list()
    )
}

/// A newline-separated, indented list of available format names.
pub fn format_list() -> String {
    Spoke::ALL
        .iter()
        .map(|s| format!("    {}", s.name()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders mapping diagnostics into a human-readable, newline-terminated block.
///
/// Returns an empty string when there are no diagnostics.
pub fn render_diagnostics(diagnostics: &[MappingDiagnostic]) -> String {
    let mut out = String::new();
    for d in diagnostics {
        let level = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        out.push_str(&format!(
            "{level} [{}] {}: {}\n",
            d.code, d.source_node, d.message
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_render_diagnostics_empty_is_empty() {
        assert_eq!(render_diagnostics(&[]), "");
    }

    #[test]
    fn test_render_diagnostics_formats_each_line() {
        let diags = vec![
            MappingDiagnostic::new(
                Severity::Warning,
                "FALLBACK_USED",
                "Invoice.ID",
                "took fallback",
            ),
            MappingDiagnostic::new(
                Severity::Error,
                "REQUIRED_MISSING",
                "Invoice.ID",
                "is required",
            ),
        ];
        let out = render_diagnostics(&diags);
        assert_eq!(
            out,
            "warning [FALLBACK_USED] Invoice.ID: took fallback\n\
             error [REQUIRED_MISSING] Invoice.ID: is required\n"
        );
    }

    #[test]
    fn test_format_list_contains_every_spoke() {
        let list = format_list();
        for spoke in Spoke::ALL {
            assert!(list.contains(spoke.name()), "missing {}", spoke.name());
        }
    }
}
