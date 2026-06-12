//! Adapters: named, deterministic, pure string conversions.
//!
//! This module owns the runtime contract "Adapters": compiler-known
//! conversions that generated code invokes by name. Adapters perform no I/O and
//! are deterministic and pure. Generated code now uses native Rust types, so an
//! adapter operates over `&str` and returns an owned `String`.
//!
//! # Structure
//!
//! - [`AdapterError`] — the failure type for a conversion.
//! - [`Adapter`] — the documenting trait for a named conversion.
//! - [`uppercase_currency`] — an example adapter proving the function shape that
//!   generated code calls.
//!
//! # Behavior
//!
//! Generated code typically calls free adapter functions directly (e.g.
//! [`uppercase_currency`]); the [`Adapter`] trait documents the contract for
//! any object-form usage. Adapters must be pure: same input → same output, no
//! side effects. The [`Result`] shape is kept even for conversions that cannot
//! fail, so every generated call site is uniform.
//!
//! # Testing
//!
//! Unit tests cover the example adapter's success path, the error [`Display`],
//! and trait-object usage.

use thiserror::Error;

/// The error returned when an adapter conversion fails.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("adapter `{adapter}` failed: {reason}")]
pub struct AdapterError {
    /// The adapter name.
    pub adapter: String,
    /// Why the conversion failed.
    pub reason: String,
}

/// A compiler-known, deterministic, pure string conversion.
///
/// Generated code generally calls named adapter functions directly; this trait
/// documents the contract for object-style usage. Implementations must be pure
/// and free of I/O.
pub trait Adapter {
    /// Converts `input` into an output [`String`], or fails with an
    /// [`AdapterError`].
    fn convert(&self, input: &str) -> Result<String, AdapterError>;
}

/// Upper-cases `input` (example currency-code adapter).
///
/// This conversion cannot fail for a `&str`; the [`Result`] shape is preserved
/// only so generated call sites are uniform. It never returns [`Err`].
///
/// # Examples
///
/// ```
/// use einvoice_transformator::adapter::uppercase_currency;
///
/// assert_eq!(uppercase_currency("eur").unwrap(), "EUR");
/// ```
pub fn uppercase_currency(input: &str) -> Result<String, AdapterError> {
    Ok(input.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uppercase_currency_upcases_text() {
        assert_eq!(uppercase_currency("usd").unwrap(), "USD");
    }

    #[test]
    fn test_uppercase_currency_already_upper_is_unchanged() {
        assert_eq!(uppercase_currency("EUR").unwrap(), "EUR");
    }

    #[test]
    fn test_adapter_error_display() {
        let err = AdapterError {
            adapter: "x".to_string(),
            reason: "bad".to_string(),
        };
        assert_eq!(err.to_string(), "adapter `x` failed: bad");
    }

    /// Proves the [`Adapter`] trait is object-safe and usable via the same
    /// conversion logic as the free function.
    struct UpperCurrency;
    impl Adapter for UpperCurrency {
        fn convert(&self, input: &str) -> Result<String, AdapterError> {
            uppercase_currency(input)
        }
    }

    #[test]
    fn test_adapter_trait_object_convert() {
        let a: &dyn Adapter = &UpperCurrency;
        assert_eq!(a.convert("eur").unwrap(), "EUR");
    }
}
