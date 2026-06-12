//! Normalization operations.
//!
//! A small, closed set of compiler-known operations a source node may declare in
//! `normalize = [...]`. Normalization is *not* scripting: the compiler rejects any
//! unknown operation. Operations apply in declared order, before type validation.
//!
//! # Structure
//!
//! - [`NormalizeOp`] — one normalization operation.
//!
//! # Behavior
//!
//! No type has implicit normalization: a `string`/`identifier` is not trimmed and
//! a `currency`/`unit_code` is not upper-cased unless the node declares it.

use serde::Deserialize;

/// A compiler-known normalization operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizeOp {
    /// Strip leading and trailing whitespace.
    Trim,
    /// Upper-case the value.
    Uppercase,
    /// Lower-case the value.
    Lowercase,
    /// Treat an empty (post-trim) value as missing rather than present-but-empty.
    EmptyAsMissing,
}
