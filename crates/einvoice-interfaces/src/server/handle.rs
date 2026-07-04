//! HTTP-agnostic request handling: query string + body bytes → status + body.
//!
//! [`handle`] owns everything between "the body has been read" and "the
//! response is known": query-parameter parsing, format resolution (reusing
//! the [`cli`](crate::cli) helpers), driving
//! [`Engine::transform`](crate::Engine::transform), and mapping outcomes to
//! HTTP status codes. It knows nothing of sockets or `tiny_http`, so the
//! whole decision table is unit-testable with plain byte slices.
//!
//! # Status mapping
//!
//! | Outcome                                          | Status |
//! |--------------------------------------------------|--------|
//! | transformed, no error diagnostics                | 200    |
//! | missing/unknown `to` or `from`, failed detection | 400    |
//! | source bytes not well-formed for the spoke       | 400    |
//! | error-severity mapping diagnostics               | 422    |
//! | produced model unserializable (engine bug)       | 500    |
//!
//! Warning-severity diagnostics on a 200 are returned separately in
//! [`Reply::warnings`] so the transport can surface them in a header without
//! corrupting the XML body.
//!
//! Besides [`handle`] (the `/transform` core), [`formats`] and [`analyze`]
//! back the body-less capability routes `GET /formats` (JSON format list)
//! and `GET /analyze` (the CLI's `--analyze` table, reused verbatim).
//!
//! # Testing
//!
//! Unit tests cover every row of the table plus source auto-detection from
//! the document signature, the JSON shape of [`formats`], and the
//! known/unknown-source paths of [`analyze`].

use crate::cli::{detect_source, render_diagnostics, resolve_spoke};
use crate::{Engine, EngineError};

/// The computed response: status code, body, and any warning text destined
/// for a response header (never the body — the body is XML on success).
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// Response body: the transformed XML on 200, human-readable problem
    /// text otherwise.
    pub body: String,
    /// Warning-severity diagnostics accompanying a success, single-line.
    /// Empty when there are none.
    pub warnings: String,
}

/// Transforms `body` according to the request `query` (`to=<format>`,
/// optional `from=<format>`, auto-detected when absent) and maps the outcome
/// to an HTTP reply. See the module docs for the status table.
pub fn handle(query: &str, body: &[u8]) -> Reply {
    match transform(query, body) {
        Ok(reply) => reply,
        Err((status, message)) => Reply {
            status,
            body: message,
            warnings: String::new(),
        },
    }
}

/// The fallible core of [`handle`]; errors are `(status, problem text)`.
fn transform(query: &str, body: &[u8]) -> Result<Reply, (u16, String)> {
    let Some(to) = param(query, "to") else {
        return Err((400, "missing required query parameter: to=<format>".into()));
    };
    let to = resolve_spoke(to).map_err(|e| (400, e.to_string()))?;
    let from = match param(query, "from") {
        Some(name) => resolve_spoke(name).map_err(|e| (400, e.to_string()))?,
        None => detect_source(body).map_err(|e| (400, e.to_string()))?,
    };

    let result = Engine::new()
        .transform(from, to, body)
        .map_err(|e| match e {
            // The client sent bytes that are not a well-formed `from` document.
            EngineError::Deserialize(e) => (400, format!("source deserialization failed: {e}")),
            // The engine produced an unserializable model: our bug, not theirs.
            EngineError::Serialize(e) => (500, format!("target serialization failed: {e}")),
        })?;

    match result.value {
        Some(xml) if !result.has_errors() => Ok(Reply {
            status: 200,
            body: xml,
            // Header values cannot contain newlines; join the rendered lines.
            warnings: render_diagnostics(&result.diagnostics)
                .lines()
                .collect::<Vec<_>>()
                .join("; "),
        }),
        _ => Err((
            422,
            format!(
                "transformation failed with errors:\n{}",
                render_diagnostics(&result.diagnostics).trim_end()
            ),
        )),
    }
}

