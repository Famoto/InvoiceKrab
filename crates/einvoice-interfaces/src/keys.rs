//! The canonical-key authoring aid (an authoring view over the derived hub).
//!
//! Writing a spoke `mappings/*.toml` means attaching source nodes to canonical
//! `canonical_key`s. Those keys are not declared in one central place: the hub is
//! *derived* as the union of every `canonical_key` across every spoke (see
//! `einvoice-dsl`'s `derive_hub`). That makes it easy, while writing a new
//! mapping, to (a) not know which canonical names already exist — and so invent a
//! near-duplicate with a typo that silently becomes a brand-new, single-spoke hub
//! field — or (b) forget which already-established keys this spoke has not mapped
//! yet. This module answers both questions without parsing any XML, purely from
//! the generated [`Spoke::covered_keys`] / [`Spoke::required_keys`] footprints.
//!
//! # Structure
//!
//! - [`KeyInfo`] — one main key and the spokes that define (and require) it.
//! - [`hub_keys`] — the whole vocabulary: every main key, who defines it.
//! - [`SpokeKeys`] / [`CoveredKey`] / [`UnusedKey`] — one spoke's authoring view.
//! - [`spoke_keys`] — split the hub into "this spoke covers it" vs. "unused here".
//! - [`render_hub_keys`] / [`render_spoke_keys`] — aligned, deterministic tables.
//!
//! # Behavior
//!
//! A *main key* is a scope-qualified canonical label (e.g. `InvoiceNumber`,
//! `InvoiceLines/LineId`) — exactly the strings the generated accessors return. A
//! key is *defined by* a spoke when that spoke maps it, and *unused* by a spoke
//! when some other spoke defines it but this one does not. Everything is sorted by
//! key (then spoke name), so identical inputs render identically.
//!
//! # Testing
//!
//! Unit tests below assert the vocabulary unions across the bundled spokes, that
//! coverage and the unused set partition the hub for each spoke, and that the
//! renderers carry the headers/markers callers assert on. Integration tests in
//! `tests/cli.rs` drive the `--keys` command end to end.

use std::collections::{BTreeMap, BTreeSet};

use crate::Spoke;

/// One canonical hub key and the spokes that define and require it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// The scope-qualified canonical label (e.g. `InvoiceLines/LineId`).
    pub key: String,
    /// Names of the spokes that map this key, sorted.
    pub defined_by: Vec<String>,
    /// Names of the spokes that mark this key `required`, sorted.
    pub required_by: Vec<String>,
}

/// The full hub vocabulary: every main key any bundled spoke defines, with the
/// spokes that define and require it. Sorted by key.
///
/// This is the "what keys are defined" view — the authoritative list of canonical
/// names to map onto so a new mapping aligns with the existing hub instead of
/// minting a typo'd near-duplicate.
pub fn hub_keys() -> Vec<KeyInfo> {
    // The vocabulary is derived from the compile-time `Spoke::ALL`, so it never
    // changes within a run: build it once and clone on subsequent calls (e.g.
    // `spoke_keys` per spoke) instead of rescanning every spoke each time.
    static CACHE: std::sync::OnceLock<Vec<KeyInfo>> = std::sync::OnceLock::new();
    CACHE.get_or_init(build_hub_keys).clone()
}

/// Builds the hub vocabulary from every spoke's footprint (the uncached core of
/// [`hub_keys`]).
fn build_hub_keys() -> Vec<KeyInfo> {
    // key -> (definers, requirers); BTreeSet keeps names sorted and de-duped.
    let mut acc: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();

    for &spoke in Spoke::ALL {
        let required: BTreeSet<&str> = spoke.required_keys().iter().copied().collect();
        for &key in spoke.covered_keys() {
            let entry = acc.entry(key.to_string()).or_default();
            entry.0.insert(spoke.name().to_string());
            if required.contains(key) {
                entry.1.insert(spoke.name().to_string());
            }
        }
    }

    acc.into_iter()
        .map(|(key, (defined_by, required_by))| KeyInfo {
            key,
            defined_by: defined_by.into_iter().collect(),
            required_by: required_by.into_iter().collect(),
        })
        .collect()
}

/// A main key one spoke covers, and whether it marks it required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoveredKey {
    /// The scope-qualified canonical label.
    pub key: String,
    /// Whether this spoke marks the key `required`.
    pub required: bool,
}

/// A hub main key one spoke does *not* cover — a candidate to add to its TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedKey {
    /// The scope-qualified canonical label.
    pub key: String,
    /// The other spokes that already define it (where to crib the mapping from).
    pub defined_by: Vec<String>,
}

