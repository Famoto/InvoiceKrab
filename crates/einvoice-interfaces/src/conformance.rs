//! Schema conformance: the checks the mappings declare, derived instead of
//! hand-written.
//!
//! A mapping declares the XSD its format is defined by (`[meta.schema]`,
//! inherited by a CIUS) and the sample documents that prove the mapping right
//! (`[[meta.samples]]`). The build embeds both in the registry
//! ([`Spoke::schema`], [`Spoke::samples`]); [`check`] derives the checks from
//! them. For every sample `D`, read by its spoke `R`, and every spoke `S` with
//! a schema:
//!
//! 1. **Sample validity** — `D` validates against `R`'s XSD, and `R` reads it
//!    without error diagnostics. A broken fixture fails here, at the fixture,
//!    and its pairs are not run.
//! 2. **Emitted validity** — `D` read by `R` and written by `S` validates
//!    against `S`'s XSD, up to `S`'s `known_gaps`: substring patterns of the
//!    schema errors its output is documented to still produce. Every error
//!    must match a gap, and every gap must still match an error of a document
//!    `S` emitted; a stale gap fails, so the lists only ever shrink.
//! 3. **Round trip** — the emitted document read back by `S` yields the same
//!    hub values as reading `D`, for every canonical key `S` covers. The keys
//!    `D` carries that `S` does not cover (dropped), and covered keys `S` pins
//!    to a constant on write, are reported, never failed.
//!
//! Validation runs `xmllint --noout --nonet --schema <xsd>`, with
//! `XML_CATALOG_FILES` pointing at the declared catalog. Without `xmllint`
//! the schema checks are skipped with a notice; the round trip still runs.
//!
//! # Structure
//!
//! - [`Schema`] — a spoke's `[meta.schema]`, embedded in the registry.
//! - [`Xmllint`] — the XSD validator.
//! - [`check`] — runs every derived check into a [`ConformanceReport`]: a
//!   [`SampleReport`] per sample, a [`PairReport`] per sample and target.
//! - [`round_trip`] — the hub comparison behind check 3 ([`RoundTrip`]).
//!
//! # Testing
//!
//! Unit tests below cover gap matching, the round-trip comparison, rendering,
//! and a schema-less run over the bundled mappings. `tests/xsd_validation.rs`
//! runs the full [`check`] (CI installs `xmllint`), and the CLI exposes it as
//! `krab-cli --check`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr as _;

use einvoice_transformator::result::{MappingDiagnostic, MappingResult, Severity};
use rust_decimal::Decimal;

use crate::contract::TransformationContract;
use crate::{Engine, MainKey, Spoke};

/// A spoke's `[meta.schema]`: the XSD its documents are validated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema {
    /// The root XSD, relative to the workspace root.
    pub xsd: &'static str,
    /// The XML catalog resolving the schema's remote imports offline,
    /// relative to the workspace root.
    pub catalog: Option<&'static str>,
    /// Substring patterns of the schema errors the spoke's output is
    /// documented to still produce.
    pub known_gaps: &'static [&'static str],
    /// The samples the spoke is documented to refuse (it cannot represent
    /// their data): a refusal there is reported, a clean write fails.
    pub refuses: &'static [&'static str],
}

/// The `xmllint` XSD validator (libxml2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xmllint;

impl Xmllint {
    /// The validator, when `xmllint` runs on this machine.
    pub fn detect() -> Option<Xmllint> {
        Command::new("xmllint")
            .arg("--version")
            .output()
            .ok()
            .map(|_| Xmllint)
    }

