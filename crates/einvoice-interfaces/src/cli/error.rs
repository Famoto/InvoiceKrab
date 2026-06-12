//! CLI failures and their process exit codes.
//!
//! [`CliError`] classifies every way an invocation can fail and maps each to a
//! BSD `sysexits.h` exit code via [`CliError::exit_code`], so [`run`](super::run)
//! can report a failure and exit with a meaningful status.

use crate::EngineError;

/// A CLI failure, carrying its intended process exit code.
#[derive(Debug)]
pub enum CliError {
    /// Malformed arguments. Exit code 64 (`EX_USAGE`).
    Usage(String),
    /// A format name that matches no spoke. Exit code 64 (`EX_USAGE`).
    UnknownFormat(String),
    /// Auto-detection found zero or several candidate source formats. Exit 64.
    AmbiguousSource(String),
    /// A filesystem/stdin/stdout failure. Exit code 74 (`EX_IOERR`).
    Io(String),
    /// The bytes could not be parsed or rendered by the engine. Exit code 65.
    Engine(String),
    /// The mapping produced error-severity diagnostics. Exit code 65 (`EX_DATAERR`).
    Mapping(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Usage(m) => write!(f, "usage error: {m}"),
            CliError::UnknownFormat(m) => write!(f, "unknown format: {m}"),
            CliError::AmbiguousSource(m) => write!(f, "{m}"),
            CliError::Io(m) => write!(f, "io error: {m}"),
            CliError::Engine(m) => write!(f, "engine error: {m}"),
            CliError::Mapping(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CliError {}

impl CliError {
    /// The process exit code this error maps to (BSD `sysexits.h` conventions).
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) | CliError::UnknownFormat(_) | CliError::AmbiguousSource(_) => 64,
            CliError::Io(_) => 74,
            CliError::Engine(_) | CliError::Mapping(_) => 65,
        }
    }
}

impl From<EngineError> for CliError {
    fn from(e: EngineError) -> Self {
        CliError::Engine(e.to_string())
    }
}
