//! Lexical codecs: the shared, declarative translation between a canonical
//! value and the lexical form a format writes it in.
//!
//! The hub stores every date as ISO `YYYY-MM-DD`, every date-time as ISO
//! `YYYY-MM-DDThh:mm:ss`, and booleans natively. Formats disagree on the wire
//! form — CII writes `<udt:DateTimeString format="102">20260718</udt:DateTimeString>`
//! — so a node names a **codec** and the compiler emits the decode on read and
//! the encode (plus any fixed *wire attributes*) on write. Codecs live in their
//! own files (`config/codecs/*.toml`), are loaded before the mappings, and are
//! shared by every mapping:
//!
//! ```toml
//! [codec.cii-date-102]
//! for_type = "date"
//! lexical  = "YYYYMMDD"
//! wire     = { "@format" = "102" }
//! ```
//!
//! # Structure
//!
//! - [`Codec`] — one codec: id, canonical type, compiled [`Pattern`], wire
//!   attributes.
//! - [`Pattern`] / [`Token`] — the compiled lexical pattern.
//! - [`CodecTable`] — codecs by id, as the compiler consumes them.
//! - [`parse_codecs`] — one `*.toml` file → codecs (patterns validated).
//! - [`compile_pattern`] — the pattern language, validated against the type.
//!
//! # The pattern language
//!
//! Deliberately small and fully checked at build time. For `date` and
//! `datetime` a pattern is a sequence of the tokens `YYYY`, `MM`, `DD`, `hh`,
//! `mm`, `ss` and literal separators from `- . / : T` and space: a `date` uses
//! `YYYY`, `MM` and `DD` exactly once and no time token; a `datetime` adds `hh`
//! and `mm` exactly once and `ss` at most once. For `boolean` the pattern is
//! `yes|no`: the two lexical literals, distinct and non-empty. The runtime
//! (`einvoice_transformator::codec`) interprets the same strings; the compiler
//! guarantees it only ever sees patterns that passed here.
//!
//! Four further kinds replace `lexical` (exactly one kind per codec):
//!
//! - `values = [["380", "TD01"], …]` — a code table for code-valued types;
//!   encode takes the first pair with the canonical value, decode the first
//!   with the wire value, and an empty wire value writes nothing.
//! - `fraction_digits = [min, max]` — a `decimal` written with `min`..`max`
//!   fraction digits (padded, trailing zeros trimmed, never rounded).
//! - `charset = "latin-1"` — text restricted to ISO 8859-1.
//! - `split = { at = 2, into = ["IdPaese", "IdCodice"] }` — one value written
//!   as two child elements of the node's element and read as their
//!   concatenation.
//!
//! Two optional *settings* may accompany the kind:
//!
//! - `rounding = "half_up" | "half_even" | "down" | "up"` — on a
//!   `fraction_digits` codec, a value with more than `max` fraction digits is
//!   rounded to `max` with this mode instead of being refused.
//! - `max_length = n` — on a text codec, an encoded value longer than `n`
//!   characters is refused on write (`VALUE_TOO_LONG`) and warned about on
//!   read. A codec may consist of `max_length` alone (no translation kind).
//!
//! A mapping's `[meta.defaults]` sets the same two for every node of the
//! mapping; a codec's own setting wins (see [`effective_rounding`]).
//!
//! # Behavior
//!
//! Parsing is total: a file with an unknown key, an unknown `for_type`, an
//! invalid id, an invalid pattern, or a malformed `wire` entry is a
//! [`ConfigError`] (codecs are configuration, loaded before compilation starts).
//! Duplicate ids across files are rejected by the loader. Node-level problems —
//! an unknown codec id (E084), a codec whose type disagrees with the node's
//! (E085), a wire attribute colliding with a declared attribute node (E087) —
//! are compile diagnostics.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::types::MappingType;

/// One lexical codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    /// Globally unique, stable id (`cii-date-102`).
    pub id: String,
    /// The canonical type this codec translates.
    pub for_type: MappingType,
    /// The lexical pattern as authored (handed to the runtime verbatim), or for
    /// the other kinds a one-line summary used in diagnostics.
    pub lexical: String,
    /// The compiled pattern.
    pub pattern: Pattern,
    /// Fixed attributes written on the element next to the encoded value, by
    /// attribute local name (`format` → `102`). Ignored on read.
    pub wire: BTreeMap<String, String>,
    /// Human description (reports only).
    pub description: Option<String>,
    /// How a `fraction_digits` value with more than `max` digits is rounded;
    /// `None` refuses it (the mapping's `[meta.defaults]` may still supply a
    /// mode).
    pub rounding: Option<Rounding>,
    /// The most characters an encoded text value may have.
    pub max_length: Option<usize>,
}