    /// Validates `document` against `schema`, its paths resolved under
    /// `root`, and returns the schema validity errors (none: valid).
    ///
    /// # Errors
    ///
    /// When `xmllint` cannot be run, or fails without reporting a schema error
    /// (the schema does not load, or the document is not well-formed): the
    /// message carries its output.
    pub fn validate(
        self,
        root: &Path,
        schema: &Schema,
        document: &[u8],
    ) -> Result<Vec<String>, String> {
        let mut cmd = Command::new("xmllint");
        cmd.args(["--noout", "--nonet", "--schema"])
            .arg(root.join(schema.xsd))
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(catalog) = schema.catalog {
            cmd.env("XML_CATALOG_FILES", root.join(catalog));
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot run xmllint: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // Feed the document from its own thread, so a full stderr pipe can
        // never deadlock against a full stdin pipe. Dropping `stdin` at the
        // end of the thread closes it.
        let output = std::thread::scope(|scope| {
            scope.spawn(move || {
                // A write error means xmllint stopped reading; its status
                // and stderr say why.
                let _ = stdin.write_all(document);
            });
            child.wait_with_output()
        })
        .map_err(|e| format!("xmllint did not finish: {e}"))?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        let errors: Vec<String> = stderr
            .lines()
            .filter_map(|line| line.split_once("Schemas validity error : "))
            .map(|(_, message)| message.trim().to_string())
            .collect();
        if output.status.success() || !errors.is_empty() {
            Ok(errors)
        } else {
            Err(format!(
                "xmllint failed without a schema error ({}): {}",
                output.status,
                stderr.trim()
            ))
        }
    }
}

/// Everything [`check`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Why the schema checks did not run, when they did not.
    pub schema_skipped: Option<String>,
    /// The spokes with a schema: the target of every sample, registry order.
    pub targets: Vec<Spoke>,
    /// One report per declared sample, in registry order.
    pub samples: Vec<SampleReport>,
    /// Known gaps no error of their spoke's emitted documents matched.
    pub stale_gaps: Vec<(Spoke, &'static str)>,
}

/// The checks of one sample: its own validity, then one pair per target.
/// A broken sample (any [`SampleReport::failures`]) runs no pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    /// The sample, relative to the workspace root.
    pub file: &'static str,
    /// The spoke that reads it.
    pub reader: Spoke,
    /// The XSD the sample was validated against, when the schema check ran.
    pub validated_against: Option<&'static str>,
    /// The schema errors that validation reported (failures: a sample has no
    /// known gaps).
    pub schema_errors: Vec<String>,
    /// What else broke: the file is unreadable, `xmllint` itself failed, or
    /// the reader reported error diagnostics.
    pub errors: Vec<String>,
    /// One report per target, in [`ConformanceReport::targets`] order.
    pub pairs: Vec<PairReport>,
}

/// The checks of one sample written by one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairReport {
    /// The spoke that wrote (and read back) the sample.
    pub target: Spoke,
    /// Whether the emitted document was validated against the target's XSD.
    pub validated: bool,
    /// Schema errors no `known_gaps` pattern covers (failures).
    pub schema_errors: Vec<String>,
    /// Schema errors a `known_gaps` pattern covers (reported, not failed).
    pub known_gap_errors: Vec<String>,
    /// The round trip, when the emitted document could be read back.
    pub round_trip: Option<RoundTrip>,
    /// What else broke: the write, `xmllint` itself, or the read-back.
    pub errors: Vec<String>,
    /// The write's errors for a sample the target documents it refuses
    /// (`[meta.schema].refuses`): reported, not failed.
    pub refused: Vec<String>,
}

impl SampleReport {
    /// The sample's own failures, one line each (its pairs' are their own).
    pub fn failures(&self) -> Vec<String> {
        let mut out = self.errors.clone();
        out.extend(self.schema_errors.iter().map(|e| format!("schema: {e}")));
        out
    }
}

impl PairReport {
    /// Every failure of the pair, one line each: what broke, the uncovered
    /// schema errors, and the covered keys whose values did not survive.
    pub fn failures(&self) -> Vec<String> {
        let mut out = self.errors.clone();
        out.extend(self.schema_errors.iter().map(|e| format!("schema: {e}")));
        if let Some(rt) = &self.round_trip {
            out.extend(rt.changed.iter().map(|m| {
                format!(
                    "round trip: {}: sample {:?}, read back {:?}",
                    m.label, m.sample, m.emitted
                )
            }));
        }
        out
    }
}

/// The round-trip comparison of one emitted document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundTrip {
    /// Covered labels whose values survived.
    pub preserved: Vec<&'static str>,
    /// Covered labels whose values changed (failures).
    pub changed: Vec<Mismatch>,
    /// Covered labels the target pins to a constant on write, whose values
    /// changed accordingly (reported).
    pub pinned: Vec<Mismatch>,
    /// Covered labels the target writes through a codec that recodes the
    /// value (a Latin-1 transliteration, a many-to-one code table), whose
    /// values changed accordingly (reported).
    pub recoded: Vec<Mismatch>,
    /// Covered labels the sample lacks and the engine derived on write by an
    /// EN 16931 calculation rule (reported).
    pub derived: Vec<Mismatch>,
    /// Labels the sample carries that the target does not cover (reported).
    pub dropped: Vec<&'static str>,
}

/// A label whose values differ between the sample and the emitted document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The scope-qualified canonical label.
    pub label: &'static str,
    /// Its values read from the sample, in document order.
    pub sample: Vec<String>,
    /// Its values read back from the emitted document, in document order.
    pub emitted: Vec<String>,
}

