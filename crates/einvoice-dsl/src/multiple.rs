//! Multiple-value policy.
//!
//! Controls how the generated mapper handles a *scalar element leaf* that may
//! appear more than once in the source document. Declaring `multiple` changes
//! the synthesized source field to `Vec<String>` (so repeated elements parse
//! instead of failing deserialization); the policy then decides how the values
//! collapse into the single canonical value. A node without `multiple` is
//! strictly single-valued: a repeated element fails source deserialization.
//!
//! # Structure
//!
//! - [`MultiplePolicy`] — the policy keyword.
//!
//! # Behavior
//!
//! [`MultiplePolicy::Join`] requires a `join_with` separator; every other policy
//! forbids it (validated by the compiler). `multiple` is only valid on a plain
//! scalar element leaf — not on collections, attributes, `$text` overrides, or
//! valued containers — and cannot be combined with `fallbacks`.

use serde::Deserialize;

/// How repeated scalar values collapse into the canonical value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiplePolicy {
    /// Runtime diagnostic error if more than one value is found.
    Error,
    /// Use the first value in source order; warn when more were present.
    First,
    /// Join all values in source order using `join_with`.
    Join,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        multiple: MultiplePolicy,
    }

    #[test]
    fn test_policies_parse_from_keywords() {
        for (kw, expected) in [
            ("error", MultiplePolicy::Error),
            ("first", MultiplePolicy::First),
            ("join", MultiplePolicy::Join),
        ] {
            let h: Holder =
                toml::from_str(&format!("multiple = \"{kw}\"")).expect("keyword parses");
            assert_eq!(h.multiple, expected);
        }
    }

    #[test]
    fn test_array_policy_is_rejected_at_parse() {
        // `array` was documented but never implemented; it is no longer part of
        // the DSL surface, so parsing rejects it with the valid variants listed.
        let err = toml::from_str::<Holder>("multiple = \"array\"").unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }
}
