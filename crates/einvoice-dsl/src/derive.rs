//! Derivations: canonical values computed from other canonical values when a
//! source does not carry them.
//!
//! EN 16931 defines its document totals by calculation rules (BR-CO-10 to
//! BR-CO-16): the sum of line net amounts, the total without VAT, and so on.
//! A format that omits a total the others require (FatturaPA states no sum of
//! line net amounts) would otherwise make every transform out of it fail.
//! The rules live in `config/derivations.toml`, one `[[derive]]` table each,
//! applied in file order:
//!
//! ```toml
//! [[derive]]
//! key = "SumOfInvoiceLineNetAmount"
//! rule = "BR-CO-10"
//! sum = "InvoiceLines/LineNetAmount"
//!
//! [[derive]]
//! key = "SumOfChargesDocumentLevel"
//! rule = "BR-CO-12"
//! sum = "DocumentAllowanceCharges/AllowanceChargeAmount"
//! where = { ChargeIndicator = "true" }
//!
//! [[derive]]
//! key = "InvoiceTotalWithoutVat"
//! rule = "BR-CO-13"
//! add = ["SumOfInvoiceLineNetAmount", "SumOfChargesDocumentLevel"]
//! subtract = ["SumOfAllowancesDocumentLevel"]
//! ```
//!
//! A rule may also fill in a value EN 16931 demands where a format has none:
//! FatturaPA's `ScontoMaggiorazione` carries no reason, while every allowance
//! needs a reason or reason code (BR-42):
//!
//! ```toml
//! [[derive]]
//! key = "InvoiceLines/LineAllowanceCharges/LineAllowanceChargeReasonCode"
//! rule = "BR-42"
//! value = "95"
//! where = { LineChargeIndicator = "false" }
//! unless = ["LineAllowanceChargeReason"]
//! ```
//!
//! # Semantics
//!
//! A rule only ever fills a key the hub lacks; a value the source carries is
//! never replaced.
//!
//! A `sum` or `add` rule also *checks* a value the hub carries: once every
//! rule has run, the total is recomputed from its operands (with the same
//! presence conditions as deriving it) and a mismatch is reported as a
//! `VALUE_INCONSISTENT` diagnostic. `check` sets its severity: `"warning"`
//! (default), `"error"` (the transform fails), or `"off"` — for a rule that
//! restates another one's equation, such as BR-CO-16 solved for the paid
//! amount. Values compare numerically (`100.0` equals `100.00`).
//!
//! - `sum` adds the values of one decimal key across the items of a root
//!   collection (those matching `where`, an equality on another key of the
//!   item); with no contributing item it derives nothing.
//! - `add` / `subtract` computes from root keys: the first `add` operand and
//!   every `requires` key must be present, any other operand that is absent
//!   counts as zero. `skip_zero = true` derives nothing when the result is
//!   zero, `skip_negative = true` nothing when it is negative. Every key
//!   involved is a root-scope `decimal`.
//!
//! Sums and arithmetic are checked: a result outside the decimal range
//! derives nothing, and a check that overflows is reported as a mismatch.
//! - `value` sets a literal on the target — a root key, or an item key named
//!   by its label (`Collection/…/Key`) on every item matching `where` — when
//!   neither the target nor any `unless` key of the item is present.
//!
//! # Diagnostics
//!
//! [`parse_derivations`] rejects malformed files (a [`ConfigError`]);
//! [`check_derivations`] checks the rules against the derived hub:
//! `E110` a target that is not a root `decimal` key (a `value` target: not a
//! scalar key), or a total computed by two rules (several `value` rules may
//! share a target: each fills only what is still absent), `E111` a `sum` that does not name a decimal
//! key of a root collection, `E112` a `where` / `unless` key that is not a
//! scalar of the scope or a literal that does not fit its type, `E113` an
//! operand that is not a root `decimal` key or a required one derived by a
//! later rule, `E114` a `value` literal that does not fit the target's type.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::error::{ConfigError, Diagnostic, Severity};
use crate::hub::{CanonicalModel, CanonicalScope};
use crate::types::MappingType;
use crate::validate::constant_literal_error;

/// What a mismatch between a carried value and its rule is reported as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckLevel {
    /// Not checked.
    Off,
    /// A warning diagnostic; the transform still succeeds.
    #[default]
    Warning,
    /// An error diagnostic; the transform fails.
    Error,
}

