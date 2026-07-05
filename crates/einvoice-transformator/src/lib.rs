//! `einvoice-transformator` — the small pure-helper runtime that generated
//! mappers link against.
//!
//! The e-invoice engine is reworked so the canonical hub is a **generated typed
//! struct** (`MainKey`) living in the interfaces crate, not a dynamic map.
//! Generated reader/writer code therefore uses plain Rust types directly
//! (`compact_str::CompactString`, `rust_decimal::Decimal`, `bool`, `Vec<…>`;
//! the inline string type keeps values of ≤ 24 bytes off the heap, which is
//! most invoice fields). This crate no longer
//! owns a `Value` type or a dynamic hub; it provides only the small set of pure
//! helpers that generated code still calls plus the structured mapping result.
//!
//! # Structure
//!
//! - [`normalize`] — pure string transforms emitted as direct function calls.
//! - [`validate`] — lexical shape checks for `currency`/`date`/`datetime`/
//!   `unit_code`, called by generated code before a field is built.
//! - [`adapter`] — the string-based [`Adapter`] contract for named, deterministic
//!   conversions.
//! - [`result`] — [`MappingResult`] and [`MappingDiagnostic`], the structured
//!   mapper output and runtime diagnostic model.
//!
//! # Behavior
//!
//! Everything here is pure data and pure functions: the crate performs no I/O
//! and has no dependency on `einvoice-dsl`. Generated code normalizes/validates
//! `&str` inputs, calls adapter functions, and returns a [`MappingResult`]
//! carrying diagnostics. There is no `Value` and no dynamic hub.
//!
//! # Testing
//!
//! Each module carries `#[cfg(test)]` unit tests plus doc tests on the public
//! API.

pub mod adapter;
pub mod normalize;
pub mod result;
pub mod validate;

pub use adapter::{Adapter, AdapterError, uppercase_currency};
pub use normalize::{empty_as_missing, lowercase, trim, uppercase};
pub use result::{MappingDiagnostic, MappingResult, Severity};
pub use validate::{is_currency, is_date, is_datetime, is_unit_code};
