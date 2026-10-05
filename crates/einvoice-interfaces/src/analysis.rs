//! Static transform analysis: the loss/error state of every spoke-to-spoke
//! transform, computed from the two spokes' transformation contracts.
//!
//! The engine is hub-and-spoke (N–1–N): a transform `source → target` reads the
//! source into the canonical hub, then writes the hub to the target. So no XML is
//! needed to know how a transform will behave — it is fully determined by the
//! source's and the target's [`TransformationContract`]: which canonical keys
//! each maps (with type and scope), how the target's `required` nodes get their
//! write value, and what the source declares it collapses on read.
//!
//! # Structure
//!
//! - [`TransformState`] — the four-way verdict for one transform.
//! - [`Finding`] — one concrete fact about a pair: a required write route the
//!   source cannot feed, a type clash, a dropped key, a pin, a recode, a
//!   declared collapse.
//! - [`TransformReport`] — a transform's state plus its findings.
//! - [`analyze_contracts`] — the pure core: two contracts → findings + state.
//! - [`analyze`] / [`analyze_all`] — over the bundled spokes.
//! - [`render_table`] — the aligned matrix; [`render_pair`] — one pair in full.
//!
//! # Behavior
//!
//! Let `S` be the source's keys, `T` the target's, and `R` the hub keys the
//! target's `required` nodes need (their `Route::Hub` / `Route::Clone` routes —
//! a `Route::Constant` is always satisfied). A transform is:
//!
//! - [`Lossless`](TransformState::Lossless): `R ⊆ S`, `S ⊆ T`, and every shared
//!   key agrees on type — all required routes fed and nothing dropped.
//! - [`Lossful`](TransformState::Lossful): `R ⊆ S` but `S ⊄ T` — valid output,
//!   but some source keys have no slot in the target and are dropped.
//! - [`Partial`](TransformState::Partial): some required route cannot be fed
//!   (or a shared key clashes in type) yet some required route can; the target
//!   document is produced with `REQUIRED_MISSING` diagnostics.
//! - [`Error`](TransformState::Error): the source feeds none of the target's
//!   required routes; the formats are incompatible.
//!
//! A target with no required routes is never `Partial`/`Error`. Optional
//! feeds (a required target route fed by a key the source maps but does not
//! itself require, so a document lacking it is `REQUIRED_MISSING` at runtime),
//! pins (the target writes a constant where the source carried a value),
//! recodes (a key read and written through different codecs) and the source's
//! declared collapses (`multiple = "first" | "join"`, single-valued `match`
//! nodes) are reported as findings but do not change the state: they describe
//! what a particular document may lose or lack, not what the pair always does.
//!
//! # Testing
//!
//! Unit tests below drive [`classify`] and [`analyze_contracts`] over hand-built
//! contracts across every boundary, and check the renderers. Integration tests
//! in `tests/cli.rs` exercise `--analyze` over the bundled spokes.

use std::collections::BTreeSet;

use crate::Spoke;
use crate::contract::{Route, TransformationContract};

/// The four-way verdict for a single `source → target` transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformState {
    /// Every required target route is fed and no source key is dropped.
    Lossless,
    /// All required routes fed, but some source keys are dropped.
    Lossful,
    /// Some required routes can be fed and some cannot.
    Partial,
    /// The source feeds none of the target's required routes.
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

