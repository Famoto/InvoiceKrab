//! String normalization operations.
//!
//! This module owns the small set of pure, deterministic string transforms that
//! generated mapper code emits as direct function calls (there is intentionally
//! no runtime enum / dispatch — codegen picks the function at build time).
//!
//! # Structure
//!
//! - [`trim`] — strips leading/trailing whitespace.
//! - [`uppercase`] / [`lowercase`] — case folding.
//! - [`empty_as_missing`] — maps the literal empty string to `None`.
//!
//! # Behavior
//!
//! All functions are pure. They **consume** an owned [`CompactString`] and
//! return one, so generated mapper code can chain them
//! (`.map(normalize::trim)`) over a single owned value. Values of 24 bytes or
//! fewer live inline (no heap), so rebuilding one is a stack copy; [`trim`]
//! returns the input untouched when there is nothing to strip, and
//! [`empty_as_missing`] hands the value straight back. Note that
//! [`empty_as_missing`] only treats the *literal* empty string as missing; a
//! whitespace-only string is considered present. To treat whitespace as
//! missing, [`trim`] first.
//!
//! # Testing
//!
//! Unit tests cover each function including the whitespace edge case of
//! [`empty_as_missing`].

use compact_str::CompactString;

/// Trims leading and trailing whitespace (ASCII and Unicode) from `s`.
/// Returns `s` unchanged (no copy) when there is nothing to strip.
///
/// # Examples
///
/// ```
/// use compact_str::CompactString;
/// use einvoice_transformator::normalize::trim;
///
/// assert_eq!(trim(CompactString::from("  hi \t\n")), "hi");
/// ```
pub fn trim(s: CompactString) -> CompactString {
    let trimmed = s.trim();
    if trimmed.len() == s.len() {
        s
    } else {
        CompactString::from(trimmed)
    }
}

/// Returns `s` upper-cased.
///
/// # Examples
///
/// ```
/// use compact_str::CompactString;
/// use einvoice_transformator::normalize::uppercase;
///
/// assert_eq!(uppercase(CompactString::from("eur")), "EUR");
/// ```
pub fn uppercase(s: CompactString) -> CompactString {
    CompactString::from(s.as_str().to_uppercase())
}

/// Returns `s` lower-cased.
///
/// # Examples
///
/// ```
/// use compact_str::CompactString;
/// use einvoice_transformator::normalize::lowercase;
///
/// assert_eq!(lowercase(CompactString::from("EUR")), "eur");
/// ```
pub fn lowercase(s: CompactString) -> CompactString {
    CompactString::from(s.as_str().to_lowercase())
}

/// Maps the *literal* empty string to `None`, otherwise returns the owned `s`
/// unchanged.
///
/// Only the literal empty string `""` is treated as missing. A whitespace-only
/// string such as `"   "` is considered present and returns `Some`. Apply
/// [`trim`] first if whitespace should count as missing.
///
/// # Examples
///
/// ```
/// use compact_str::CompactString;
/// use einvoice_transformator::normalize::empty_as_missing;
///
/// assert_eq!(empty_as_missing(CompactString::new("")), None);
/// assert_eq!(
///     empty_as_missing(CompactString::from("x")),
///     Some(CompactString::from("x"))
/// );
/// assert_eq!(
///     empty_as_missing(CompactString::from("   ")),
///     Some(CompactString::from("   "))
/// );
/// ```
pub fn empty_as_missing(s: CompactString) -> Option<CompactString> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(s: &str) -> CompactString {
        CompactString::from(s)
    }

    #[test]
    fn test_trim_removes_surrounding_whitespace() {
        assert_eq!(trim(cs("  hello  ")), "hello");
        assert_eq!(trim(cs("\t\nx\r\n")), "x");
        assert_eq!(trim(cs("no-pad")), "no-pad");
    }

    #[test]
    fn test_trim_unicode_whitespace() {
        assert_eq!(trim(cs("\u{00A0}hi\u{2003}")), "hi");
    }

    #[test]
    fn test_trim_already_trimmed_heap_string_keeps_allocation() {
        // A value past the inline capacity lives on the heap; when nothing
        // needs stripping the same allocation must come back, not a copy.
        let original = cs("this value is longer than twenty-four bytes");
        assert!(original.is_heap_allocated(), "fixture must be heap");
        let ptr = original.as_ptr();
        let trimmed = trim(original);
        assert_eq!(trimmed, "this value is longer than twenty-four bytes");
        assert_eq!(trimmed.as_ptr(), ptr);
    }

    #[test]
    fn test_uppercase() {
        assert_eq!(uppercase(cs("eur")), "EUR");
        assert_eq!(uppercase(cs("MiXeD")), "MIXED");
    }

    #[test]
    fn test_lowercase() {
        assert_eq!(lowercase(cs("EUR")), "eur");
        assert_eq!(lowercase(cs("MiXeD")), "mixed");
    }

    #[test]
    fn test_empty_as_missing_on_empty_returns_none() {
        assert_eq!(empty_as_missing(CompactString::new("")), None);
    }

    #[test]
    fn test_empty_as_missing_on_nonempty_returns_some() {
        assert_eq!(empty_as_missing(cs("x")), Some(cs("x")));
    }

    #[test]
    fn test_empty_as_missing_on_whitespace_returns_some() {
        // Only the literal empty string maps to None; whitespace is "present".
        assert_eq!(empty_as_missing(cs("   ")), Some(cs("   ")));
    }
}
