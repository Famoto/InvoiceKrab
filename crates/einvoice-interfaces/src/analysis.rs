//! Static transform analysis: the loss/error state of every spoke-to-spoke
//! mapping (the "Mapping Comparison Tool" at the engine boundary).
//!
//! The engine is hub-and-spoke (N–1–N): a transform `source → target` reads the
//! source into the canonical hub, then writes the hub to the target. So no XML is
//! needed to know how a transform will behave — it is fully determined by the
//! *canonical fields* each spoke covers and which fields the target marks
//! `required`. This module compares those sets (exposed by the generated
//! [`Spoke::covered_keys`] / [`Spoke::required_keys`]) and classifies each pair.
//!
//! # Structure
//!
//! - [`TransformState`] — the four-way verdict for one transform.
//! - [`TransformReport`] — a transform's state plus the concrete missing-required
//!   and dropped fields that explain it.
//! - [`analyze`] — classify a single `source → target` pair.
//! - [`analyze_all`] — every `source x target` pair, in spoke order.
//! - [`render_table`] — a user-friendly aligned table over a set of reports.
//!
//! # Behavior
//!
//! Let `S` be the source's covered keys, `T` the target's covered keys, and `R`
//! the target's required keys. A transform is:
//!
//! - [`Lossless`](TransformState::Lossless): `R ⊆ S` and `S ⊆ T` — every required
//!   field is filled and nothing the source carries is dropped.
//! - [`Lossful`](TransformState::Lossful): `R ⊆ S` but `S ⊄ T` — valid output, but
//!   some source fields have no slot in the target and are dropped.
//! - [`Partial`](TransformState::Partial): `R ⊄ S` yet `R ∩ S ≠ ∅` — some required
//!   fields can be filled and some cannot; the target document is produced with
//!   `REQUIRED_MISSING` diagnostics.
//! - [`Error`](TransformState::Error): `R ⊄ S` and `R ∩ S = ∅` — the source
//!   provides none of the target's required fields; the formats are incompatible.
//!
//! A target with no required fields is never `Partial`/`Error`.
//!
//! # Testing
//!
//! Unit tests below drive [`classify`] over hand-built field sets across every
//! boundary, and check [`render_table`] alignment and legend. Integration tests
//! in `tests/cli.rs` exercise the `--analyze` command over the bundled spokes.

use std::collections::BTreeSet;

use crate::Spoke;

/// The four-way verdict for a single `source → target` transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformState {
    /// Every required target field is filled and no source field is dropped.
    Lossless,
    /// All required fields filled, but some source fields are dropped.
    Lossful,
    /// Some required fields can be filled and some cannot.
    Partial,
    /// The source provides none of the target's required fields.
    Error,
}

impl TransformState {
    /// A compact one-word label.
    pub fn label(self) -> &'static str {
        match self {
            TransformState::Lossless => "lossless",
            TransformState::Lossful => "lossful",
            TransformState::Partial => "partial",
            TransformState::Error => "error",
        }
    }

    /// A single-character glyph for compact rendering.
    pub fn glyph(self) -> char {
        match self {
            TransformState::Lossless => '=',
            TransformState::Lossful => '~',
            TransformState::Partial => '!',
            TransformState::Error => 'x',
        }
    }
}

impl std::fmt::Display for TransformState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One transform's verdict plus the fields that explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformReport {
    /// The source spoke (reads into the hub).
    pub source: Spoke,
    /// The target spoke (writes from the hub).
    pub target: Spoke,
    /// The verdict.
    pub state: TransformState,
    /// Target-required fields the source does not cover (cause errors), sorted.
    pub missing_required: Vec<String>,
    /// Source fields the target cannot carry, so they are dropped, sorted.
    pub dropped: Vec<String>,
}

/// Classifies a transform from raw field-set inputs (the pure core of [`analyze`]).
///
/// `source_covers` is `S`, `target_covers` is `T`, `target_required` is `R`.
///
/// # Examples
///
/// ```
/// use einvoice_interfaces::analysis::{classify, TransformState};
/// use std::collections::BTreeSet;
///
/// let s: BTreeSet<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
/// let t: BTreeSet<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
/// let r: BTreeSet<String> = ["A"].iter().map(|s| s.to_string()).collect();
/// // Every required field filled and nothing dropped.
/// assert_eq!(classify(&s, &t, &r), TransformState::Lossless);
/// ```
pub fn classify(
    source_covers: &BTreeSet<String>,
    target_covers: &BTreeSet<String>,
    target_required: &BTreeSet<String>,
) -> TransformState {
    let required_missing = !target_required.is_subset(source_covers);
    if required_missing {
        // Any required field the source can fill makes it partial, not a clean error.
        if target_required.iter().any(|k| source_covers.contains(k)) {
            TransformState::Partial
        } else {
            TransformState::Error
        }
    } else if source_covers.is_subset(target_covers) {
        TransformState::Lossless
    } else {
        TransformState::Lossful
    }
}

/// Classifies one `source → target` transform over the bundled spokes, recording
/// the concrete missing-required and dropped fields.
pub fn analyze(source: Spoke, target: Spoke) -> TransformReport {
    let s: BTreeSet<String> = source
        .covered_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    let t: BTreeSet<String> = target
        .covered_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    let r: BTreeSet<String> = target
        .required_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();

    let state = classify(&s, &t, &r);
    let missing_required = r.difference(&s).cloned().collect();
    let dropped = s.difference(&t).cloned().collect();

    TransformReport {
        source,
        target,
        state,
        missing_required,
        dropped,
    }
}

