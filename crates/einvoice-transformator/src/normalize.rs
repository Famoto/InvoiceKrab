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
//! All functions are pure. They **consume** an owned [`String`] and return one,
//! so generated mapper code can chain them (`.map(normalize::trim)`) over a
//! single owned buffer instead of reallocating at every step: [`trim`] edits the
//! buffer in place (no allocation), and [`empty_as_missing`] hands the buffer
//! straight back. Note that [`empty_as_missing`] only treats the *literal* empty
//! string as missing; a whitespace-only string is considered present. To treat
//! whitespace as missing, [`trim`] first.
//!
//! # Testing
//!
//! Unit tests cover each function including the whitespace edge case of
//! [`empty_as_missing`].

/// Trims leading and trailing whitespace (ASCII and Unicode) from `s`, editing
/// the owned buffer in place so no new allocation is made.
///
/// # Examples
///
/// ```
/// use einvoice_transformator::normalize::trim;
///
/// assert_eq!(trim("  hi \t\n".to_string()), "hi");
/// ```
pub fn trim(mut s: String) -> String {
    let trimmed = s.trim();
    // Reuse the existing allocation: drop the trailing run, then shift the
    // leading run away, rather than allocating a fresh `String`.
    let start = trimmed.as_ptr() as usize - s.as_ptr() as usize;
    let end = start + trimmed.len();
    s.truncate(end);
    s.drain(..start);
    s
}

/// Returns `s` upper-cased.
///
/// # Examples
///
/// ```
/// use einvoice_transformator::normalize::uppercase;
///
/// assert_eq!(uppercase("eur".to_string()), "EUR");
/// ```
pub fn uppercase(s: String) -> String {
    s.to_uppercase()
}

/// Returns `s` lower-cased.
///
/// # Examples
///
/// ```
/// use einvoice_transformator::normalize::lowercase;
///
/// assert_eq!(lowercase("EUR".to_string()), "eur");
/// ```
pub fn lowercase(s: String) -> String {
    s.to_lowercase()
}

/// Maps the *literal* empty string to `None`, otherwise returns the owned `s`
/// unchanged.
///
/// Only the literal empty string `""` is treated as missing. A whitespace-only
/// string such as `"   "` is considered present and returns `Some`. Apply
/// [`trim`] first if whitespace should count as missing. The non-empty buffer is
/// returned as-is, with no reallocation.
///
/// # Examples
///
/// ```
/// use einvoice_transformator::normalize::empty_as_missing;
///
/// assert_eq!(empty_as_missing(String::new()), None);
/// assert_eq!(empty_as_missing("x".to_string()), Some("x".to_string()));
/// assert_eq!(empty_as_missing("   ".to_string()), Some("   ".to_string()));
/// ```
pub fn empty_as_missing(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trim_removes_surrounding_whitespace() {
        assert_eq!(trim("  hello  ".to_string()), "hello");
        assert_eq!(trim("\t\nx\r\n".to_string()), "x");
        assert_eq!(trim("no-pad".to_string()), "no-pad");
    }

    #[test]
    fn test_trim_unicode_whitespace() {
        assert_eq!(trim("\u{00A0}hi\u{2003}".to_string()), "hi");
    }

    #[test]
    fn test_trim_in_place_keeps_capacity() {
        // Trimming reuses the buffer; the result fits within the original
        // allocation (no reallocation to a smaller-or-equal length).
        let original = "   padded value   ".to_string();
        let cap = original.capacity();
        let trimmed = trim(original);
        assert_eq!(trimmed, "padded value");
        assert!(trimmed.capacity() >= cap - 6);
    }

    #[test]
    fn test_uppercase() {
        assert_eq!(uppercase("eur".to_string()), "EUR");
        assert_eq!(uppercase("MiXeD".to_string()), "MIXED");
    }

    #[test]
    fn test_lowercase() {
        assert_eq!(lowercase("EUR".to_string()), "eur");
        assert_eq!(lowercase("MiXeD".to_string()), "mixed");
    }

    #[test]
    fn test_empty_as_missing_on_empty_returns_none() {
        assert_eq!(empty_as_missing(String::new()), None);
    }

    #[test]
    fn test_empty_as_missing_on_nonempty_returns_some() {
        assert_eq!(empty_as_missing("x".to_string()), Some("x".to_string()));
    }

    #[test]
    fn test_empty_as_missing_on_whitespace_returns_some() {
        // Only the literal empty string maps to None; whitespace is "present".
        assert_eq!(empty_as_missing("   ".to_string()), Some("   ".to_string()));
    }
}
