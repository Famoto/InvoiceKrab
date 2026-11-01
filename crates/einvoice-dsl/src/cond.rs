//! Conditions: the entry forms of a `match` table.
//!
//! Each entry names a value and says what it must be:
//!
//! | Form | Holds when the value |
//! |---|---|
//! | `Code = "50"` | equals `50` (trimmed) |
//! | `Code = ["50", "130"]` | equals one of them |
//! | `Code = { present = true }` | exists (`false`: does not exist) |
//! | `Code = "$root.Key"` | equals the document's value for root key `Key` |
//!
//! All entries of a table must hold. On write, a `match` writes its values back
//! as discriminators: a literal or reference always, the first of a list when
//! the element has none of its own, and nothing for `present`.

use serde::Deserialize;

/// One entry of a condition table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// Equals this text.
    Is(String),
    /// Equals one of these texts.
    OneOf(Vec<String>),
    /// Exists (`true`) or does not (`false`).
    Present(Presence),
}

/// The `{ present = … }` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    /// Whether the value must exist.
    pub present: bool,
}

impl Condition {
    /// The texts the condition accepts, `None` for a presence test.
    pub fn values(&self) -> Option<&[String]> {
        match self {
            Condition::Is(v) => Some(std::slice::from_ref(v)),
            Condition::OneOf(vs) => Some(vs),
            Condition::Present(_) => None,
        }
    }

    /// The `$…` reference an `Is` condition names (`$root.TaxCurrency`).
    pub fn reference(&self) -> Option<&str> {
        match self {
            Condition::Is(v) if v.starts_with('$') => Some(v),
            _ => None,
        }
    }

    /// Whether no value can satisfy both conditions.
    pub fn excludes(&self, other: &Condition) -> bool {
        match (self, other) {
            (Condition::Present(a), Condition::Present(b)) => a.present != b.present,
            // A value that must be absent equals nothing.
            (Condition::Present(p), _) | (_, Condition::Present(p)) => !p.present,
            // A reference's value is only known at run time.
            _ if self.reference().is_some() || other.reference().is_some() => false,
            _ => {
                let (a, b) = (self.values().unwrap_or(&[]), other.values().unwrap_or(&[]));
                !a.iter().any(|v| b.contains(v))
            }
        }
    }

    /// What is wrong with the condition as written: an empty list, or a
    /// reference inside a list.
    pub fn problem(&self) -> Option<&'static str> {
        match self {
            Condition::OneOf(vs) if vs.is_empty() => Some("an empty list matches nothing"),
            Condition::OneOf(vs) if vs.iter().any(|v| v.starts_with('$')) => {
                Some("a `$` reference stands alone, not in a list")
            }
            _ => None,
        }
    }

    /// The condition as the author wrote it, for reports and the contract.
    pub fn display(&self) -> String {
        match self {
            Condition::Is(v) => v.clone(),
            Condition::OneOf(vs) => vs.join(" | "),
            Condition::Present(Presence { present: true }) => "(present)".to_string(),
            Condition::Present(Presence { present: false }) => "(absent)".to_string(),
        }
    }
}

impl From<&str> for Condition {
    fn from(v: &str) -> Self {
        Condition::Is(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_value: &str) -> Condition {
        #[derive(Deserialize)]
        struct T {
            c: Condition,
        }
        toml::from_str::<T>(&format!("c = {toml_value}")).unwrap().c
    }

    #[test]
    fn test_forms_parse() {
        assert_eq!(parse("\"50\""), Condition::Is("50".into()));
        assert_eq!(
            parse("[\"50\", \"130\"]"),
            Condition::OneOf(vec!["50".into(), "130".into()])
        );
        assert_eq!(
            parse("{ present = false }"),
            Condition::Present(Presence { present: false })
        );
        for bad in ["5", "{ present = 1 }", "{ absent = true }", "[1]"] {
            #[derive(Deserialize, Debug)]
            #[allow(dead_code)]
            struct T {
                c: Condition,
            }
            assert!(toml::from_str::<T>(&format!("c = {bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_excludes() {
        let is = |v: &str| Condition::Is(v.into());
        let list = |vs: &[&str]| Condition::OneOf(vs.iter().map(|v| v.to_string()).collect());
        let present = |p| Condition::Present(Presence { present: p });
        assert!(is("50").excludes(&is("51")));
        assert!(!is("50").excludes(&is("50")));
        assert!(!list(&["50", "130"]).excludes(&is("130")));
        assert!(list(&["50", "130"]).excludes(&list(&["1", "2"])));
        assert!(present(true).excludes(&present(false)));
        assert!(!present(true).excludes(&present(true)));
        assert!(present(false).excludes(&is("50")), "absent equals nothing");
        assert!(!present(true).excludes(&is("50")));
        assert!(list(&[]).problem().is_some());
        assert!(list(&["$root.A"]).problem().is_some());
        assert!(
            !is("$root.A").excludes(&is("50")),
            "a reference may equal anything"
        );
        assert!(present(false).excludes(&is("$root.A")));
    }
}
