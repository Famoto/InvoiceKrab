//! Lexical codecs: translate between the hub's canonical forms and a format's
//! wire forms, driven by the small pattern language the compiler validated.
//!
//! The canonical forms are ISO `YYYY-MM-DD` for dates, ISO
//! `YYYY-MM-DDThh:mm:ss` for date-times (an optional fraction / zone suffix is
//! kept on decode input but dropped on encode), and `bool` for booleans. A
//! pattern is a sequence of the tokens `YYYY`, `MM`, `DD`, `hh`, `mm`, `ss`
//! and literal separator characters; a boolean codec is the pair of literals.
//!
//! # Structure
//!
//! - [`decode_date`] / [`encode_date`] — pattern ⇄ ISO date.
//! - [`decode_datetime`] / [`encode_datetime`] — pattern ⇄ ISO date-time.
//! - [`decode_bool`] / [`encode_bool`] — literal pair ⇄ `bool`.
//!
//! # Behavior
//!
//! Pure and allocation-light: one inline string per call. Every function is
//! total: a value that does not match the pattern, a token out of range
//! (month 13, hour 24), or a malformed pattern yields `None`. Generated code
//! only ever passes patterns the compiler accepted, so `None` means the
//! *document* value is wrong and becomes a diagnostic.
//!
//! # Testing
//!
//! Unit tests cover the CII `102` / `204` round trips, ISO identity patterns,
//! separators, range checks, and malformed input.

use compact_str::CompactString;

/// A temporal pattern element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Literal(char),
}

/// The calendar fields a pattern can carry, as ASCII digit strings.
#[derive(Debug, Default, Clone, Copy)]
struct Fields {
    year: [u8; 4],
    month: [u8; 2],
    day: [u8; 2],
    hour: [u8; 2],
    minute: [u8; 2],
    second: [u8; 2],
}

impl Fields {
    fn midnight() -> Self {
        Fields {
            year: *b"0000",
            month: *b"01",
            day: *b"01",
            hour: *b"00",
            minute: *b"00",
            second: *b"00",
        }
    }

    fn in_range(&self) -> bool {
        let n2 = |d: [u8; 2]| (d[0] - b'0') * 10 + (d[1] - b'0');
        (1..=12).contains(&n2(self.month))
            && (1..=31).contains(&n2(self.day))
            && n2(self.hour) <= 23
            && n2(self.minute) <= 59
            && n2(self.second) <= 59
    }
}