/// One spoke's authoring view of the hub: the keys it covers and the keys it does
/// not yet map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeKeys {
    /// The spoke's display name.
    pub spoke: String,
    /// The main keys this spoke covers, sorted by key.
    pub covered: Vec<CoveredKey>,
    /// Hub keys defined elsewhere but unused by this spoke, sorted by key.
    pub unused: Vec<UnusedKey>,
}

/// Builds the authoring view for `spoke`: every hub key is either covered by it
/// (with its required flag) or unused here (with the spokes that do define it).
///
/// `covered` and `unused` partition the hub vocabulary, so their lengths always
/// sum to `hub_keys().len()`.
pub fn spoke_keys(spoke: Spoke) -> SpokeKeys {
    let covers: BTreeSet<&str> = spoke.covered_keys().iter().copied().collect();
    let requires: BTreeSet<&str> = spoke.required_keys().iter().copied().collect();

    let mut covered = Vec::new();
    let mut unused = Vec::new();
    for info in hub_keys() {
        if covers.contains(info.key.as_str()) {
            covered.push(CoveredKey {
                required: requires.contains(info.key.as_str()),
                key: info.key,
            });
        } else {
            unused.push(UnusedKey {
                key: info.key,
                defined_by: info.defined_by,
            });
        }
    }

    SpokeKeys {
        spoke: spoke.name().to_string(),
        covered,
        unused,
    }
}