/// Returns the raw value of `name` in a `k=v&k=v` query string. Spoke names
/// are plain `[a-z0-9-]`, so no percent-decoding is needed.
fn param<'q>(query: &'q str, name: &str) -> Option<&'q str> {
    query
        .split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name))
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// The `GET /formats` body: a JSON array of every format name this build
/// accepts as `to=`/`from=` values.
pub fn formats() -> String {
    let names: Vec<&str> = crate::Spoke::ALL.iter().map(|s| s.name()).collect();
    // Serializing &[&str] cannot fail.
    serde_json::to_string(&names).unwrap_or_default()
}

/// The `GET /analyze[?from=<format>]` body: the static loss/error table of
/// every transform, scoped to one source when `from` is given (the CLI's
/// `--analyze`, verbatim).
///
/// # Errors
///
/// A 400-worthy problem text when `from` names no known format.
pub fn analyze(query: &str) -> Result<String, String> {
    crate::cli::analyze_table(param(query, "from")).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBL: &[u8] = br#"<Invoice>
        <ID>INV-42</ID>
        <IssueDate>2026-06-27</IssueDate>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <LegalMonetaryTotal>
            <PayableAmount currencyID="EUR">119.00</PayableAmount>
        </LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity>2</InvoicedQuantity><Item><Name>Widget</Name></Item></InvoiceLine>
    </Invoice>"#;

    #[test]
    fn test_handle_valid_transform_returns_200_xml() {
        let reply = handle("to=ubl-invoice&from=ubl-invoice", UBL);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(reply.body.contains("<ID>INV-42</ID>"), "{}", reply.body);
        assert!(reply.warnings.is_empty(), "{}", reply.warnings);
    }

    #[test]
    fn test_handle_detects_source_when_from_absent() {
        let reply = handle("to=ubl-invoice", UBL);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(reply.body.contains("<ID>INV-42</ID>"), "{}", reply.body);
    }

    #[test]
    fn test_handle_missing_to_returns_400() {
        let reply = handle("", UBL);
        assert_eq!(reply.status, 400);
        assert!(
            reply.body.contains("to="),
            "names the parameter: {}",
            reply.body
        );
    }

    #[test]
    fn test_handle_unknown_target_returns_400() {
        let reply = handle("to=not-a-format", UBL);
        assert_eq!(reply.status, 400);
        assert!(reply.body.contains("not-a-format"), "{}", reply.body);
    }

    #[test]
    fn test_handle_unknown_source_returns_400() {
        let reply = handle("to=ubl-invoice&from=not-a-format", UBL);
        assert_eq!(reply.status, 400);
        assert!(reply.body.contains("not-a-format"), "{}", reply.body);
    }

    #[test]
    fn test_handle_undetectable_source_returns_400() {
        let reply = handle("to=ubl-invoice", b"<Mystery/>");
        assert_eq!(reply.status, 400, "{}", reply.body);
    }

    #[test]
    fn test_handle_malformed_xml_returns_400() {
        let reply = handle("to=ubl-invoice&from=ubl-invoice", b"not xml <<<");
        assert_eq!(reply.status, 400, "{}", reply.body);
    }

    #[test]
    fn test_handle_mapping_errors_return_422_with_diagnostics() {
        // Plain UBL lacks the XRechnung-required CustomizationID: the writer
        // reports REQUIRED_MISSING (see the crate-level tests).
        let reply = handle("to=xrechnung-invoice&from=ubl-invoice", UBL);
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert!(reply.body.contains("REQUIRED_MISSING"), "{}", reply.body);
    }

    #[test]
    fn test_formats_is_json_listing_every_spoke() {
        let names: Vec<String> =
            serde_json::from_str(&formats()).expect("formats() emits valid JSON");
        for spoke in crate::Spoke::ALL {
            assert!(
                names.iter().any(|n| n == spoke.name()),
                "{} missing from {names:?}",
                spoke.name()
            );
        }
    }

    #[test]
    fn test_analyze_full_matrix_renders_table() {
        let table = analyze("").expect("no scope is valid");
        assert!(table.contains("legend:"), "{table}");
    }

    #[test]
    fn test_analyze_scoped_to_known_source_renders_table() {
        let table = analyze("from=ubl-invoice").expect("known format");
        assert!(table.contains("legend:"), "{table}");
    }

    #[test]
    fn test_analyze_unknown_source_is_problem_text() {
        let problem = analyze("from=not-a-format").expect_err("unknown format");
        assert!(problem.contains("not-a-format"), "{problem}");
    }
}