/// One concrete fact about a `source → target` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A `required` target node whose write route needs a hub key the source
    /// does not map: the output carries `REQUIRED_MISSING`. Blocking.
    MissingRequired {
        /// The target node id.
        node: String,
        /// The hub label the route needs.
        label: String,
        /// Whether the route is a `clone_of` of that label.
        via_clone: bool,
    },
    /// A `required` target route fed by a key the source maps but does not
    /// itself require: a source document without the value yields
    /// `REQUIRED_MISSING` at runtime although the pair is structurally sound.
    /// Informational.
    OptionalFeed {
        /// The target node id.
        node: String,
        /// The hub label the route needs.
        label: String,
    },
    /// A key both spokes map under different semantic types. Blocking.
    TypeMismatch {
        /// The key label.
        label: String,
        /// The source's type name.
        source: String,
        /// The target's type name.
        target: String,
    },
    /// A source key the target has no slot for: dropped. Lossy.
    Dropped {
        /// The key label.
        label: String,
    },
    /// The target writes a fixed constant for a key the source supplies, so
    /// the source's value is not what comes out. Informational.
    Pinned {
        /// The key label.
        label: String,
        /// The constant written.
        value: String,
    },
    /// A shared key read and written through different codecs (or one side
    /// without one): the lexical form changes. Informational.
    Recoded {
        /// The key label.
        label: String,
        /// The source's codec id, if any.
        source: Option<String>,
        /// The target's codec id, if any.
        target: Option<String>,
    },
    /// The source collapses repeated values of a key into one (`multiple =
    /// "first" | "join"`): a document with several loses the rest.
    /// Informational.
    Collapsed {
        /// The source node id.
        node: String,
        /// The key label.
        label: String,
        /// The policy (`first` / `join`).
        policy: String,
    },
    /// The source keeps only the first occurrence a single-valued `match`
    /// node selects (`MATCH_MULTIPLE`): surplus occurrences are not read.
    /// Informational.
    FirstMatch {
        /// The source node id.
        node: String,
        /// The physical element's local name.
        element: String,
    },
}

impl Finding {
    /// Whether the finding makes the output invalid (`Partial` / `Error`).
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Finding::MissingRequired { .. } | Finding::TypeMismatch { .. }
        )
    }
}

/// One transform's verdict plus the findings that explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformReport {
    /// The source spoke (reads into the hub).
    pub source: Spoke,
    /// The target spoke (writes from the hub).
    pub target: Spoke,
    /// The verdict.
    pub state: TransformState,
    /// Hub labels of target-required routes the source cannot feed, sorted.
    pub missing_required: Vec<String>,
    /// Source labels the target cannot carry, so they are dropped, sorted.
    pub dropped: Vec<String>,
    /// Every finding, blocking ones first, then dropped keys, then the
    /// informational ones; stable within each group.
    pub findings: Vec<Finding>,
}

/// The pure result of comparing two contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairAnalysis {
    /// The verdict.
    pub state: TransformState,
    /// Hub labels of target-required routes the source cannot feed, sorted.
    pub missing_required: Vec<String>,
    /// Source labels the target cannot carry, sorted.
    pub dropped: Vec<String>,
    /// Every finding (see [`TransformReport::findings`]).
    pub findings: Vec<Finding>,
}

/// Classifies a transform from raw key-set inputs (the set core of
/// [`analyze_contracts`]).
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

/// Compares two contracts: the pure core of [`analyze`].
pub fn analyze_contracts(
    source: &TransformationContract,
    target: &TransformationContract,
) -> PairAnalysis {
    let s: BTreeSet<String> = source.labels().map(str::to_string).collect();
    let t: BTreeSet<String> = target.labels().map(str::to_string).collect();
    let r: BTreeSet<String> = target
        .required_labels()
        .into_iter()
        .map(str::to_string)
        .collect();

    let source_required: BTreeSet<&str> = source.required_labels().into_iter().collect();
    let mut blocking = Vec::new();
    let mut notes = Vec::new();
    for route in target.required {
        let (label, via_clone) = match route.route {
            Route::Hub(label) => (label, false),
            Route::Clone(label) => (label, true),
            Route::Constant(_) => continue,
        };
        if !source.covers(label) {
            blocking.push(Finding::MissingRequired {
                node: route.node.to_string(),
                label: label.to_string(),
                via_clone,
            });
        } else if !source_required.contains(label) {
            notes.push(Finding::OptionalFeed {
                node: route.node.to_string(),
                label: label.to_string(),
            });
        }
    }
    for key in source.keys {
        let Some(other) = target.key(key.label) else {
            continue;
        };
        if key.ty != other.ty {
            blocking.push(Finding::TypeMismatch {
                label: key.label.to_string(),
                source: key.ty.to_string(),
                target: other.ty.to_string(),
            });
        }
        if let Some(value) = other.pinned {
            notes.push(Finding::Pinned {
                label: key.label.to_string(),
                value: value.to_string(),
            });
        }
        if key.codec != other.codec {
            notes.push(Finding::Recoded {
                label: key.label.to_string(),
                source: key.codec.map(str::to_string),
                target: other.codec.map(str::to_string),
            });
        }
    }
    for collapse in source.collapses {
        if target.covers(collapse.label) {
            notes.push(Finding::Collapsed {
                node: collapse.node.to_string(),
                label: collapse.label.to_string(),
                policy: collapse.policy.to_string(),
            });
        }
    }
    for selector in source.selectors.iter().filter(|s| s.single) {
        notes.push(Finding::FirstMatch {
            node: selector.node.to_string(),
            element: selector.element.to_string(),
        });
    }

    let missing_required: Vec<String> = r.difference(&s).cloned().collect();
    let dropped: Vec<String> = s.difference(&t).cloned().collect();
    let mut state = classify(&s, &t, &r);
    // A type clash is as blocking as a missing route: the target cannot take
    // the value. It never *lowers* the state below what the routes say.
    if blocking
        .iter()
        .any(|f| matches!(f, Finding::TypeMismatch { .. }))
        && matches!(state, TransformState::Lossless | TransformState::Lossful)
    {
        state = TransformState::Partial;
    }

    let mut findings = blocking;
    findings.extend(dropped.iter().map(|label| Finding::Dropped {
        label: label.clone(),
    }));
    findings.extend(notes);

    PairAnalysis {
        state,
        missing_required,
        dropped,
        findings,
    }
}