/// A rounding mode for a `fraction_digits` codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    /// Half away from zero (commercial rounding): 2.345 → 2.35, -2.345 → -2.35.
    HalfUp,
    /// Half to even (banker's rounding): 2.345 → 2.34, 2.355 → 2.36.
    HalfEven,
    /// Toward zero (truncation of the extra digits).
    Down,
    /// Away from zero.
    Up,
}

impl Rounding {
    /// The `rust_decimal::RoundingStrategy` variant the generated code uses.
    pub fn strategy(self) -> &'static str {
        match self {
            Rounding::HalfUp => "MidpointAwayFromZero",
            Rounding::HalfEven => "MidpointNearestEven",
            Rounding::Down => "ToZero",
            Rounding::Up => "AwayFromZero",
        }
    }

    /// The keyword as authored.
    pub fn as_str(self) -> &'static str {
        match self {
            Rounding::HalfUp => "half_up",
            Rounding::HalfEven => "half_even",
            Rounding::Down => "down",
            Rounding::Up => "up",
        }
    }
}

/// The rounding mode a node's codec applies: the codec's own `rounding`, else
/// the mapping default — and only for a `fraction_digits` codec, the one kind
/// that has a digit limit to round to.
pub fn effective_rounding(codec: &Codec, default: Option<Rounding>) -> Option<Rounding> {
    matches!(codec.pattern, Pattern::Fraction { .. })
        .then(|| codec.rounding.or(default))
        .flatten()
}

/// A compiled lexical pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// Calendar tokens and literal separators, for `date` / `datetime`.
    Temporal(Vec<Token>),
    /// The two boolean literals, for `boolean`.
    Boolean {
        /// The lexical form of `true`.
        yes: String,
        /// The lexical form of `false`.
        no: String,
    },
    /// A code translation table, `(canonical, wire)` pairs in declared order:
    /// encoding takes the first pair with the canonical value, decoding the
    /// first with the wire value, so several codes may share one on either
    /// side. An empty wire value writes nothing. For code-valued types.
    Values(Vec<(String, String)>),
    /// A decimal written with between `min` and `max` fraction digits
    /// (zero-padded; trailing zeros trimmed; never rounded).
    Fraction {
        /// Fewest fraction digits written.
        min: usize,
        /// Most fraction digits a value may need.
        max: usize,
    },
    /// Text restricted to ISO 8859-1 (typographic punctuation transliterated).
    Latin1,
    /// Exactly this many ASCII digits (an Italian CAP); any other value is
    /// refused on write, never padded or cut.
    Digits(usize),
    /// One value written as two child elements: its first `at` characters
    /// into `head`, the rest into `tail`; read back as their concatenation.
    Split {
        /// Characters in the head part.
        at: usize,
        /// Local name of the head element.
        head: String,
        /// Local name of the tail element.
        tail: String,
    },
    /// No translation: the value is written and read as it is. Only a codec
    /// that declares settings alone (`max_length`) has it.
    Plain,
}

/// One element of a temporal pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// `YYYY`
    Year,
    /// `MM`
    Month,
    /// `DD`
    Day,
    /// `hh`
    Hour,
    /// `mm`
    Minute,
    /// `ss`
    Second,
    /// A literal separator.
    Literal(char),
}

/// Codecs by id.
pub type CodecTable = BTreeMap<String, Codec>;

