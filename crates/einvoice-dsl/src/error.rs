//! Compiler diagnostics
//!
//! Two layers share this module:
//!
//! - [`ConfigError`] — a single fatal parse/load failure (E001 class), carrying
//!   the byte span of the offending TOML when available.
//! - [`Diagnostic`] / [`Severity`] — the aggregated, never-first-error-only
//!   validation findings
//!   compile-time validation pipeline collects these without stopping at the
//!   first error.

use std::ops::Range;

/// Diagnostic severity ("Runtime Validation": `error`, `warning`, `info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// A condition that fails compilation (or marks a mapping invalid).
    Error,
    /// A non-fatal condition the reviewer should see (e.g. fallback used).
    Warning,
    /// Informational only.
    Info,
}

impl Severity {
    /// The lower-case label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// A single config parse/load failure (E001 class). Fatal: parsing cannot
/// continue past it, unlike the aggregated [`Diagnostic`]s.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    /// Human-readable message.
    pub message: String,
    /// Byte span into the source TOML, when the underlying error carries one.
    pub span: Option<Range<usize>>,
}

impl ConfigError {
    /// A spanless config error from a message.
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(err: toml::de::Error) -> Self {
        Self {
            message: err.to_string(),
            span: err.span(),
        }
    }
}

/// One aggregated validation finding (Compile-Time Validation).
///
/// The pipeline collects every finding rather than stopping at the first, and
/// renders them in deterministic order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable diagnostic code (e.g. `E001`, or a runtime code like `FALLBACK_USED`).
    pub code: String,
    /// Severity.
    pub severity: Severity,
    /// The source node ID this finding concerns, when applicable.
    pub source_node: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Byte span into the source TOML, when available.
    pub span: Option<Range<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_from_toml_carries_span() {
        // A duplicate key is a TOML error with a span.
        let err = toml::from_str::<toml::Value>("a = 1\na = 2")
            .map_err(ConfigError::from)
            .unwrap_err();
        assert!(err.span.is_some(), "toml errors should carry a byte span");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn test_severity_ordering_error_is_lowest() {
        // Error sorts before Warning before Info (useful for stable report order).
        assert!(Severity::Error < Severity::Warning);
        assert!(Severity::Warning < Severity::Info);
    }
}