/// Renders the whole hub vocabulary as an aligned table ending in a newline.
///
/// Columns are `KEY`, `DEFINED` (the number of spokes that map it), and `SPOKES`
/// (their names). A `*` after a spoke name in `SPOKES` means it marks the key
/// `required`.
pub fn render_hub_keys(keys: &[KeyInfo]) -> String {
    let rows: Vec<[String; 3]> = keys
        .iter()
        .map(|info| {
            let spokes = info
                .defined_by
                .iter()
                .map(|name| {
                    if info.required_by.contains(name) {
                        format!("{name}*")
                    } else {
                        name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            [info.key.clone(), info.defined_by.len().to_string(), spokes]
        })
        .collect();

    let mut out = crate::table::aligned(["KEY", "DEFINED", "SPOKES"], &rows);
    out.push_str(&format!(
        "\n{} main keys across {} spokes. `*` = required by that spoke.\n",
        keys.len(),
        Spoke::ALL.len(),
    ));
    out
}

/// Renders one spoke's authoring view: a covered section and an unused section.
///
/// Covered keys are prefixed `✓` (with `(required)` when the spoke requires
/// them); unused keys are prefixed `·` and annotated with the spokes that already
/// define them. The output ends in a newline.
pub fn render_spoke_keys(view: &SpokeKeys) -> String {
    let total = view.covered.len() + view.unused.len();
    let mut out = format!(
        "main keys for {} — {} covered, {} unused of {} total\n",
        view.spoke,
        view.covered.len(),
        view.unused.len(),
        total,
    );

    out.push_str("\nCOVERED (mapped in this spoke):\n");
    if view.covered.is_empty() {
        out.push_str("  (none)\n");
    }
    for c in &view.covered {
        if c.required {
            out.push_str(&format!("  ✓ {} (required)\n", c.key));
        } else {
            out.push_str(&format!("  ✓ {}\n", c.key));
        }
    }

    out.push_str("\nUNUSED (defined by other spokes, not mapped here — candidates to add):\n");
    if view.unused.is_empty() {
        out.push_str("  (none)\n");
    }
    for u in &view.unused {
        out.push_str(&format!(
            "  · {} — defined by: {}\n",
            u.key,
            u.defined_by.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_hub_keys_unions_every_spoke_footprint() {
        let keys = hub_keys();
        // The vocabulary is exactly the union of every spoke's covered keys.
        let union: BTreeSet<String> = Spoke::ALL
            .iter()
            .flat_map(|s| s.covered_keys().iter().map(|k| k.to_string()))
            .collect();
        let listed: BTreeSet<String> = keys.iter().map(|k| k.key.clone()).collect();
        assert_eq!(listed, union);
    }

    #[test]
    fn test_hub_keys_are_sorted_and_unique() {
        let keys = hub_keys();
        let sorted: Vec<String> = {
            let mut v: Vec<String> = keys.iter().map(|k| k.key.clone()).collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(
            keys.iter().map(|k| k.key.clone()).collect::<Vec<_>>(),
            sorted
        );
    }

    #[test]
    fn test_hub_keys_definers_match_covering_spokes() {
        // For a non-trivial key, `defined_by` is exactly the spokes covering it.
        let keys = hub_keys();
        let sample = keys.first().expect("at least one main key");
        let expected: Vec<String> = Spoke::ALL
            .iter()
            .filter(|s| s.covered_keys().contains(&sample.key.as_str()))
            .map(|s| s.name().to_string())
            .collect();
        assert_eq!(sample.defined_by, expected);
        assert!(!sample.defined_by.is_empty());
    }

    #[test]
    fn test_required_by_is_subset_of_defined_by() {
        for info in hub_keys() {
            for r in &info.required_by {
                assert!(
                    info.defined_by.contains(r),
                    "{} required by {r} but not in defined_by",
                    info.key
                );
            }
        }
    }

    #[test]
    fn test_spoke_keys_partition_the_hub() {
        let total = hub_keys().len();
        for &spoke in Spoke::ALL {
            let view = spoke_keys(spoke);
            assert_eq!(
                view.covered.len() + view.unused.len(),
                total,
                "{} covered+unused must cover the whole hub",
                spoke.name()
            );
            // No key is both covered and unused.
            let covered: BTreeSet<&str> = view.covered.iter().map(|c| c.key.as_str()).collect();
            for u in &view.unused {
                assert!(!covered.contains(u.key.as_str()));
            }
        }
    }

    #[test]
    fn test_spoke_keys_covered_matches_generated_footprint() {
        for &spoke in Spoke::ALL {
            let view = spoke_keys(spoke);
            let covered: BTreeSet<String> = view.covered.iter().map(|c| c.key.clone()).collect();
            let expected: BTreeSet<String> =
                spoke.covered_keys().iter().map(|k| k.to_string()).collect();
            assert_eq!(covered, expected, "{}", spoke.name());
        }
    }

    #[test]
    fn test_spoke_keys_unused_are_defined_by_someone_else() {
        for &spoke in Spoke::ALL {
            for u in spoke_keys(spoke).unused {
                assert!(!u.defined_by.is_empty(), "{} has no definers", u.key);
                assert!(
                    !u.defined_by.contains(&spoke.name().to_string()),
                    "{} listed as definer of its own unused key {}",
                    spoke.name(),
                    u.key
                );
            }
        }
    }

    #[test]
    fn test_spoke_keys_required_flag_tracks_generated_required() {
        for &spoke in Spoke::ALL {
            let view = spoke_keys(spoke);
            let required: BTreeSet<String> = spoke
                .required_keys()
                .iter()
                .map(|k| k.to_string())
                .collect();
            for c in view.covered {
                assert_eq!(
                    c.required,
                    required.contains(&c.key),
                    "{} required flag wrong for {}",
                    spoke.name(),
                    c.key
                );
            }
        }
    }

    #[test]
    fn test_render_hub_keys_has_header_and_summary() {
        let table = render_hub_keys(&hub_keys());
        assert!(table.contains("KEY"));
        assert!(table.contains("DEFINED"));
        assert!(table.contains("SPOKES"));
        assert!(table.contains("main keys across"));
        assert!(table.ends_with('\n'));
    }

    #[test]
    fn test_render_hub_keys_marks_required_with_star() {
        // Build an info with one required and one non-required definer.
        let keys = vec![KeyInfo {
            key: "InvoiceNumber".into(),
            defined_by: vec!["a".into(), "b".into()],
            required_by: vec!["a".into()],
        }];
        let table = render_hub_keys(&keys);
        assert!(table.contains("a*, b"), "got: {table}");
    }

    #[test]
    fn test_render_spoke_keys_has_both_sections() {
        let view = SpokeKeys {
            spoke: "ubl-invoice".into(),
            covered: vec![CoveredKey {
                key: "InvoiceNumber".into(),
                required: true,
            }],
            unused: vec![UnusedKey {
                key: "BuyerName".into(),
                defined_by: vec!["peppol-invoice".into()],
            }],
        };
        let out = render_spoke_keys(&view);
        assert!(out.contains("COVERED"));
        assert!(out.contains("✓ InvoiceNumber (required)"));
        assert!(out.contains("UNUSED"));
        assert!(out.contains("· BuyerName — defined by: peppol-invoice"));
        assert!(out.contains("1 covered, 1 unused of 2 total"));
    }

    #[test]
    fn test_render_spoke_keys_empty_sections_say_none() {
        let view = SpokeKeys {
            spoke: "x".into(),
            covered: Vec::new(),
            unused: Vec::new(),
        };
        let out = render_spoke_keys(&view);
        assert_eq!(out.matches("(none)").count(), 2);
    }
}