/// Splits a pattern into tokens; `None` when it holds a character that is
/// neither a token nor a plausible separator (anything alphanumeric).
fn tokenize(pattern: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut rest = pattern;
    while !rest.is_empty() {
        let (token, width) = if rest.starts_with("YYYY") {
            (Token::Year, 4)
        } else if rest.starts_with("MM") {
            (Token::Month, 2)
        } else if rest.starts_with("DD") {
            (Token::Day, 2)
        } else if rest.starts_with("hh") {
            (Token::Hour, 2)
        } else if rest.starts_with("mm") {
            (Token::Minute, 2)
        } else if rest.starts_with("ss") {
            (Token::Second, 2)
        } else {
            let c = rest.chars().next()?;
            if c.is_ascii_alphanumeric() && c != 'T' {
                return None;
            }
            (Token::Literal(c), c.len_utf8())
        };
        tokens.push(token);
        rest = &rest[width..];
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// Parses `value` against `pattern` into calendar fields.
fn decode_fields(value: &str, pattern: &str) -> Option<Fields> {
    let tokens = tokenize(pattern)?;
    let mut fields = Fields::midnight();
    let mut rest = value;
    for token in tokens {
        match token {
            Token::Literal(c) => {
                rest = rest.strip_prefix(c)?;
            }
            Token::Year => {
                fields.year = take_digits::<4>(&mut rest)?;
            }
            Token::Month => fields.month = take_digits::<2>(&mut rest)?,
            Token::Day => fields.day = take_digits::<2>(&mut rest)?,
            Token::Hour => fields.hour = take_digits::<2>(&mut rest)?,
            Token::Minute => fields.minute = take_digits::<2>(&mut rest)?,
            Token::Second => fields.second = take_digits::<2>(&mut rest)?,
        }
    }
    if !rest.is_empty() || !fields.in_range() {
        return None;
    }
    Some(fields)
}

/// Consumes exactly `N` ASCII digits from the front of `rest`.
fn take_digits<const N: usize>(rest: &mut &str) -> Option<[u8; N]> {
    let bytes = rest.as_bytes();
    if bytes.len() < N || !bytes[..N].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[..N]);
    *rest = &rest[N..];
    Some(out)
}

/// Renders `fields` through `pattern`.
fn encode_fields(fields: &Fields, pattern: &str) -> Option<CompactString> {
    let tokens = tokenize(pattern)?;
    let mut out = CompactString::with_capacity(pattern.len());
    for token in tokens {
        let digits: &[u8] = match token {
            Token::Literal(c) => {
                out.push(c);
                continue;
            }
            Token::Year => &fields.year,
            Token::Month => &fields.month,
            Token::Day => &fields.day,
            Token::Hour => &fields.hour,
            Token::Minute => &fields.minute,
            Token::Second => &fields.second,
        };
        out.push_str(std::str::from_utf8(digits).expect("ASCII digits"));
    }
    Some(out)
}

fn iso_date(fields: &Fields) -> CompactString {
    encode_fields(fields, "YYYY-MM-DD").expect("ISO date pattern is valid")
}

fn iso_datetime(fields: &Fields) -> CompactString {
    encode_fields(fields, "YYYY-MM-DDThh:mm:ss").expect("ISO date-time pattern is valid")
}

/// Decodes a date written in `pattern` into the canonical ISO `YYYY-MM-DD`.
///
/// ```
/// use einvoice_transformator::codec::decode_date;
/// assert_eq!(decode_date("20260718", "YYYYMMDD").as_deref(), Some("2026-07-18"));
/// assert_eq!(decode_date("18.07.2026", "DD.MM.YYYY").as_deref(), Some("2026-07-18"));
/// assert_eq!(decode_date("2026-13-01", "YYYY-MM-DD"), None);
/// ```
pub fn decode_date(value: &str, pattern: &str) -> Option<CompactString> {
    decode_fields(value, pattern).map(|f| iso_date(&f))
}

/// Encodes a canonical ISO `YYYY-MM-DD` date in `pattern`.
///
/// ```
/// use einvoice_transformator::codec::encode_date;
/// assert_eq!(encode_date("2026-07-18", "YYYYMMDD").as_deref(), Some("20260718"));
/// assert_eq!(encode_date("2026/07/18", "YYYYMMDD"), None);
/// ```
pub fn encode_date(iso: &str, pattern: &str) -> Option<CompactString> {
    let fields = decode_fields(iso, "YYYY-MM-DD")?;
    encode_fields(&fields, pattern)
}

/// Decodes a date-time written in `pattern` into the canonical ISO
/// `YYYY-MM-DDThh:mm:ss`. A pattern without `ss` yields `:00` seconds.
///
/// ```
/// use einvoice_transformator::codec::decode_datetime;
/// assert_eq!(
///     decode_datetime("202607181530", "YYYYMMDDhhmm").as_deref(),
///     Some("2026-07-18T15:30:00")
/// );
/// ```
pub fn decode_datetime(value: &str, pattern: &str) -> Option<CompactString> {
    decode_fields(value, pattern).map(|f| iso_datetime(&f))
}

/// Encodes a canonical ISO date-time in `pattern`. A trailing fraction or zone
/// (`.123`, `Z`, `+01:00`) on the input is dropped.
///
/// ```
/// use einvoice_transformator::codec::encode_datetime;
/// assert_eq!(
///     encode_datetime("2026-07-18T15:30:45Z", "YYYYMMDDhhmmss").as_deref(),
///     Some("20260718153045")
/// );
/// ```
pub fn encode_datetime(iso: &str, pattern: &str) -> Option<CompactString> {
    let core = iso.get(..19)?;
    let fields = decode_fields(core, "YYYY-MM-DDThh:mm:ss")?;
    encode_fields(&fields, pattern)
}

/// Decodes a boolean written as one of the two literals.
///
/// ```
/// use einvoice_transformator::codec::decode_bool;
/// assert_eq!(decode_bool("1", "1", "0"), Some(true));
/// assert_eq!(decode_bool("maybe", "1", "0"), None);
/// ```
pub fn decode_bool(value: &str, yes: &str, no: &str) -> Option<bool> {
    if value == yes {
        Some(true)
    } else if value == no {
        Some(false)
    } else {
        None
    }
}

/// Encodes a boolean as one of the two literals.
///
/// ```
/// use einvoice_transformator::codec::encode_bool;
/// assert_eq!(encode_bool(false, "1", "0"), "0");
/// ```
pub fn encode_bool(value: bool, yes: &str, no: &str) -> CompactString {
    CompactString::from(if value { yes } else { no })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cii_format_102_round_trips() {
        assert_eq!(
            decode_date("20260718", "YYYYMMDD").as_deref(),
            Some("2026-07-18")
        );
        assert_eq!(
            encode_date("2026-07-18", "YYYYMMDD").as_deref(),
            Some("20260718")
        );
    }

    #[test]
    fn test_iso_pattern_is_identity() {
        assert_eq!(
            decode_date("2026-07-18", "YYYY-MM-DD").as_deref(),
            Some("2026-07-18")
        );
        assert_eq!(
            encode_date("2026-07-18", "YYYY-MM-DD").as_deref(),
            Some("2026-07-18")
        );
    }

    #[test]
    fn test_separators_and_reordered_tokens() {
        assert_eq!(
            decode_date("18/07/2026", "DD/MM/YYYY").as_deref(),
            Some("2026-07-18")
        );
        assert_eq!(
            encode_date("2026-07-18", "DD.MM.YYYY").as_deref(),
            Some("18.07.2026")
        );
    }

    #[test]
    fn test_decode_rejects_mismatch_trailing_and_range() {
        assert_eq!(decode_date("2026-07-18", "YYYYMMDD"), None, "separators");
        assert_eq!(decode_date("202607181", "YYYYMMDD"), None, "trailing input");
        assert_eq!(decode_date("2026071", "YYYYMMDD"), None, "too short");
        assert_eq!(decode_date("2026AB18", "YYYYMMDD"), None, "non-digit");
        assert_eq!(decode_date("20261318", "YYYYMMDD"), None, "month 13");
        assert_eq!(decode_date("20260700", "YYYYMMDD"), None, "day 0");
    }

    #[test]
    fn test_encode_rejects_non_iso_input() {
        assert_eq!(encode_date("18.07.2026", "YYYYMMDD"), None);
        assert_eq!(encode_date("", "YYYYMMDD"), None);
    }

    #[test]
    fn test_cii_format_204_round_trips_and_drops_zone() {
        assert_eq!(
            decode_datetime("20260718153045", "YYYYMMDDhhmmss").as_deref(),
            Some("2026-07-18T15:30:45")
        );
        assert_eq!(
            encode_datetime("2026-07-18T15:30:45+01:00", "YYYYMMDDhhmmss").as_deref(),
            Some("20260718153045")
        );
        assert_eq!(
            decode_datetime("202607181530", "YYYYMMDDhhmm").as_deref(),
            Some("2026-07-18T15:30:00"),
            "missing seconds default to 00"
        );
        assert_eq!(decode_datetime("20260718243000", "YYYYMMDDhhmmss"), None);
        assert_eq!(
            encode_datetime("2026-07-18", "YYYYMMDDhhmm"),
            None,
            "too short"
        );
    }

    #[test]
    fn test_bool_literals() {
        assert_eq!(decode_bool("true", "true", "false"), Some(true));
        assert_eq!(decode_bool("false", "true", "false"), Some(false));
        assert_eq!(decode_bool("TRUE", "true", "false"), None, "exact match");
        assert_eq!(encode_bool(true, "1", "0"), "1");
    }

    #[test]
    fn test_malformed_pattern_yields_none() {
        assert_eq!(decode_date("2026", "YYYX"), None);
        assert_eq!(encode_date("2026-07-18", ""), None);
    }
}