impl ConformanceReport {
    /// Every failure, one line each: the samples', the pairs', stale gaps.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        for sample in &self.samples {
            for failure in sample.failures() {
                out.push(format!("{}: {failure}", sample.file));
            }
            for pair in &sample.pairs {
                for failure in pair.failures() {
                    out.push(format!(
                        "{} -> {}: {failure}",
                        sample.file,
                        pair.target.name()
                    ));
                }
            }
        }
        for (spoke, gap) in &self.stale_gaps {
            out.push(format!("{}: stale known gap `{gap}`", spoke.name()));
        }
        out
    }

    /// Whether every check passed (a skipped schema check is not a failure).
    pub fn is_ok(&self) -> bool {
        self.failures().is_empty()
    }

    /// The human-readable report: per sample its validity and one line per
    /// target, with the gaps, pins and dropped keys reported under it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let pairs: usize = self.samples.iter().map(|s| s.pairs.len()).sum();
        let _ = writeln!(
            out,
            "schema conformance: {} sample(s), {} spoke(s) with a schema, {pairs} pair(s)",
            self.samples.len(),
            self.targets.len()
        );
        if let Some(why) = &self.schema_skipped {
            let _ = writeln!(
                out,
                "notice: {why}: schema validation skipped, the round trips still run"
            );
        }
        for sample in &self.samples {
            let _ = writeln!(
                out,
                "\nsample {} (read by {})",
                sample.file,
                sample.reader.name()
            );
            if let Some(xsd) = sample.validated_against {
                let verdict = if sample.schema_errors.is_empty() {
                    "valid"
                } else {
                    "invalid"
                };
                let _ = writeln!(out, "  {verdict} against {xsd}");
            }
            let failures = sample.failures();
            for failure in &failures {
                let _ = writeln!(out, "  FAILED: {failure}");
            }
            if !failures.is_empty() {
                let _ = writeln!(out, "  (pairs not run: the sample itself failed)");
            }
            for pair in &sample.pairs {
                render_pair(&mut out, pair);
            }
        }
        if !self.stale_gaps.is_empty() {
            out.push_str(
                "\nstale known gaps (no schema error matches them any more; delete them):\n",
            );
            for (spoke, gap) in &self.stale_gaps {
                let _ = writeln!(out, "  {}: {gap}", spoke.name());
            }
        }
        let failures = self.failures().len();
        let _ = writeln!(
            out,
            "\n{}",
            if failures == 0 {
                "result: every check passed".to_string()
            } else {
                format!("result: {failures} failure(s)")
            }
        );
        out
    }
}

/// Renders one pair: a verdict line, then its failures and reported findings.
fn render_pair(out: &mut String, pair: &PairReport) {
    let mut verdict = Vec::new();
    if pair.validated {
        let gaps = match pair.known_gap_errors.len() {
            0 => String::new(),
            n => format!(" up to {n} known-gap error(s)"),
        };
        verdict.push(match pair.schema_errors.len() {
            0 => format!("valid{gaps}"),
            n => format!("invalid: {n} schema error(s) beyond its known gaps"),
        });
    }
    if let Some(rt) = &pair.round_trip {
        verdict.push(format!("{} key(s) round-trip", rt.preserved.len()));
    }
    if !pair.refused.is_empty() {
        verdict.push("refused, as declared".to_string());
    }
    let failures = pair.failures();
    let status = if failures.is_empty() { "ok" } else { "FAILED" };
    let _ = write!(out, "  -> {}: {status}", pair.target.name());
    if !verdict.is_empty() {
        let _ = write!(out, " ({})", verdict.join(", "));
    }
    out.push('\n');
    for failure in &failures {
        let _ = writeln!(out, "       {failure}");
    }
    for error in &pair.refused {
        let _ = writeln!(out, "       refused: {error}");
    }
    for error in &pair.known_gap_errors {
        let _ = writeln!(out, "       known gap: {error}");
    }
    if let Some(rt) = &pair.round_trip {
        for pin in &rt.pinned {
            let _ = writeln!(
                out,
                "       pinned: {}: sample {:?}, written as {:?}",
                pin.label, pin.sample, pin.emitted
            );
        }
        for derived in &rt.derived {
            let _ = writeln!(
                out,
                "       derived: {}: written as {:?}",
                derived.label, derived.emitted
            );
        }
        for recode in &rt.recoded {
            let _ = writeln!(
                out,
                "       recoded: {}: sample {:?}, read back {:?}",
                recode.label, recode.sample, recode.emitted
            );
        }
        if !rt.dropped.is_empty() {
            wrap_list(
                out,
                &format!("dropped ({}): ", rt.dropped.len()),
                &rt.dropped,
            );
        }
    }
}