/// One derivation rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    /// The canonical key the rule fills: a root key, or for a `value` rule
    /// possibly an item key's scoped label (`InvoiceLines/LineId`).
    pub key: String,
    /// The rule's reference (`BR-CO-10`), carried into the diagnostics.
    pub rule: String,
    /// How the value is computed.
    pub kind: DerivationKind,
    /// How a carried value that contradicts the rule is reported (`sum` and
    /// `add` rules; always [`CheckLevel::Off`] for `value` rules).
    pub check: CheckLevel,
}

/// How a derivation computes its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivationKind {
    /// The sum of `item` across the items of `collection` that satisfy
    /// `filter` (item key, literal).
    Sum {
        /// The root collection key.
        collection: String,
        /// The decimal item key summed.
        item: String,
        /// Equality filters on other item keys.
        filter: Vec<(String, String)>,
    },
    /// `add[0] + add[1..] - subtract[..]`.
    Arithmetic {
        /// Added operands; the first one must be present.
        add: Vec<String>,
        /// Subtracted operands.
        subtract: Vec<String>,
        /// Further operands that must be present.
        requires: Vec<String>,
        /// Whether a zero result derives nothing.
        skip_zero: bool,
        /// Whether a negative result derives nothing.
        skip_negative: bool,
    },
    /// A literal for the target on every item satisfying `filter` that has
    /// neither the target nor any `unless` key.
    Value {
        /// The literal, in the target's canonical form.
        value: String,
        /// Equality filters on other keys of the item.
        filter: Vec<(String, String)>,
        /// Keys of the item whose presence stops the rule.
        unless: Vec<String>,
    },
}

impl Derivation {
    /// The labels the rule cannot derive without: the summed item label, or
    /// the required operands (the first `add` operand plus `requires`). A
    /// `value` rule needs nothing.
    pub fn needs(&self) -> Vec<String> {
        match &self.kind {
            DerivationKind::Sum {
                collection, item, ..
            } => vec![format!("{collection}/{item}")],
            DerivationKind::Arithmetic { add, requires, .. } => {
                add.iter().take(1).chain(requires).cloned().collect()
            }
            DerivationKind::Value { .. } => Vec::new(),
        }
    }

