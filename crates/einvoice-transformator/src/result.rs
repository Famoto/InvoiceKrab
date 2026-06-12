//! Mapping results and runtime diagnostics.
//!
//! This module owns the structured output of a generated mapper run: a
//! [`MappingResult`] carrying an optional produced value plus a list of
//! [`MappingDiagnostic`]s.
//!
//! # Structure
//!
//! - [`Severity`] — the diagnostic level (error / warning / info).
//! - [`MappingDiagnostic`] — a single runtime diagnostic with provenance
//!   (source node, canonical key, source path, fallback chain, ...).
//! - [`MappingResult`] — the value produced by a mapper plus its diagnostics.
//!
//! # Behavior
//!
//! Diagnostics are data only; generated code constructs them with
//! [`MappingDiagnostic::new`] and fills optional provenance fields directly.
//! [`MappingResult::has_errors`] reports whether any diagnostic is an error.
//!
//! # Testing
//!
//! Unit tests cover construction defaults and the `has_errors` predicate across
//! severities.

use serde::Serialize;

/// The level of a [`MappingDiagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A hard failure: the mapping could not satisfy a requirement.
    Error,
    /// A non-fatal issue worth surfacing.
    Warning,
    /// Informational provenance (e.g. a fallback was taken).
    Info,
}

/// A runtime diagnostic emitted by a generated mapper.
///
/// Construct with [`MappingDiagnostic::new`] and set the optional provenance
/// fields directly as needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MappingDiagnostic {
    /// Diagnostic severity.
    pub severity: Severity,
    /// Stable machine code, e.g. `"FALLBACK_USED"`, `"REQUIRED_MISSING"`.
    pub code: String,
    /// The node id that produced this diagnostic.
    pub source_node: String,
    /// The canonical hub key involved, if any.
    pub canonical_key: Option<String>,
    /// The source document path involved, if any.
    pub source_path: Option<String>,
    /// The collection index involved, if this is within a collection.
    pub collection_index: Option<usize>,
    /// Node ids tried, in order, when resolving a fallback chain.
    pub fallback_chain: Vec<String>,
    /// Human-readable message.
    pub message: String,
}

impl MappingDiagnostic {
    /// Creates a diagnostic with the required fields set; all optional
    /// provenance fields default to `None` / empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use einvoice_transformator::result::{MappingDiagnostic, Severity};
    ///
    /// let d = MappingDiagnostic::new(Severity::Error, "REQUIRED_MISSING", "n1", "missing");
    /// assert_eq!(d.severity, Severity::Error);
    /// assert!(d.canonical_key.is_none());
    /// assert!(d.fallback_chain.is_empty());
    /// ```
    pub fn new(
        severity: Severity,
        code: &str,
        source_node: &str,
        message: impl Into<String>,
    ) -> Self {
        MappingDiagnostic {
            severity,
            code: code.to_string(),
            source_node: source_node.to_string(),
            canonical_key: None,
            source_path: None,
            collection_index: None,
            fallback_chain: Vec::new(),
            message: message.into(),
        }
    }
}

/// The result of running a generated mapper.
#[derive(Debug, Clone, PartialEq)]
pub struct MappingResult<T> {
    /// The produced value, if the mapping succeeded in building one.
    pub value: Option<T>,
    /// All diagnostics emitted during the run, in emission order.
    pub diagnostics: Vec<MappingDiagnostic>,
}

impl<T> MappingResult<T> {
    /// Creates a result from a value and its diagnostics.
    pub fn new(value: Option<T>, diagnostics: Vec<MappingDiagnostic>) -> Self {
        MappingResult { value, diagnostics }
    }

    /// Returns `true` if any diagnostic has [`Severity::Error`].
    ///
    /// # Examples
    ///
    /// ```
    /// use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};
    ///
    /// let ok: MappingResult<()> = MappingResult::new(Some(()), vec![]);
    /// assert!(!ok.has_errors());
    ///
    /// let bad: MappingResult<()> = MappingResult::new(
    ///     None,
    ///     vec![MappingDiagnostic::new(Severity::Error, "X", "n", "boom")],
    /// );
    /// assert!(bad.has_errors());
    /// ```
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_diagnostic_defaults_optional_fields() {
        let d = MappingDiagnostic::new(
            Severity::Warning,
            "FALLBACK_USED",
            "node-7",
            "used fallback",
        );
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.code, "FALLBACK_USED");
        assert_eq!(d.source_node, "node-7");
        assert_eq!(d.message, "used fallback");
        assert_eq!(d.canonical_key, None);
        assert_eq!(d.source_path, None);
        assert_eq!(d.collection_index, None);
        assert!(d.fallback_chain.is_empty());
    }

    #[test]
    fn test_has_errors_with_no_diagnostics_is_false() {
        let r: MappingResult<i32> = MappingResult::new(Some(1), vec![]);
        assert!(!r.has_errors());
    }

    #[test]
    fn test_has_errors_with_only_warning_and_info_is_false() {
        let r: MappingResult<i32> = MappingResult::new(
            Some(1),
            vec![
                MappingDiagnostic::new(Severity::Warning, "W", "n", "w"),
                MappingDiagnostic::new(Severity::Info, "I", "n", "i"),
            ],
        );
        assert!(!r.has_errors());
    }

    #[test]
    fn test_has_errors_with_error_is_true() {
        let r: MappingResult<i32> = MappingResult::new(
            None,
            vec![
                MappingDiagnostic::new(Severity::Info, "I", "n", "i"),
                MappingDiagnostic::new(Severity::Error, "E", "n", "e"),
            ],
        );
        assert!(r.has_errors());
    }

    #[test]
    fn test_new_result_carries_value_and_diagnostics() {
        let diags = vec![MappingDiagnostic::new(Severity::Info, "I", "n", "i")];
        let r = MappingResult::new(Some("v".to_string()), diags.clone());
        assert_eq!(r.value, Some("v".to_string()));
        assert_eq!(r.diagnostics, diags);
    }
}
