//! Parsed command-line intents.
//!
//! These are the pure data types [`parse_args`](super::parse_args) produces:
//! the requested [`Command`] and, for a transform, its resolved [`Args`]. They
//! carry no behavior — resolution, detection, and IO live in the sibling
//! modules.

/// A parsed command-line invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Transform a document from one format to another.
    Transform(Args),
    /// List the available formats and exit.
    ListFormats,
    /// Report the loss/error state of transforms: the full source x target
    /// matrix, one source's row, or one pair in full (see [`AnalyzeArgs`]).
    Analyze(AnalyzeArgs),
    /// Show the canonical main keys, as an authoring aid for writing mappings.
    /// `None` lists the whole hub vocabulary; `Some(format)` shows that spoke's
    /// covered vs. unused keys.
    Keys(Option<String>),
    /// Run the schema-conformance checks the mappings declare (see
    /// [`crate::conformance`]) against the files under a workspace root:
    /// `None` means the current directory.
    Check(Option<String>),
    /// Print usage and exit successfully.
    Help,
}

/// The inputs of an `--analyze` command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalyzeArgs {
    /// Scope to transforms from this source format; `None` means every source.
    pub source: Option<String>,
    /// With a source, the single target to report in full; `None` means every
    /// target (a table row per target).
    pub target: Option<String>,
    /// Exit non-zero (65) when any reported transform is not lossless.
    pub deny_lossy: bool,
}

/// The resolved inputs of a transform command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Source document: a filesystem path, or `-` for standard input.
    pub input: String,
    /// The requested target format name (matched against [`Spoke::name`](crate::Spoke::name)).
    pub target_format: String,
    /// An explicit source format; `None` means auto-detect.
    pub source_format: Option<String>,
    /// Destination: a filesystem path, or `None` for standard output.
    pub output: Option<String>,
}