    /// The target's enclosing collections (outermost first) and its key.
    pub fn target_path(&self) -> (Vec<&str>, &str) {
        let mut segs: Vec<&str> = self.key.split('/').collect();
        let key = segs.pop().expect("split yields at least one segment");
        (segs, key)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    derive: Vec<RawDerivation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDerivation {
    key: String,
    rule: String,
    #[serde(default)]
    sum: Option<String>,
    #[serde(default, rename = "where")]
    filter: Option<toml::Table>,
    #[serde(default)]
    add: Option<Vec<String>>,
    #[serde(default)]
    subtract: Option<Vec<String>>,
    #[serde(default)]
    requires: Option<Vec<String>>,
    #[serde(default)]
    skip_zero: Option<bool>,
    #[serde(default)]
    skip_negative: Option<bool>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    unless: Option<Vec<String>>,
    #[serde(default)]
    check: Option<String>,
}

/// Parses a derivations file into its rules, in file order.
///
/// # Errors
///
/// A TOML error, an unknown field, a rule that declares not exactly one of
/// `sum`, `add` and `value`, a field beside the wrong kind (`where` on `add`;
/// `subtract`, `requires`, `skip_zero`, `skip_negative` off `add`; `unless` off `value`), a
/// `sum` that is not `Collection/Key`, an empty `add`, or a non-string `where`
/// value.
pub fn parse_derivations(src: &str) -> Result<Vec<Derivation>, ConfigError> {
    let file: RawFile = toml::from_str(src)?;
    file.derive
        .into_iter()
        .map(|raw| {
            let key = raw.key.clone();
            build(raw).map_err(|e| ConfigError::msg(format!("derivation of `{key}`: {e}")))
        })
        .collect()
}

fn build(raw: RawDerivation) -> Result<Derivation, String> {
    let kinds = [raw.sum.is_some(), raw.add.is_some(), raw.value.is_some()];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err("declare exactly one of `sum`, `add` and `value`".to_string());
    }
    let off_add = raw.subtract.is_some()
        || raw.requires.is_some()
        || raw.skip_zero.is_some()
        || raw.skip_negative.is_some();
    if raw.add.is_none() && off_add {
        return Err(
            "`subtract`, `requires`, `skip_zero` and `skip_negative` belong to an `add` rule"
                .to_string(),
        );
    }
    if raw.value.is_none() && raw.unless.is_some() {
        return Err("`unless` belongs to a `value` rule".to_string());
    }
    if raw.value.is_some() && raw.check.is_some() {
        return Err("`check` belongs to a `sum` or `add` rule".to_string());
    }
    let check = match raw.check.as_deref() {
        None if raw.value.is_some() => CheckLevel::Off,
        None | Some("warning") => CheckLevel::Warning,
        Some("error") => CheckLevel::Error,
        Some("off") => CheckLevel::Off,
        Some(other) => {
            return Err(format!(
                "`check = \"{other}\"` must be \"warning\", \"error\" or \"off\""
            ));
        }
    };
    let filter = || -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for (k, v) in raw.filter.clone().unwrap_or_default() {
            let toml::Value::String(v) = v else {
                return Err(format!("`where.{k}` must be a string literal"));
            };
            out.push((k, v));
        }
        Ok(out)
    };
    let kind = if let Some(sum) = &raw.sum {
        let Some((collection, item)) = sum.split_once('/') else {
            return Err(format!("`sum = \"{sum}\"` must name `Collection/Key`"));
        };
        DerivationKind::Sum {
            collection: collection.to_string(),
            item: item.to_string(),
            filter: filter()?,
        }
    } else if let Some(add) = &raw.add {
        if raw.filter.is_some() {
            return Err("`where` belongs to a `sum` or `value` rule".to_string());
        }
        if add.is_empty() {
            return Err("`add` needs at least one operand".to_string());
        }
        DerivationKind::Arithmetic {
            add: add.clone(),
            subtract: raw.subtract.clone().unwrap_or_default(),
            requires: raw.requires.clone().unwrap_or_default(),
            skip_zero: raw.skip_zero.unwrap_or(false),
            skip_negative: raw.skip_negative.unwrap_or(false),
        }
    } else {
        DerivationKind::Value {
            value: raw.value.clone().expect("exactly one kind is declared"),
            filter: filter()?,
            unless: raw.unless.clone().unwrap_or_default(),
        }
    };
    Ok(Derivation {
        key: raw.key,
        rule: raw.rule,
        kind,
        check,
    })
}

/// The canonical scope a label's collections name (`["InvoiceLines"]` →
/// the `InvoiceLines` item scope), or `None` when a segment is not a
/// collection of the enclosing scope.
fn scope_of(hub: &CanonicalModel, collections: &[&str]) -> Option<CanonicalScope> {
    let mut scope = CanonicalScope::Root;
    for coll in collections {
        hub.get(&scope, coll).filter(|f| f.is_collection)?;
        scope = scope.child(coll);
    }
    Some(scope)
}

/// Whether `literal` is a canonical value of type `ty` (literal filters on
/// decimals are not supported).
fn literal_fits(ty: MappingType, literal: &str) -> bool {
    match ty {
        MappingType::Boolean => matches!(literal, "true" | "false"),
        MappingType::Decimal | MappingType::Collection => false,
        _ => !literal.is_empty(),
    }
}