/// Writes `items` comma-separated after `lead`, wrapped at about 100 columns
/// and indented under the pair line.
fn wrap_list(out: &mut String, lead: &str, items: &[&str]) {
    const INDENT: &str = "       ";
    let mut line = format!("{INDENT}{lead}");
    let continuation = " ".repeat(INDENT.len() + lead.len());
    for (i, item) in items.iter().enumerate() {
        let sep = if i + 1 < items.len() { "," } else { "" };
        if i > 0 && line.len() + 1 + item.len() + sep.len() > 100 {
            out.push_str(line.trim_end());
            out.push('\n');
            line = continuation.clone();
        } else if i > 0 {
            line.push(' ');
        }
        line.push_str(item);
        line.push_str(sep);
    }
    out.push_str(&line);
    out.push('\n');
}

/// Runs every schema-conformance check the bundled mappings declare (see the
/// module docs), against the files under `root`, the workspace root their
/// paths are relative to. With `xmllint` = `None` the schema checks are
/// skipped and the report says so; the round trips still run.
pub fn check(root: &Path, xmllint: Option<Xmllint>) -> ConformanceReport {
    let targets: Vec<(Spoke, &'static Schema)> = Spoke::ALL
        .iter()
        .filter_map(|&spoke| spoke.schema().map(|schema| (spoke, schema)))
        .collect();
    // Per target, every schema error its emitted documents produced: the
    // evidence a known gap is still live.
    let mut target_errors: Vec<Vec<String>> = vec![Vec::new(); targets.len()];

    let mut samples = Vec::new();
    for &reader in Spoke::ALL {
        for &file in reader.samples() {
            samples.push(check_sample(
                root,
                xmllint,
                reader,
                file,
                &targets,
                &mut target_errors,
            ));
        }
    }

    let mut stale_gaps = Vec::new();
    if xmllint.is_some() {
        for ((spoke, schema), errors) in targets.iter().zip(&target_errors) {
            for gap in stale(schema.known_gaps, errors) {
                stale_gaps.push((*spoke, gap));
            }
        }
    }

    ConformanceReport {
        schema_skipped: xmllint
            .is_none()
            .then(|| "xmllint not found on PATH".to_string()),
        targets: targets.iter().map(|(spoke, _)| *spoke).collect(),
        samples,
        stale_gaps,
    }
}

/// Checks one sample, then writes it through every target.
fn check_sample(
    root: &Path,
    xmllint: Option<Xmllint>,
    reader: Spoke,
    file: &'static str,
    targets: &[(Spoke, &'static Schema)],
    target_errors: &mut [Vec<String>],
) -> SampleReport {
    let mut report = SampleReport {
        file,
        reader,
        validated_against: None,
        schema_errors: Vec::new(),
        errors: Vec::new(),
        pairs: Vec::new(),
    };
    let bytes = match std::fs::read(root.join(file)) {
        Ok(bytes) => bytes,
        Err(e) => {
            report.errors.push(format!("cannot read the sample: {e}"));
            return report;
        }
    };

    if let (Some(xmllint), Some(schema)) = (xmllint, reader.schema()) {
        match xmllint.validate(root, schema, &bytes) {
            Ok(errors) => {
                report.validated_against = Some(schema.xsd);
                report.schema_errors = errors;
            }
            Err(e) => report.errors.push(e),
        }
    }
    let hub = match read_clean(reader, &bytes) {
        Ok(hub) => hub,
        Err(errors) => {
            report
                .errors
                .extend(errors.into_iter().map(|e| format!("read: {e}")));
            return report;
        }
    };
    if !report.failures().is_empty() {
        return report;
    }

    for (&(target, schema), errors) in targets.iter().zip(target_errors.iter_mut()) {
        report.pairs.push(check_pair(
            root, xmllint, file, &hub, target, schema, errors,
        ));
    }
    report
}

/// Writes `hub` (the sample's) with `target`, validates the document, and
/// reads it back for the round trip. Every schema error is also appended to
/// `errors`, the target's evidence for its known gaps.
fn check_pair(
    root: &Path,
    xmllint: Option<Xmllint>,
    file: &str,
    hub: &MainKey,
    target: Spoke,
    schema: &Schema,
    errors: &mut Vec<String>,
) -> PairReport {
    let mut pair = PairReport {
        target,
        validated: false,
        schema_errors: Vec::new(),
        known_gap_errors: Vec::new(),
        round_trip: None,
        errors: Vec::new(),
        refused: Vec::new(),
    };
    let declared_refusal = schema.refuses.contains(&file);
    let written = Engine::new()
        .from_hub(target, hub.clone())
        .map_err(|e| vec![e.to_string()])
        .and_then(clean);
    let xml = match written {
        Ok(_) if declared_refusal => {
            pair.errors.push(format!(
                "declared refusal is stale: `{file}` now writes cleanly; remove it from `[meta.schema].refuses`"
            ));
            return pair;
        }
        Ok(xml) => xml,
        Err(errors) if declared_refusal => {
            pair.refused = errors;
            return pair;
        }
        Err(errors) => {
            pair.errors
                .extend(errors.into_iter().map(|e| format!("write: {e}")));
            return pair;
        }
    };

    if let Some(xmllint) = xmllint {
        match xmllint.validate(root, schema, xml.as_bytes()) {
            Ok(found) => {
                pair.validated = true;
                (pair.schema_errors, pair.known_gap_errors) =
                    split_by_gaps(&found, schema.known_gaps);
                errors.extend(found);
            }
            Err(e) => pair.errors.push(e),
        }
    }

    match read_clean(target, xml.as_bytes()) {
        Ok(emitted) => pair.round_trip = Some(round_trip(hub, &emitted, target.contract())),
        Err(errors) => pair
            .errors
            .extend(errors.into_iter().map(|e| format!("read back: {e}"))),
    }
    pair
}

/// Reads `bytes` with `spoke`, failing on an unparsable document or any
/// error diagnostic.
fn read_clean(spoke: Spoke, bytes: &[u8]) -> Result<MainKey, Vec<String>> {
    match Engine::new().to_hub(spoke, bytes) {
        Ok(result) => clean(result),
        Err(e) => Err(vec![e.to_string()]),
    }
}

/// The value of a mapping run without error diagnostics, else the errors.
fn clean<T>(result: MappingResult<T>) -> Result<T, Vec<String>> {
    let errors: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(describe)
        .collect();
    match result.value {
        Some(value) if errors.is_empty() => Ok(value),
        Some(_) => Err(errors),
        None => Err(vec!["the mapper produced no value".to_string()]),
    }
}

/// One diagnostic as `[CODE] node: message`.
fn describe(d: &MappingDiagnostic) -> String {
    format!("[{}] {}: {}", d.code, d.source_node, d.message)
}

/// Splits schema `errors` into those no `gaps` pattern matches (failures) and
/// those one does (known gaps). Patterns match as substrings.
pub fn split_by_gaps(errors: &[String], gaps: &[&str]) -> (Vec<String>, Vec<String>) {
    errors
        .iter()
        .cloned()
        .partition(|e| !gaps.iter().any(|gap| e.contains(gap)))
}

/// The `gaps` patterns no error in `errors` matches any more.
pub fn stale(gaps: &[&'static str], errors: &[String]) -> Vec<&'static str> {
    gaps.iter()
        .copied()
        .filter(|gap| !errors.iter().any(|e| e.contains(gap)))
        .collect()
}

/// Compares the hub read from a sample with the hub read back from the
/// document `target` wrote from it, label by label (see [`MainKey::values`]).
///
/// A label `target` covers must carry the same values in the same order;
/// otherwise it changed, unless `target` pins it to a constant on write. A
/// label the sample carries and `target` does not cover is dropped.
pub fn round_trip(
    sample: &MainKey,
    emitted: &MainKey,
    target: &TransformationContract,
) -> RoundTrip {
    let sample = by_label(sample);
    let emitted = by_label(emitted);
    let labels: BTreeSet<&'static str> = sample.keys().chain(emitted.keys()).copied().collect();
    let mut rt = RoundTrip::default();
    for label in labels {
        let before = sample.get(label).cloned().unwrap_or_default();
        let after = emitted.get(label).cloned().unwrap_or_default();
        match target.key(label) {
            None => {
                // A reader only fills keys its spoke covers, so only the
                // sample side can hold values here.
                if !before.is_empty() {
                    rt.dropped.push(label);
                }
            }
            Some(key) if same_values(key.ty, &before, &after) => rt.preserved.push(label),
            Some(key) => {
                let mismatch = Mismatch {
                    label,
                    sample: before,
                    emitted: after,
                };
                let derivable = MainKey::DERIVATIONS.iter().any(|(t, _, _)| *t == label);
                if key.pinned.is_some() {
                    rt.pinned.push(mismatch);
                } else if mismatch.sample.is_empty() && derivable {
                    rt.derived.push(mismatch);
                } else if key.codec.is_some() {
                    rt.recoded.push(mismatch);
                } else {
                    rt.changed.push(mismatch);
                }
            }
        }
    }
    rt
}

/// Whether two label value lists are the same: equal strings, or for a
/// `decimal` label equal numbers (`19` and `19.00` are one value at two
/// scales; a format may fix the scale it writes).
fn same_values(ty: &str, before: &[String], after: &[String]) -> bool {
    if before == after {
        return true;
    }
    ty == "decimal"
        && before.len() == after.len()
        && before.iter().zip(after).all(|(a, b)| {
            matches!(
                (Decimal::from_str(a), Decimal::from_str(b)),
                (Ok(a), Ok(b)) if a == b
            )
        })
}

/// A hub's populated values grouped by label, each label's in walk order.
fn by_label(hub: &MainKey) -> BTreeMap<&'static str, Vec<String>> {
    let mut map: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for (label, value) in hub.values() {
        map.entry(label).or_default().push(value);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::KeyContract;
    use crate::generated::hub::InvoiceLinesItem;
    use rust_decimal::Decimal;

    /// A target covering the invoice number, the line ids, and a document
    /// currency it pins to `EUR` on write.
    static TARGET: TransformationContract = TransformationContract {
        spoke: "target",
        keys: &[
            KeyContract {
                label: "DocumentCurrency",
                key: "DocumentCurrency",
                scope: &[],
                ty: "currency",
                codec: None,
                pinned: Some("EUR"),
            },
            KeyContract {
                label: "InvoiceLines/LineId",
                key: "LineId",
                scope: &["InvoiceLines"],
                ty: "identifier",
                codec: None,
                pinned: None,
            },
            KeyContract {
                label: "InvoiceNumber",
                key: "InvoiceNumber",
                scope: &[],
                ty: "identifier",
                codec: None,
                pinned: None,
            },
        ],
        required: &[],
        collapses: &[],
        selectors: &[],
    };

    fn line(id: &str) -> InvoiceLinesItem {
        InvoiceLinesItem {
            line_id: Some(id.into()),
            ..Default::default()
        }
    }

    fn hub(lines: &[&str]) -> MainKey {
        MainKey {
            invoice_number: Some("INV-1".into()),
            document_currency: Some("EUR".into()),
            payable_amount: Some(Decimal::new(11900, 2)),
            invoice_lines: lines.iter().map(|id| line(id)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_hub_values_are_scope_qualified_and_in_item_order() {
        let values = hub(&["1", "2"]).values();
        assert!(values.contains(&("InvoiceNumber", "INV-1".to_string())));
        assert!(
            values.contains(&("PayableAmount", "119.00".to_string())),
            "decimals keep their scale"
        );
        let ids: Vec<&str> = values
            .iter()
            .filter(|(label, _)| *label == "InvoiceLines/LineId")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn test_round_trip_preserves_covered_keys_and_reports_dropped_ones() {
        let rt = round_trip(&hub(&["1", "2"]), &hub(&["1", "2"]), &TARGET);
        assert_eq!(
            rt.preserved,
            ["DocumentCurrency", "InvoiceLines/LineId", "InvoiceNumber"]
        );
        assert!(rt.changed.is_empty() && rt.pinned.is_empty());
        assert_eq!(rt.dropped, ["PayableAmount"], "not covered: reported");
    }

    #[test]
    fn test_round_trip_changed_covered_key_is_a_failure() {
        let rt = round_trip(&hub(&["1", "2"]), &hub(&["1"]), &TARGET);
        assert_eq!(
            rt.changed,
            [Mismatch {
                label: "InvoiceLines/LineId",
                sample: vec!["1".into(), "2".into()],
                emitted: vec!["1".into()],
            }]
        );
    }

    #[test]
    fn test_round_trip_pinned_key_is_reported_not_failed() {
        let mut emitted = hub(&["1"]);
        emitted.document_currency = Some("EUR".into());
        let mut sample = hub(&["1"]);
        sample.document_currency = Some("USD".into());
        let rt = round_trip(&sample, &emitted, &TARGET);
        assert!(rt.changed.is_empty(), "{:?}", rt.changed);
        assert_eq!(rt.pinned.len(), 1);
        assert_eq!(rt.pinned[0].label, "DocumentCurrency");
    }

    #[test]
    fn test_round_trip_compares_decimals_by_value_and_reports_recodes() {
        let mut contract_keys = TARGET.keys.to_vec();
        contract_keys.push(KeyContract {
            label: "PayableAmount",
            key: "PayableAmount",
            scope: &[],
            ty: "decimal",
            codec: Some("amount-2"),
            pinned: None,
        });
        contract_keys.push(KeyContract {
            label: "SellerName",
            key: "SellerName",
            scope: &[],
            ty: "string",
            codec: Some("latin-1"),
            pinned: None,
        });
        let keys: &'static [KeyContract] = Box::leak(contract_keys.into_boxed_slice());
        let target = TransformationContract { keys, ..TARGET };
        let mut sample = hub(&["1"]);
        sample.payable_amount = Some(Decimal::from_str("19").unwrap());
        sample.seller_name = Some("A — B".into());
        let mut emitted = hub(&["1"]);
        emitted.payable_amount = Some(Decimal::from_str("19.00").unwrap());
        emitted.seller_name = Some("A - B".into());
        let rt = round_trip(&sample, &emitted, &target);
        assert!(rt.changed.is_empty(), "{:?}", rt.changed);
        assert!(
            rt.preserved.contains(&"PayableAmount"),
            "same number: {rt:?}"
        );
        assert_eq!(rt.recoded.len(), 1, "{rt:?}");
        assert_eq!(rt.recoded[0].label, "SellerName");
    }

    #[test]
    fn test_round_trip_reports_a_derived_total() {
        let mut keys = TARGET.keys.to_vec();
        keys.push(KeyContract {
            label: "SumOfInvoiceLineNetAmount",
            key: "SumOfInvoiceLineNetAmount",
            scope: &[],
            ty: "decimal",
            codec: None,
            pinned: None,
        });
        let keys: &'static [KeyContract] = Box::leak(keys.into_boxed_slice());
        let target = TransformationContract { keys, ..TARGET };
        let sample = hub(&["1"]);
        let mut emitted = hub(&["1"]);
        emitted.sum_of_invoice_line_net_amount = Some(Decimal::from_str("100").unwrap());
        let rt = round_trip(&sample, &emitted, &target);
        assert!(rt.changed.is_empty(), "{:?}", rt.changed);
        assert_eq!(rt.derived.len(), 1, "{rt:?}");
    }

    #[test]
    fn test_split_by_gaps_and_stale_gaps() {
        let errors = vec![
            "Element 'A': Missing child element(s). Expected is ( Header ).".to_string(),
            "Element 'B': This element is not expected.".to_string(),
        ];
        let (uncovered, covered) = split_by_gaps(&errors, &["Expected is ( Header )"]);
        assert_eq!(uncovered, [errors[1].clone()]);
        assert_eq!(covered, [errors[0].clone()]);
        assert_eq!(
            stale(&["Expected is ( Header )", "gone"], &errors),
            ["gone"],
            "a pattern no error matches is stale"
        );
        assert_eq!(
            stale(&["gone"], &[]),
            ["gone"],
            "no errors: every gap is stale"
        );
    }

    #[test]
    fn test_check_without_xmllint_runs_the_round_trips_and_notes_the_skip() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = check(&root, None);
        assert!(report.is_ok(), "{}", report.render());
        assert!(!report.samples.is_empty(), "the mappings declare samples");
        for sample in &report.samples {
            assert_eq!(sample.validated_against, None);
            assert_eq!(sample.pairs.len(), report.targets.len());
            for pair in &sample.pairs {
                assert!(!pair.validated);
                // Every pair round-trips, except a refusal the target declares.
                assert!(
                    pair.round_trip.is_some() || !pair.refused.is_empty(),
                    "{}",
                    pair.target.name()
                );
            }
        }
        assert!(
            report.stale_gaps.is_empty(),
            "staleness needs the schema check"
        );
        let text = report.render();
        assert!(text.contains("notice: xmllint not found on PATH"), "{text}");
        assert!(text.contains("result: every check passed"), "{text}");
    }

    #[test]
    fn test_render_lists_failures_findings_and_stale_gaps() {
        let report = ConformanceReport {
            schema_skipped: None,
            targets: vec![Spoke::UblInvoice],
            samples: vec![
                SampleReport {
                    file: "testfiles/a.xml",
                    reader: Spoke::UblInvoice,
                    validated_against: Some("a.xsd"),
                    schema_errors: Vec::new(),
                    errors: Vec::new(),
                    pairs: vec![PairReport {
                        target: Spoke::UblInvoice,
                        validated: true,
                        schema_errors: vec!["Element 'B': This element is not expected.".into()],
                        known_gap_errors: vec!["Expected is ( Header )".into()],
                        round_trip: Some(RoundTrip {
                            preserved: vec!["InvoiceNumber"],
                            changed: vec![Mismatch {
                                label: "InvoiceLines/LineId",
                                sample: vec!["1".into()],
                                emitted: Vec::new(),
                            }],
                            pinned: Vec::new(),
                            recoded: Vec::new(),
                            derived: Vec::new(),
                            dropped: vec!["PayableAmount"],
                        }),
                        errors: Vec::new(),
                        refused: Vec::new(),
                    }],
                },
                SampleReport {
                    file: "testfiles/broken.xml",
                    reader: Spoke::UblInvoice,
                    validated_against: Some("a.xsd"),
                    schema_errors: vec!["Element 'Invoice': No matching global declaration".into()],
                    errors: Vec::new(),
                    pairs: Vec::new(),
                },
            ],
            stale_gaps: vec![(Spoke::UblInvoice, "gone")],
        };
        let text = report.render();
        for needle in [
            "sample testfiles/a.xml (read by ubl-invoice:2.1)",
            "  valid against a.xsd",
            "  -> ubl-invoice:2.1: FAILED (invalid: 1 schema error(s) beyond its known gaps, 1 key(s) round-trip)",
            "       schema: Element 'B': This element is not expected.",
            "       round trip: InvoiceLines/LineId: sample [\"1\"], read back []",
            "       known gap: Expected is ( Header )",
            "       dropped (1): PayableAmount",
            "  invalid against a.xsd\n  FAILED: schema: Element 'Invoice': No matching global declaration\n  (pairs not run: the sample itself failed)",
            "  ubl-invoice:2.1: gone",
            "result: 4 failure(s)",
        ] {
            assert!(text.contains(needle), "{needle:?} in\n{text}");
        }
        assert_eq!(
            report.failures(),
            [
                "testfiles/a.xml -> ubl-invoice:2.1: schema: Element 'B': This element is not expected.",
                "testfiles/a.xml -> ubl-invoice:2.1: round trip: InvoiceLines/LineId: sample [\"1\"], read back []",
                "testfiles/broken.xml: schema: Element 'Invoice': No matching global declaration",
                "ubl-invoice:2.1: stale known gap `gone`",
            ]
        );
    }

    #[test]
    fn test_render_marks_a_pair_valid_up_to_its_known_gaps() {
        let pair = PairReport {
            target: Spoke::Fatturapa,
            validated: true,
            schema_errors: Vec::new(),
            known_gap_errors: vec!["Expected is ( Header )".into()],
            round_trip: None,
            errors: Vec::new(),
            refused: Vec::new(),
        };
        let mut out = String::new();
        render_pair(&mut out, &pair);
        assert!(
            out.starts_with("  -> fatturapa:1.2.2: ok (valid up to 1 known-gap error(s))\n"),
            "{out}"
        );
        assert!(pair.failures().is_empty());
    }

    #[test]
    fn test_a_declared_refusal_is_reported_and_a_stale_one_fails() {
        let schema = Schema {
            xsd: "unused.xsd",
            catalog: None,
            known_gaps: &[],
            refuses: &["doc.xml"],
        };
        let root = Path::new(".");
        let mut errors = Vec::new();
        // An empty hub cannot be written as UBL: the declared refusal holds.
        let refused = check_pair(
            root,
            None,
            "doc.xml",
            &MainKey::default(),
            Spoke::UblInvoice,
            &schema,
            &mut errors,
        );
        assert!(refused.failures().is_empty(), "{:?}", refused.failures());
        assert!(
            refused
                .refused
                .iter()
                .any(|e| e.contains("REQUIRED_MISSING")),
            "{refused:?}"
        );
        let mut out = String::new();
        render_pair(&mut out, &refused);
        assert!(out.contains("ok (refused, as declared)"), "{out}");

        // A complete hub writes cleanly: the declaration is stale.
        let sample = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testfiles/xrechnung-3.0.2-beispiel.xml"
        ))
        .unwrap();
        let hub = read_clean(Spoke::XrechnungInvoice, &sample).expect("sample reads cleanly");
        let stale = check_pair(
            root,
            None,
            "doc.xml",
            &hub,
            Spoke::UblInvoice,
            &schema,
            &mut errors,
        );
        assert!(
            stale
                .failures()
                .iter()
                .any(|f| f.contains("declared refusal is stale")),
            "{:?}",
            stale.failures()
        );
    }

    #[test]
    fn test_wrap_list_wraps_long_lists_under_the_lead() {
        let items: Vec<String> = (0..30).map(|i| format!("Key{i:02}")).collect();
        let items: Vec<&str> = items.iter().map(String::as_str).collect();
        let mut out = String::new();
        wrap_list(&mut out, "dropped (30): ", &items);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() > 1, "{out}");
        assert!(lines.iter().all(|l| l.len() <= 100), "{out}");
        assert!(lines[1].starts_with(&" ".repeat(7 + "dropped (30): ".len())));
        assert!(out.trim_end().ends_with("Key29"), "{out}");
    }
}