/// Classifies one `source → target` transform over the bundled spokes.
pub fn analyze(source: Spoke, target: Spoke) -> TransformReport {
    let pair = analyze_contracts(source.contract(), target.contract());
    TransformReport {
        source,
        target,
        state: pair.state,
        missing_required: pair.missing_required,
        dropped: pair.dropped,
        findings: pair.findings,
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

/// Renders one transform in full: its verdict, then every finding grouped by
/// kind. The output ends with a newline.
pub fn render_pair(report: &TransformReport) -> String {
    let mut out = format!(
        "{} -> {}: {} {}\n",
        report.source.name(),
        report.target.name(),
        report.state.glyph(),
        report.state.label()
    );

    let section = |out: &mut String, title: &str, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        out.push('\n');
        out.push_str(&format!("{title} ({}):\n", lines.len()));
        for line in lines {
            out.push_str("  ");
            out.push_str(&line);
            out.push('\n');
        }
    };

    let mut missing = Vec::new();
    let mut optional = Vec::new();
    let mut clashes = Vec::new();
    let mut dropped = Vec::new();
    let mut pinned = Vec::new();
    let mut recoded = Vec::new();
    let mut collapsed = Vec::new();
    for finding in &report.findings {
        match finding {
            Finding::MissingRequired {
                node,
                label,
                via_clone,
            } => missing.push(if *via_clone {
                format!("{label} — needed by `{node}` (a clone of it); the source does not map it")
            } else {
                format!("{label} — required by `{node}`; the source does not map it")
            }),
            Finding::OptionalFeed { node, label } => optional.push(format!(
                "{label} — required by `{node}`, optional in the source: a document without it fails"
            )),
            Finding::TypeMismatch {
                label,
                source,
                target,
            } => clashes.push(format!("{label} — source `{source}`, target `{target}`")),
            Finding::Dropped { label } => dropped.push(label.clone()),
            Finding::Pinned { label, value } => {
                pinned.push(format!("{label} — written as the constant {value:?}"));
            }
            Finding::Recoded {
                label,
                source,
                target,
            } => recoded.push(format!(
                "{label} — {} -> {}",
                source.as_deref().unwrap_or("canonical"),
                target.as_deref().unwrap_or("canonical")
            )),
            Finding::Collapsed {
                node,
                label,
                policy,
            } => collapsed.push(format!(
                "{label} — `{node}` keeps {} of several values",
                if policy == "first" {
                    "the first"
                } else {
                    "a joined form"
                }
            )),
            Finding::FirstMatch { node, element } => collapsed.push(format!(
                "{element} — `{node}` reads the first matching occurrence only"
            )),
        }
    }

    section(
        &mut out,
        "missing required routes (REQUIRED_MISSING at runtime)",
        missing,
    );
    section(&mut out, "type clashes", clashes);
    section(
        &mut out,
        "required routes fed by optional source keys (REQUIRED_MISSING when absent)",
        optional,
    );
    section(&mut out, "dropped (no slot in the target)", dropped);
    section(&mut out, "pinned by the target", pinned);
    section(&mut out, "recoded", recoded);
    section(&mut out, "collapsed by the source on read", collapsed);
    if report.findings.is_empty() {
        out.push_str(
            "\nnothing is lost: every source key has a slot and every required route is fed\n",
        );
    }
    out
}

/// The `DETAIL` cell: a short human summary of what the verdict costs.
fn detail(r: &TransformReport) -> String {
    match r.state {
        TransformState::Lossless => "—".to_string(),
        TransformState::Lossful => format!("drops {}", join_fields(&r.dropped)),
        TransformState::Partial | TransformState::Error => {
            if r.missing_required.is_empty() {
                let clashes: Vec<String> = r
                    .findings
                    .iter()
                    .filter_map(|f| match f {
                        Finding::TypeMismatch { label, .. } => Some(label.clone()),
                        _ => None,
                    })
                    .collect();
                format!("type clash {}", join_fields(&clashes))
            } else {
                format!("missing required {}", join_fields(&r.missing_required))
            }
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
    use crate::contract::{Collapse, KeyContract, RequiredRoute, Selector};
    use pretty_assertions::assert_eq;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const fn key(label: &'static str, ty: &'static str) -> KeyContract {
        KeyContract {
            label,
            key: label,
            scope: &[],
            ty,
            codec: None,
            pinned: None,
        }
    }

    static SOURCE: TransformationContract = TransformationContract {
        spoke: "src",
        keys: &[
            key("A", "identifier"),
            key("B", "decimal"),
            KeyContract {
                codec: Some("iso"),
                ..key("D", "date")
            },
            key("Note", "string"),
        ],
        required: &[],
        collapses: &[Collapse {
            node: "Src.Note",
            label: "Note",
            policy: "join",
        }],
        selectors: &[
            Selector {
                node: "Src.Object",
                element: "Ref",
                selector: &[("type_code", "130")],
                single: true,
            },
            Selector {
                node: "Src.Ref",
                element: "Ref",
                selector: &[],
                single: false,
            },
        ],
    };

    static TARGET: TransformationContract = TransformationContract {
        spoke: "tgt",
        keys: &[
            KeyContract {
                pinned: Some("fixed"),
                ..key("A", "identifier")
            },
            key("B", "string"),
            key("C", "identifier"),
            KeyContract {
                codec: Some("cii-date-102"),
                ..key("D", "date")
            },
        ],
        required: &[
            RequiredRoute {
                node: "Tgt.A",
                route: Route::Hub("A"),
            },
            RequiredRoute {
                node: "Tgt.C",
                route: Route::Hub("C"),
            },
            RequiredRoute {
                node: "Tgt.CopyC",
                route: Route::Clone("C"),
            },
            RequiredRoute {
                node: "Tgt.Version",
                route: Route::Constant("2.1"),
            },
        ],
        collapses: &[],
        selectors: &[],
    };

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
    fn test_analyze_contracts_reports_every_kind_of_finding() {
        let pair = analyze_contracts(&SOURCE, &TARGET);
        assert_eq!(pair.state, TransformState::Partial, "{pair:?}");
        assert_eq!(pair.missing_required, ["C"]);
        assert_eq!(pair.dropped, ["Note"]);
        assert_eq!(
            pair.findings,
            [
                Finding::MissingRequired {
                    node: "Tgt.C".into(),
                    label: "C".into(),
                    via_clone: false
                },
                Finding::MissingRequired {
                    node: "Tgt.CopyC".into(),
                    label: "C".into(),
                    via_clone: true
                },
                Finding::TypeMismatch {
                    label: "B".into(),
                    source: "decimal".into(),
                    target: "string".into()
                },
                Finding::Dropped {
                    label: "Note".into()
                },
                Finding::OptionalFeed {
                    node: "Tgt.A".into(),
                    label: "A".into()
                },
                Finding::Pinned {
                    label: "A".into(),
                    value: "fixed".into()
                },
                Finding::Recoded {
                    label: "D".into(),
                    source: Some("iso".into()),
                    target: Some("cii-date-102".into())
                },
                Finding::FirstMatch {
                    node: "Src.Object".into(),
                    element: "Ref".into()
                },
            ]
        );
        // The constant route never shows up as missing; the collapse of a key
        // the target does not carry is moot (it is dropped anyway).
        assert!(
            !pair
                .findings
                .iter()
                .any(|f| matches!(f, Finding::Collapsed { .. }))
        );
    }

    #[test]
    fn test_analyze_contracts_type_clash_alone_is_partial() {
        static S: TransformationContract = TransformationContract {
            spoke: "s",
            keys: &[key("B", "decimal")],
            required: &[],
            collapses: &[],
            selectors: &[],
        };
        static T: TransformationContract = TransformationContract {
            spoke: "t",
            keys: &[key("B", "string")],
            required: &[],
            collapses: &[],
            selectors: &[],
        };
        let pair = analyze_contracts(&S, &T);
        assert_eq!(pair.state, TransformState::Partial);
        assert!(pair.missing_required.is_empty());
        assert!(pair.findings.iter().all(Finding::is_blocking));
    }

    #[test]
    fn test_analyze_contracts_identity_keeps_notes_but_is_lossless() {
        let pair = analyze_contracts(&SOURCE, &SOURCE);
        assert_eq!(pair.state, TransformState::Lossless);
        assert!(pair.missing_required.is_empty() && pair.dropped.is_empty());
        // The source's own collapse and first-match are declared loss of a
        // *document*, not of the pair: findings, not state.
        assert!(pair.findings.iter().any(|f| matches!(
            f,
            Finding::Collapsed { label, policy, .. } if label == "Note" && policy == "join"
        )));
        assert!(pair.findings.iter().all(|f| !f.is_blocking()));
    }

    #[test]
    fn test_analyze_ubl_to_xrechnung_is_lossless_but_flags_the_optional_feed() {
        // The pair is structurally sound (UBL maps BusinessProcessType), yet
        // plain UBL documents may lack it while XRechnung requires it: the
        // report says so without changing the verdict, matching the runtime
        // REQUIRED_MISSING behaviour.
        let report = analyze(Spoke::UblInvoice, Spoke::XrechnungInvoice);
        assert_eq!(report.state, TransformState::Lossless);
        assert!(report.findings.iter().any(|f| matches!(
            f,
            Finding::OptionalFeed { node, label } if node == "Invoice.ProfileID" && label == "BusinessProcessType"
        )));
        let text = render_pair(&report);
        assert!(
            text.contains(
                "BusinessProcessType — required by `Invoice.ProfileID`, optional in the source"
            ),
            "{text}"
        );
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
    fn test_render_pair_groups_findings_by_kind() {
        let pair = analyze_contracts(&SOURCE, &TARGET);
        let report = TransformReport {
            source: Spoke::ALL[0],
            target: Spoke::ALL[0],
            state: pair.state,
            missing_required: pair.missing_required,
            dropped: pair.dropped,
            findings: pair.findings,
        };
        let text = render_pair(&report);
        assert!(text.starts_with(&format!(
            "{} -> {}: ! partial\n",
            Spoke::ALL[0].name(),
            Spoke::ALL[0].name()
        )));
        assert!(text.contains("missing required routes (REQUIRED_MISSING at runtime) (2):"));
        assert!(text.contains("  C — required by `Tgt.C`; the source does not map it"));
        assert!(text.contains("  C — needed by `Tgt.CopyC` (a clone of it)"));
        assert!(text.contains(
            "required routes fed by optional source keys (REQUIRED_MISSING when absent) (1):\n  A — required by `Tgt.A`, optional in the source"
        ));
        assert!(text.contains("type clashes (1):\n  B — source `decimal`, target `string`"));
        assert!(text.contains("dropped (no slot in the target) (1):\n  Note"));
        assert!(
            text.contains("pinned by the target (1):\n  A — written as the constant \"fixed\"")
        );
        assert!(text.contains("recoded (1):\n  D — iso -> cii-date-102"));
        assert!(text.contains("collapsed by the source on read (1):\n  Ref — `Src.Object` reads the first matching occurrence only"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn test_render_pair_lossless_without_findings_says_so() {
        let report = TransformReport {
            source: Spoke::ALL[0],
            target: Spoke::ALL[0],
            state: TransformState::Lossless,
            missing_required: Vec::new(),
            dropped: Vec::new(),
            findings: Vec::new(),
        };
        let text = render_pair(&report);
        assert!(text.contains("= lossless"));
        assert!(text.contains("nothing is lost"));
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