/// Checks `derivations` against the derived hub (see the module docs for the
/// codes). Every problem is reported, in rule order.
pub fn check_derivations(hub: &CanonicalModel, derivations: &[Derivation]) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let root_decimal = |key: &str| {
        hub.get(&CanonicalScope::Root, key)
            .is_some_and(|f| f.ty == MappingType::Decimal && !f.is_collection)
    };
    // A total is computed by one rule; `value` fills only absent keys, so
    // several may share a target (with disjoint `where` filters), but not with
    // a computed total.
    let mut computed: BTreeSet<&str> = BTreeSet::new();
    let mut filled: BTreeSet<&str> = BTreeSet::new();
    for (index, d) in derivations.iter().enumerate() {
        let mut err = |code: &str, message: String| {
            diags.push(Diagnostic {
                code: code.to_string(),
                severity: Severity::Error,
                source_node: Some(format!("derive {} ({})", d.key, d.rule)),
                message,
                span: None,
            });
        };
        let clash = match d.kind {
            DerivationKind::Value { .. } => {
                filled.insert(&d.key);
                computed.contains(d.key.as_str())
            }
            _ => !computed.insert(&d.key) || filled.contains(d.key.as_str()),
        };
        if clash {
            err(
                "E110",
                format!("`{}` is computed by more than one rule", d.key),
            );
        }
        match &d.kind {
            DerivationKind::Sum {
                collection,
                item,
                filter,
            } => {
                if !root_decimal(&d.key) {
                    err(
                        "E110",
                        format!("`{}` is not a root `decimal` canonical key", d.key),
                    );
                }
                let scope = scope_of(hub, &[collection.as_str()]);
                let item_ty = scope.as_ref().and_then(|s| hub.get(s, item)).map(|f| f.ty);
                if item_ty != Some(MappingType::Decimal) {
                    err(
                        "E111",
                        format!(
                            "`sum = \"{collection}/{item}\"` does not name a `decimal` key of a root collection"
                        ),
                    );
                }
                if let Some(scope) = &scope {
                    check_item_keys(hub, scope, filter, &[], collection, &mut err);
                }
            }
            DerivationKind::Arithmetic {
                add,
                subtract,
                requires,
                ..
            } => {
                if !root_decimal(&d.key) {
                    err(
                        "E110",
                        format!("`{}` is not a root `decimal` canonical key", d.key),
                    );
                }
                let required: BTreeSet<&str> = add
                    .iter()
                    .take(1)
                    .chain(requires)
                    .map(String::as_str)
                    .collect();
                for operand in add.iter().chain(subtract).chain(requires) {
                    if !root_decimal(operand) {
                        err(
                            "E113",
                            format!("operand `{operand}` is not a root `decimal` canonical key"),
                        );
                    } else if required.contains(operand.as_str())
                        && derivations[index..].iter().any(|r| &r.key == operand)
                    {
                        err(
                            "E113",
                            format!(
                                "required operand `{operand}` is derived by this or a later rule; derive it first"
                            ),
                        );
                    }
                }
            }
            DerivationKind::Value {
                value,
                filter,
                unless,
            } => {
                let (collections, key) = d.target_path();
                let scope = scope_of(hub, &collections);
                match scope.as_ref().and_then(|s| hub.get(s, key)) {
                    Some(f) if !f.is_collection => {
                        // The literal is emitted into generated code and
                        // parsed there, so it must pass the same check as a
                        // `constant` (a plain decimal: no `NaN`, no exponent).
                        if let Some(reason) = constant_literal_error(f.ty, value) {
                            err(
                                "E114",
                                format!(
                                    "`value = \"{value}\"` does not fit the `{}` key: {reason}",
                                    f.ty
                                ),
                            );
                        }
                    }
                    _ => err("E110", format!("`{}` is not a scalar canonical key", d.key)),
                }
                if collections.is_empty() && !filter.is_empty() {
                    err(
                        "E112",
                        format!(
                            "`where` filters the items of a collection; `{}` is a root key",
                            d.key
                        ),
                    );
                } else if let Some(scope) = &scope {
                    let unless: Vec<&str> = unless.iter().map(String::as_str).collect();
                    let owner = collections.join("/");
                    check_item_keys(hub, scope, filter, &unless, &owner, &mut err);
                }
            }
        }
    }
    diags
}

