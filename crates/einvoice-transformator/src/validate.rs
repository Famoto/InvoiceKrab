//! Lexical validation for typed canonical values.
//!
//! Generated reader code decodes a normalized source string into a [`Value`].
//! For the textual types that have a defined lexical form
//! (`currency`/`date`/`datetime`/`unit_code`) the generated code first calls one
//! of these predicates and emits a `TYPE_INVALID` diagnostic when it returns
//! `false`. `string` and `identifier` have no lexical form here (an identifier's
//! non-emptiness is enforced upstream by [`crate::normalize::empty_as_missing`]).
//!
//! # Structure
//!
//! - [`is_currency`] — ISO 4217 *shape*: three ASCII uppercase letters.
//! - [`is_date`] — `YYYY-MM-DD` calendar shape.
//! - [`is_datetime`] — `YYYY-MM-DDThh:mm:ss` with an optional fraction/zone.
//! - [`is_unit_code`] — UN/ECE Rec 20 *shape*: 1–3 ASCII alphanumerics.
//!
//! # Behavior
//!
//! These are deliberately **shape** checks, not registry lookups: they reject
//! obviously malformed values without embedding the full ISO 4217 / UN/ECE code
//! lists. They are pure and allocation-free.
//!
//! # Testing
//!
//! Unit tests cover accepted forms and the main rejection paths for each
//! predicate.

/// Returns whether `s` has the ISO 4217 currency-code shape: exactly three
/// ASCII uppercase letters (e.g. `EUR`).
pub fn is_currency(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// Returns whether `s` has the UN/ECE Rec 20 unit-code shape: 1–3 ASCII
/// alphanumeric characters (e.g. `C62`, `KGM`, `H87`).
pub fn is_unit_code(s: &str) -> bool {
    (1..=3).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Returns whether `s` is a `YYYY-MM-DD` calendar date with an in-range month
/// (01–12) and day (01–31). This is a shape check; it does not reject e.g.
/// February 30.
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    if !(b[0..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Returns whether `s` is an ISO 8601 date-time: a [`is_date`] date, a `T`
/// separator, and a `hh:mm:ss` time, optionally followed by a fractional second
/// and/or a `Z`/`±hh:mm` zone. The time components and zone offset are
/// range-checked by shape only.
pub fn is_datetime(s: &str) -> bool {
    let Some((date, rest)) = s.split_once('T') else {
        return false;
    };
    if !is_date(date) {
        return false;
    }
    let b = rest.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return false;
    }
    if !(b[0..2].iter().all(u8::is_ascii_digit)
        && b[3..5].iter().all(u8::is_ascii_digit)
        && b[6..8].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let hour = (b[0] - b'0') * 10 + (b[1] - b'0');
    let min = (b[3] - b'0') * 10 + (b[4] - b'0');
    let sec = (b[6] - b'0') * 10 + (b[7] - b'0');
    if hour > 23 || min > 59 || sec > 59 {
        return false;
    }
    // Optional trailing fraction/zone: accept the common forms without a full
    // grammar (e.g. `.123`, `Z`, `+01:00`).
    let tail = &rest[8..];
    tail.is_empty()
        || tail == "Z"
        || tail.starts_with('.')
        || tail.starts_with('+')
        || tail.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_currency_accepts_three_upper_letters() {
        assert!(is_currency("EUR"));
        assert!(is_currency("USD"));
    }

    #[test]
    fn test_is_currency_rejects_wrong_shape() {
        assert!(!is_currency("eur"));
        assert!(!is_currency("EU"));
        assert!(!is_currency("EURO"));
        assert!(!is_currency("E1R"));
    }

    #[test]
    fn test_is_unit_code_accepts_short_alnum() {
        assert!(is_unit_code("C62"));
        assert!(is_unit_code("KGM"));
        assert!(is_unit_code("EA"));
    }

    #[test]
    fn test_is_unit_code_rejects_empty_or_long_or_symbol() {
        assert!(!is_unit_code(""));
        assert!(!is_unit_code("ABCD"));
        assert!(!is_unit_code("A-B"));
    }

    #[test]
    fn test_is_date_accepts_valid_calendar_shape() {
        assert!(is_date("2026-06-27"));
        assert!(is_date("2000-01-01"));
    }

    #[test]
    fn test_is_date_rejects_bad_shape_or_range() {
        assert!(!is_date("2026-6-27"));
        assert!(!is_date("2026/06/27"));
        assert!(!is_date("2026-13-01"));
        assert!(!is_date("2026-06-32"));
        assert!(!is_date("not-a-date"));
    }

    #[test]
    fn test_is_datetime_accepts_iso_forms() {
        assert!(is_datetime("2026-06-27T10:30:00"));
        assert!(is_datetime("2026-06-27T10:30:00Z"));
        assert!(is_datetime("2026-06-27T10:30:00.500"));
        assert!(is_datetime("2026-06-27T10:30:00+01:00"));
    }

    #[test]
    fn test_is_datetime_rejects_bad_forms() {
        assert!(!is_datetime("2026-06-27"));
        assert!(!is_datetime("2026-06-27 10:30:00"));
        assert!(!is_datetime("2026-06-27T25:00:00"));
        assert!(!is_datetime("2026-06-27T10:60:00"));
    }
}
