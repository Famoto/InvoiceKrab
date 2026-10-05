//! The `krab-cli` command-line surface for the engine.
//!
//! This module owns the CLI: it parses argv into a [`Command`], resolves
//! human-typed format names to the generated [`Spoke`](crate::Spoke) variants,
//! and drives [`Engine::transform`](crate::Engine::transform) to turn a source
//! XML document into a target format. It owns *only* the CLI concerns — argument
//! parsing, format resolution, IO wiring, and diagnostic rendering; all mapping
//! logic lives in the rest of this crate and in `einvoice-transformator`. The
//! thin `krab-cli` binary (`src/bin/krab-cli.rs`) just forwards the real
//! argv and standard streams into [`run`].
//!
//! # Structure
//!
//! The module is split by concern into private submodules whose public items are
//! re-exported here, so callers keep using the flat `cli::` paths:
//!
//! - [`command`] — [`Command`] (the parsed intent) and [`Args`] (a transform's
//!   resolved inputs).
//! - [`error`] — [`CliError`], mapped to process exit codes via
//!   [`CliError::exit_code`].
//! - [`parse`] — [`parse_args`], pure argv → [`Command`].
//! - [`detect`] — [`resolve_spoke`] (format-name → [`Spoke`](crate::Spoke)) and
//!   [`detect_source`] (auto-detection from the document signature).
//! - [`render`] — [`usage`], [`format_list`], and [`render_diagnostics`].
//! - [`run`] — [`run`], the IO entry point used by the binary.
//!
//! # Behavior
//!
//! `krab-cli <INPUT> <TARGET-FORMAT> [--from <SOURCE-FORMAT>] [--out <FILE>]`
//! reads `INPUT` (or stdin when `-`), transforms it from its source format to
//! `TARGET-FORMAT`, and writes the result to stdout (or `--out`). When `--from`
//! is omitted the source format is auto-detected by matching the document's root
//! element against the generated spoke registry (then disambiguating by
//! `CustomizationID`); detection that is empty or ambiguous is a [`CliError`]
//! asking for `--from`.
//! Mapping diagnostics are rendered to stderr; an error-severity diagnostic makes
//! the process exit non-zero without emitting partial output.
//!
//! `krab-cli --analyze [SOURCE-FORMAT [TARGET-FORMAT]] [--deny-lossy]` instead
//! emits a static report of transforms' loss/error state (no input document
//! needed): the full matrix, one source's row, or — with both formats — one
//! pair in full (every missing required route, dropped key, pin, recode and
//! declared collapse). `--deny-lossy` exits 65 when any reported transform is
//! not lossless, for CI gates. See [`crate::analysis`].
//!
//! `krab-cli --keys [FORMAT]` is an authoring aid for writing the mapping
//! TOMLs: with no format it lists the whole canonical "main key" vocabulary and
//! which spokes define each; with a format it shows that spoke's covered keys and
//! the hub keys it does not yet map (candidates to add). See [`crate::keys`].
//!
//! `krab-cli --check [ROOT]` runs the schema-conformance checks the mappings
//! declare (`[meta.schema]`, `[[meta.samples]]`): every sample validates
//! against its reader's XSD, and written by every spoke with a schema it
//! validates against that spoke's XSD (up to its `known_gaps`) and round-trips
//! its covered keys. Paths resolve under ROOT (default: the current
//! directory); a failure exits 65 with the report on stderr. See
//! [`crate::conformance`].
//!
//! # Testing
//!
//! Unit tests live beside each submodule: argument parsing (including error
//! paths) in [`parse`], case-insensitive format resolution and detection in
//! [`detect`], diagnostic and list rendering in [`render`], and the
//! `--analyze` / `--keys` / `--check` outputs in [`run`]. Integration tests in
//! `tests/cli.rs` drive [`run`] end to end against the bundled spokes.

mod command;
mod detect;
mod error;
mod parse;
mod render;
mod run;

pub use command::{AnalyzeArgs, Args, Command};
pub use detect::{detect_source, resolve_spoke};
pub use error::CliError;
pub use parse::parse_args;
pub use render::{format_list, render_diagnostics, usage};
pub use run::{analyze_output, check_output, run, write_output_file};