/// E112 for `where` / `unless` keys that are not scalars of `scope`, or
/// `where` literals that do not fit their key's type.
fn check_item_keys(
    hub: &CanonicalModel,
    scope: &CanonicalScope,
    filter: &[(String, String)],
    unless: &[&str],
    owner: &str,
    err: &mut impl FnMut(&str, String),
) {
    let scalar = |key: &str| hub.get(scope, key).filter(|f| !f.is_collection);
    for (key, literal) in filter {
        match scalar(key) {
            Some(f) if literal_fits(f.ty, literal) => {}
            Some(f) => err(
                "E112",
                format!(
                    "`where.{key} = \"{literal}\"` does not fit the `{}` key",
                    f.ty
                ),
            ),
            None => err(
                "E112",
                format!("`where.{key}` is not a scalar key of `{owner}`"),
            ),
        }
    }
    for key in unless {
        if scalar(key).is_none() {
            err(
                "E112",
                format!("`unless` key `{key}` is not a scalar key of `{owner}`"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::derive_hub;
    use crate::ir::build_ir;
    use crate::parse::parse_mapping;
    use rstest::rstest;

    const RULES: &str = r#"
        [[derive]]
        key = "SumOfInvoiceLineNetAmount"
        rule = "BR-CO-10"
        sum = "InvoiceLines/LineNetAmount"

        [[derive]]
        key = "SumOfChargesDocumentLevel"
        rule = "BR-CO-12"
        sum = "DocumentAllowanceCharges/AllowanceChargeAmount"
        where = { ChargeIndicator = "true" }

        [[derive]]
        key = "InvoiceTotalWithoutVat"
        rule = "BR-CO-13"
        add = ["SumOfInvoiceLineNetAmount", "SumOfChargesDocumentLevel"]
        subtract = ["SumOfAllowancesDocumentLevel"]
    "#;

    /// A hub with the keys the rules above use.
    fn hub() -> CanonicalModel {
        let src = r#"[meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Doc"
            [Doc.A]
            type = "decimal"
            canonical_key = "SumOfInvoiceLineNetAmount"
            [Doc.B]
            type = "decimal"
            canonical_key = "SumOfChargesDocumentLevel"
            [Doc.C]
            type = "decimal"
            canonical_key = "SumOfAllowancesDocumentLevel"
            [Doc.D]
            type = "decimal"
            canonical_key = "InvoiceTotalWithoutVat"
            [Doc.N]
            type = "string"
            canonical_key = "InvoiceNumber"
            [Doc.L]
            type = "collection"
            canonical_key = "InvoiceLines"
            [Doc.L.X]
            type = "decimal"
            canonical_key = "LineNetAmount"
            [Doc.AC]
            type = "collection"
            canonical_key = "DocumentAllowanceCharges"
            [Doc.AC.I]
            type = "boolean"
            canonical_key = "ChargeIndicator"
            [Doc.AC.M]
            type = "decimal"
            canonical_key = "AllowanceChargeAmount"
        "#;
        let (ir, _, diags) = build_ir(&[parse_mapping(src).unwrap()]);
        assert!(diags.is_empty(), "{diags:?}");
        derive_hub(std::slice::from_ref(&ir)).0
    }

    #[test]
    fn test_parse_reads_rules_in_file_order() {
        let rules = parse_derivations(RULES).expect("parses");
        let keys: Vec<_> = rules.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "SumOfInvoiceLineNetAmount",
                "SumOfChargesDocumentLevel",
                "InvoiceTotalWithoutVat"
            ]
        );
        assert_eq!(
            rules[1].kind,
            DerivationKind::Sum {
                collection: "DocumentAllowanceCharges".into(),
                item: "AllowanceChargeAmount".into(),
                filter: vec![("ChargeIndicator".into(), "true".into())],
            }
        );
        assert_eq!(rules[2].needs(), ["SumOfInvoiceLineNetAmount"]);
        assert_eq!(rules[0].needs(), ["InvoiceLines/LineNetAmount"]);
    }

    #[test]
    fn test_parse_reads_check_levels() {
        let src = r#"
            [[derive]]
            key = "A"
            rule = "R"
            sum = "L/X"

            [[derive]]
            key = "B"
            rule = "R"
            add = ["A"]
            check = "error"

            [[derive]]
            key = "C"
            rule = "R"
            add = ["A"]
            check = "off"

            [[derive]]
            key = "L/Y"
            rule = "R"
            value = "1"
        "#;
        let levels: Vec<_> = parse_derivations(src)
            .expect("parses")
            .iter()
            .map(|r| r.check)
            .collect();
        assert_eq!(
            levels,
            [
                CheckLevel::Warning,
                CheckLevel::Error,
                CheckLevel::Off,
                CheckLevel::Off
            ]
        );
    }

    #[test]
    fn test_valid_rules_check_clean() {
        let rules = parse_derivations(RULES).unwrap();
        assert!(check_derivations(&hub(), &rules).is_empty());
    }

    #[rstest]
    #[case::both("sum = \"L/X\"\nadd = [\"A\"]", "exactly one")]
    #[case::sum_and_value("sum = \"L/X\"\nvalue = \"1\"", "exactly one")]
    #[case::unless_on_sum("sum = \"L/X\"\nunless = [\"A\"]", "belongs to a `value`")]
    #[case::requires_on_value("value = \"1\"\nrequires = [\"A\"]", "belong to an `add`")]
    #[case::neither("", "exactly one")]
    #[case::sum_path("sum = \"NoSlash\"", "Collection/Key")]
    #[case::empty_add("add = []", "at least one")]
    #[case::where_on_add(
        "add = [\"A\"]\nwhere = { K = \"1\" }",
        "belongs to a `sum` or `value`"
    )]
    #[case::subtract_on_sum("sum = \"L/X\"\nsubtract = [\"A\"]", "belong to an `add`")]
    #[case::skip_negative_on_sum("sum = \"L/X\"\nskip_negative = true", "belong to an `add`")]
    #[case::non_string_where("sum = \"L/X\"\nwhere = { K = true }", "string literal")]
    #[case::check_on_value("value = \"1\"\ncheck = \"error\"", "belongs to a `sum` or `add`")]
    #[case::check_unknown("sum = \"L/X\"\ncheck = \"loud\"", "\"warning\", \"error\" or \"off\"")]
    fn test_parse_rejects(#[case] body: &str, #[case] needle: &str) {
        let src = format!("[[derive]]\nkey = \"K\"\nrule = \"R\"\n{body}");
        let err = parse_derivations(&src).unwrap_err();
        assert!(err.message.contains(needle), "{needle}: {}", err.message);
    }

    #[rstest]
    #[case::target_not_decimal(
        "E110",
        "key = \"InvoiceNumber\"\nrule = \"R\"\nsum = \"InvoiceLines/LineNetAmount\""
    )]
    #[case::sum_not_collection(
        "E111",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nsum = \"InvoiceNumber/X\""
    )]
    #[case::sum_item_unknown(
        "E111",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nsum = \"InvoiceLines/Nope\""
    )]
    #[case::where_unknown(
        "E112",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nsum = \"DocumentAllowanceCharges/AllowanceChargeAmount\"\nwhere = { Nope = \"x\" }"
    )]
    #[case::where_bad_bool(
        "E112",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nsum = \"DocumentAllowanceCharges/AllowanceChargeAmount\"\nwhere = { ChargeIndicator = \"yes\" }"
    )]
    #[case::operand_unknown(
        "E113",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nadd = [\"Nope\"]"
    )]
    #[case::operand_self(
        "E113",
        "key = \"InvoiceTotalWithoutVat\"\nrule = \"R\"\nadd = [\"InvoiceTotalWithoutVat\"]"
    )]
    fn test_check_rejects(#[case] code: &str, #[case] body: &str) {
        let rules = parse_derivations(&format!("[[derive]]\n{body}")).unwrap();
        let diags = check_derivations(&hub(), &rules);
        assert!(diags.iter().any(|d| d.code == code), "{code}: {diags:?}");
    }

    #[test]
    fn test_value_rules_parse_and_check() {
        let src = r#"
            [[derive]]
            key = "DocumentAllowanceCharges/AllowanceChargeAmount"
            rule = "TEST"
            value = "1.00"
            where = { ChargeIndicator = "false" }

            [[derive]]
            key = "InvoiceNumber"
            rule = "TEST"
            value = "X"
            unless = ["SumOfInvoiceLineNetAmount"]
        "#;
        let rules = parse_derivations(src).unwrap();
        assert_eq!(
            rules[0].target_path(),
            (vec!["DocumentAllowanceCharges"], "AllowanceChargeAmount")
        );
        assert!(rules[0].needs().is_empty());
        assert!(
            check_derivations(&hub(), &rules).is_empty(),
            "{:?}",
            check_derivations(&hub(), &rules)
        );
    }

    #[rstest]
    #[case::bad_target("E110", "key = \"InvoiceLines/Nope\"\nrule = \"R\"\nvalue = \"x\"")]
    #[case::collection_target("E110", "key = \"InvoiceLines\"\nrule = \"R\"\nvalue = \"x\"")]
    #[case::bad_literal(
        "E114",
        "key = \"DocumentAllowanceCharges/ChargeIndicator\"\nrule = \"R\"\nvalue = \"maybe\""
    )]
    #[case::nan_decimal(
        "E114",
        "key = \"DocumentAllowanceCharges/AllowanceChargeAmount\"\nrule = \"R\"\nvalue = \"NaN\""
    )]
    #[case::infinite_decimal(
        "E114",
        "key = \"DocumentAllowanceCharges/AllowanceChargeAmount\"\nrule = \"R\"\nvalue = \"inf\""
    )]
    #[case::exponent_decimal(
        "E114",
        "key = \"DocumentAllowanceCharges/AllowanceChargeAmount\"\nrule = \"R\"\nvalue = \"1e3\""
    )]
    #[case::where_on_root_target(
        "E112",
        "key = \"InvoiceNumber\"\nrule = \"R\"\nvalue = \"x\"\nwhere = { InvoiceNumber = \"a\" }"
    )]
    #[case::bad_unless(
        "E112",
        "key = \"InvoiceNumber\"\nrule = \"R\"\nvalue = \"x\"\nunless = [\"Nope\"]"
    )]
    fn test_value_rules_reject(#[case] code: &str, #[case] body: &str) {
        let rules = parse_derivations(&format!("[[derive]]\n{body}")).unwrap();
        let diags = check_derivations(&hub(), &rules);
        assert!(diags.iter().any(|d| d.code == code), "{code}: {diags:?}");
    }

    #[test]
    fn test_value_rules_may_share_a_target_but_not_with_a_computed_total() {
        let src = r#"
            [[derive]]
            key = "DocumentAllowanceCharges/AllowanceChargeAmount"
            rule = "A"
            value = "1.00"
            where = { ChargeIndicator = "false" }

            [[derive]]
            key = "DocumentAllowanceCharges/AllowanceChargeAmount"
            rule = "B"
            value = "2.00"
            where = { ChargeIndicator = "true" }

            [[derive]]
            key = "InvoiceTotalWithoutVat"
            rule = "C"
            value = "1.00"

            [[derive]]
            key = "InvoiceTotalWithoutVat"
            rule = "D"
            add = ["SumOfInvoiceLineNetAmount"]
        "#;
        let diags = check_derivations(&hub(), &parse_derivations(src).unwrap());
        let found: Vec<_> = diags
            .iter()
            .map(|d| (d.code.as_str(), d.source_node.clone().unwrap_or_default()))
            .collect();
        assert_eq!(
            found,
            [("E110", "derive InvoiceTotalWithoutVat (D)".to_string())],
            "{diags:?}"
        );
    }

    #[test]
    fn test_optional_operand_may_be_derived_later_but_a_required_one_may_not() {
        let src = r#"
            [[derive]]
            key = "InvoiceTotalWithoutVat"
            rule = "A"
            add = ["SumOfInvoiceLineNetAmount"]
            subtract = ["SumOfAllowancesDocumentLevel"]

            [[derive]]
            key = "SumOfAllowancesDocumentLevel"
            rule = "B"
            add = ["InvoiceTotalWithoutVat"]
            requires = ["SumOfChargesDocumentLevel"]

            [[derive]]
            key = "SumOfChargesDocumentLevel"
            rule = "C"
            sum = "DocumentAllowanceCharges/AllowanceChargeAmount"
        "#;
        let diags = check_derivations(&hub(), &parse_derivations(src).unwrap());
        let found: Vec<_> = diags
            .iter()
            .map(|d| {
                (
                    d.code.as_str(),
                    d.message.contains("SumOfChargesDocumentLevel"),
                )
            })
            .collect();
        assert_eq!(found, [("E113", true)], "{diags:?}");
    }

    #[test]
    fn test_check_rejects_a_key_derived_twice_and_an_operand_derived_later() {
        let src = r#"
            [[derive]]
            key = "InvoiceTotalWithoutVat"
            rule = "BR-CO-13"
            add = ["SumOfInvoiceLineNetAmount"]

            [[derive]]
            key = "SumOfInvoiceLineNetAmount"
            rule = "BR-CO-10"
            sum = "InvoiceLines/LineNetAmount"

            [[derive]]
            key = "SumOfInvoiceLineNetAmount"
            rule = "again"
            sum = "InvoiceLines/LineNetAmount"
        "#;
        let diags = check_derivations(&hub(), &parse_derivations(src).unwrap());
        let codes: Vec<_> = diags.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, ["E113", "E110"], "{diags:?}");
    }
}