/// Literal separators a `date` pattern may use.
const DATE_LITERALS: &[char] = &['-', '.', '/'];
/// Literal separators a `datetime` pattern may use.
const DATETIME_LITERALS: &[char] = &['-', '.', '/', ':', 'T', ' '];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodec {
    for_type: MappingType,
    #[serde(default)]
    lexical: Option<String>,
    #[serde(default)]
    values: Option<Vec<(String, String)>>,
    #[serde(default)]
    fraction_digits: Option<(usize, usize)>,
    #[serde(default)]
    charset: Option<String>,
    #[serde(default)]
    digits: Option<usize>,
    #[serde(default)]
    split: Option<RawSplit>,
    #[serde(default)]
    wire: BTreeMap<String, String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    rounding: Option<Rounding>,
    #[serde(default)]
    max_length: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSplit {
    at: usize,
    into: (String, String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodecFile {
    #[serde(default)]
    codec: BTreeMap<String, RawCodec>,
}

/// Parses one codec file (`[codec.<id>]` tables) into validated codecs, in id
/// order.
///
/// # Errors
///
/// Any unknown key, unknown `for_type`, invalid id, invalid pattern, or
/// malformed `wire` entry is a [`ConfigError`] naming the codec.
pub fn parse_codecs(src: &str) -> Result<Vec<Codec>, ConfigError> {
    let file: CodecFile = toml::from_str(src)?;
    let mut out = Vec::with_capacity(file.codec.len());
    for (id, raw) in file.codec {
        let codec =
            build_codec(&id, raw).map_err(|e| ConfigError::msg(format!("codec `{id}`: {e}")))?;
        out.push(codec);
    }
    Ok(out)
}

fn build_codec(id: &str, raw: RawCodec) -> Result<Codec, String> {
    validate_id(id)?;
    let (lexical, pattern) = codec_kind(&raw)?;
    if matches!(pattern, Pattern::Split { .. }) && !raw.wire.is_empty() {
        return Err("a `split` codec writes elements, not wire attributes".to_string());
    }
    let mut wire = BTreeMap::new();
    for (key, value) in raw.wire {
        let Some(name) = key.strip_prefix('@') else {
            return Err(format!("wire key `{key}` must name an attribute (`@name`)"));
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(format!("wire key `{key}` is not a valid attribute name"));
        }
        if value.is_empty() {
            return Err(format!("wire attribute `{key}` has an empty value"));
        }
        wire.insert(name.to_string(), value);
    }
    if raw.rounding.is_some() && !matches!(pattern, Pattern::Fraction { .. }) {
        return Err("`rounding` needs a `fraction_digits` codec".to_string());
    }
    if let Some(n) = raw.max_length {
        let text_like = matches!(
            raw.for_type,
            MappingType::String
                | MappingType::Identifier
                | MappingType::Currency
                | MappingType::UnitCode
        );
        if !text_like {
            return Err(format!(
                "`max_length` is for text codecs, not `{}`",
                raw.for_type
            ));
        }
        if n == 0 {
            return Err("`max_length` must be at least 1".to_string());
        }
        if matches!(pattern, Pattern::Split { .. }) {
            return Err("`max_length` does not combine with `split`".to_string());
        }
    }
    Ok(Codec {
        id: id.to_string(),
        for_type: raw.for_type,
        lexical,
        pattern,
        wire,
        description: raw.description,
        rounding: raw.rounding,
        max_length: raw.max_length,
    })
}

/// The one codec kind a raw codec declares — `lexical`, `values`,
/// `fraction_digits`, `charset`, `digits` or `split` — validated against `for_type`,
/// with the summary diagnostics quote it by.
fn codec_kind(raw: &RawCodec) -> Result<(String, Pattern), String> {
    let declared = [
        raw.lexical.is_some(),
        raw.values.is_some(),
        raw.fraction_digits.is_some(),
        raw.charset.is_some(),
        raw.digits.is_some(),
        raw.split.is_some(),
    ];
    let kinds = declared.iter().filter(|d| **d).count();
    // A codec may consist of a length limit alone: no translation.
    if kinds == 0
        && raw.rounding.is_none()
        && let Some(n) = raw.max_length
    {
        return Ok((format!("max_length {n}"), Pattern::Plain));
    }
    if kinds != 1 {
        return Err(
            "declare exactly one of `lexical`, `values`, `fraction_digits`, `charset`, `digits`, `split` (or `max_length` alone)"
                .to_string(),
        );
    }
    let ty = raw.for_type;
    let text_like = matches!(ty, MappingType::String | MappingType::Identifier);
    let code_like = text_like || matches!(ty, MappingType::Currency | MappingType::UnitCode);
    let need = |ok: bool, kind: &str, types: &str| {
        if ok {
            Ok(())
        } else {
            Err(format!("a `{kind}` codec is for {types}, not `{ty}`"))
        }
    };
    if let Some(lexical) = &raw.lexical {
        return Ok((lexical.clone(), compile_pattern(ty, lexical)?));
    }
    if let Some(values) = &raw.values {
        need(
            code_like,
            "values",
            "`string`, `identifier`, `currency` or `unit_code`",
        )?;
        if values.is_empty() {
            return Err("`values` needs at least one `[canonical, wire]` pair".to_string());
        }
        if let Some((c, _)) = values.iter().find(|(c, _)| c.trim().is_empty()) {
            return Err(format!("`values` has an empty canonical value `{c}`"));
        }
        return Ok(("values".to_string(), Pattern::Values(values.clone())));
    }
    if let Some((min, max)) = raw.fraction_digits {
        need(ty == MappingType::Decimal, "fraction_digits", "`decimal`")?;
        if min > max || max > 28 {
            return Err(format!(
                "`fraction_digits = [{min}, {max}]` needs min <= max <= 28"
            ));
        }
        return Ok((
            format!("fraction_digits [{min}, {max}]"),
            Pattern::Fraction { min, max },
        ));
    }
    if let Some(charset) = &raw.charset {
        need(text_like, "charset", "`string` or `identifier`")?;
        if charset != "latin-1" {
            return Err(format!(
                "unknown charset `{charset}` (supported: `latin-1`)"
            ));
        }
        return Ok(("charset latin-1".to_string(), Pattern::Latin1));
    }
    if let Some(digits) = raw.digits {
        need(text_like, "digits", "`string` or `identifier`")?;
        if digits == 0 {
            return Err("`digits` must be at least 1".to_string());
        }
        return Ok((format!("{digits} digits"), Pattern::Digits(digits)));
    }
    let split = raw.split.as_ref().expect("exactly one kind is declared");
    need(text_like, "split", "`string` or `identifier`")?;
    let (head, tail) = &split.into;
    if split.at == 0 {
        return Err("`split.at` must be at least 1".to_string());
    }
    for name in [head, tail] {
        if !crate::ident::is_xml_name(name) {
            return Err(format!("`split.into` element `{name}` is not an XML name"));
        }
    }
    if head == tail {
        return Err("`split.into` names two distinct elements".to_string());
    }
    Ok((
        format!("split at {} into {head} + {tail}", split.at),
        Pattern::Split {
            at: split.at,
            head: head.clone(),
            tail: tail.clone(),
        },
    ))
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("id must be non-empty and use only letters, digits, `-` and `_`".to_string());
    }
    Ok(())
}

/// Compiles and validates `lexical` for `for_type` (see the module docs for the
/// language).
///
/// # Errors
///
/// A human-readable reason when the type has no lexical codec or the pattern
/// breaks the rules for the type.
pub fn compile_pattern(for_type: MappingType, lexical: &str) -> Result<Pattern, String> {
    match for_type {
        MappingType::Date => {
            let tokens = tokenize(lexical, DATE_LITERALS)?;
            require_once(&tokens, Token::Year, "YYYY")?;
            require_once(&tokens, Token::Month, "MM")?;
            require_once(&tokens, Token::Day, "DD")?;
            for (tok, name) in [
                (Token::Hour, "hh"),
                (Token::Minute, "mm"),
                (Token::Second, "ss"),
            ] {
                if tokens.contains(&tok) {
                    return Err(format!(
                        "a `date` pattern cannot contain the time token `{name}`"
                    ));
                }
            }
            Ok(Pattern::Temporal(tokens))
        }
        MappingType::Datetime => {
            let tokens = tokenize(lexical, DATETIME_LITERALS)?;
            require_once(&tokens, Token::Year, "YYYY")?;
            require_once(&tokens, Token::Month, "MM")?;
            require_once(&tokens, Token::Day, "DD")?;
            require_once(&tokens, Token::Hour, "hh")?;
            require_once(&tokens, Token::Minute, "mm")?;
            if tokens.iter().filter(|t| **t == Token::Second).count() > 1 {
                return Err("token `ss` may appear at most once".to_string());
            }
            Ok(Pattern::Temporal(tokens))
        }
        MappingType::Boolean => {
            let Some((yes, no)) = lexical.split_once('|') else {
                return Err(
                    "a `boolean` pattern is `yes|no`: the two literals separated by `|`"
                        .to_string(),
                );
            };
            if yes.is_empty() || no.is_empty() {
                return Err("boolean literals must be non-empty".to_string());
            }
            if yes == no {
                return Err("boolean literals must be distinct".to_string());
            }
            if no.contains('|') {
                return Err("a `boolean` pattern has exactly one `|`".to_string());
            }
            Ok(Pattern::Boolean {
                yes: yes.to_string(),
                no: no.to_string(),
            })
        }
        other => Err(format!("there is no lexical codec for type `{other}`")),
    }
}

/// Splits a temporal pattern into tokens; every character outside a token must
/// be one of `literals`.
fn tokenize(lexical: &str, literals: &[char]) -> Result<Vec<Token>, String> {
    if lexical.is_empty() {
        return Err("pattern is empty".to_string());
    }
    let mut tokens = Vec::new();
    let mut rest = lexical;
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
            let c = rest.chars().next().expect("non-empty");
            if !literals.contains(&c) {
                return Err(format!(
                    "`{c}` is neither a token (YYYY, MM, DD, hh, mm, ss) nor an allowed separator ({})",
                    literals
                        .iter()
                        .map(|l| format!("`{l}`"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            (Token::Literal(c), c.len_utf8())
        };
        tokens.push(token);
        rest = &rest[width..];
    }
    Ok(tokens)
}

fn require_once(tokens: &[Token], token: Token, name: &str) -> Result<(), String> {
    match tokens.iter().filter(|t| **t == token).count() {
        1 => Ok(()),
        0 => Err(format!("token `{name}` is required")),
        _ => Err(format!("token `{name}` may appear only once")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn test_parse_codecs_reads_ids_types_patterns_and_wire() {
        let codecs = parse_codecs(
            r#"
            [codec.date-iso]
            for_type = "date"
            lexical = "YYYY-MM-DD"

            [codec.cii-date-102]
            for_type = "date"
            lexical = "YYYYMMDD"
            wire = { "@format" = "102" }
            description = "UN/CEFACT format 102"
            "#,
        )
        .expect("parses");
        assert_eq!(codecs.len(), 2);
        let cii = codecs.iter().find(|c| c.id == "cii-date-102").unwrap();
        assert_eq!(cii.for_type, MappingType::Date);
        assert_eq!(cii.lexical, "YYYYMMDD");
        assert_eq!(cii.wire["format"], "102");
        assert_eq!(cii.description.as_deref(), Some("UN/CEFACT format 102"));
        assert_eq!(
            cii.pattern,
            Pattern::Temporal(vec![Token::Year, Token::Month, Token::Day])
        );
        let iso = codecs.iter().find(|c| c.id == "date-iso").unwrap();
        assert!(iso.wire.is_empty());
    }

    #[test]
    fn test_parse_codecs_reads_the_other_kinds() {
        let codecs = parse_codecs(
            r#"
            [codec.doc-type]
            for_type = "string"
            values = [["380", "TD01"], ["381", "TD04"], ["380", "TD06"]]

            [codec.amount-2]
            for_type = "decimal"
            fraction_digits = [2, 2]

            [codec.text]
            for_type = "string"
            charset = "latin-1"

            [codec.vat-split]
            for_type = "identifier"
            split = { at = 2, into = ["IdPaese", "IdCodice"] }

            [codec.cap]
            for_type = "string"
            digits = 5
            "#,
        )
        .expect("parses");
        let by = |id: &str| codecs.iter().find(|c| c.id == id).unwrap().pattern.clone();
        assert_eq!(
            by("doc-type"),
            Pattern::Values(vec![
                ("380".into(), "TD01".into()),
                ("381".into(), "TD04".into()),
                ("380".into(), "TD06".into()),
            ])
        );
        assert_eq!(by("amount-2"), Pattern::Fraction { min: 2, max: 2 });
        assert_eq!(by("text"), Pattern::Latin1);
        assert_eq!(by("cap"), Pattern::Digits(5));
        assert_eq!(
            by("vat-split"),
            Pattern::Split {
                at: 2,
                head: "IdPaese".into(),
                tail: "IdCodice".into()
            }
        );
    }

    #[rstest]
    #[case::two_kinds(
        "for_type = \"string\"\ncharset = \"latin-1\"\nvalues = [[\"a\", \"b\"]]",
        "exactly one"
    )]
    #[case::no_kind("for_type = \"string\"", "exactly one")]
    #[case::values_on_date("for_type = \"date\"\nvalues = [[\"a\", \"b\"]]", "not `date`")]
    #[case::empty_values("for_type = \"string\"\nvalues = []", "at least one")]
    #[case::fraction_on_string("for_type = \"string\"\nfraction_digits = [2, 2]", "not `string`")]
    #[case::fraction_inverted("for_type = \"decimal\"\nfraction_digits = [3, 2]", "min <= max")]
    #[case::unknown_charset("for_type = \"string\"\ncharset = \"ascii\"", "unknown charset")]
    #[case::digits_zero("for_type = \"string\"\ndigits = 0", "at least 1")]
    #[case::digits_on_decimal("for_type = \"decimal\"\ndigits = 5", "not `decimal`")]
    #[case::split_at_zero(
        "for_type = \"string\"\nsplit = { at = 0, into = [\"A\", \"B\"] }",
        "at least 1"
    )]
    #[case::split_bad_name(
        "for_type = \"string\"\nsplit = { at = 2, into = [\"a b\", \"B\"] }",
        "XML name"
    )]
    #[case::split_same(
        "for_type = \"string\"\nsplit = { at = 2, into = [\"A\", \"A\"] }",
        "distinct"
    )]
    #[case::rounding_without_fraction(
        "for_type = \"string\"\ncharset = \"latin-1\"\nrounding = \"half_up\"",
        "fraction_digits"
    )]
    #[case::rounding_alone("for_type = \"decimal\"\nrounding = \"half_up\"", "exactly one")]
    #[case::max_length_on_decimal(
        "for_type = \"decimal\"\nfraction_digits = [2, 2]\nmax_length = 5",
        "text codecs"
    )]
    #[case::max_length_zero("for_type = \"string\"\nmax_length = 0", "at least 1")]
    #[case::max_length_with_split(
        "for_type = \"string\"\nsplit = { at = 2, into = [\"A\", \"B\"] }\nmax_length = 5",
        "split"
    )]
    #[case::split_with_wire(
        "for_type = \"string\"\nsplit = { at = 2, into = [\"A\", \"B\"] }\nwire = { \"@x\" = \"1\" }",
        "wire"
    )]
    fn test_parse_codecs_rejects_bad_kinds(#[case] body: &str, #[case] needle: &str) {
        let err = parse_codecs(&format!("[codec.x]\n{body}")).unwrap_err();
        assert!(err.message.contains(needle), "{needle}: {}", err.message);
    }

    #[test]
    fn test_parse_codecs_reads_settings() {
        let codecs = parse_codecs(
            r#"
            [codec.amount-8]
            for_type = "decimal"
            fraction_digits = [2, 8]
            rounding = "half_up"

            [codec.text-80]
            for_type = "string"
            charset = "latin-1"
            max_length = 80

            [codec.id-35]
            for_type = "identifier"
            max_length = 35
            "#,
        )
        .expect("parses");
        let by = |id: &str| codecs.iter().find(|c| c.id == id).unwrap().clone();
        assert_eq!(by("amount-8").rounding, Some(Rounding::HalfUp));
        assert_eq!(by("text-80").max_length, Some(80));
        assert_eq!(by("text-80").pattern, Pattern::Latin1);
        assert_eq!(by("id-35").pattern, Pattern::Plain);
        assert_eq!(by("id-35").lexical, "max_length 35");
    }

    #[test]
    fn test_effective_rounding_prefers_the_codec_and_needs_fraction_digits() {
        let codecs = parse_codecs(
            "[codec.a]\nfor_type = \"decimal\"\nfraction_digits = [2, 2]\n\
             [codec.b]\nfor_type = \"decimal\"\nfraction_digits = [2, 2]\nrounding = \"down\"\n\
             [codec.t]\nfor_type = \"string\"\ncharset = \"latin-1\"",
        )
        .expect("parses");
        let by = |id: &str| codecs.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            effective_rounding(by("a"), None),
            None,
            "no rounding by default"
        );
        assert_eq!(
            effective_rounding(by("a"), Some(Rounding::HalfEven)),
            Some(Rounding::HalfEven)
        );
        assert_eq!(
            effective_rounding(by("b"), Some(Rounding::HalfEven)),
            Some(Rounding::Down),
            "the codec's own mode wins"
        );
        assert_eq!(effective_rounding(by("t"), Some(Rounding::Up)), None);
    }

    #[test]
    fn test_parse_codecs_empty_file_is_fine() {
        assert!(parse_codecs("").expect("parses").is_empty());
    }

    #[rstest]
    #[case::unknown_key(
        "[codec.x]\nfor_type = \"date\"\nlexical = \"YYYY-MM-DD\"\nbogus = 1",
        "bogus"
    )]
    #[case::unknown_type("[codec.x]\nfor_type = \"money\"\nlexical = \"x\"", "money")]
    #[case::no_codec_for_type(
        "[codec.x]\nfor_type = \"decimal\"\nlexical = \"x\"",
        "no lexical codec"
    )]
    #[case::bad_id(
        "[codec.\"a b\"]\nfor_type = \"date\"\nlexical = \"YYYY-MM-DD\"",
        "id must"
    )]
    #[case::wire_not_attribute(
        "[codec.x]\nfor_type = \"date\"\nlexical = \"YYYYMMDD\"\nwire = { format = \"102\" }",
        "@name"
    )]
    #[case::wire_empty_value(
        "[codec.x]\nfor_type = \"date\"\nlexical = \"YYYYMMDD\"\nwire = { \"@format\" = \"\" }",
        "empty value"
    )]
    fn test_parse_codecs_rejects(#[case] src: &str, #[case] needle: &str) {
        let err = parse_codecs(src).unwrap_err();
        assert!(err.message.contains(needle), "{}", err.message);
    }

    #[rstest]
    #[case(MappingType::Date, "YYYY-MM-DD")]
    #[case(MappingType::Date, "YYYYMMDD")]
    #[case(MappingType::Date, "DD.MM.YYYY")]
    #[case(MappingType::Datetime, "YYYY-MM-DDThh:mm:ss")]
    #[case(MappingType::Datetime, "YYYYMMDDhhmm")]
    #[case(MappingType::Datetime, "YYYY-MM-DD hh:mm")]
    #[case(MappingType::Boolean, "true|false")]
    #[case(MappingType::Boolean, "1|0")]
    fn test_compile_pattern_accepts(#[case] ty: MappingType, #[case] lexical: &str) {
        compile_pattern(ty, lexical).expect("valid pattern");
    }

    #[rstest]
    #[case::date_missing_day(MappingType::Date, "YYYY-MM", "`DD` is required")]
    #[case::date_twice(MappingType::Date, "YYYY-MM-DD-DD", "only once")]
    #[case::date_with_time(MappingType::Date, "YYYY-MM-DDhh", "time token")]
    #[case::date_bad_separator(MappingType::Date, "YYYY:MM:DD", "separator")]
    #[case::date_lone_letter(MappingType::Date, "YYYY-MM-DDZ", "separator")]
    #[case::datetime_missing_minute(MappingType::Datetime, "YYYYMMDDhh", "`mm` is required")]
    #[case::datetime_seconds_twice(MappingType::Datetime, "YYYYMMDDhhmmssss", "at most once")]
    #[case::bool_no_bar(MappingType::Boolean, "true", "`|`")]
    #[case::bool_same(MappingType::Boolean, "x|x", "distinct")]
    #[case::bool_empty(MappingType::Boolean, "|0", "non-empty")]
    #[case::bool_two_bars(MappingType::Boolean, "a|b|c", "exactly one")]
    #[case::empty(MappingType::Date, "", "empty")]
    #[case::wrong_type(MappingType::Currency, "YYYY", "no lexical codec")]
    fn test_compile_pattern_rejects(
        #[case] ty: MappingType,
        #[case] lexical: &str,
        #[case] needle: &str,
    ) {
        let err = compile_pattern(ty, lexical).unwrap_err();
        assert!(err.contains(needle), "{err}");
    }

    #[test]
    fn test_boolean_pattern_splits_literals() {
        assert_eq!(
            compile_pattern(MappingType::Boolean, "1|0").unwrap(),
            Pattern::Boolean {
                yes: "1".into(),
                no: "0".into()
            }
        );
    }
}
