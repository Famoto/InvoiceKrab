//! Mapping value types.
//!
//! The closed set of types a source node may declare. `amount` is intentionally
//! excluded from the minimal set: an amount value and its currency are mapped as
//! separate nodes (`decimal` + `currency`).
//!
//! # Structure
//!
//! - [`MappingType`] — the type tag carried by every active source node.
//!
//! # Behavior
//!
//! Parsing is total and case-sensitive (TOML lower-case keywords). An unknown
//! keyword is rejected by deserialization (the E001 unknown-value diagnostic).

use std::fmt;

use serde::Deserialize;

/// The type of a source node's value.
///
/// [`MappingType::Collection`] is structural: it marks a node that selects a
/// repeated source item and opens a child scope, rather than a scalar value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingType {
    /// Free text. Optionally empty after normalization.
    String,
    /// An identifier. Empty/whitespace-only after normalization is missing.
    Identifier,
    /// A calendar date.
    Date,
    /// A date-time.
    Datetime,
    /// A scale-preserving decimal (zero is valid).
    Decimal,
    /// An ISO 4217 currency code.
    Currency,
    /// A unit-of-measure code.
    UnitCode,
    /// A boolean.
    Boolean,
    /// A repeated source item; opens a child scope for its child nodes.
    Collection,
}

impl MappingType {
    /// Whether this type marks a collection node (structural, opens a scope).
    pub fn is_collection(self) -> bool {
        matches!(self, MappingType::Collection)
    }

    /// The canonical lower-case keyword for this type.
    pub fn as_str(self) -> &'static str {
        match self {
            MappingType::String => "string",
            MappingType::Identifier => "identifier",
            MappingType::Date => "date",
            MappingType::Datetime => "datetime",
            MappingType::Decimal => "decimal",
            MappingType::Currency => "currency",
            MappingType::UnitCode => "unit_code",
            MappingType::Boolean => "boolean",
            MappingType::Collection => "collection",
        }
    }
}

impl fmt::Display for MappingType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("string", MappingType::String)]
    #[case("identifier", MappingType::Identifier)]
    #[case("date", MappingType::Date)]
    #[case("datetime", MappingType::Datetime)]
    #[case("decimal", MappingType::Decimal)]
    #[case("currency", MappingType::Currency)]
    #[case("unit_code", MappingType::UnitCode)]
    #[case("boolean", MappingType::Boolean)]
    #[case("collection", MappingType::Collection)]
    fn test_as_str_roundtrips_keyword(#[case] input: &str, #[case] expected: MappingType) {
        assert_eq!(expected.as_str(), input);
    }

    #[test]
    fn test_collection_is_structural() {
        assert!(MappingType::Collection.is_collection());
        assert!(!MappingType::Decimal.is_collection());
    }

    #[test]
    fn test_deserialize_snake_case() {
        #[derive(Deserialize)]
        struct Holder {
            ty: MappingType,
        }
        let h: Holder = toml::from_str(r#"ty = "unit_code""#).unwrap();
        assert_eq!(h.ty, MappingType::UnitCode);
    }
}