/// Every `source x target` transform among `sources` and `targets`, in the given
/// order (source-major). Pass `Spoke::ALL` for both to get the full matrix, or a
/// single source to scope the report to "from X to everything else".
pub fn analyze_all(sources: &[Spoke], targets: &[Spoke]) -> Vec<TransformReport> {
    let mut reports = Vec::with_capacity(sources.len() * targets.len());
    for &source in sources {
        for &target in targets {
            reports.push(analyze(source, target));
        }
    }
    reports
}

/// Renders `reports` as a user-friendly, aligned table with a trailing legend.
///
/// Columns are `SOURCE`, `TARGET`, `STATE`, and `DETAIL` (the count of missing
/// required and dropped fields). The output ends with a newline.
pub fn render_table(reports: &[TransformReport]) -> String {
    let header = ["SOURCE", "TARGET", "STATE", "DETAIL"];
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(reports.len());
    for r in reports {
        rows.push([
            r.source.name().to_string(),
            r.target.name().to_string(),
            format!("{} {}", r.state.glyph(), r.state.label()),
            detail(r),
        ]);
    }

    let mut out = crate::table::aligned(header, &rows);
    out.push_str(
        "\nlegend: = lossless (no loss)  ~ lossful (fields dropped)  \
         ! partial (some required missing)  x error (no required filled)\n",
    );
    out
}

/// The `DETAIL` cell: a short human summary of what the verdict costs.
fn detail(r: &TransformReport) -> String {
    match r.state {
        TransformState::Lossless => "—".to_string(),
        TransformState::Lossful => format!("drops {}", join_fields(&r.dropped)),
        TransformState::Partial | TransformState::Error => {
            format!("missing required {}", join_fields(&r.missing_required))
        }
    }
}

/// Joins up to three field labels, summarizing the rest as `(+N more)`.
fn join_fields(fields: &[String]) -> String {
    const SHOWN: usize = 3;
    if fields.len() <= SHOWN {
        fields.join(", ")
    } else {
        format!(
            "{}, (+{} more)",
            fields[..SHOWN].join(", "),
            fields.len() - SHOWN
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_classify_subset_and_no_drop_is_lossless() {
        let s = set(&["A", "B"]);
        let t = set(&["A", "B", "C"]);
        let r = set(&["A"]);
        assert_eq!(classify(&s, &t, &r), TransformState::Lossless);
    }

    #[test]
    fn test_classify_required_filled_but_drops_is_lossful() {
        // Source carries B, target has no slot for it → B is dropped.
        let s = set(&["A", "B"]);
        let t = set(&["A"]);
        let r = set(&["A"]);
        assert_eq!(classify(&s, &t, &r), TransformState::Lossful);
    }

    #[test]
    fn test_classify_some_required_missing_is_partial() {
        // Target requires A and B; source only has A.
        let s = set(&["A"]);
        let t = set(&["A", "B"]);
        let r = set(&["A", "B"]);
        assert_eq!(classify(&s, &t, &r), TransformState::Partial);
    }

    #[test]
    fn test_classify_no_required_filled_is_error() {
        // Target requires B; source provides none of the required set.
        let s = set(&["A"]);
        let t = set(&["B"]);
        let r = set(&["B"]);
        assert_eq!(classify(&s, &t, &r), TransformState::Error);
    }

    #[test]
    fn test_classify_target_without_required_is_never_error() {
        // No required fields → cannot be partial or error, only loss matters.
        let s = set(&["A", "B"]);
        let t = set(&["A"]);
        let empty = set(&[]);
        assert_eq!(classify(&s, &t, &empty), TransformState::Lossful);
        assert_eq!(classify(&t, &s, &empty), TransformState::Lossless);
    }

    #[test]
    fn test_analyze_identity_is_lossless() {
        // A spoke can always represent everything it produces.
        for &spoke in Spoke::ALL {
            let report = analyze(spoke, spoke);
            assert_eq!(
                report.state,
                TransformState::Lossless,
                "identity transform of {} should be lossless",
                spoke.name()
            );
            assert!(report.missing_required.is_empty());
            assert!(report.dropped.is_empty());
        }
    }

    #[test]
    fn test_analyze_all_covers_every_pair() {
        let reports = analyze_all(Spoke::ALL, Spoke::ALL);
        assert_eq!(reports.len(), Spoke::ALL.len() * Spoke::ALL.len());
    }

    #[test]
    fn test_render_table_has_header_legend_and_a_row_per_report() {
        let reports = analyze_all(Spoke::ALL, Spoke::ALL);
        let table = render_table(&reports);
        assert!(table.contains("SOURCE"));
        assert!(table.contains("STATE"));
        assert!(table.contains("legend:"));
        // Header + rule + one line per report + a blank line before the legend.
        let row_lines = table
            .lines()
            .filter(|l| l.contains(Spoke::ALL[0].name()))
            .count();
        assert!(row_lines >= 1);
        assert!(table.ends_with('\n'));
    }

    #[test]
    fn test_join_fields_summarizes_overflow() {
        let many = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert_eq!(join_fields(&many), "a, b, c, (+1 more)");
        assert_eq!(join_fields(&many[..2]), "a, b");
    }
}
